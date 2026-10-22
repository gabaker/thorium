use base64::Engine;
use cidr::{Ipv4Cidr, Ipv6Cidr};
use futures::{StreamExt, TryStreamExt, stream};
use rand::seq::IndexedRandom;
use rand::{Rng, SeedableRng, seq::IteratorRandom};
use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::LazyLock;
use uuid::Uuid;

use crate::client::{ClientSettings, Users};
use crate::models::entities::network_activity::{
    NetConState, NetworkConnection, TransportLayerProtocol,
};
use crate::models::entities::rules::{SigmaActionToTake, SigmaAutoFlag};
use crate::models::{
    AgentConfig, AgentEntityRequest, AgenticTool, AgenticToolBacking, AllowedEndpoint,
    CredentialKind, EntityUpdate, HarnessCapabilities, HarnessSpec, K8sEnvironment,
    ModelCatalogEntryRequest, ModelCredentialEntityRequest, ModelEndpointEntityRequest,
    ModelProfileEntityRequest, ModelProvider, ModelSecret, ModelsConfUpdate, SchedulingSettings,
    SystemSettingsUpdate, SystemSettingsUpdateParams, ToolMechanism, ToolSetEntityRequest,
    ToolSetInclusion, ToolSetInstanceEntity, ToolSetInstanceEntityRequest, ToolSetInstanceStatus,
    ToolSetInstanceTrigger, ToolSetMember, WorkloadEnvironment, WorkloadSpec,
};
use crate::models::{
    ArgStrategy, Buffer, BulkReactionResponse, ChildFilters, Cleanup, CollectionEntityRequest,
    CollectionKind, CompiledFunction, CompiledInstruction, Confidence, CriticalSector,
    DecompiledFunction, Dependencies, DependencyPassStrategy, DeviceEntityRequest, Entity,
    EntityMetadata, EntityMetadataRequest, EntityMetadataUpdate, EntityRequest,
    EphemeralDependencySettings, FileSystemEntity, FileSystemFolderEntity, FilesHandler, Flag,
    GenericJobArgs, GroupAllowed, GroupRequest, GroupUpdate, GroupUsersRequest, GroupUsersUpdate,
    ImageLifetime, ImageRequest, ImageScaler, IncidentRequest, IpBlock, IpBlockRaw, Ipv4Block,
    Ipv6Block, KwargDependency, NetworkPolicyCustomK8sRule, NetworkPolicyCustomLabel,
    NetworkPolicyPort, NetworkPolicyRequest, NetworkPolicyRuleRaw, NetworkProtocol,
    NodeRegistration, OriginRequest, OutputCollection, OutputDisplayType, PeImportEntity,
    PeSectionEntity, Pipeline, PipelineRequest, Pools, ReactionCreation, ReactionRequest,
    RepoCheckout, RepoDependencySettings, RepoRequest, Resources, ResourcesRequest,
    ResultDependencySettings, SampleDependencySettings, SampleRequest, ScopedTokenRequest,
    SigmaRule, SigmaRuleAppliesTo, StageLogsAdd, ToolEntityRequest, ToolSpec, ToolVersion,
    UserCreate, UserRole, VendorEntityRequest, Volume, VolumeTypes, WindowsProcessEntity,
    WindowsProcessTreeEntity, WorkerDeleteMap, WorkerRegistrationList,
};
use crate::models::{
    BareMetalEnvironment, InstancedAgent, InstancedTool, InstancedToolBinding, KvmEnvironment,
    ToolKind, WindowsEnvironment, WorkflowCatalogEntityRequest, WorkflowInstanceEntity,
    WorkflowInstanceEntityRequest, WorkflowInstanceStatus, WorkflowInstanceTrigger,
};
use crate::test_utilities;
use crate::{Error, Thorium};

static UTF8_CHARS: LazyLock<Vec<char>> = LazyLock::new(|| {
    [
        0x0030..=0x0039,   // Numbers
        0x0041..=0x005A,   // Uppercase letters
        0x0061..=0x007A,   // Lowercase letters
        0x2600..=0x26FF,   // Miscellaneous Symbols
        0x1F600..=0x1F64F, // Emoticons
    ]
    .into_iter()
    .flat_map(IntoIterator::into_iter)
    .filter_map(std::char::from_u32)
    .collect()
});

macro_rules! gen_int {
    ($min:expr, $max:expr) => {
        rand::rngs::SmallRng::from_os_rng().random_range($min..$max)
    };
}

/// Generate an option with the given probability that it's `Some`
macro_rules! gen_opt {
    ($prob:literal, $build:expr) => {
        if rand::rngs::SmallRng::from_os_rng().random_bool($prob) {
            Some($build)
        } else {
            None
        }
    };
}

