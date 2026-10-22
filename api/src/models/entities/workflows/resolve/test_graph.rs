//! A test graph of workflows, agents, tools, and toolsets over the in-memory reader
//!
//! The workflow and toolset resolver tests both build their graphs with it.

use chrono::{Duration, Utc};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

use super::{MemoryGraph, ResolvedWorkflow};
use crate::models::entities::digests::ThorDigest;
use crate::models::entities::toolsets::ResolvedToolSet;
use crate::models::{
    AgentConfig, AgentEntity, AgenticTool, AgenticToolBacking, AssociationKind,
    ContainerToolBacking, Entity, EntityKinds, EntityMetadata, HarnessCapabilities, HarnessSpec,
    HarnessToolPolicy, K8sEnvironment, ToolEffect, ToolEntity, ToolInstanceEntity, ToolMechanism,
    ToolSetEntity, ToolSetInclusion, ToolSetInstanceEntity, ToolSetInstanceStatus,
    ToolSetInstanceTrigger, ToolSetMember, ToolSetSelector, ToolSpec, ToolVersion,
    WorkflowCatalogEntity, WorkloadEnvironment, WorkloadSpec,
};

/// The main group every test entity is in
pub(crate) const MINERVA: &str = "minerva-rslv";

/// A second group some entities are also in
pub(crate) const MARS: &str = "mars-rslv";

/// An agentic tool spec served by Thorium's MCP server
///
/// # Arguments
///
/// * `function_name` - The function name a model calls the tool by
pub(crate) fn agentic(function_name: &str) -> ToolSpec {
    // an agentic tool behind an MCP tool of the same name
    ToolSpec::Agentic(AgenticTool {
        function_name: function_name.to_owned(),
        model_description: format!("Runs {function_name}"),
        backing: AgenticToolBacking::ThoriumMcp {
            tool: function_name.to_owned(),
        },
        ..AgenticTool::default()
    })
}

/// An agentic tool spec backed by a pinned Image tool instance
///
/// # Arguments
///
/// * `function_name` - The function name a model calls the tool by
/// * `image` - The Image tool instance the backing pins
pub(crate) fn container(function_name: &str, image: Uuid) -> ToolSpec {
    // an agentic tool whose backing pins the Image instance
    ToolSpec::Agentic(AgenticTool {
        function_name: function_name.to_owned(),
        model_description: format!("Runs {function_name} in a container"),
        backing: AgenticToolBacking::Container(ContainerToolBacking {
            tool_instance: image,
            ..ContainerToolBacking::default()
        }),
        ..AgenticTool::default()
    })
}

/// An Image tool spec
///
/// # Arguments
///
/// * `timeout` - The job timeout, which tells two image specs apart
pub(crate) fn image(timeout: u64) -> ToolSpec {
    // a workload whose timeout makes its hash unique
    let mut workload = WorkloadSpec::default();
    workload.execution.timeout = Some(timeout);
    ToolSpec::Image(workload)
}

/// A Harness tool spec running a K8s image that can call tools behind Thorium every way and
/// hand files to them
///
/// # Arguments
///
/// * `tag` - The image tag, which tells two harness specs apart
pub(crate) fn harness(tag: u64) -> ToolSpec {
    // a harness supporting every mechanism for tools behind Thorium, with file I/O
    let capabilities = HarnessCapabilities {
        mechanisms: [
            ToolMechanism::ThoriumApi,
            ToolMechanism::Cli,
            ToolMechanism::Mcp,
        ]
        .into_iter()
        .collect(),
        file_io: true,
        ..HarnessCapabilities::default()
    };
    harness_with(tag, capabilities, HarnessToolPolicy::default())
}

/// A Harness tool spec running a K8s image with some capabilities and tool policy
///
/// # Arguments
///
/// * `tag` - The image tag, which tells two harness specs apart
/// * `capabilities` - The ways the harness can carry out tool calls
/// * `tool_policy` - The limits on the tools agents running in it may use
pub(crate) fn harness_with(
    tag: u64,
    capabilities: HarnessCapabilities,
    tool_policy: HarnessToolPolicy,
) -> ToolSpec {
    // a K8s workload whose image tag makes its hash unique
    let workload = WorkloadSpec {
        environment: WorkloadEnvironment::K8s(K8sEnvironment {
            image: format!("repo/harness:{tag}"),
            ..K8sEnvironment::default()
        }),
        ..WorkloadSpec::default()
    };
    ToolSpec::Harness(HarnessSpec {
        workload,
        capabilities,
        tool_policy,
        ..HarnessSpec::default()
    })
}

/// A Harness tool spec whose declared mechanisms are exactly some mechanisms
///
/// # Arguments
///
/// * `tag` - The image tag, which tells two harness specs apart
/// * `mechanisms` - The mechanisms the harness declares
pub(crate) fn harness_supporting(tag: u64, mechanisms: &[ToolMechanism]) -> ToolSpec {
    // only these mechanisms, without file I/O
    let capabilities = HarnessCapabilities {
        mechanisms: mechanisms.iter().copied().collect(),
        ..HarnessCapabilities::default()
    };
    harness_with(tag, capabilities, HarnessToolPolicy::default())
}

/// An agentic tool spec built into a harness
///
/// # Arguments
///
/// * `function_name` - The function name a model calls the tool by
pub(crate) fn native(function_name: &str) -> ToolSpec {
    // an agentic tool the harness carries out itself
    ToolSpec::Agentic(AgenticTool {
        function_name: function_name.to_owned(),
        model_description: format!("Runs the built in {function_name}"),
        backing: AgenticToolBacking::HarnessNative {
            tool: function_name.to_owned(),
            config: None,
        },
        ..AgenticTool::default()
    })
}

/// An agentic tool spec that declares some effects
///
/// # Arguments
///
/// * `function_name` - The function name a model calls the tool by
/// * `effects` - The effects the tool declares
pub(crate) fn with_effects(function_name: &str, effects: &[ToolEffect]) -> ToolSpec {
    // an MCP tool that declares the effects
    let mut spec = agentic(function_name);
    if let ToolSpec::Agentic(tool) = &mut spec {
        tool.capabilities.effects = effects.iter().copied().collect();
    }
    spec
}

/// A test graph of workflows, agents, tools, and toolsets over the in-memory reader
#[derive(Default)]
pub(crate) struct Graph {
    /// The graph the resolver reads
    pub(crate) reader: MemoryGraph,
    /// The instances of each tool, oldest first
    history: HashMap<Uuid, Vec<Uuid>>,
    /// The number of entities added so far, which orders their creation times
    created: i64,
    /// The agents with a harness edge
    harnessed: HashSet<Uuid>,
    /// The harness instance each entry agent was given when its workflow was added
    given_harnesses: HashMap<Uuid, Uuid>,
}

impl Graph {
    /// Add an entity
    ///
    /// # Arguments
    ///
    /// * `kind` - The entity's kind
    /// * `name` - The entity's name
    /// * `groups` - The entity's groups
    /// * `metadata` - The entity's metadata
    pub(crate) fn put(
        &mut self,
        kind: EntityKinds,
        name: &str,
        groups: &[&str],
        metadata: EntityMetadata,
    ) -> Uuid {
        // each entity is created a second after the last one
        self.created += 1;
        let entity = Entity {
            id: Uuid::new_v4(),
            name: name.to_owned(),
            kind,
            metadata,
            description: None,
            submitter: "thorium".to_owned(),
            groups: groups.iter().map(|group| (*group).to_owned()).collect(),
            tags: HashMap::default(),
            image: None,
            created: Utc::now() + Duration::seconds(self.created),
        };
        let id = entity.id;
        self.reader.insert(entity);
        id
    }

    /// Add an agent with a config built from its name
    ///
    /// # Arguments
    ///
    /// * `name` - The agent's name
    /// * `groups` - The agent's groups
    pub(crate) fn agent(&mut self, name: &str, groups: &[&str]) -> Uuid {
        // a config that names the agent
        let config = AgentConfig {
            system_prompt: Some(format!("You are {name}")),
            function_name: Some(name.replace('-', "_")),
            model_description: Some(format!("Delegate to {name}")),
            ..AgentConfig::default()
        };
        self.agent_with(name, groups, config)
    }