/// generate a random string
#[must_use]
pub fn gen_string(len: usize) -> String {
    // build the possible values we can generate
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789-";
    let mut rng = rand::rngs::SmallRng::from_os_rng();
    // generate the correct number of values
    (0..len)
        .map(|_| {
            let idx = rng.random_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

/// generate a random string with UTF-8 "characters"
fn gen_utf8_string(num_chars: usize) -> String {
    let mut rng = rand::rngs::SmallRng::from_os_rng();
    // Generate a random string from the selected chars
    (0..num_chars)
        .map(|_| *UTF8_CHARS.choose(&mut rng).unwrap())
        .collect()
}

/// Generate a random group request
#[allow(dead_code)]
#[must_use]
pub fn gen_group() -> GroupRequest {
    let name = gen_string(50);
    GroupRequest::new(name.clone())
        .owners(GroupUsersRequest::default().direct("thorium"))
        .description(format!("{name} description"))
}

/// Create a number of random groups in Thorium
///
/// # Arguments
///
/// * `cnt` - The number of groups to create
/// * `client` - The client to use when creating these images
#[allow(dead_code)]
pub async fn groups(cnt: usize, client: &Thorium) -> Result<Vec<GroupRequest>, Error> {
    // create a 20 random groups
    let groups: Vec<GroupRequest> = (0..cnt).map(|_| gen_group()).collect();
    // create groups
    for group in &groups {
        client.groups.create(group).await?;
    }
    Ok(groups)
}

/// Create a number of random users in Thorium
///
/// # Arguments
///
/// * `cnt` - The number of users to create
/// * `client` - The client to get a host string from when creating these users
#[allow(dead_code)]
pub async fn users(cnt: usize, client: &Thorium) -> Result<Vec<String>, Error> {
    // generate usernames
    let usernames: Vec<String> = (0..cnt).map(|_| gen_string(24)).collect();
    // generate user creation blueprints
    let blueprints: Vec<UserCreate> = usernames
        .iter()
        .map(|username| {
            // use my
            UserCreate::new(username, gen_string(64), "fake@fake.gov").skip_verification()
        })
        .collect();
    // use default client settings
    let settings = ClientSettings::default();
    // get our secret key
    let secret_key = Some(&test_utilities::CONF.thorium.secret_key);
    // create these users in Thorium
    for bp in blueprints {
        Users::create(&client.host, bp, secret_key, &settings).await?;
    }
    Ok(usernames)
}

#[allow(dead_code)]
pub async fn client(client: &Thorium) -> Result<Thorium, Error> {
    // generate username and password
    let username = gen_string(24);
    let password = gen_string(64);
    // build user create blueprint
    let bp = UserCreate::new(&username, &password, "fake@fake.gov")
        .skip_verification()
        .role(UserRole::Developer {
            k8s: true,
            bare_metal: true,
            windows: true,
            kvm: false,
            external: true,
        });
    // use default client settings
    let settings = ClientSettings::default();
    // get our secret key
    let secret_key = Some(&test_utilities::CONF.thorium.secret_key);
    // create user in Thorium
    Users::create(&client.host, bp, secret_key, &settings).await?;
    // build client for this user
    Thorium::build(&client.host)
        .basic_auth(username, password)
        .build()
        .await
}

/// Create a new user that is a member of the given groups and build a client for them
///
/// # Arguments
///
/// * `admin` - An admin client to create the user and update the groups with
/// * `groups` - The groups to add the new user to
#[allow(dead_code)]
pub async fn client_in_groups(admin: &Thorium, groups: &[String]) -> Result<Thorium, Error> {
    // create a new user and build a client for them
    let new_client = client(admin).await?;
    // get the username of our new user
    let username = new_client.users.info().await?.username;
    // add our new user to each group
    for group in groups {
        // build an update adding our new user as a direct member
        let update =
            GroupUpdate::default().users(GroupUsersUpdate::default().direct_add(&username));
        admin.groups.update(group, &update).await?;
    }
    Ok(new_client)
}

/// Build a client that authenticates with a new scoped token of another client's user
///
/// The scoped token is limited to a new group owned by that user.
///
/// # Arguments
///
/// * `client` - The client whose user creates the scoped token
pub async fn scoped_client(client: &Thorium) -> Result<Thorium, Error> {
    // create a group so the scoped token has a scope
    let group = groups(1, client).await?.remove(0).name;
    // build a uniquely named scoped token request
    let name = format!("scoped{}", gen_string(16));
    let req = ScopedTokenRequest::new(name).group(&group);
    // create the scoped token for this client's user
    let scoped = client.users.create_scoped_token(&req).await?;
    // build a client that authenticates with the scoped token
    Thorium::build(&client.host)
        .token(&scoped.token)
        .build()
        .await
}

/// A test user along with the token and client it authenticates with
#[allow(dead_code)]
pub struct TestUser {
    /// The name of this user
    pub username: String,
    /// The raw token this user authenticates with
    pub token: String,
    /// A client authenticated as this user
    pub client: Thorium,
}

impl TestUser {
    /// Get the value of the authorization header this user sends
    #[must_use]
    pub fn auth_header(&self) -> String {
        // base64 encode our token like the client does
        let encoded = base64::engine::general_purpose::STANDARD.encode(self.token.as_bytes());
        format!("token {encoded}")
    }
}

/// Generate a short suffix that keeps one test's readable names unique across runs
#[must_use]
pub fn name_suffix() -> String {
    // the first characters of a random UUID are unique enough for one test run
    Uuid::new_v4().simple().to_string()[..6].to_owned()
}

/// Build a readable test name, such as a Jupiter moon for a user or a Roman god for a group,
/// made unique with a per-test suffix
///
/// # Arguments
///
/// * `base` - The readable part of the name
/// * `suffix` - The suffix shared by every name in one test
#[must_use]
pub fn readable_name(base: &str, suffix: &str) -> String {
    format!("{base}-{suffix}")
}

/// Create a plain test user named after a moon of Jupiter
///
/// # Arguments
///
/// * `admin` - The admin client to create the user with
/// * `moon` - The moon the user is named after
/// * `suffix` - The test's name suffix
pub async fn moon_user(admin: &Thorium, moon: &str, suffix: &str) -> Result<TestUser, Error> {
    // a plain user with a readable name
    let name = readable_name(moon, suffix);
    named_user_with_role(admin, &name, UserRole::User).await
}

/// Create a test group named after a Roman god that allows everything
///
/// Returns the name of the new group.
///
/// # Arguments
///
/// * `admin` - The admin client to create the group with
/// * `god` - The god the group is named after
/// * `suffix` - The test's name suffix
/// * `roles` - The users holding each role in the group
pub async fn god_group(
    admin: &Thorium,
    god: &str,
    suffix: &str,
    roles: &GroupRoles,
) -> Result<String, Error> {
    // a group that allows everything, with a readable name
    let name = readable_name(god, suffix);
    named_group_with_roles(&name, roles, GroupAllowed::default(), admin).await
}

/// Create a user with a specific role and get a client authenticated as them
///
/// # Arguments
///
/// * `client` - The client to get a host string from when creating this user
/// * `role` - The role to give this user
#[allow(dead_code)]
pub async fn user_with_role(client: &Thorium, role: UserRole) -> Result<TestUser, Error> {
    // create the user under a random name
    named_user_with_role(client, &gen_string(24), role).await
}

/// Create a user with a specific name and role and get a client authenticated as them
///
/// # Arguments
///
/// * `client` - The client to get a host string from when creating this user
/// * `username` - The name to give this user
/// * `role` - The role to give this user
pub async fn named_user_with_role(
    client: &Thorium,
    username: &str,
    role: UserRole,
) -> Result<TestUser, Error> {
    // own the name and generate a password
    let username = username.to_owned();
    let password = gen_string(64);
    // build user create blueprint
    let bp = UserCreate::new(&username, &password, "fake@fake.gov")
        .skip_verification()
        .role(role);
    // use default client settings
    let settings = ClientSettings::default();
    // get our secret key
    let secret_key = Some(&test_utilities::CONF.thorium.secret_key);
    // create user in Thorium
    Users::create(&client.host, bp, secret_key, &settings).await?;
    // get a token for this user
    let (token, _) =
        Thorium::auth(&client.host, &username, &password, &reqwest::Client::new()).await?;
    // build a client for this user with its token
    let user_client = Thorium::build(&client.host).token(&token).build().await?;
    Ok(TestUser {
        username,
        token,
        client: user_client,
    })
}

/// The users holding each role in a test group
#[derive(Debug, Default, Clone)]
#[allow(dead_code)]
pub struct GroupRoles {
    /// The owners of this group (the admin `thorium` is always an owner)
    pub owners: Vec<String>,
    /// The managers of this group
    pub managers: Vec<String>,
    /// The users of this group
    pub users: Vec<String>,
    /// The monitors of this group
    pub monitors: Vec<String>,
}

impl GroupRoles {
    /// Add an owner to this group
    ///
    /// # Arguments
    ///
    /// * `user` - The user to add as an owner
    #[must_use]
    pub fn owner(mut self, user: &str) -> Self {
        self.owners.push(user.to_owned());
        self
    }

    /// Add a manager to this group
    ///
    /// # Arguments
    ///
    /// * `user` - The user to add as a manager
    #[must_use]
    pub fn manager(mut self, user: &str) -> Self {
        self.managers.push(user.to_owned());
        self
    }

    /// Add a user to this group
    ///
    /// # Arguments
    ///
    /// * `user` - The user to add as a user
    #[must_use]
    pub fn user(mut self, user: &str) -> Self {
        self.users.push(user.to_owned());
        self
    }

    /// Add a monitor to this group
    ///
    /// # Arguments
    ///
    /// * `user` - The user to add as a monitor
    #[must_use]
    pub fn monitor(mut self, user: &str) -> Self {
        self.monitors.push(user.to_owned());
        self
    }
}

/// Create a group with chosen role holders and allow flags
///
/// Returns the name of the new group.
///
/// # Arguments
///
/// * `roles` - The users holding each role in this group
/// * `allowed` - What this group allows
/// * `client` - The admin client to create the group with
#[allow(dead_code)]
pub async fn group_with_roles(
    roles: &GroupRoles,
    allowed: GroupAllowed,
    client: &Thorium,
) -> Result<String, Error> {
    // build the members of a role from a list of usernames
    let members = |names: &[String]| {
        names
            .iter()
            .fold(GroupUsersRequest::default(), GroupUsersRequest::direct)
    };
    // start from a random group owned by the admin
    let mut req = gen_group();
    // add the extra owners
    for owner in &roles.owners {
        req.owners = req.owners.direct(owner);
    }
    // set the other roles and allow flags
    req = req
        .managers(members(&roles.managers))
        .users(members(&roles.users))
        .monitors(members(&roles.monitors));
    req.allowed = allowed;
    // create this group
    client.groups.create(&req).await?;
    Ok(req.name)
}

/// Create a group with a chosen name, role holders, and allow flags
///
/// Returns the name of the new group.
///
/// # Arguments
///
/// * `name` - The name of the group, such as a Roman god with a per-test suffix
/// * `roles` - The users holding each role in this group
/// * `allowed` - What this group allows
/// * `client` - The admin client to create the group with
#[allow(dead_code)]
pub async fn named_group_with_roles(
    name: &str,
    roles: &GroupRoles,
    allowed: GroupAllowed,
    client: &Thorium,
) -> Result<String, Error> {
    // build the members of a role from a list of usernames
    let members = |names: &[String]| {
        names
            .iter()
            .fold(GroupUsersRequest::default(), GroupUsersRequest::direct)
    };
    // start from a group owned by the admin
    let mut owners = GroupUsersRequest::default().direct("thorium");
    for owner in &roles.owners {
        owners = owners.direct(owner);
    }
    let mut req = GroupRequest::new(name.to_owned())
        .owners(owners)
        .managers(members(&roles.managers))
        .users(members(&roles.users))
        .monitors(members(&roles.monitors))
        .description(format!("{name} description"));
    req.allowed = allowed;
    // create this group
    client.groups.create(&req).await?;
    Ok(req.name)
}

/// Generate an agent config that another agent can delegate to
///
/// # Arguments
///
/// * `function_name` - The function name other agents delegate to it with
#[allow(dead_code)]
#[must_use]
pub fn gen_agent_config(function_name: &str) -> AgentConfig {
    // a random prompt with a delegation name and description
    AgentConfig {
        system_prompt: Some(format!("You are {function_name}")),
        prompt: Some(gen_string(24)),
        max_turns: Some(10),
        function_name: Some(function_name.to_owned()),
        model_description: Some(format!("Delegate to {function_name}")),
        ..AgentConfig::default()
    }
}

/// Build a request to create an agent
///
/// # Arguments
///
/// * `name` - The name of the agent
/// * `groups` - The groups to create the agent in
/// * `req` - The agent's config and edges
#[allow(dead_code)]
#[must_use]
pub fn gen_agent(name: &str, groups: &[&str], req: AgentEntityRequest) -> EntityRequest {
    // wrap the agent request in an entity request
    EntityRequest::new(name, EntityMetadataRequest::Agent(req), groups.to_vec())
}

/// Build a request to create a toolset
///
/// # Arguments
///
/// * `name` - The name of the toolset
/// * `groups` - The groups to create the toolset in
/// * `req` - The toolset's settings and included tools
#[allow(dead_code)]
#[must_use]
pub fn gen_toolset(name: &str, groups: &[&str], req: ToolSetEntityRequest) -> EntityRequest {
    // wrap the toolset request in an entity request
    EntityRequest::new(name, EntityMetadataRequest::ToolSet(req), groups.to_vec())
}

/// Build a ready toolset instance of a toolset from its member tool instances, with its digest
///
/// Each member records the tool instance's current groups and is included at latest.
///
/// # Arguments
///
/// * `tool_set` - The toolset, in all of its groups
/// * `revision` - The instance's revision
/// * `previous` - The instance this one supersedes
/// * `members` - The member tool instances, in all of their groups
///
/// # Panics
///
/// Panics if a member isn't a tool instance or the digest can't be computed
#[allow(dead_code)]
#[must_use]
pub fn gen_toolset_instance(
    tool_set: &Entity,
    revision: u64,
    previous: Option<Uuid>,
    members: &[&Entity],
) -> ToolSetInstanceEntity {
    // describe each member as it's stored now
    let members = members
        .iter()
        .map(|entity| {
            let EntityMetadata::ToolInstance(instance) = &entity.metadata else {
                panic!("{} is not a tool instance", entity.id);
            };
            ToolSetMember {
                tool: instance.tool,
                tool_instance: entity.id,
                name: entity.name.clone(),
                groups: entity.groups.clone(),
                version: instance.version.to_string(),
                spec_hash: instance.spec_hash.clone(),
                function_name: instance.spec.function_name().unwrap_or_default().to_owned(),
                inclusion: ToolSetInclusion::ExplicitLatest,
            }
        })
        .collect();
    // take the settings from the toolset
    let (read_only, selector) = match &tool_set.metadata {
        EntityMetadata::ToolSet(meta) => (meta.read_only, meta.selector.clone()),
        _ => (false, None),
    };
    let mut instance = ToolSetInstanceEntity {
        tool_set: tool_set.id,
        tool_set_groups: tool_set.groups.clone(),
        revision,
        previous,
        status: ToolSetInstanceStatus::Ready,
        warnings: Vec::new(),
        trigger: ToolSetInstanceTrigger::Manual,
        triggered_by: String::new(),
        read_only,
        selector,
        members,
        excluded: Vec::new(),
        digest: String::new(),
    };
    instance.digest = instance
        .compute_digest()
        .expect("failed to compute a toolset instance digest");
    instance
}

/// Build a request to submit a toolset instance in its toolset's groups
///
/// # Arguments
///
/// * `instance` - The toolset instance to submit
///
/// # Panics
///
/// Panics if the instance can't be serialized
#[allow(dead_code)]
#[must_use]
pub fn gen_toolset_instance_req(instance: &ToolSetInstanceEntity) -> EntityRequest {
    // the instance is named by the API, so any name is sent
    let req = ToolSetInstanceEntityRequest::new(instance)
        .expect("failed to serialize a toolset instance");
    EntityRequest::new(
        "toolset-instance",
        EntityMetadataRequest::ToolSetInstance(req),
        instance.tool_set_groups.clone(),
    )
}

/// Build a request to create a workflow catalog
///
/// # Arguments
///
/// * `name` - The name of the workflow
/// * `groups` - The groups to create the workflow in
/// * `req` - The workflow's agents and settings
#[allow(dead_code)]
#[must_use]
pub fn gen_workflow(
    name: &str,
    groups: &[&str],
    req: WorkflowCatalogEntityRequest,
) -> EntityRequest {
    // wrap the workflow request in an entity request
    EntityRequest::new(
        name,
        EntityMetadataRequest::WorkflowCatalog(req),
        groups.to_vec(),
    )
}

/// Describe a tool instance as a workflow instance snapshots it, followed at latest
///
/// # Arguments
///
/// * `entity` - The tool instance, in all of its groups
///
/// # Panics
///
/// Panics if the entity isn't a tool instance
#[allow(dead_code)]
#[must_use]
pub fn gen_instanced_tool(entity: &Entity) -> InstancedTool {
    // copy what the snapshot records from the stored instance
    let EntityMetadata::ToolInstance(instance) = &entity.metadata else {
        panic!("{} is not a tool instance", entity.id);
    };
    InstancedTool {
        tool: instance.tool,
        tool_instance: entity.id,
        name: entity.name.clone(),
        groups: entity.groups.clone(),
        kind: instance.spec.kind(),
        version: instance.version.to_string(),
        spec_hash: instance.spec_hash.clone(),
        function_name: instance.spec.function_name().map(ToOwned::to_owned),
        pinned: false,
        dependency: false,
    }
}

/// Build a ready workflow instance of a workflow from its agents and their tool instances, with
/// its digests
///
/// Each agent and tool records its current groups, every tool is followed at latest, and the
/// first agent is the entry agent. A Harness instance among an agent's tool instances is the
/// harness it runs in, and the agent's other tool instances are each bound with the Thorium API.
///
/// # Arguments
///
/// * `workflow` - The workflow catalog, in all of its groups
/// * `revision` - The instance's revision
/// * `previous` - The instance this one supersedes
/// * `agents` - Each agent, in all of its groups, with the tool and harness instances it was given
///
/// # Panics
///
/// Panics if an agent isn't an agent, a tool isn't a tool instance, or a digest can't be
/// computed
#[allow(dead_code)]
#[must_use]
pub fn gen_workflow_instance(
    workflow: &Entity,
    revision: u64,
    previous: Option<Uuid>,
    agents: &[(&Entity, &[&Entity])],
) -> WorkflowInstanceEntity {
    // describe every tool once
    let mut tools: Vec<InstancedTool> = Vec::new();
    for (_, agent_tools) in agents {
        for entity in *agent_tools {
            if !tools.iter().any(|tool| tool.tool_instance == entity.id) {
                tools.push(gen_instanced_tool(entity));
            }
        }
    }
    // describe each agent as it's stored now
    let instanced = agents
        .iter()
        .map(|(entity, agent_tools)| {
            let EntityMetadata::Agent(agent) = &entity.metadata else {
                panic!("{} is not an agent", entity.id);
            };
            // split the harness instance from the tool instances the agent calls
            let (harnesses, called): (Vec<&&Entity>, Vec<&&Entity>) =
                agent_tools.iter().partition(|tool| {
                    tools.iter().any(|described| {
                        described.tool_instance == tool.id && described.kind == ToolKind::Harness
                    })
                });
            let mut instanced = InstancedAgent {
                agent: entity.id,
                name: entity.name.clone(),
                groups: entity.groups.clone(),
                config: agent.config.clone(),
                config_hash: agent.config_hash.clone(),
                harness: harnesses.first().map(|harness| harness.id),
                tools: called.iter().map(|tool| tool.id).collect(),
                tool_sets: Vec::new(),
                bindings: called
                    .iter()
                    .map(|tool| InstancedToolBinding {
                        tool_instance: tool.id,
                        mechanism: ToolMechanism::ThoriumApi,
                    })
                    .collect(),
                sub_agents: Vec::new(),
                toolset_digest: String::new(),
            };
            instanced.toolset_digest = instanced
                .compute_toolset_digest(&tools)
                .expect("failed to compute a toolset digest");
            instanced
        })
        .collect::<Vec<InstancedAgent>>();
    // take the settings from the workflow
    let (limits, default_models) = match &workflow.metadata {
        EntityMetadata::WorkflowCatalog(meta) => (meta.limits.clone(), meta.default_models.clone()),
        _ => (crate::models::WorkflowLimits::default(), Vec::new()),
    };
    let mut instance = WorkflowInstanceEntity {
        workflow: workflow.id,
        workflow_groups: workflow.groups.clone(),
        revision,
        previous,
        status: WorkflowInstanceStatus::Ready,
        warnings: Vec::new(),
        trigger: WorkflowInstanceTrigger::Manual,
        triggered_by: String::new(),
        entry_agent: instanced.first().map(|agent| agent.agent),
        limits,
        default_models,
        agents: instanced,
        tools,
        digest: String::new(),
    };
    instance.digest = instance
        .compute_digest(&BTreeMap::new())
        .expect("failed to compute a workflow instance digest");
    instance
}

/// Build a request to submit a workflow instance in its workflow's groups
///
/// # Arguments
///
/// * `instance` - The workflow instance to submit
///
/// # Panics
///
/// Panics if the instance can't be serialized
#[allow(dead_code)]
#[must_use]
pub fn gen_workflow_instance_req(instance: &WorkflowInstanceEntity) -> EntityRequest {
    // the instance is named by the API, so any name is sent
    let req = WorkflowInstanceEntityRequest::new(instance)
        .expect("failed to serialize a workflow instance");
    EntityRequest::new(
        "workflow-instance",
        EntityMetadataRequest::WorkflowInstance(req),
        instance.workflow_groups.clone(),
    )
}

/// Generate a random image request
///
/// # Arguments
///
/// * `group` - The group this image should be in
///
/// # Panics
///
/// Panics if semver version fails to parse, which shouldn't happen because
/// we always set it to a good value
#[allow(dead_code)]
#[must_use]
pub fn gen_image(group: &str) -> ImageRequest {
    let name = gen_string(25);
    ImageRequest::new(group, &name)
        .version(ToolVersion::SemVer(
            semver::Version::parse("1.0.0").unwrap(),
        ))
        .image(gen_string(90))
        .lifetime(ImageLifetime::jobs(3))
        .timeout(300)
        .resources(
            ResourcesRequest::default()
                .cores(2.0)
                .memory("1Gi")
                // this will never fail as 1Gi is valid and hardcoded
                .unwrap()
                .nvidia_gpu(1)
                .amd_gpu(5),
        )
        .env("ENV_ARG", "Test")
        .unset_env("REMOVE_ARG")
        .volume(Volume::new("woot", "/woots", VolumeTypes::Secret))
        .description(name + " image description")
        .display_type(OutputDisplayType::String)
        .output_collection(
            OutputCollection::default().files(
                FilesHandler::default()
                    .results("/data/corn")
                    .result_files("/data/corn_files")
                    .names(vec!["corn.png", "corn.json"]),
            ),
        )
        .child_filters(
            ChildFilters::default()
                .mimes([r"(?m)^([^:]+):([0-9]+):(.+)$", r"Hello (?<name>\w+)!"])
                .file_name(r"note.*")
                .file_extension("exe"),
        )
        .clean_up(
            Cleanup::new("/scripts/script.py".to_owned())
                .job_id(ArgStrategy::Kwarg("--job_id".to_owned()))
                .results(ArgStrategy::Kwarg("--results".to_owned()))
                .result_files_dir(ArgStrategy::Append),
        )
        .dependencies(
            Dependencies::default()
                .samples(
                    SampleDependencySettings::default()
                        .location("/test/samples")
                        .kwarg("--samples")
                        .strategy(DependencyPassStrategy::Directory),
                )
                .ephemeral(
                    EphemeralDependencySettings::new("/ephemeral", DependencyPassStrategy::Names)
                        .kwarg("--ephemeral"),
                )
                .results(
                    ResultDependencySettings::new(vec!["plant", "harvest"])
                        .location("/tmp/prior-harvests")
                        .kwarg(KwargDependency::List("--prior".to_owned()))
                        .strategy(DependencyPassStrategy::Names)
                        .name("fields.txt"),
                )
                .repos(
                    RepoDependencySettings::default()
                        .location("/test/repos")
                        .kwarg("--repos")
                        .strategy(DependencyPassStrategy::Directory),
                ),
        )
}

/// Generate a random external image request
///
/// # Arguments
///
/// * `group` - The group this image should be in
#[allow(dead_code)]
#[must_use]
pub fn gen_ext_image(group: &str) -> ImageRequest {
    let name = gen_string(25);
    ImageRequest::new(group, &name)
        .scaler(ImageScaler::External)
        .description(name + " external image description")
}

/// Setup a number of random images in a group
///
/// The group is first allowed image writes, since the image routes refuse writes in any group a
/// test has not opted in.
///
/// # Arguments
///
/// * `group` - The group these images should be in
/// * `cnt` - The number of images to create
/// * `external` - Whether to create images for the external scaler
/// * `client` - The client to use when creating these images
#[allow(dead_code)]
pub async fn images(
    group: &str,
    cnt: usize,
    external: bool,
    client: &Thorium,
) -> Result<Vec<ImageRequest>, Error> {
    // these images are seeded through the image routes, which refuse writes elsewhere
    test_utilities::faults::allow_image_writes(group);
    // create a 20 random images then
    let images: Vec<ImageRequest> = if external {
        (0..cnt).map(|_| gen_ext_image(group)).collect()
    } else {
        (0..cnt).map(|_| gen_image(group)).collect()
    };
    // create images
    for image in &images {
        client.images.create(image).await?;
    }
    Ok(images)
}

/// Generate an image with a [`crate::models::HostPath`] with the given mount
///
/// # Arguments
///
/// * `group` - The group to create the image in
/// * `path` - The path to set for the `HostPath`
#[allow(dead_code)]
#[must_use]
pub fn gen_host_path<T: Into<String>>(group: &str, path: T) -> ImageRequest {
    gen_image(group).volume(Volume::new(gen_string(20), path, VolumeTypes::HostPath))
}

/// Generate a random pipeline request whose stages each run a new Image tool
///
/// Each stage is named after its tool and pinned to the tool's first instance.
///
/// # Arguments
///
/// * `group` - The group this pipeline should be in
/// * `image_cnt` - The number of stages in this pipeline
/// * `external` - Whether the stages' tools run outside of Thorium
/// * `client` - The client to use when creating the tools for this pipeline
#[allow(dead_code)]
pub async fn gen_pipe(
    group: &str,
    image_cnt: usize,
    external: bool,
    client: &Thorium,
) -> Result<PipelineRequest, Error> {
    let pipe_name = gen_string(25);
    // pick the environment the stages' tools run in
    let scaler = if external {
        ImageScaler::External
    } else {
        ImageScaler::K8s
    };
    // create a tool for each stage
    let mut stages = Vec::with_capacity(image_cnt);
    for _ in 0..image_cnt {
        stages.push(stage_tool(group, &gen_image_spec_for(scaler), client).await?);
    }
    // run every stage in parallel in one step
    let names = stages
        .iter()
        .map(|(name, _)| name.clone())
        .collect::<Vec<String>>();
    let order = serde_json::json!(vec![names]);
    // back each stage with its tool's first instance
    let pipe = stages.into_iter().fold(
        PipelineRequest::new(group, &pipe_name, order)
            .sla(gen_int!(1, 86400))
            .description(pipe_name + " pipeline description"),
        |pipe, (stage, instance)| pipe.stage_tool(stage, instance),
    );
    Ok(pipe)
}

/// Generate a random generator pipeline
///
/// The first stage's tool is a generator and the second stage's tool isn't.
///
/// # Arguments
///
/// * `group` - The group this pipeline should be in
/// * `client` - The client to use when creating the tools for this pipeline
#[allow(dead_code)]
pub async fn gen_generator_pipe(group: &str, client: &Thorium) -> Result<PipelineRequest, Error> {
    let pipe_name = gen_string(25);
    // build our generator tool
    let mut generator = gen_image_spec_for(ImageScaler::K8s);
    if let ToolSpec::Image(workload) = &mut generator {
        workload.execution.generator = true;
    }
    let first = stage_tool(group, &generator, client).await?;
    // build our final tool
    let last = stage_tool(group, &gen_image_spec_for(ImageScaler::K8s), client).await?;
    // run the generator stage before the final stage
    let order = serde_json::json!(vec![first.0.clone(), last.0.clone()]);
    // build a pipeline request
    let pipe = PipelineRequest::new(group, &pipe_name, order)
        .sla(gen_int!(1, 86400))
        .description(pipe_name + " generator pipe description")
        .stage_tool(first.0, first.1)
        .stage_tool(last.0, last.1);
    // create this pipeline in Thorium
    client.pipelines.create(&pipe).await?;
    Ok(pipe)
}

/// Setup a number of random pipelines in a group
///
/// # Arguments
///
/// * `group` - The group these pipelines should be in
/// * `cnt` - The number of pipelines to create
/// * `external` - Whether this pipeline should be built of external images or not
/// * `name` - The name of the test that called this
/// * `client` - The client to use when creating these pipelines
#[allow(dead_code)]
pub async fn pipelines(
    group: &str,
    cnt: usize,
    external: bool,
    client: &Thorium,
) -> Result<Vec<PipelineRequest>, Error> {
    // create cnt random pipelines then
    let mut pipelines = Vec::with_capacity(cnt);
    for _ in 0..cnt {
        pipelines.push(gen_pipe(group, 3, external, client).await?);
    }
    // create pipelines
    for pipe in &pipelines {
        client.pipelines.create(pipe).await?;
    }
    Ok(pipelines)
}

/// Generate a simple pipeline with a configurable number of jobs
///
/// This will reuse a pipeline if it already exists. The group is first allowed image writes, since
/// the image routes refuse writes in any group a test has not opted in.
///
/// # Arguments
///
/// * `group` - The group to create jobs for
/// * `pipeline` - The pipeline to create these jobs for
/// * `images` - The images to create for this pipeline
/// * `reactions` - The number of reactions to create
/// * `client` - The client to use when talking to Thorium
#[allow(dead_code)]
pub async fn gen_jobs(
    group: &str,
    pipeline: &PipelineRequest,
    images: &[ImageRequest],
    reactions: u64,
    client: &Thorium,
) -> Result<(), Error> {
    // check if this group exist already
    if client.groups.get(group).await.is_err() {
        // assume the error is because this group doesn't exist yet
        client.groups.create(&GroupRequest::new(group)).await?;
    }
    // these images are seeded through the image routes, which refuse writes elsewhere
    test_utilities::faults::allow_image_writes(group);
    // crawl the images in this pipeline
    for image in images {
        // check if this image exists already
        if client.images.get(group, &image.name).await.is_err() {
            // assume the error is because this pipeline doesn't exist yet
            client.images.create(image).await?;
        }
    }
    println!(
        "pipelines -> {:#?}",
        client.pipelines.list(group).details().exec().await?.details
    );
    // check if this pipeline already exists
    if client.pipelines.get(group, &pipeline.name).await.is_err() {
        // assume the error is because this pipeline doesn't exist yet
        client.pipelines.create(pipeline).await?;
    }
    // create the reaction request for our job
    let req = ReactionRequest::new(group, &pipeline.name);
    // create a list of the right number of reactions
    let req_list = (0..reactions)
        .map(|_| req.clone())
        .collect::<Vec<ReactionRequest>>();
    // create our reactions if any were added
    if !req_list.is_empty() {
        // create our reactions in bulk
        client.reactions.create_bulk(&req_list).await?;
    }
    Ok(())
}

/// Setup a number of random pipelines and returnn all request data needed
///
/// # Arguments
///
/// * `cnt` - The number of images to create
/// * `name` - The name of the test that called this
/// * `client` - The client to use when creating these images
#[allow(dead_code)]
pub async fn gen_all(
    cnt: usize,
    client: &Thorium,
) -> Result<(Vec<GroupRequest>, Vec<ImageRequest>, Vec<PipelineRequest>), Error> {
    // Create a group
    let groups = groups(cnt, client).await?;
    // build the vectors to store our built images and pipelines
    let mut images: Vec<ImageRequest> = vec![];
    let mut pipes: Vec<PipelineRequest> = vec![];
    // create random images and pipelines
    let mut rng = rand::rngs::SmallRng::from_os_rng();
    for group in &groups {
        let mut group_images: Vec<ImageRequest> = vec![];
        // get cnt internal and external images
        group_images.extend((0..cnt).map(|_| gen_image(&group.name)));
        group_images.extend((0..cnt).map(|_| gen_ext_image(&group.name)));
        // these images are seeded through the image routes, which refuse writes elsewhere
        test_utilities::faults::allow_image_writes(&group.name);
        // create these images
        for image in &group_images {
            client.images.create(image).await?;
        }
        // create a tool backing each image's stage, in the image's environment
        let mut stage_tools = BTreeMap::new();
        for image in &group_images {
            let scaler = image.scaler;
            let (_, instance) =
                stage_tool(&group.name, &gen_image_spec_for(scaler), client).await?;
            stage_tools.insert(image.name.clone(), instance);
        }
        // create pipelines using these random iamges
        for _ in 0..cnt {
            // generate randome pipeline name
            let pipe_name = gen_string(25);
            // get some random images for this pipeline
            let reqs: Vec<&ImageRequest> = group_images.iter().choose_multiple(&mut rng, cnt);
            let names: Vec<&String> = reqs.iter().map(|item| &item.name).collect();
            // cast the names to a json list
            let order = serde_json::json!(names);
            // create our pipeline request with a tool backing each stage
            let pipe = names.iter().fold(
                PipelineRequest::new(&group.name, &pipe_name, order)
                    .sla(gen_int!(1, 86400))
                    .description(pipe_name + " pipeline description"),
                |pipe, name| pipe.stage_tool(*name, stage_tools[*name]),
            );
            // send our request to the API
            client.pipelines.create(&pipe).await?;
            pipes.push(pipe);
        }
        // add the group images to our full images list
        images.extend(group_images);
    }
    Ok((groups, images, pipes))
}

/// Generate random args for a stage of a reaction
#[allow(dead_code)]
#[must_use]
pub fn gen_args() -> GenericJobArgs {
    // generate a random number of positional args
    let positionals: Vec<String> = (0..gen_int!(3, 10))
        .map(|_| gen_string(gen_int!(5, 64)))
        .collect();
    // generate a random number of positional args
    let kwargs: BTreeMap<String, Vec<String>> = (0..gen_int!(3, 10))
        .map(|_| {
            (
                gen_string(gen_int!(5, 64)),
                vec![gen_string(gen_int!(5, 64))],
            )
        })
        .collect();
    // generate a random number of switches
    let switches: Vec<String> = (0..gen_int!(3, 10))
        .map(|_| gen_string(gen_int!(5, 64)))
        .collect();
    GenericJobArgs::default()
        .positionals(positionals)
        .set_kwargs(kwargs)
        .switches(switches)
}

/// Generate a random [`ReactionRequest`]
///
/// # Arguments
///
/// * `group` - The group this reaction should be in
/// * `pipe` - The pipeline this reaction is for
/// * `tag` - The tag to use for the pipeline
#[allow(dead_code)]
#[must_use]
pub fn gen_reaction(group: &str, pipe: &Pipeline, tag: Option<&str>) -> ReactionRequest {
    // create a reaction request
    let react_req = ReactionRequest::new(group, &pipe.name);
    // inject tags if they exist
    let react_req = match tag {
        Some(tag) => react_req.tag(tag),
        None => react_req,
    };
    // generate and inject args into this reaction request
    pipe.order
        .iter()
        .flatten()
        .fold(react_req, |req, image| req.args(image.clone(), gen_args()))
}

/// Setup a number of random reactions in a group for a specific pipeline
///
/// # Arguments
///
/// * `group` - The group these reactions should be in
/// * `cnt` - The number of reactions to create
/// * `name` - The name of the test that called this
/// * `client` - The client to use when creating these pipelines
#[allow(dead_code)]
pub async fn reactions(
    group: &str,
    cnt: usize,
    tag: Option<&str>,
    client: &Thorium,
) -> Result<(Vec<ReactionRequest>, BulkReactionResponse), Error> {
    // create a random pipeline for these reactions
    let pipe_req = pipelines(group, 1, false, client).await?.remove(0);
    // get the pipeline for this pipeline order
    let pipe = client.pipelines.get(group, &pipe_req.name).await?;
    // create reactions requests
    let react_reqs: Vec<ReactionRequest> =
        (0..cnt).map(|_| gen_reaction(group, &pipe, tag)).collect();
    let resp = client.reactions.create_bulk(&react_reqs).await?;
    Ok((react_reqs, resp))
}

/// Creates N random sub reactions
#[allow(dead_code)]
pub async fn sub_reactions(
    group: &str,
    cnt: usize,
    parent: &Uuid,
    client: &Thorium,
) -> Result<(Vec<ReactionRequest>, Vec<ReactionCreation>, Pipeline), Error> {
    // create a random pipeline
    let pipe_req = pipelines(group, 1, false, client).await?.remove(0);
    // get the pipeline for this pipeline order
    let pipe = client.pipelines.get(group, &pipe_req.name).await?;
    // track our spawned sub reactions
    let mut sub_reacts = vec![];
    let mut creates = vec![];
    // spawn 3 sub reactions
    for _ in 0..cnt {
        // Create a random reaction based on our pipeline request
        let sub_req = gen_reaction(group, &pipe, None);
        let sub_req = sub_req.parent(*parent);
        // make sure that we were able to create a reaction and our jobs
        let resp = client.reactions.create(&sub_req).await?;
        sub_reacts.push(sub_req);
        creates.push(resp);
    }
    Ok((sub_reacts, creates, pipe))
}

/// Builds random stage logs
///
/// This assumes a return code of 0
#[allow(dead_code)]
#[must_use]
pub fn stage_logs() -> StageLogsAdd {
    // create default stage logs
    let mut logs = StageLogsAdd::default().code(0);
    // create random logs
    let lines = (0..gen_int!(10, 50))
        .map(|_| gen_string(gen_int!(256, 1024)))
        .collect();
    // add random logs
    logs.add_logs(lines);
    logs
}

/// Generate a random sample request
///
/// # Arguments
///
/// * `group` - The group this sample should be in
#[allow(dead_code)]
#[must_use]
pub fn gen_sample(group: &str) -> SampleRequest {
    SampleRequest::new_buffer(Buffer::new(gen_string(gen_int!(2048, 4096))), vec![group])
        .description(gen_string(gen_int!(20, 2048)))
        .tag(gen_string(gen_int!(4, 32)), gen_string(gen_int!(8, 64)))
        .tag(gen_string(gen_int!(4, 32)), gen_string(gen_int!(8, 64)))
        .tag(gen_string(gen_int!(4, 32)), gen_string(gen_int!(8, 64)))
        .origin(OriginRequest::downloaded(
            gen_string(gen_int!(4, 50)),
            Some(gen_string(gen_int!(8, 24))),
        ))
}

/// Setup a number of random samples in a group
///
/// # Arguments
///
/// * `group` - The group these samples should be in
/// * `cnt` - The number of samples to create
/// * `name` - The name of the test that called this
/// * `client` - The client to use when creating these samples
#[allow(dead_code)]
pub async fn samples(
    group: &str,
    cnt: usize,
    client: &Thorium,
) -> Result<Vec<SampleRequest>, Error> {
    // build a sample request
    let reqs = (0..cnt)
        .map(|_| gen_sample(group))
        .collect::<Vec<SampleRequest>>();
    // upload these files
    for req in &reqs {
        client.files.create(req.clone()).await?;
    }
    Ok(reqs)
}

/// Setup a number of random samples in a group that have the same tag
///
/// # Arguments
///
/// * `group` - The group these samples should be in
/// * `cnt` - The number of samples to create
/// * `name` - The name of the test that called this
/// * `client` - The client to use when creating these samples
#[allow(dead_code)]
pub async fn samples_tagged(
    group: &str,
    cnt: usize,
    client: &Thorium,
) -> Result<(String, String, Vec<SampleRequest>), Error> {
    // build a sample request
    let reqs = (0..cnt)
        .map(|_| gen_sample(group))
        .collect::<Vec<SampleRequest>>();
    // build a shared tag for all these requests
    let key = gen_utf8_string(16);
    let value = gen_utf8_string(16);
    // add the same tag to all of our sample requests
    let reqs = reqs
        .into_iter()
        .map(|req| req.tag(&key, &value))
        .collect::<Vec<SampleRequest>>();
    // upload these files
    for req in &reqs {
        client.files.create(req.clone()).await?;
    }
    Ok((key, value, reqs))
}

/// Setup a number of random samples in a group with the given tag
///
/// # Arguments
///
/// * `group` - The group these samples should be in
/// * `cnt` - The number of samples to create
/// * `name` - The name of the test that called this
/// * `client` - The client to use when creating these samples
#[allow(dead_code)]
pub async fn samples_with_tag(
    group: &str,
    cnt: usize,
    key: &str,
    value: &str,
    client: &Thorium,
) -> Result<Vec<SampleRequest>, Error> {
    // build a sample request
    let reqs = (0..cnt)
        .map(|_| gen_sample(group))
        .collect::<Vec<SampleRequest>>();
    // add the same tag to all of our sample requests
    let reqs = reqs
        .into_iter()
        .map(|req| req.tag(key, value))
        .collect::<Vec<SampleRequest>>();
    // upload these files
    for req in &reqs {
        client.files.create(req.clone()).await?;
    }
    Ok(reqs)
}

/// Generate a random repo request
///
/// # Arguments
///
/// * `group` - The group this repo should be in
#[allow(dead_code)]
#[must_use]
pub fn gen_repo(group: &str) -> RepoRequest {
    RepoRequest::new(
        format!(
            "provider.tld/{}/{}",
            gen_string(gen_int!(4, 32)),
            gen_string(gen_int!(4, 32)),
        ),
        vec![group],
        Some(RepoCheckout::branch("main")),
    )
    .tag(gen_string(gen_int!(4, 32)), gen_string(gen_int!(8, 64)))
    .tag(gen_string(gen_int!(4, 32)), gen_string(gen_int!(8, 64)))
    .tag(gen_string(gen_int!(4, 32)), gen_string(gen_int!(8, 64)))
}

/// Setup a number of random repos in a group
///
/// # Arguments
///
/// * `group` - The group these repos should be in
/// * `cnt` - The number of repos to create
/// * `name` - The name of the test that called this
/// * `client` - The client to use when creating these repos
#[allow(dead_code)]
pub async fn repos(group: &str, cnt: usize, client: &Thorium) -> Result<Vec<RepoRequest>, Error> {
    // build a repo request
    let reqs = (0..cnt)
        .map(|_| gen_repo(group))
        .collect::<Vec<RepoRequest>>();
    // upload these repos
    for req in &reqs {
        client.repos.create(req).await?;
    }
    Ok(reqs)
}

/// Setup a number of random repos in a group that have the same tag
///
/// # Arguments
///
/// * `group` - The group these repos should be in
/// * `cnt` - The number of repos to create
/// * `name` - The name of the test that called this
/// * `client` - The client to use when creating these repos
#[allow(dead_code)]
pub async fn repos_tagged(
    group: &str,
    cnt: usize,
    client: &Thorium,
) -> Result<(String, String, Vec<RepoRequest>), Error> {
    // generate a key and value pair
    let key = gen_string(16);
    let value = gen_string(16);
    // build repo requests with those key/value tags
    let reqs = (0..cnt)
        .map(|_| gen_repo(group))
        .map(|req| req.tag(&key, &value))
        .collect::<Vec<RepoRequest>>();
    // upload these repos
    for req in &reqs {
        client.repos.create(req).await?;
    }
    Ok((key, value, reqs))
}

/// Setup a node
///
/// # Arguments
///
/// * `cluster` - The cluster this node should be in
/// * `node` - The name of the node to register
/// * `resources` - The resources this node has
pub async fn node(
    cluster: &str,
    node: &str,
    resources: Resources,
    client: &Thorium,
) -> Result<(), Error> {
    // register this node
    client
        .system
        .register_node(&NodeRegistration::new(cluster, node, resources))
        .await?;
    Ok(())
}

/// Register a worker for a node
///
/// # Arguments
///
/// * `cluster` - The cluster this worker is in
/// * `node` - The node this worker will be on
/// * `name` - The name of this worker
/// * `group` - The group this worker is executing a job in
/// * `pipe` - The pipeline this worker is executing a job for
/// * `stage` - The stage this worker is executing a job for
/// * `client` - The client to register this worker with
pub async fn worker(
    cluster: &str,
    node: &str,
    name: &str,
    group: &str,
    pipe: &str,
    stage: &str,
    client: &Thorium,
) -> Result<(), Error> {
    // get our username
    let user = client.users.info().await?.username;
    // register this worker
    client
        .system
        .register_workers(
            ImageScaler::K8s,
            &WorkerRegistrationList::default().add(
                cluster,
                node,
                name,
                user,
                group,
                pipe,
                stage,
                Resources::default(),
                Pools::Deadline,
            ),
        )
        .await?;
    Ok(())
}

/// Register an external worker for a node
///
/// # Arguments
///
/// * `cluster` - The cluster this worker is in
/// * `node` - The node this worker will be on
/// * `name` - The name of this worker
/// * `group` - The group this worker is executing a job in
/// * `pipe` - The pipeline this worker is executing a job for
/// * `stage` - The stage this worker is executing a job for
/// * `client` - The client to register this worker with
pub async fn worker_ext(
    cluster: &str,
    node: &str,
    name: &str,
    group: &str,
    pipe: &str,
    stage: &str,
    client: &Thorium,
) -> Result<(), Error> {
    // get our username
    let user = client.users.info().await?.username;
    // register this worker
    client
        .system
        .register_workers(
            ImageScaler::External,
            &WorkerRegistrationList::default().add(
                cluster,
                node,
                name,
                user,
                group,
                pipe,
                stage,
                Resources::default(),
                Pools::Deadline,
            ),
        )
        .await?;
    Ok(())
}

/// Delete a worker
///
/// # Arguments
///
/// * `worker` - The name of the worker to delete
/// * `client` - The client to delete this worker with
pub async fn delete_worker(worker: &str, client: &Thorium) -> Result<(), Error> {
    // register this node
    client
        .system
        .delete_workers(ImageScaler::K8s, &WorkerDeleteMap::default().add(worker))
        .await?;
    Ok(())
}

/// Delete an external worker
///
/// # Arguments
///
/// * `worker` - The name of the worker to delete
/// * `client` - The client to delete this worker with
pub async fn delete_worker_ext(worker: &str, client: &Thorium) -> Result<(), Error> {
    // register this node
    client
        .system
        .delete_workers(
            ImageScaler::External,
            &WorkerDeleteMap::default().add(worker),
        )
        .await?;
    Ok(())
}

/// Generate a random [`Ipv4Cidr`]
///
/// Network length is always 24 to keep things simple
#[must_use]
fn gen_ipv4_cidr() -> Ipv4Cidr {
    Ipv4Cidr::new(
        Ipv4Addr::new(gen_int!(1, 255), gen_int!(1, 255), gen_int!(1, 255), 0),
        24,
    )
    .unwrap()
}

/// Generate a random [`Ipv6Cidr`]
///
/// Network length is always 64 to keep things simple
#[must_use]
fn gen_ipv6_cidr() -> Ipv6Cidr {
    Ipv6Cidr::new(
        Ipv6Addr::new(
            gen_int!(1, 65535),
            gen_int!(1, 65535),
            gen_int!(1, 65535),
            gen_int!(1, 65535),
            0,
            0,
            0,
            0,
        ),
        64,
    )
    .unwrap()
}

/// Generate a random [`IpBlock`]
#[must_use]
fn gen_ip_block() -> IpBlock {
    if rand::rngs::SmallRng::from_os_rng().random_bool(0.5) {
        let block = Ipv4Block {
            cidr: gen_ipv4_cidr(),
            // leave "except" as None to avoid issues when checking
            // if it's a subset of the above cidr
            except: None,
        };
        IpBlock::V4(block)
    } else {
        let block = Ipv6Block {
            cidr: gen_ipv6_cidr(),
            // leave "except" as None to avoid issues when checking
            // if it's a subset of the above cidr
            except: None,
        };
        IpBlock::V6(block)
    }
}

#[must_use]
fn gen_custom_network_policy_rule() -> NetworkPolicyCustomK8sRule {
    NetworkPolicyCustomK8sRule {
        // generate a random None/Some list of random custom labels
        namespace_labels: gen_opt!(0.9, {
            (0..10)
                .map(|_| {
                    NetworkPolicyCustomLabel::new(
                        gen_string(gen_int!(1, 63)),
                        gen_string(gen_int!(1, 63)),
                    )
                })
                .collect()
        }),
        pod_labels: gen_opt!(
            0.9,
            (0..10)
                .map(|_| {
                    NetworkPolicyCustomLabel::new(
                        gen_string(gen_int!(1, 63)),
                        gen_string(gen_int!(1, 63)),
                    )
                })
                .collect()
        ),
    }
}

/// Generate random network policy settings
///
/// # Arguments
///
/// * `groups` - The possible groups that will be in the settings
#[must_use]
pub fn gen_network_policy_rule(groups: &[String]) -> NetworkPolicyRuleRaw {
    let allowed_ips = (0..gen_int!(1, 10))
        .map(|_| {
            // create a real ip block to ensure our addresses are valid
            let ip_block = gen_ip_block();
            // convert the ip block to a raw ip block
            let (cidr, except) = match ip_block {
                IpBlock::V4(ipv4_block) => (
                    ipv4_block.cidr.to_string(),
                    ipv4_block
                        .except
                        .map(|except| except.into_iter().map(|cidr| cidr.to_string()).collect()),
                ),
                IpBlock::V6(ipv6_block) => (
                    ipv6_block.cidr.to_string(),
                    ipv6_block
                        .except
                        .map(|except| except.into_iter().map(|cidr| cidr.to_string()).collect()),
                ),
            };
            IpBlockRaw { cidr, except }
        })
        .collect();
    let ports = (0..gen_int!(1, 10))
        .map(|_| NetworkPolicyPort {
            port: gen_int!(1, 65535),
            end_port: gen_opt!(0.5, gen_int!(1, 65535)),
            protocol: gen_opt!(0.5, {
                if rand::rngs::SmallRng::from_os_rng().random_bool(0.5) {
                    NetworkProtocol::TCP
                } else {
                    NetworkProtocol::UDP
                }
            }),
        })
        .collect();
    let allowed_custom = (0..gen_int!(0, 10))
        .map(|_| gen_custom_network_policy_rule())
        .collect();
    NetworkPolicyRuleRaw {
        allowed_ips,
        allowed_groups: groups
            .choose_multiple(
                &mut rand::rngs::SmallRng::from_os_rng(),
                gen_int!(0, groups.len()),
            )
            .cloned()
            .collect(),
        // refrain from adding allowed tools to avoid failing tools existing check
        allowed_tools: Vec::new(),
        allowed_local: rand::rngs::SmallRng::from_os_rng().random_bool(0.5),
        allowed_internet: rand::rngs::SmallRng::from_os_rng().random_bool(0.5),
        allowed_all: rand::rngs::SmallRng::from_os_rng().random_bool(0.5),
        ports,
        allowed_custom,
    }
}

/// Generate a network policy request
///
/// # Arguments
///
/// * `groups` - The groups the network policy will be in
#[must_use]
pub fn gen_network_policy(groups: &[String]) -> NetworkPolicyRequest {
    // generate a random number of ingress/egress rules
    let ingress = gen_opt!(
        0.9,
        (0..gen_int!(1, 10))
            .map(|_| gen_network_policy_rule(groups))
            .collect()
    );
    let egress = gen_opt!(
        0.9,
        (0..gen_int!(1, 10))
            .map(|_| gen_network_policy_rule(groups))
            .collect()
    );
    NetworkPolicyRequest {
        name: gen_utf8_string(gen_int!(1, 63)),
        groups: groups.to_vec(),
        ingress,
        egress,
        forced_policy: false,
        default_policy: false,
    }
}

/// Create the given number of network policies in Thorium
///
/// # Arguments
///
/// * `groups` - The groups the network policies should be in
/// * `cnt` - The number of network policies to create
/// * `client` - The Thorium client
pub async fn network_policies(
    groups: &[String],
    cnt: usize,
    client: &Thorium,
) -> Result<Vec<NetworkPolicyRequest>, Error> {
    // generate the requests
    let reqs: Vec<NetworkPolicyRequest> = (0..cnt).map(|_| gen_network_policy(groups)).collect();
    // create the network policies concurrently
    stream::iter(reqs.iter())
        .map(Ok::<&NetworkPolicyRequest, Error>)
        .try_for_each_concurrent(100, |req| async {
            client.network_policies.create(req.clone()).await?;
            Ok(())
        })
        .await?;
    // return the created requests
    Ok(reqs)
}

// Entity generators

/// A minimal, valid sigma rule used to generate sigma rule entities in tests
const TEST_SIGMA_RULE: &str = r"title: A rule with keywords
logsource:
    service: test
detection:
    keywords:
        - '* hello world?'
        - 'evil'
    condition: keywords
";

/// Generate metadata for an entity with no unique kind (an `Other` entity)
#[allow(dead_code)]
#[must_use]
pub fn gen_other_meta() -> EntityMetadataRequest {
    // other entities have no metadata
    EntityMetadataRequest::Other
}

/// Generate metadata for a random vendor entity
#[allow(dead_code)]
#[must_use]
pub fn gen_vendor_meta() -> EntityMetadataRequest {
    // build a set of countries this vendor operates in (alpha-2 codes)
    let mut countries = BTreeSet::new();
    countries.insert("US".to_owned());
    // build the critical sectors this vendor is associated with
    let mut critical_sectors = BTreeSet::new();
    critical_sectors.insert(CriticalSector::InformationTechnology);
    // build our vendor metadata request
    EntityMetadataRequest::Vendor(VendorEntityRequest {
        countries,
        critical_sectors,
    })
}

/// Generate metadata for a random device entity
///
/// # Arguments
///
/// * `vendors` - The ids of the vendor entities this device is associated with
#[allow(dead_code)]
#[must_use]
pub fn gen_device_meta(vendors: Vec<Uuid>) -> EntityMetadataRequest {
    // build the critical sectors this device is associated with
    let mut critical_sectors = BTreeSet::new();
    critical_sectors.insert(CriticalSector::Energy);
    // build our device metadata request
    EntityMetadataRequest::Device(DeviceEntityRequest {
        urls: vec![format!("https://{}.example.com", gen_string(8))],
        vendors,
        critical_system: Some(true),
        sensitive_location: Some(false),
        critical_sectors,
    })
}

/// Generate metadata for a random flag entity
#[allow(dead_code)]
#[must_use]
pub fn gen_flag_meta() -> EntityMetadataRequest {
    // build our flag metadata request
    EntityMetadataRequest::Flag(Flag {
        suspicion: gen_int!(0, 100),
        confidence: Confidence::Likely,
        content: Some(gen_string(gen_int!(8, 32))),
        reasoning: gen_string(gen_int!(8, 64)),
    })
}

/// Generate metadata for a sigma rule entity
///
/// # Panics
///
/// Panics if the built-in test sigma rule fails to validate
#[allow(dead_code)]
#[must_use]
pub fn gen_sigma_meta() -> EntityMetadataRequest {
    // build a validated sigma rule from our test rule
    let rule = SigmaRule::new(TEST_SIGMA_RULE, SigmaRuleAppliesTo::WindowsProcesses)
        .expect("failed to build test sigma rule");
    // build our sigma rule metadata request
    EntityMetadataRequest::SigmaRule(rule)
}

/// Generate metadata for a random network connection entity
#[allow(dead_code)]
#[must_use]
pub fn gen_network_connection_meta() -> EntityMetadataRequest {
    // build our network connection metadata request, leaving timestamps unset
    EntityMetadataRequest::NetworkConnection(NetworkConnection {
        protocol: Some(TransportLayerProtocol::TCP),
        source: IpAddr::V4(Ipv4Addr::new(10, 0, 0, gen_int!(1, 254))),
        source_port: Some(gen_int!(1024, 65535)),
        destination: IpAddr::V4(Ipv4Addr::new(10, 0, 0, gen_int!(1, 254))),
        destination_port: gen_int!(1, 65535),
        state: Some(NetConState::Established),
        pid: Some(gen_int!(1, 65535)),
        process: Some(gen_string(gen_int!(4, 16))),
        create_time: None,
    })
}

/// Generate metadata for a random collection entity
#[allow(dead_code)]
#[must_use]
pub fn gen_collection_meta() -> EntityMetadataRequest {
    // build a single random tag for this collection
    let mut collection_tags = BTreeMap::new();
    let mut values = BTreeSet::new();
    values.insert(gen_string(gen_int!(4, 16)));
    collection_tags.insert(gen_string(gen_int!(4, 16)), values);
    // build our collection metadata request, leaving timestamps unset
    EntityMetadataRequest::Collection(CollectionEntityRequest {
        collection_kind: CollectionKind::Files,
        collection_tags,
        tags_case_insensitive: Some(false),
        ignore_groups: Some(false),
        start: None,
        end: None,
    })
}

/// Generate metadata for a random filesystem entity
///
/// # Arguments
///
/// * `tools` - The tool instances that found this filesystem
#[allow(dead_code)]
#[must_use]
pub fn gen_filesystem_meta(tools: Vec<Uuid>) -> EntityMetadataRequest {
    // build our filesystem metadata request
    EntityMetadataRequest::FileSystem(FileSystemEntity {
        sha256: gen_string(64),
        tools,
        legacy_tools: Vec::new(),
    })
}

/// Generate metadata for a random filesystem folder entity
///
/// # Arguments
///
/// * `filesystem_id` - The id of the filesystem entity this folder belongs to
#[allow(dead_code)]
#[must_use]
pub fn gen_folder_meta(filesystem_id: Uuid) -> EntityMetadataRequest {
    // build our folder metadata request
    EntityMetadataRequest::Folder(FileSystemFolderEntity {
        filesystem_id,
        names_sha256: gen_string(64),
        data_sha256: gen_string(64),
        all_sha256: gen_string(64),
    })
}

/// Generate metadata for a windows process tree entity
#[allow(dead_code)]
#[must_use]
pub fn gen_windows_process_tree_meta() -> EntityMetadataRequest {
    // a windows process tree recorded without any tool instances
    EntityMetadataRequest::WindowsProcessTree(WindowsProcessTreeEntity::default())
}

/// Generate metadata for a random windows process entity
#[allow(dead_code)]
#[must_use]
pub fn gen_windows_process_meta() -> EntityMetadataRequest {
    // build a process with only a pid and a few descriptive fields set
    let mut process = WindowsProcessEntity::new(gen_int!(1, 65535));
    process.name = Some(gen_string(gen_int!(4, 16)));
    process.image_path = Some(format!("C:\\\\{}.exe", gen_string(gen_int!(4, 16))));
    process.command = Some(gen_string(gen_int!(8, 32)));
    process.threads = Some(gen_int!(1, 64));
    process.handles = Some(gen_int!(1, 256));
    process.session_id = Some(gen_int!(0, 8));
    // build our windows process metadata request
    EntityMetadataRequest::WindowsProcess(process)
}

/// Generate metadata for a random PE section entity
#[allow(dead_code)]
#[must_use]
pub fn gen_pe_section_meta() -> EntityMetadataRequest {
    // build our PE section metadata request with a clean entropy value
    EntityMetadataRequest::PeSection(PeSectionEntity {
        md5: Some(gen_string(32)),
        raw_size: Some(gen_int!(1, 100_000)),
        virtual_size: Some(gen_int!(1, 100_000)),
        entropy: Some(7.5),
    })
}

/// Generate metadata for a random PE import entity
#[allow(dead_code)]
#[must_use]
pub fn gen_pe_import_meta() -> EntityMetadataRequest {
    // build our PE import metadata request with a couple of imported functions
    EntityMetadataRequest::PeImport(
        PeImportEntity::new()
            .function(gen_string(gen_int!(4, 16)))
            .function(gen_string(gen_int!(4, 16))),
    )
}

/// Generate metadata for a random incident entity
#[allow(dead_code)]
#[must_use]
pub fn gen_incident_meta() -> EntityMetadataRequest {
    // build our incident metadata request with a cover term and a few list fields
    EntityMetadataRequest::Incident(IncidentRequest {
        cover_term: Some(gen_string(gen_int!(4, 16))),
        mission_teams: vec![gen_string(gen_int!(4, 16)), gen_string(gen_int!(4, 16))],
        networks: vec![gen_string(gen_int!(4, 16))],
        machines: vec![gen_string(gen_int!(4, 16))],
        locations: vec![gen_string(gen_int!(4, 16))],
    })
}

/// Generate metadata for a random compiled function entity
#[allow(dead_code)]
#[must_use]
pub fn gen_compiled_function_meta() -> EntityMetadataRequest {
    // build a couple of disassembled instructions for this function
    let disassembly = vec![
        CompiledInstruction {
            address: gen_int!(1, 100_000),
            instruction: gen_string(gen_int!(4, 16)),
        },
        CompiledInstruction {
            address: gen_int!(1, 100_000),
            instruction: gen_string(gen_int!(4, 16)),
        },
    ];
    // build our compiled function metadata request
    EntityMetadataRequest::CompiledFunction(CompiledFunction {
        address: gen_int!(1, 100_000),
        disassembly,
    })
}

/// Generate metadata for a random decompiled function entity
///
/// # Arguments
///
/// * `tools` - The tool instances that decompiled this function
#[allow(dead_code)]
#[must_use]
pub fn gen_decompiled_function_meta(tools: Vec<Uuid>) -> EntityMetadataRequest {
    // build our decompiled function metadata request
    EntityMetadataRequest::DecompiledFunction(DecompiledFunction {
        address: gen_int!(1, 100_000),
        tools,
        legacy_tools: Vec::new(),
        content: gen_string(gen_int!(16, 128)),
    })
}

/// A fixed second-precision timestamp for update tests
///
/// Whole-second timestamps round-trip cleanly through scylla's millisecond
/// precision, avoiding spurious mismatches when comparing datetimes.
///
/// # Arguments
///
/// * `offset_secs` - The number of seconds to offset from the known base epoch second
#[allow(dead_code)]
#[must_use]
fn fixed_timestamp(offset_secs: i64) -> chrono::DateTime<chrono::Utc> {
    // build a fixed timestamp offset from a known epoch second
    chrono::DateTime::from_timestamp(1_700_000_000 + offset_secs, 0)
        .expect("failed to build fixed timestamp")
}

/// Generate a device metadata update touching every device-specific field
///
/// # Arguments
///
/// * `existing` - The created entity to pull removal targets from
#[allow(dead_code)]
#[must_use]
pub fn gen_device_update(existing: &Entity) -> EntityMetadataUpdate {
    // pull an existing url and critical sector to remove
    let (remove_urls, remove_critical_sectors) = match &existing.metadata {
        EntityMetadata::Device(dev) => (
            dev.urls.first().cloned().into_iter().collect(),
            dev.critical_sectors
                .iter()
                .next()
                .copied()
                .into_iter()
                .collect(),
        ),
        _ => (Vec::new(), Vec::new()),
    };
    // build a device update that adds/removes urls and sectors and toggles flags
    EntityMetadataUpdate::Device {
        add_urls: vec![format!("https://{}.example.com", gen_string(8))],
        remove_urls,
        critical_system: Some(false),
        clear_critical_system: None,
        sensitive_location: None,
        clear_sensitive_location: Some(true),
        add_critical_sectors: vec![CriticalSector::Communications],
        remove_critical_sectors,
    }
}

/// Generate a vendor metadata update touching every vendor-specific field
#[allow(dead_code)]
#[must_use]
pub fn gen_vendor_update(_existing: &Entity) -> EntityMetadataUpdate {
    // the create generator always uses US and InformationTechnology
    EntityMetadataUpdate::Vendor {
        add_countries: vec!["CA".to_owned()],
        remove_countries: vec!["US".to_owned()],
        add_critical_sectors: vec![CriticalSector::Communications],
        remove_critical_sectors: vec![CriticalSector::InformationTechnology],
    }
}

/// Generate a collection metadata update touching every collection-specific field
///
/// # Arguments
///
/// * `existing` - The created entity to pull removal targets from
#[allow(dead_code)]
#[must_use]
pub fn gen_collection_update(existing: &Entity) -> EntityMetadataUpdate {
    // pull an existing tag to delete
    let delete_collection_tags = match &existing.metadata {
        EntityMetadata::Collection(col) => {
            col.collection_tags
                .iter()
                .next()
                .map_or_default(|(key, values)| {
                    let mut map = std::collections::HashMap::new();
                    map.insert(key.clone(), values.iter().cloned().collect());
                    map
                })
        }
        _ => std::collections::HashMap::new(),
    };
    // add a new random tag
    let mut add_collection_tags = std::collections::HashMap::new();
    let mut values = std::collections::HashSet::new();
    values.insert(gen_string(gen_int!(4, 16)));
    add_collection_tags.insert(gen_string(gen_int!(4, 16)), values);
    // build our collection update
    EntityMetadataUpdate::Collection {
        add_collection_tags,
        delete_collection_tags,
        tags_case_insensitive: Some(true),
        ignore_groups: Some(true),
        // the api requires the start to be more recent than the end
        start: Some(fixed_timestamp(100)),
        end: Some(fixed_timestamp(0)),
        clear_start: None,
        clear_end: None,
    }
}

/// Generate a filesystem metadata update touching every filesystem-specific field
///
/// # Arguments
///
/// * `existing` - The created entity to pull removal targets from
/// * `add_tools` - The tool instances to add
#[allow(dead_code)]
#[must_use]
pub fn gen_filesystem_update(existing: &Entity, add_tools: Vec<Uuid>) -> EntityMetadataUpdate {
    // pull an existing tool to remove
    let remove_tools = match &existing.metadata {
        EntityMetadata::FileSystem(fs) => fs.tools.first().copied().into_iter().collect(),
        _ => Vec::new(),
    };
    // build a filesystem update that adds and removes a tool
    EntityMetadataUpdate::FileSystem {
        add_tools,
        remove_tools,
        remove_legacy_tools: Vec::new(),
    }
}

/// Generate a windows process tree metadata update
///
/// Process trees start with no tools, so this only exercises adding a tool.
///
/// # Arguments
///
/// * `_existing` - The created entity, which has no tools to remove
/// * `add_tools` - The tool instances to add
#[allow(dead_code)]
#[must_use]
pub fn gen_windows_process_tree_update(
    _existing: &Entity,
    add_tools: Vec<Uuid>,
) -> EntityMetadataUpdate {
    // build a process tree update that adds a tool
    EntityMetadataUpdate::WindowsProcessTree {
        add_tools,
        remove_tools: Vec::new(),
        remove_legacy_tools: Vec::new(),
    }
}

/// Generate a windows process metadata update touching every process-specific field
#[allow(dead_code)]
#[must_use]
pub fn gen_windows_process_update(_existing: &Entity) -> EntityMetadataUpdate {
    // build a process update that sets every scalar field
    EntityMetadataUpdate::WindowsProcess {
        name: Some(gen_string(gen_int!(4, 16))),
        image_path: Some(format!("C:\\\\{}.exe", gen_string(gen_int!(4, 16)))),
        command: Some(gen_string(gen_int!(8, 32))),
        offset: Some(gen_int!(1, 100_000)),
        threads: Some(gen_int!(1, 64)),
        handles: Some(gen_int!(1, 256)),
        is_wow64: Some(true),
        session_id: Some(gen_int!(0, 8)),
        create_time: Some(fixed_timestamp(0)),
        exit_time: Some(fixed_timestamp(100)),
    }
}

/// Generate a network connection metadata update touching every field
#[allow(dead_code)]
#[must_use]
pub fn gen_network_connection_update(_existing: &Entity) -> EntityMetadataUpdate {
    // build a network connection update that sets every field
    EntityMetadataUpdate::NetworkConnection {
        protocol: Some(TransportLayerProtocol::UDP),
        source: Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, gen_int!(1, 254)))),
        source_port: Some(gen_int!(1024, 65535)),
        destination: Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, gen_int!(1, 254)))),
        destination_port: Some(gen_int!(1, 65535)),
        state: Some(NetConState::Closed),
        pid: Some(gen_int!(1, 65535)),
        process: Some(gen_string(gen_int!(4, 16))),
        create_time: Some(fixed_timestamp(0)),
    }
}

/// Generate a sigma rule metadata update touching every sigma-specific field
#[allow(dead_code)]
#[must_use]
pub fn gen_sigma_update(_existing: &Entity) -> EntityMetadataUpdate {
    // build a sigma rule update that changes the rule, score, applies-to, and actions
    EntityMetadataUpdate::SigmaRule {
        sigma_rule: Some(TEST_SIGMA_RULE.to_owned()),
        score: Some(gen_int!(1, 100)),
        add_applies_to: vec![SigmaRuleAppliesTo::NetworkConnections],
        remove_applies_to: vec![SigmaRuleAppliesTo::WindowsProcesses],
        add_actions: vec![SigmaActionToTake::Flag(SigmaAutoFlag {
            confidence: Confidence::Likely,
            content: Some(gen_string(gen_int!(4, 16))),
            reasoning: gen_string(gen_int!(8, 32)),
        })],
        remove_actions: std::collections::BTreeSet::new(),
    }
}

/// Generate a flag metadata update touching every flag-specific field
#[allow(dead_code)]
#[must_use]
pub fn gen_flag_update(_existing: &Entity) -> EntityMetadataUpdate {
    // build a flag update that sets every field
    EntityMetadataUpdate::Flag {
        suspicion: Some(gen_int!(0, 100)),
        confidence: Some(Confidence::Unsure),
        reasoning: Some(gen_string(gen_int!(8, 32))),
        content: Some(gen_string(gen_int!(4, 16))),
    }
}