    /// Add an agent with a config
    ///
    /// # Arguments
    ///
    /// * `name` - The agent's name
    /// * `groups` - The agent's groups
    /// * `config` - The agent's config
    pub(crate) fn agent_with(&mut self, name: &str, groups: &[&str], config: AgentConfig) -> Uuid {
        // hash the config the way the API does
        let agent = AgentEntity {
            config_hash: config.config_hash().unwrap(),
            config,
            modified: Utc::now(),
            ..AgentEntity::default()
        };
        self.put(
            EntityKinds::Agent,
            name,
            groups,
            EntityMetadata::Agent(agent),
        )
    }

    /// Add a tool and its first instance, version `1.0.0`
    ///
    /// # Arguments
    ///
    /// * `name` - The tool's name
    /// * `groups` - The tool's groups
    /// * `spec` - The first instance's spec
    pub(crate) fn tool(&mut self, name: &str, groups: &[&str], spec: ToolSpec) -> (Uuid, Uuid) {
        // add the root with no instances yet
        let root = ToolEntity {
            kind: spec.kind(),
            latest: None,
            latest_version: None,
            latest_spec_hash: None,
            modified: Utc::now(),
            instances: Vec::new(),
            harness_env: None,
            disabled_models: std::collections::BTreeSet::new(),
        };
        let root = self.put(EntityKinds::Tool, name, groups, EntityMetadata::Tool(root));
        // publish its first instance
        let instance = self.publish(root, spec, "1.0.0");
        (root, instance)
    }

    /// Publish a new instance of a tool, which becomes its latest
    ///
    /// # Arguments
    ///
    /// * `root` - The tool
    /// * `spec` - The new instance's spec
    /// * `version` - The new instance's version label
    pub(crate) fn publish(&mut self, root: Uuid, spec: ToolSpec, version: &str) -> Uuid {
        // a container backing hashes the spec hash of the instance it pins
        let pinned = spec.pinned_instance().map(|pinned| self.spec_hash(pinned));
        let spec_hash = ThorDigest::tool_spec(&spec, pinned.as_deref()).unwrap();
        // the instance is in its tool's groups and named after it
        let tool = self.reader.entity(root).unwrap().clone();
        let groups = tool
            .groups
            .iter()
            .map(String::as_str)
            .collect::<Vec<&str>>();
        let previous = self
            .history
            .get(&root)
            .and_then(|found| found.last().copied());
        let instance = ToolInstanceEntity {
            tool: root,
            version: ToolVersion::from(version),
            previous,
            spec_hash,
            deprecated: false,
            spec,
            portability: None,
            provides: Vec::new(),
        };
        let name = format!("{}@{version}", tool.name);
        let id = self.put(
            EntityKinds::ToolInstance,
            &name,
            &groups,
            EntityMetadata::ToolInstance(instance),
        );
        self.reader.edge(AssociationKind::PublishedFrom, id, root);
        self.history.entry(root).or_default().push(id);
        self.refresh_latest(root);
        id
    }

    /// Deprecate a tool instance and move its tool's latest
    ///
    /// # Arguments
    ///
    /// * `root` - The tool
    /// * `instance` - The instance to deprecate
    pub(crate) fn deprecate(&mut self, root: Uuid, instance: Uuid) {
        // mark the instance deprecated
        if let Some(EntityMetadata::ToolInstance(meta)) = self
            .reader
            .entity_mut(instance)
            .map(|entity| &mut entity.metadata)
        {
            meta.deprecated = true;
        }
        self.refresh_latest(root);
    }