/// Generate an incident metadata update touching every incident-specific field
///
/// # Arguments
///
/// * `existing` - The created entity to pull removal targets from
#[allow(dead_code)]
#[must_use]
pub fn gen_incident_update(existing: &Entity) -> EntityMetadataUpdate {
    // pull one existing value from each list to remove
    let (remove_mission_teams, remove_networks, remove_machines, remove_locations) =
        match &existing.metadata {
            EntityMetadata::Incident(incident) => (
                incident
                    .mission_teams
                    .first()
                    .cloned()
                    .into_iter()
                    .collect(),
                incident.networks.first().cloned().into_iter().collect(),
                incident.machines.first().cloned().into_iter().collect(),
                incident.locations.first().cloned().into_iter().collect(),
            ),
            _ => (Vec::new(), Vec::new(), Vec::new(), Vec::new()),
        };
    // build an incident update that sets the cover term and edits every list
    EntityMetadataUpdate::Incident {
        cover_term: Some(gen_string(gen_int!(4, 16))),
        add_mission_teams: vec![gen_string(gen_int!(4, 16))],
        remove_mission_teams,
        add_networks: vec![gen_string(gen_int!(4, 16))],
        remove_networks,
        add_machines: vec![gen_string(gen_int!(4, 16))],
        remove_machines,
        add_locations: vec![gen_string(gen_int!(4, 16))],
        remove_locations,
    }
}