    /// Point a tool's latest at its newest instance that isn't deprecated
    ///
    /// # Arguments
    ///
    /// * `root` - The tool
    pub(crate) fn refresh_latest(&mut self, root: Uuid) {
        // find the newest instance that isn't deprecated
        let latest = self.history.get(&root).and_then(|found| {
            found.iter().rev().copied().find(|id| {
                !matches!(
                    self.reader.entity(*id).map(|entity| &entity.metadata),
                    Some(EntityMetadata::ToolInstance(meta)) if meta.deprecated
                )
            })
        });
        let hash = latest.map(|id| self.spec_hash(id));
        if let Some(EntityMetadata::Tool(tool)) = self
            .reader
            .entity_mut(root)
            .map(|entity| &mut entity.metadata)
        {
            tool.latest = latest;
            tool.latest_spec_hash = hash;
        }
    }

    /// Get a tool instance's spec hash
    ///
    /// # Arguments
    ///
    /// * `instance` - The tool instance
    pub(crate) fn spec_hash(&self, instance: Uuid) -> String {
        // only a tool instance has a spec hash
        match self.reader.entity(instance).map(|entity| &entity.metadata) {
            Some(EntityMetadata::ToolInstance(meta)) => meta.spec_hash.clone(),
            _ => panic!("{instance} is not a tool instance"),
        }
    }

    /// Add a workflow catalog orchestrating some agents
    ///
    /// An entry agent without a harness edge is given a pinned harness in its own groups, since
    /// an entry agent must run in one.
    ///
    /// # Arguments
    ///
    /// * `name` - The workflow's name
    /// * `groups` - The workflow's groups
    /// * `agents` - The agents it orchestrates
    /// * `entry` - Its entry agent
    pub(crate) fn workflow(
        &mut self,
        name: &str,
        groups: &[&str],
        agents: &[Uuid],
        entry: Option<Uuid>,
    ) -> Uuid {
        // give an entry agent without a harness a pinned one
        if let Some(entry) = entry
            && !self.harnessed.contains(&entry)
            && let Some(agent) = self.reader.entity(entry)
        {
            // name the harness after the agent and put it in the agent's groups
            let harness_name = format!("harness-{}", agent.name);
            let owned = agent.groups.clone();
            let agent_groups = owned.iter().map(String::as_str).collect::<Vec<&str>>();
            // pin the agent to the harness's first instance
            let (_, instance) = self.tool(&harness_name, &agent_groups, harness(1));
            self.edge(AssociationKind::RunsIn, entry, instance);
            self.given_harnesses.insert(entry, instance);
        }
        self.bare_workflow(name, groups, agents, entry)
    }

    /// Get the harness instance an entry agent was given when its workflow was added
    ///
    /// # Arguments
    ///
    /// * `agent` - The entry agent
    pub(crate) fn given_harness(&self, agent: Uuid) -> Option<Uuid> {
        // only entry agents added without a harness were given one
        self.given_harnesses.get(&agent).copied()
    }

    /// Add a workflow catalog orchestrating some agents, giving its entry agent no harness
    ///
    /// # Arguments
    ///
    /// * `name` - The workflow's name
    /// * `groups` - The workflow's groups
    /// * `agents` - The agents it orchestrates
    /// * `entry` - Its entry agent
    pub(crate) fn bare_workflow(
        &mut self,
        name: &str,
        groups: &[&str],
        agents: &[Uuid],
        entry: Option<Uuid>,
    ) -> Uuid {
        // the catalog with its entry agent
        let catalog = WorkflowCatalogEntity {
            entry_agent: entry,
            auto_instance: true,
            modified: Utc::now(),
            ..WorkflowCatalogEntity::default()
        };
        let id = self.put(
            EntityKinds::WorkflowCatalog,
            name,
            groups,
            EntityMetadata::WorkflowCatalog(catalog),
        );
        // an Orchestrates edge to each agent
        for agent in agents {
            self.reader.edge(AssociationKind::Orchestrates, id, *agent);
        }
        id
    }

    /// Change a workflow catalog's metadata
    ///
    /// # Arguments
    ///
    /// * `workflow` - The workflow
    /// * `change` - Changes the catalog
    pub(crate) fn change_workflow(
        &mut self,
        workflow: Uuid,
        change: impl FnOnce(&mut WorkflowCatalogEntity),
    ) {
        // change the catalog in place
        if let Some(EntityMetadata::WorkflowCatalog(meta)) = self
            .reader
            .entity_mut(workflow)
            .map(|entity| &mut entity.metadata)
        {
            change(meta);
        }
    }