/// Generate a compiled function metadata update touching every field
#[allow(dead_code)]
#[must_use]
pub fn gen_compiled_function_update(_existing: &Entity) -> EntityMetadataUpdate {
    // build a compiled function update that sets the address and replaces disassembly
    EntityMetadataUpdate::CompiledFunction {
        function_address: Some(gen_int!(1, 100_000)),
        disassembly: vec![CompiledInstruction {
            address: gen_int!(1, 100_000),
            instruction: gen_string(gen_int!(4, 16)),
        }],
    }
}

/// Generate a decompiled function metadata update touching every field
///
/// # Arguments
///
/// * `existing` - The created entity to pull removal targets from
/// * `add_tools` - The tool instances to add
#[allow(dead_code)]
#[must_use]
pub fn gen_decompiled_function_update(
    existing: &Entity,
    add_tools: Vec<Uuid>,
) -> EntityMetadataUpdate {
    // pull an existing tool to remove
    let remove_tools = match &existing.metadata {
        EntityMetadata::DecompiledFunction(decomp) => {
            decomp.tools.first().copied().into_iter().collect()
        }
        _ => Vec::new(),
    };
    // build a decompiled function update that sets the address, content, and tools
    EntityMetadataUpdate::DecompiledFunction {
        function_address: Some(gen_int!(1, 100_000)),
        decompilation_content: Some(gen_string(gen_int!(16, 128))),
        add_tools,
        remove_tools,
        remove_legacy_tools: Vec::new(),
    }
}

/// Generate a PE section metadata update touching every field
#[allow(dead_code)]
#[must_use]
pub fn gen_pe_section_update(_existing: &Entity) -> EntityMetadataUpdate {
    // build a PE section update that sets every field
    EntityMetadataUpdate::PeSection {
        md5: Some(gen_string(32)),
        raw_size: Some(gen_int!(1, 100_000)),
        virtual_size: Some(gen_int!(1, 100_000)),
        entropy: Some(3.25),
    }
}

/// Generate a PE import metadata update replacing the imported functions
#[allow(dead_code)]
#[must_use]
pub fn gen_pe_import_update(_existing: &Entity) -> EntityMetadataUpdate {
    // build a PE import update that replaces the imported functions
    EntityMetadataUpdate::PeImport {
        functions: vec![gen_string(gen_int!(4, 16)), gen_string(gen_int!(4, 16))],
    }
}

/// Build an entity request with a random name, description, and tags
///
/// # Arguments
///
/// * `group` - The group this entity should be in
/// * `metadata` - The kind-specific metadata for this entity
#[allow(dead_code)]
#[must_use]
pub fn gen_entity(group: &str, metadata: EntityMetadataRequest) -> EntityRequest {
    // build our entity request with a random name and two random tags
    let mut req = EntityRequest::new(
        gen_string(gen_int!(8, 32)),
        metadata,
        vec![group.to_owned()],
    )
    .tag(gen_string(gen_int!(4, 16)), gen_string(gen_int!(4, 16)))
    .tag(gen_string(gen_int!(4, 16)), gen_string(gen_int!(4, 16)));
    // add a random description
    req.description = Some(gen_string(gen_int!(8, 64)));
    req
}