    /// Add an edge
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of edge
    /// * `source` - The edge's source
    /// * `target` - The edge's target
    pub(crate) fn edge(&mut self, kind: AssociationKind, source: Uuid, target: Uuid) {
        // remember which agents run in a harness
        if kind == AssociationKind::RunsIn {
            self.harnessed.insert(source);
        }
        // store both rows of the edge
        self.reader.edge(kind, source, target);
    }

    /// Store a resolved instance as the newest instance of its workflow, tagged as the API tags it
    ///
    /// # Arguments
    ///
    /// * `resolved` - The resolved workflow
    /// * `revision` - The revision to store it at
    pub(crate) fn store(&mut self, resolved: &ResolvedWorkflow, revision: u64) -> Uuid {
        // store the instance in its workflow's groups
        let mut instance = resolved.instance.clone();
        instance.revision = revision;
        let groups = resolved.workflow.groups.clone();
        let refs = groups.iter().map(String::as_str).collect::<Vec<&str>>();
        let name = format!("{}#r{revision}", resolved.workflow.name);
        let id = self.put(
            EntityKinds::WorkflowInstance,
            &name,
            &refs,
            EntityMetadata::WorkflowInstance(instance.clone()),
        );
        // tag it with its workflow, agents, and followed roots
        self.reader
            .tag(id, "WorkflowId", &instance.workflow.to_string());
        for agent in &instance.agents {
            self.reader
                .tag(id, "InstanceAgent", &agent.agent.to_string());
        }
        for tool in &resolved.follows_tools {
            self.reader
                .tag(id, "InstanceFollowsTool", &tool.to_string());
        }
        for tool_set in &resolved.follows_tool_sets {
            self.reader
                .tag(id, "InstanceFollowsToolSet", &tool_set.to_string());
        }
        // the workflow names it as its newest instance, and as its latest when it's ready
        if let Some(EntityMetadata::WorkflowCatalog(meta)) = self
            .reader
            .entity_mut(instance.workflow)
            .map(|entity| &mut entity.metadata)
        {
            meta.newest_instance = Some(id);
            if instance.is_ready() {
                meta.latest_instance = Some(id);
            }
            meta.highest_revision = meta.highest_revision.max(revision);
        }
        id
    }

    /// Add a toolset
    ///
    /// # Arguments
    ///
    /// * `name` - The toolset's name
    /// * `groups` - The toolset's groups
    /// * `read_only` - Whether only tools without effects are members
    /// * `selector` - The tag that selects members, if any
    pub(crate) fn tool_set(
        &mut self,
        name: &str,
        groups: &[&str],
        read_only: bool,
        selector: Option<ToolSetSelector>,
    ) -> Uuid {
        // the toolset with its settings and no instance yet
        let meta = ToolSetEntity {
            read_only,
            selector,
            auto_instance: true,
            modified: Utc::now(),
            ..ToolSetEntity::default()
        };
        self.put(
            EntityKinds::ToolSet,
            name,
            groups,
            EntityMetadata::ToolSet(meta),
        )
    }

    /// Tag an entity in some groups only
    ///
    /// # Arguments
    ///
    /// * `id` - The entity to tag
    /// * `key` - The tag key
    /// * `value` - The tag value
    /// * `groups` - The groups to tag it in
    pub(crate) fn tag_in(&mut self, id: Uuid, key: &str, value: &str, groups: &[&str]) {
        // store the tag in just these groups
        self.reader.tag_in(id, key, value, groups);
    }

    /// Describe a stored tool instance as a toolset member
    ///
    /// # Arguments
    ///
    /// * `instance` - The tool instance
    /// * `inclusion` - How the toolset included it
    pub(crate) fn member(&self, instance: Uuid, inclusion: ToolSetInclusion) -> ToolSetMember {
        // the member is described as the instance is stored
        let entity = self.reader.entity(instance).unwrap();
        let EntityMetadata::ToolInstance(meta) = &entity.metadata else {
            panic!("{instance} is not a tool instance");
        };
        let mut groups = entity.groups.clone();
        groups.sort();
        ToolSetMember {
            tool: meta.tool,
            tool_instance: instance,
            name: entity.name.clone(),
            groups,
            version: meta.version.to_string(),
            spec_hash: meta.spec_hash.clone(),
            function_name: meta.spec.function_name().unwrap_or_default().to_owned(),
            inclusion,
        }
    }