/// Create an entity in Thorium and return both the request and the created entity
///
/// # Arguments
///
/// * `req` - The entity request to create
/// * `client` - The client to use to create the entity
#[allow(dead_code)]
pub async fn entity(
    req: EntityRequest,
    client: &Thorium,
) -> Result<(EntityRequest, Entity), Error> {
    // create the entity in Thorium
    let resp = client.entities.create(req.clone()).await?;
    // get the full entity we just created
    let entity = client.entities.get(resp.id).await?;
    Ok((req, entity))
}

/// Create a vendor entity in a group and return the created entity
///
/// This is used to satisfy the vendor dependency of device entities.
///
/// # Arguments
///
/// * `group` - The group this vendor should be in
/// * `client` - The client to use to create the vendor
#[allow(dead_code)]
pub async fn vendor_entity(group: &str, client: &Thorium) -> Result<Entity, Error> {
    // build and create a vendor entity
    let (_, entity) = entity(gen_entity(group, gen_vendor_meta()), client).await?;
    Ok(entity)
}

/// Create a filesystem entity in a group and return the created entity
///
/// This is used to satisfy the filesystem dependency of folder entities.
///
/// # Arguments
///
/// * `group` - The group this filesystem should be in
/// * `client` - The client to use to create the filesystem
#[allow(dead_code)]
pub async fn filesystem_entity(group: &str, client: &Thorium) -> Result<Entity, Error> {
    // build and create a filesystem entity
    let (_, entity) = entity(gen_entity(group, gen_filesystem_meta(Vec::new())), client).await?;
    Ok(entity)
}

/// Generate a random agentic tool spec backed by one of Thorium's MCP tools
#[allow(dead_code)]
#[must_use]
pub fn gen_agentic_spec() -> ToolSpec {
    // build an object input schema with a random property
    let mut input_schema = serde_json::Map::new();
    input_schema.insert("type".into(), "object".into());
    input_schema.insert(
        "properties".into(),
        serde_json::json!({ "sha256": {"type": "string", "description": gen_string(16)} }),
    );
    // build a random agentic tool behind the MCP get_sample tool
    ToolSpec::Agentic(AgenticTool {
        function_name: format!("tool_{}", gen_string(12).to_lowercase()),
        title: Some(gen_string(12)),
        model_description: gen_string(32),
        input_schema,
        backing: AgenticToolBacking::ThoriumMcp {
            tool: "get_sample".into(),
        },
        timeout: Some(gen_int!(30, 600)),
        ..AgenticTool::default()
    })
}

/// Generate a random Image tool spec that runs in K8s
#[allow(dead_code)]
#[must_use]
pub fn gen_image_spec() -> ToolSpec {
    // build a K8s workload with a random image and resources
    ToolSpec::Image(WorkloadSpec {
        environment: WorkloadEnvironment::K8s(K8sEnvironment {
            image: format!(
                "registry.local/{}:{}",
                gen_string(12).to_lowercase(),
                gen_int!(1, 99)
            ),
            scheduling: SchedulingSettings {
                resources: Resources {
                    cpu: 1000,
                    memory: 512,
                    ..Resources::default()
                },
                ..SchedulingSettings::default()
            },
            ..K8sEnvironment::default()
        }),
        ..WorkloadSpec::default()
    })
}

/// Generate a random Harness tool spec that runs in K8s with the standard file layout, can call
/// tools behind Thorium every way, and can hand files to them
#[allow(dead_code)]
#[must_use]
pub fn gen_harness_spec() -> ToolSpec {
    // a K8s workload with a random image and every interface path at its default
    let ToolSpec::Image(workload) = gen_image_spec() else {
        unreachable!("an image spec is always an Image tool spec")
    };
    // support every mechanism for tools behind Thorium, with file I/O
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
    ToolSpec::Harness(HarnessSpec {
        workload,
        capabilities,
        ..HarnessSpec::default()
    })
}

/// Generate a random agentic tool spec built into a harness
#[allow(dead_code)]
#[must_use]
pub fn gen_native_spec() -> ToolSpec {
    // a native tool with a random function and harness tool name
    let name = format!("native_{}", gen_string(10).to_lowercase());
    ToolSpec::Agentic(AgenticTool {
        function_name: name.clone(),
        model_description: format!("Runs the harness's built in {name}"),
        input_schema: [
            ("type".to_owned(), serde_json::json!("object")),
            ("properties".to_owned(), serde_json::json!({})),
        ]
        .into_iter()
        .collect(),
        backing: AgenticToolBacking::HarnessNative {
            tool: name,
            config: None,
        },
        ..AgenticTool::default()
    })
}

/// Build a request to create a tool and its first instance in some groups
///
/// # Arguments
///
/// * `groups` - The groups the tool should be in
/// * `version` - The version label of the first instance
/// * `spec` - The spec of the first instance
///
/// # Panics
///
/// Panics if the spec can't be serialized, which never happens for a typed spec
#[allow(dead_code)]
#[must_use]
pub fn gen_tool(groups: &[&str], version: &str, spec: &ToolSpec) -> EntityRequest {
    // encode the spec into a tool request
    let meta = ToolEntityRequest::new(version, spec).expect("failed to encode a tool spec");
    // build the entity request with a random name
    let mut req = EntityRequest::new(
        format!("tool{}", gen_string(12).to_lowercase()),
        EntityMetadataRequest::Tool(meta),
        groups.to_vec(),
    );
    req.description = Some(gen_string(24));
    req
}

/// Create a tool and its first instance, returning the tool and the instance
///
/// # Arguments
///
/// * `groups` - The groups the tool should be in
/// * `version` - The version label of the first instance
/// * `spec` - The spec of the first instance
/// * `client` - The client to create the tool with
#[allow(dead_code)]
pub async fn tool(
    groups: &[&str],
    version: &str,
    spec: &ToolSpec,
    client: &Thorium,
) -> Result<(Entity, Entity), Error> {
    // create the tool
    let resp = client
        .entities
        .create(gen_tool(groups, version, spec))
        .await?;
    // get the tool and its first instance back
    let tool = client.entities.get(resp.id).await?;
    let instance_id = resp
        .tool_instance
        .ok_or_else(|| Error::new("A tool create returned no tool instance".to_owned()))?;
    let instance = client.entities.get(instance_id).await?;
    Ok((tool, instance))
}

/// Create an Image tool in some groups and return the id of its first tool instance
///
/// # Arguments
///
/// * `groups` - The groups the tool should be in
/// * `client` - The client to create the tool with
#[allow(dead_code)]
pub async fn tool_instance(groups: &[&str], client: &Thorium) -> Result<Uuid, Error> {
    // create an Image tool and its first instance
    let (_, instance) = tool(groups, "1.0.0", &gen_image_spec(), client).await?;
    Ok(instance.id)
}

/// Build an update that publishes a new version of a tool
///
/// # Arguments
///
/// * `version` - The version label of the new instance
/// * `spec` - The spec of the new instance
///
/// # Panics
///
/// Panics if the spec can't be serialized, which never happens for a typed spec
#[allow(dead_code)]
#[must_use]
pub fn gen_tool_publish(version: &str, spec: &ToolSpec) -> EntityUpdate {
    // encode the spec into a tool update
    EntityUpdate::default().metadata(EntityMetadataUpdate::Tool {
        version: Some(version.to_owned()),
        spec: Some(serde_json::to_string(spec).expect("failed to encode a tool spec")),
        provides: Vec::new(),
        harness_env: None,
        clear_harness_env: None,
        disable_models: BTreeSet::new(),
        enable_models: BTreeSet::new(),
    })
}

/// Build an update that deprecates or un-deprecates a tool instance
///
/// # Arguments
///
/// * `deprecated` - Whether the instance should be deprecated
#[allow(dead_code)]
#[must_use]
pub fn gen_tool_deprecate(deprecated: bool) -> EntityUpdate {
    // set the instance's deprecated flag
    EntityUpdate::default().metadata(EntityMetadataUpdate::ToolInstance {
        deprecated: Some(deprecated),
    })
}

/// Generate an Anthropic provider that uses the default url
#[allow(dead_code)]
#[must_use]
pub fn gen_anthropic_provider() -> ModelProvider {
    // the default url and version with no betas
    ModelProvider::Anthropic {
        base_url: None,
        version: "2023-06-01".to_owned(),
        betas: BTreeSet::new(),
        workspace_id: None,
    }
}

/// Generate a `LiteLLM` provider with a random gateway url
#[allow(dead_code)]
#[must_use]
pub fn gen_litellm_provider() -> ModelProvider {
    // a random gateway that takes the key in the default header
    ModelProvider::LiteLlm {
        base_url: format!("https://{}.gateway.local/v1", gen_string(10).to_lowercase()),
        key_header: None,
    }
}

/// Generate a catalog entry for a random model id
#[allow(dead_code)]
#[must_use]
pub fn gen_catalog_entry() -> ModelCatalogEntryRequest {
    // a random model with known limits
    let mut entry =
        ModelCatalogEntryRequest::new(format!("model-{}", gen_string(10).to_lowercase()));
    entry.context_window = Some(200_000);
    entry.max_output_tokens = Some(8_192);
    entry
}

/// Build a request to create a model entity with a random name in some groups
///
/// # Arguments
///
/// * `groups` - The groups the entity should be in
/// * `metadata` - The model entity's metadata
#[allow(dead_code)]
#[must_use]
pub fn gen_model_entity(groups: &[&str], metadata: EntityMetadataRequest) -> EntityRequest {
    // pick a prefix from the kind
    let prefix = match &metadata {
        EntityMetadataRequest::ModelEndpoint(_) => "endpoint",
        EntityMetadataRequest::ModelProfile(_) => "profile",
        _ => "credential",
    };
    EntityRequest::new(
        format!("{prefix}{}", gen_string(12).to_lowercase()),
        metadata,
        groups.to_vec(),
    )
}

/// Create a model credential and read it back
///
/// # Arguments
///
/// * `groups` - The groups the credential should be in
/// * `kind` - The kind of credential
/// * `secret` - The credential's primary secret, if it has one
/// * `client` - The client to create the credential with
#[allow(dead_code)]
pub async fn model_credential(
    groups: &[&str],
    kind: CredentialKind,
    secret: Option<&str>,
    client: &Thorium,
) -> Result<Entity, Error> {
    // build and create the credential
    let meta = ModelCredentialEntityRequest::new(kind, secret.map(ModelSecret::from));
    let req = gen_model_entity(groups, EntityMetadataRequest::ModelCredential(meta));
    let resp = client.entities.create(req).await?;
    client.entities.get(resp.id).await
}

/// Serializes the whitelist changes tests make so concurrent changes are never lost
static WHITELIST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Change the deployment's allowed model endpoints as the admin
///
/// Changes from tests in one process are made one at a time, so none is lost.
///
/// # Arguments
///
/// * `update` - The change to make
/// * `scan` - Whether to run the consistency scan afterwards
#[allow(dead_code)]
pub async fn update_model_whitelist(update: ModelsConfUpdate, scan: bool) -> Result<(), Error> {
    // only one change at a time
    let _guard = WHITELIST_LOCK.lock().await;
    let admin = test_utilities::admin_client().await?;
    // pick whether the scan runs
    let params = if scan {
        SystemSettingsUpdateParams::default()
    } else {
        SystemSettingsUpdateParams::default().no_scan()
    };
    admin
        .system
        .update_settings(&SystemSettingsUpdate::default().models(update), &params)
        .await?;
    Ok(())
}

/// Allow a provider's endpoint on the deployment's whitelist if it's valid and not yet allowed
///
/// # Arguments
///
/// * `provider` - The provider to allow
#[allow(dead_code)]
pub async fn allow_model_endpoint(provider: &ModelProvider) -> Result<(), Error> {
    // providers with invalid settings are refused before the whitelist matters
    if provider.validate().is_err() {
        return Ok(());
    }
    // only one change at a time
    let _guard = WHITELIST_LOCK.lock().await;
    let admin = test_utilities::admin_client().await?;
    // skip providers that are already allowed
    let settings = admin.system.get_settings().await?;
    if settings.models.allows(provider) {
        return Ok(());
    }
    // allow it without scanning
    let update = ModelsConfUpdate::default().add_endpoint(AllowedEndpoint::for_provider(provider));
    admin
        .system
        .update_settings(
            &SystemSettingsUpdate::default().models(update),
            &SystemSettingsUpdateParams::default().no_scan(),
        )
        .await?;
    Ok(())
}

/// Create a model endpoint and read it back
///
/// # Arguments
///
/// * `groups` - The groups the endpoint should be in
/// * `req` - The endpoint request
/// * `client` - The client to create the endpoint with
#[allow(dead_code)]
pub async fn model_endpoint(
    groups: &[&str],
    req: ModelEndpointEntityRequest,
    client: &Thorium,
) -> Result<Entity, Error> {
    // allow the endpoint, then create it
    allow_model_endpoint(&req.provider).await?;
    let req = gen_model_entity(groups, EntityMetadataRequest::ModelEndpoint(req));
    let resp = client.entities.create(req).await?;
    client.entities.get(resp.id).await
}

/// Create a model profile and read it back
///
/// # Arguments
///
/// * `groups` - The groups the profile should be in
/// * `req` - The profile request
/// * `client` - The client to create the profile with
#[allow(dead_code)]
pub async fn model_profile(
    groups: &[&str],
    req: ModelProfileEntityRequest,
    client: &Thorium,
) -> Result<Entity, Error> {
    // create the profile
    let req = gen_model_entity(groups, EntityMetadataRequest::ModelProfile(req));
    let resp = client.entities.create(req).await?;
    client.entities.get(resp.id).await
}

/// Generate a random Image tool spec that runs in an environment
///
/// # Arguments
///
/// * `scaler` - The scaler whose environment the tool runs in
#[allow(dead_code)]
#[must_use]
pub fn gen_image_spec_for(scaler: ImageScaler) -> ToolSpec {
    // start from a K8s spec and take its scheduling settings
    let ToolSpec::Image(mut workload) = gen_image_spec() else {
        unreachable!("an image spec is always an Image tool spec")
    };
    let scheduling = workload
        .environment
        .scheduling()
        .cloned()
        .unwrap_or_default();
    // build the environment for this scaler
    workload.environment = match scaler {
        ImageScaler::K8s => workload.environment,
        ImageScaler::BareMetal => WorkloadEnvironment::BareMetal(BareMetalEnvironment {
            scheduling,
            ..BareMetalEnvironment::default()
        }),
        ImageScaler::Windows => WorkloadEnvironment::Windows(WindowsEnvironment {
            image: format!("registry.local/{}:1", gen_string(12).to_lowercase()),
            scheduling,
        }),
        ImageScaler::Kvm => WorkloadEnvironment::Kvm(KvmEnvironment {
            scheduling,
            kvm: None,
        }),
        ImageScaler::External => WorkloadEnvironment::External,
    };
    ToolSpec::Image(workload)
}

/// Create a tool to back a pipeline stage, returning its name and its first instance's id
///
/// # Arguments
///
/// * `group` - The group the tool should be in
/// * `spec` - The spec of the tool's first instance
/// * `client` - The client to create the tool with
#[allow(dead_code)]
pub async fn stage_tool(
    group: &str,
    spec: &ToolSpec,
    client: &Thorium,
) -> Result<(String, Uuid), Error> {
    // create the tool and its first instance
    let (tool, instance) = tool(&[group], "1.0.0", spec, client).await?;
    Ok((tool.name, instance.id))
}

/// Create groups the image routes may still write images in
///
/// # Arguments
///
/// * `cnt` - The number of groups to create
/// * `client` - The client to create the groups with
#[allow(dead_code)]
pub async fn image_groups(cnt: usize, client: &Thorium) -> Result<Vec<GroupRequest>, Error> {
    // create the groups
    let groups = groups(cnt, client).await?;
    // let tests seed images in each of them
    for group in &groups {
        test_utilities::faults::allow_image_writes(&group.name);
    }
    Ok(groups)
}

/// Get the tool instance a pipeline stage is pinned to
///
/// # Arguments
///
/// * `pipe` - The pipeline
/// * `stage` - The name of the stage
///
/// # Panics
///
/// Panics if the stage has no tool, which never happens for a generated pipeline
#[allow(dead_code)]
#[must_use]
pub fn stage_instance(pipe: &Pipeline, stage: &str) -> Uuid {
    // generated pipelines pin each stage to a tool instance
    *pipe
        .stage_tools
        .get(stage)
        .unwrap_or_else(|| panic!("stage {stage} has no tool"))
}

// Generators for sync tests

#[cfg(all(feature = "sync", not(feature = "python")))]
use crate::ThoriumBlocking;

/// Create a number of random groups in Thorium
///
/// # Arguments
///
/// * `cnt` - The number of groups to create
/// * `client` - The client to use when creating these images
#[cfg(all(feature = "sync", not(feature = "python")))]
#[allow(dead_code)]
pub fn groups_blocking(cnt: usize, client: &ThoriumBlocking) -> Result<Vec<GroupRequest>, Error> {
    // create a 20 random groups
    let groups: Vec<GroupRequest> = (0..cnt).map(|_| gen_group()).collect();
    // create groups
    for group in &groups {
        client.groups.create(group)?;
    }
    Ok(groups)
}

/// Setup a number of random images in a group
///
/// The group is first allowed image writes, since the image routes refuse writes in any group a
/// test has not opted in.
///
/// # Arguments
///
/// * `group` - The group these images should be in
/// * `cnt` - The number of images to create
/// * `external` - Whether to create images for the external scaler
/// * `client` - The client to use when creating these images
#[cfg(all(feature = "sync", not(feature = "python")))]
#[allow(dead_code)]
pub fn images_blocking(
    group: &str,
    cnt: usize,
    external: bool,
    client: &ThoriumBlocking,
) -> Result<Vec<ImageRequest>, Error> {
    // these images are seeded through the image routes, which refuse writes elsewhere
    test_utilities::faults::allow_image_writes(group);
    // create a 20 random images then
    let images: Vec<ImageRequest> = if external {
        (0..cnt).map(|_| gen_ext_image(group)).collect()
    } else {
        (0..cnt).map(|_| gen_image(group)).collect()
    };
    // create images
    for image in &images {
        client.images.create(image)?;
    }
    Ok(images)
}