    /// Store a toolset instance, tagged as the API tags it, as its toolset's newest instance
    ///
    /// A ready instance becomes the toolset's latest instance.
    ///
    /// # Arguments
    ///
    /// * `instance` - The toolset instance
    pub(crate) fn insert_tool_set_instance(&mut self, instance: ToolSetInstanceEntity) -> Uuid {
        // store the instance in its toolset's groups
        let tool_set = self.reader.entity(instance.tool_set).unwrap().clone();
        let refs = tool_set
            .groups
            .iter()
            .map(String::as_str)
            .collect::<Vec<&str>>();
        let name = format!("{}#r{}", tool_set.name, instance.revision);
        let ready = instance.is_ready();
        let follows = instance
            .members
            .iter()
            .filter(|member| member.inclusion.follows_latest())
            .map(|member| member.tool.to_string())
            .collect::<Vec<String>>();
        let id = self.put(
            EntityKinds::ToolSetInstance,
            &name,
            &refs,
            EntityMetadata::ToolSetInstance(instance),
        );
        // tag it with its toolset and the tools its members follow at latest
        self.reader.tag(id, "ToolSetId", &tool_set.id.to_string());
        for tool in &follows {
            self.reader.tag(id, "ToolSetFollowsTool", tool);
        }
        // a ready instance is the toolset's latest
        if ready
            && let Some(EntityMetadata::ToolSet(meta)) = self
                .reader
                .entity_mut(tool_set.id)
                .map(|entity| &mut entity.metadata)
        {
            meta.latest_instance = Some(id);
        }
        id
    }

    /// Store a resolved toolset instance at a revision
    ///
    /// # Arguments
    ///
    /// * `resolved` - The resolved toolset
    /// * `revision` - The revision to store it at
    pub(crate) fn store_tool_set(&mut self, resolved: &ResolvedToolSet, revision: u64) -> Uuid {
        // store a copy at the revision
        let mut instance = resolved.instance.clone();
        instance.revision = revision;
        self.insert_tool_set_instance(instance)
    }

    /// Store a toolset instance with some members, ready or not
    ///
    /// # Arguments
    ///
    /// * `tool_set` - The toolset
    /// * `members` - Each member's tool instance and how it was included
    /// * `ready` - Whether the instance is ready
    pub(crate) fn tool_set_instance(
        &mut self,
        tool_set: Uuid,
        members: &[(Uuid, ToolSetInclusion)],
        ready: bool,
    ) -> Uuid {
        // describe the members as they're stored
        let members = members
            .iter()
            .map(|(instance, inclusion)| self.member(*instance, *inclusion))
            .collect::<Vec<ToolSetMember>>();
        let (groups, read_only, selector) = match self.reader.entity(tool_set) {
            Some(Entity {
                groups,
                metadata: EntityMetadata::ToolSet(meta),
                ..
            }) => (groups.clone(), meta.read_only, meta.selector.clone()),
            _ => panic!("{tool_set} is not a toolset"),
        };
        let status = if ready {
            ToolSetInstanceStatus::Ready
        } else {
            ToolSetInstanceStatus::Invalid {
                errors: vec!["broken".to_owned()],
            }
        };
        let mut instance = ToolSetInstanceEntity {
            tool_set,
            tool_set_groups: groups,
            revision: 1,
            previous: None,
            status,
            warnings: Vec::new(),
            trigger: ToolSetInstanceTrigger::Manual,
            triggered_by: String::new(),
            read_only,
            selector,
            members,
            excluded: Vec::new(),
            digest: String::new(),
        };
        instance.sort_lists();
        instance.digest = instance.compute_digest().unwrap();
        self.insert_tool_set_instance(instance)
    }
}
