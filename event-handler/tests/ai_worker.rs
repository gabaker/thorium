//! Runs the AI worker in process against the test API
//!
//! Every test pops the shared workflow graph and model refresh queues, so the tests take turns
//! through one lock and each starts from empty queues. The workflow graph queue's maturity delay
//! is shortened to one second before the test API loads its config. Model refreshes list from mock
//! providers on local ports.

use chrono::Utc;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};
use thorium::models::entities::toolsets::{
    TOOL_SET_FOLLOWS_TOOL_TAG, TOOL_SET_ID_TAG, TOOL_SET_SELECTOR_TAG,
};
use thorium::models::entities::workflows::{
    FOLLOWS_TOOL_TAG, GraphReader, INSTANCE_AGENT_TAG, WORKFLOW_ID_TAG,
};
use thorium::models::{
    AgentEntityRequest, AiSettingsUpdate, AllowedEndpoint, AssociationKind, AzureApiMode,
    AzureClientAuth, BedrockEndpointKind, CatalogEntrySource, CrawlLimits, CredentialKind, Entity,
    EntityKinds, EntityMetadata, EntityMetadataRequest, EntityMetadataUpdate, EntityUpdate, Event,
    EventData, EventPopOpts, EventType, LimitsSource, ListedModel, ModelApi, ModelBan,
    ModelBanKind, ModelBanUpdate, ModelCatalogEntry, ModelCatalogEntryRequest,
    ModelCredentialEntity, ModelCredentialEntityRequest, ModelEndpointEntity,
    ModelEndpointEntityRequest, ModelProfileEntityRequest, ModelProvider, ModelRefreshEvent,
    ModelRefreshReason, ModelRefreshReport, ModelRefreshState, ModelRefreshTarget, ModelSecret,
    ModelsConfUpdate, NotificationLevel, ToolEffect, ToolKind, ToolSetEntityRequest,
    ToolSetExclusionReason, ToolSetInclusion, ToolSetInstanceEntity, ToolSetInstanceTrigger,
    ToolSetLimits, ToolSetSelector, ToolSpec, UserCredentialRef, UserModelEndpoint,
    UserModelSettings, UserModelSettingsUpdate, UserRole, UserSettingsUpdate, UserUpdate,
    VertexLocation, WorkflowCatalogEntity, WorkflowCatalogEntityRequest, WorkflowGraphChange,
    WorkflowGraphEvent, WorkflowInstanceEntity, WorkflowInstanceStatus, WorkflowInstanceTrigger,
    WorkflowLimits,
};
use thorium::test_utilities::generators::{GroupRoles, TestUser, god_group, moon_user};
use thorium::test_utilities::mock_provider::{MockProvider, MockReply};
use thorium::test_utilities::{self, events, generators};
use thorium::{Error, Thorium};
use thorium_event_handler::libs::workers::ai::{AiWorker, AiWorkerStats, AuthHosts, RefreshLimits};
use thorium_event_handler::libs::workers::{
    EventWorkerMutableSupport, EventWorkerSupportCore, ReactionTriggerWorker, SigmaRuleWorker,
    WorkerCacheUpdates,
};
use tokio::sync::{Mutex, MutexGuard};
use uuid::Uuid;

/// The test-only RSA key the sign-in tests sign with
const TEST_KEY: &str = include_str!("fixtures/test-only-rsa-key.pem");

/// The lock every test holds while it uses the shared queue
static SERIAL: Mutex<()> = Mutex::const_new(());

/// Sets the shortened maturity delay once, before the test API reads its config
static DELAY: Once = Once::new();

/// The config override that sets the workflow graph maturity delay
const DELAY_KEY: &str = "THORIUM__THORIUM__EVENTS__WORKFLOW_GRAPH_DELAY";

/// The maturity delay the tests run with, in seconds
const DELAY_SECS: u64 = 1;

/// A test's turn at the shared queue with an admin client and an AI worker
struct Harness {
    /// The admin client the worker and the test share
    admin: Thorium,
    /// The worker under test
    worker: AiWorker,
    /// This test's turn at the queue
    _turn: MutexGuard<'static, ()>,
}

impl Harness {
    /// Wait for this test's turn, start the test API, and empty the workflow graph queue
    async fn new() -> Result<Self, Error> {
        // take our turn before touching the queue
        let turn = SERIAL.lock().await;
        // shorten the maturity delay before the config is loaded
        DELAY.call_once(|| {
            if std::env::var_os(DELAY_KEY).is_none() {
                // SAFETY: this runs once, before any test reads the environment through the
                // config, while every other test waits for its turn
                unsafe { std::env::set_var(DELAY_KEY, DELAY_SECS.to_string()) };
            }
        });
        // start the API and drop whatever earlier tests left queued
        let admin = test_utilities::admin_client().await?;
        events::discard(EventType::WorkflowGraph).await?;
        events::discard(EventType::ModelRefresh).await?;
        let worker = new_worker(&admin);
        Ok(Harness {
            admin,
            worker,
            _turn: turn,
        })
    }

    /// Wait until every queued event is mature and pop them
    async fn pop(&self) -> Result<Vec<Event>, Error> {
        // an event is mature once its second is more than the delay before now
        mature().await;
        let opts = EventPopOpts::default().limit(1000);
        self.admin.events.pop(EventType::WorkflowGraph, &opts).await
    }

    /// Pop every mature event and process it
    async fn run(&mut self) -> Result<AiWorkerStats, Error> {
        // pop and process one page
        let events = self.pop().await?;
        self.process(events).await
    }

    /// Process some popped events
    ///
    /// # Arguments
    ///
    /// * `events` - The popped events
    async fn process(&mut self, events: Vec<Event>) -> Result<AiWorkerStats, Error> {
        // process them as the event worker wrapper does
        let mut stats = AiWorkerStats::default();
        EventWorkerMutableSupport::process(&mut self.worker, events, &mut stats).await?;
        Ok(stats)
    }

    /// Get the events still queued or in flight
    async fn leftover(&self) -> Result<events::QueueState, Error> {
        // read the queue's Redis state
        events::queue_state(EventType::WorkflowGraph).await
    }
}

/// Build an AI worker over an admin client with the test API's crawl limits
///
/// # Arguments
///
/// * `admin` - The admin client
fn new_worker(admin: &Thorium) -> AiWorker {
    // the worker instances with the admin client
    AiWorker::with_client(
        Arc::new(admin.clone()),
        CrawlLimits::from(&test_utilities::CONF.thorium.events),
        ToolSetLimits::from(&test_utilities::CONF.thorium.events),
    )
    .expect("the AI worker builds")
}

/// Wait long enough for every event queued so far to be mature
async fn mature() {
    // the delay plus the rest of the current second
    tokio::time::sleep(Duration::from_millis(DELAY_SECS * 1000 + 1100)).await;
}

/// The users and group one test uses
struct Scene {
    /// The test's name suffix
    suffix: String,
    /// The user who edits the graph
    europa: TestUser,
    /// The group everything is in
    group: String,
}

/// Create a user and a group they can edit in
///
/// # Arguments
///
/// * `admin` - The admin client
async fn scene(admin: &Thorium) -> Result<Scene, Error> {
    // europa edits in minerva
    let suffix = generators::name_suffix();
    let europa = moon_user(admin, "europa", &suffix).await?;
    let roles = GroupRoles::default().user(&europa.username);
    let group = god_group(admin, "minerva", &suffix, &roles).await?;
    Ok(Scene {
        suffix,
        europa,
        group,
    })
}

/// Create an agent that runs in a harness of its own and get it back
///
/// The agent is pinned to a new Harness tool in its groups, since a workflow's entry agent must
/// run in one; `req` may replace it with another harness.
///
/// # Arguments
///
/// * `client` - The client to create the agent and its harness with
/// * `name` - The agent's name
/// * `groups` - The agent's groups
/// * `req` - Adds the agent's edges
async fn agent(
    client: &Thorium,
    name: &str,
    groups: &[&str],
    req: impl FnOnce(AgentEntityRequest) -> AgentEntityRequest,
) -> Result<Entity, Error> {
    // publish the agent's own harness
    let (_, runtime) =
        generators::tool(groups, "1.0.0", &generators::gen_harness_spec(), client).await?;
    // build a config named after the agent, pinned to its harness, and add its edges
    let base = AgentEntityRequest::new(generators::gen_agent_config(&name.replace('-', "_")))
        .harness(runtime.id);
    let resp = client
        .entities
        .create(generators::gen_agent(name, groups, req(base)))
        .await?;
    client.entities.get(resp.id).await
}

/// Create a workflow whose only agent is its entry agent and get it back
///
/// # Arguments
///
/// * `client` - The client to create the workflow with
/// * `name` - The workflow's name
/// * `groups` - The workflow's groups
/// * `entry` - The entry agent
async fn workflow(
    client: &Thorium,
    name: &str,
    groups: &[&str],
    entry: Uuid,
) -> Result<Entity, Error> {
    // orchestrate the entry agent
    let req = WorkflowCatalogEntityRequest::default()
        .agent(entry)
        .entry_agent(entry);
    workflow_with(client, name, groups, req).await
}

/// Create a workflow from a request and get it back
///
/// # Arguments
///
/// * `client` - The client to create the workflow with
/// * `name` - The workflow's name
/// * `groups` - The workflow's groups
/// * `req` - The workflow's agents and settings
async fn workflow_with(
    client: &Thorium,
    name: &str,
    groups: &[&str],
    req: WorkflowCatalogEntityRequest,
) -> Result<Entity, Error> {
    // create the workflow and read it back
    let resp = client
        .entities
        .create(generators::gen_workflow(name, groups, req))
        .await?;
    client.entities.get(resp.id).await
}

/// Get a workflow catalog's metadata
///
/// # Arguments
///
/// * `entity` - The workflow catalog
fn catalog_meta(entity: &Entity) -> &WorkflowCatalogEntity {
    // only a catalog has catalog metadata
    match &entity.metadata {
        EntityMetadata::WorkflowCatalog(meta) => meta,
        other => panic!("expected a workflow catalog, got {other:?}"),
    }
}

/// Get a workflow instance's metadata
///
/// # Arguments
///
/// * `entity` - The workflow instance
fn instance_meta(entity: &Entity) -> &WorkflowInstanceEntity {
    // only an instance has instance metadata
    match &entity.metadata {
        EntityMetadata::WorkflowInstance(meta) => meta,
        other => panic!("expected a workflow instance, got {other:?}"),
    }
}

/// Get a workflow's instances ordered by revision, each read in full with its tags
///
/// # Arguments
///
/// * `admin` - The admin client
/// * `workflow` - The workflow catalog's id
async fn instances(admin: &Thorium, workflow: Uuid) -> Result<Vec<Entity>, Error> {
    // find the workflow's instances by tag in its groups
    let catalog = admin.entities.get(workflow).await?;
    let found = admin
        .tagged(
            EntityKinds::WorkflowInstance,
            WORKFLOW_ID_TAG,
            &workflow.to_string(),
            &catalog.groups,
        )
        .await?;
    // read each one in full and order them by revision
    let mut full = Vec::with_capacity(found.len());
    for entity in found {
        full.push(admin.entities.get(entity.id).await?);
    }
    full.sort_by_key(|entity| instance_meta(entity).revision);
    Ok(full)
}

/// Get the values of one tag on an entity, sorted
///
/// # Arguments
///
/// * `entity` - The entity
/// * `key` - The tag key
fn tag_values(entity: &Entity, key: &str) -> Vec<String> {
    // the tag's values, sorted
    let mut values = entity.tags.get(key).map_or_else(Vec::new, |values| {
        values.keys().cloned().collect::<Vec<String>>()
    });
    values.sort();
    values
}

/// Send a metadata update
///
/// # Arguments
///
/// * `client` - The client to send it with
/// * `id` - The entity to update
/// * `update` - The metadata update
async fn update(client: &Thorium, id: Uuid, update: EntityMetadataUpdate) -> Result<(), Error> {
    // wrap the metadata update in an entity update
    client
        .entities
        .update(id, EntityUpdate::default().metadata(update))
        .await?;
    Ok(())
}

/// Build an agent update that changes nothing until it's edited
///
/// # Arguments
///
/// * `change` - Sets the fields of the update
fn agent_update(change: impl FnOnce(&mut EntityMetadataUpdate)) -> EntityMetadataUpdate {
    // start from an update that changes nothing
    let mut update = EntityMetadataUpdate::Agent {
        config: None,
        model_auth: None,
        harness: None,
        clear_harness: None,
        add_tools: Vec::new(),
        remove_tools: Vec::new(),
        add_tool_sets: Vec::new(),
        remove_tool_sets: Vec::new(),
        add_sub_agents: Vec::new(),
        remove_sub_agents: Vec::new(),
        extend_target_groups: false,
    };
    change(&mut update);
    update
}

/// Build a workflow update that changes nothing until it's edited
///
/// # Arguments
///
/// * `change` - Sets the fields of the update
fn workflow_update(change: impl FnOnce(&mut EntityMetadataUpdate)) -> EntityMetadataUpdate {
    // start from an update that changes nothing
    let mut update = EntityMetadataUpdate::WorkflowCatalog {
        entry_agent: None,
        clear_entry_agent: None,
        auto_instance: None,
        limits: None,
        default_models: None,
        model_auth: None,
        add_agents: Vec::new(),
        remove_agents: Vec::new(),
        extend_target_groups: false,
    };
    change(&mut update);
    update
}

/// Build a workflow update that sets its sub-agent limit, which changes its graph
///
/// # Arguments
///
/// * `max` - The new sub-agent limit
fn limit_update(max: u32) -> EntityMetadataUpdate {
    // set only the limit
    workflow_update(|update| {
        if let EntityMetadataUpdate::WorkflowCatalog { limits, .. } = update {
            *limits = Some(WorkflowLimits {
                max_alive_sub_agents: Some(max),
            });
        }
    })
}

/// Build an agent update that replaces its config with a fresh one
///
/// # Arguments
///
/// * `name` - The function name of the new config
fn config_update(name: &str) -> EntityMetadataUpdate {
    // a new config changes the agent's config hash
    agent_update(|update| {
        if let EntityMetadataUpdate::Agent { config, .. } = update {
            *config = Some(generators::gen_agent_config(name));
        }
    })
}

/// Build a workflow graph event the way the API queues it
///
/// # Arguments
///
/// * `user` - The user who caused the change
/// * `groups` - Every group of the changed entity
/// * `change` - What changed
fn graph_event(user: &str, groups: &[&str], change: WorkflowGraphChange) -> Event {
    // a fresh top level event
    Event {
        id: Uuid::new_v4(),
        timestamp: Utc::now(),
        parent: None,
        user: user.to_owned(),
        data: EventData::WorkflowGraph(WorkflowGraphEvent {
            groups: groups.iter().map(|group| (*group).to_owned()).collect(),
            change,
        }),
        depth: 0,
    }
}

/// Get a user's notification messages and levels
///
/// # Arguments
///
/// * `user` - The user
async fn notes(user: &TestUser) -> Result<Vec<(NotificationLevel, String)>, Error> {
    // list the user's own notifications
    let notes = user.client.users.notifications().await?;
    Ok(notes
        .into_iter()
        .map(|note| (note.level, note.msg))
        .collect())
}

/// Two users who both edit in one group
struct Crew {
    /// The first user who edits the graph
    europa: TestUser,
    /// The second user who edits the graph
    callisto: TestUser,
    /// The group everything is in
    group: String,
}

/// Create two users and a group they both edit in
///
/// # Arguments
///
/// * `admin` - The admin client
async fn crew(admin: &Thorium) -> Result<Crew, Error> {
    // europa and callisto edit in minerva
    let suffix = generators::name_suffix();
    let europa = moon_user(admin, "europa", &suffix).await?;
    let callisto = moon_user(admin, "callisto", &suffix).await?;
    let roles = GroupRoles::default()
        .user(&europa.username)
        .user(&callisto.username);
    let group = god_group(admin, "minerva", &suffix, &roles).await?;
    Ok(Crew {
        europa,
        callisto,
        group,
    })
}

/// Create a toolset and get it back
///
/// # Arguments
///
/// * `client` - The client to create the toolset with
/// * `name` - The toolset's name
/// * `groups` - The toolset's groups
/// * `req` - The toolset's settings and included tools
async fn toolset(
    client: &Thorium,
    name: &str,
    groups: &[&str],
    req: ToolSetEntityRequest,
) -> Result<Entity, Error> {
    // create the toolset and read it back
    let resp = client
        .entities
        .create(generators::gen_toolset(name, groups, req))
        .await?;
    client.entities.get(resp.id).await
}

/// Get a toolset instance's metadata
///
/// # Arguments
///
/// * `entity` - The toolset instance
fn set_meta(entity: &Entity) -> &ToolSetInstanceEntity {
    // only a toolset instance has toolset instance metadata
    match &entity.metadata {
        EntityMetadata::ToolSetInstance(meta) => meta,
        other => panic!("expected a toolset instance, got {other:?}"),
    }
}

/// Get a toolset's instances ordered by revision, each read in full with its tags
///
/// # Arguments
///
/// * `admin` - The admin client
/// * `tool_set` - The toolset's id
async fn set_instances(admin: &Thorium, tool_set: Uuid) -> Result<Vec<Entity>, Error> {
    // find the toolset's instances by tag in its groups
    let root = admin.entities.get(tool_set).await?;
    let found = admin
        .tagged(
            EntityKinds::ToolSetInstance,
            TOOL_SET_ID_TAG,
            &tool_set.to_string(),
            &root.groups,
        )
        .await?;
    // read each one in full and order them by revision
    let mut full = Vec::with_capacity(found.len());
    for entity in found {
        full.push(admin.entities.get(entity.id).await?);
    }
    full.sort_by_key(|entity| set_meta(entity).revision);
    Ok(full)
}

/// Get the tool instances a toolset instance holds, sorted
///
/// # Arguments
///
/// * `entity` - The toolset instance
fn member_ids(entity: &Entity) -> Vec<Uuid> {
    // each member's tool instance, sorted
    let mut ids = set_meta(entity)
        .members
        .iter()
        .map(|member| member.tool_instance)
        .collect::<Vec<Uuid>>();
    ids.sort();
    ids
}

/// Build a toolset update that changes nothing until it's edited
///
/// # Arguments
///
/// * `change` - Sets the fields of the update
fn set_update(change: impl FnOnce(&mut EntityMetadataUpdate)) -> EntityMetadataUpdate {
    // start from an update that changes nothing
    let mut update = EntityMetadataUpdate::ToolSet {
        read_only: None,
        selector: None,
        clear_selector: None,
        auto_instance: None,
        add_tools: Vec::new(),
        remove_tools: Vec::new(),
        extend_target_groups: false,
    };
    change(&mut update);
    update
}

/// Add or remove a user tag on an entity as a user, returning the status code
///
/// # Arguments
///
/// * `user` - The user to send the request as
/// * `id` - The entity to tag
/// * `delete` - Whether to remove the tag instead of adding it
/// * `groups` - The groups to tag it in
/// * `key` - The tag key
/// * `value` - The tag value
async fn tag_entity(
    user: &TestUser,
    id: Uuid,
    delete: bool,
    groups: &[&str],
    key: &str,
    value: &str,
) -> Result<u16, Error> {
    // build the tag request for this entity
    let url = format!("{}/api/entities/tags/{id}", user.client.host);
    let client = reqwest::Client::new();
    let req = if delete {
        client.delete(url)
    } else {
        client.post(url)
    };
    let groups = groups
        .iter()
        .map(|group| format!("{group:?}"))
        .collect::<Vec<String>>()
        .join(",");
    let body = format!(r#"{{"groups":[{groups}],"tags":{{{key:?}:[{value:?}]}}}}"#);
    // send it as the user
    let resp = req
        .header("authorization", user.auth_header())
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await?;
    Ok(resp.status().as_u16())
}

/// Get the changes waiting in the workflow graph queue, oldest first
///
/// # Arguments
///
/// * `admin` - The admin client to peek with
async fn waiting(admin: &Thorium) -> Result<Vec<WorkflowGraphChange>, Error> {
    // peek at the queue and keep each change
    let queued = events::queued(admin, EventType::WorkflowGraph).await?;
    Ok(queued
        .iter()
        .filter_map(events::graph_event)
        .map(|graph| graph.change.clone())
        .collect())
}

/// Build an agentic tool spec that declares some effects
///
/// # Arguments
///
/// * `effects` - The effects the tool declares
fn spec_with_effects(effects: &[ToolEffect]) -> ToolSpec {
    // a random agentic spec with these effects
    let mut spec = generators::gen_agentic_spec();
    if let ToolSpec::Agentic(agentic) = &mut spec {
        agentic.capabilities.effects = effects.iter().copied().collect();
    }
    spec
}

/// Get a tool's latest instance
///
/// # Arguments
///
/// * `admin` - The admin client
/// * `tool` - The tool's id
async fn latest_of(admin: &Thorium, tool: Uuid) -> Result<Uuid, Error> {
    // read the tool and take its latest instance
    match admin.entities.get(tool).await?.metadata {
        EntityMetadata::Tool(meta) => Ok(meta.latest.expect("the tool has no latest instance")),
        other => panic!("expected a tool, got {other:?}"),
    }
}

/// The users, group, and mock provider one model refresh test uses
struct ModelScene {
    /// The test's name suffix
    suffix: String,
    /// The user who owns the group and its model configuration
    europa: TestUser,
    /// A member of the group who doesn't manage it
    io: TestUser,
    /// The group everything is in
    group: String,
    /// The mock provider endpoints list from
    mock: MockProvider,
}

/// Create an owner, a member, their group, and a mock provider
///
/// # Arguments
///
/// * `admin` - The admin client
async fn model_scene(admin: &Thorium) -> Result<ModelScene, Error> {
    // europa owns minerva and io is a member
    let suffix = generators::name_suffix();
    let europa = moon_user(admin, "europa", &suffix).await?;
    let io = moon_user(admin, "io", &suffix).await?;
    let roles = GroupRoles::default()
        .owner(&europa.username)
        .user(&io.username);
    let group = god_group(admin, "minerva", &suffix, &roles).await?;
    let mock = MockProvider::start().await?;
    Ok(ModelScene {
        suffix,
        europa,
        io,
        group,
        mock,
    })
}

/// Build an Anthropic provider that calls a mock provider
///
/// # Arguments
///
/// * `mock` - The mock provider
fn anthropic_on(mock: &MockProvider) -> ModelProvider {
    ModelProvider::Anthropic {
        base_url: Some(mock.url()),
        version: "2023-06-01".to_owned(),
        betas: BTreeSet::new(),
        workspace_id: None,
    }
}

/// Build an Anthropic model listing body
///
/// # Arguments
///
/// * `models` - Each model's id, max input tokens, and max output tokens
fn anthropic_listing(models: &[(&str, u64, u64)]) -> String {
    // one entry per model, on a single page
    let data = models
        .iter()
        .map(|(id, input, output)| {
            format!(r#"{{"id": "{id}", "max_input_tokens": {input}, "max_tokens": {output}}}"#)
        })
        .collect::<Vec<String>>()
        .join(", ");
    format!(r#"{{"data": [{data}], "has_more": false}}"#)
}

/// Create a group API key credential and get its id
///
/// # Arguments
///
/// * `client` - The client to create it with
/// * `group` - Its group
/// * `secret` - Its key
async fn api_key(client: &Thorium, group: &str, secret: &str) -> Result<Uuid, Error> {
    // an API key credential
    Ok(
        generators::model_credential(&[group], CredentialKind::ApiKey, Some(secret), client)
            .await?
            .id,
    )
}

/// Create a group endpoint and get its id
///
/// # Arguments
///
/// * `client` - The client to create it with
/// * `group` - Its group
/// * `provider` - Its provider
/// * `credential` - Its group credential, if any
/// * `change` - Edits the request before it's sent
async fn endpoint_with(
    client: &Thorium,
    group: &str,
    provider: ModelProvider,
    credential: Option<Uuid>,
    change: impl FnOnce(ModelEndpointEntityRequest) -> ModelEndpointEntityRequest,
) -> Result<Uuid, Error> {
    // allow the provider and create the endpoint
    let mut req = ModelEndpointEntityRequest::new(provider);
    req.credential = credential;
    Ok(generators::model_endpoint(&[group], change(req), client)
        .await?
        .id)
}

/// Read an endpoint's metadata
///
/// # Arguments
///
/// * `admin` - The admin client
/// * `id` - The endpoint
async fn endpoint_of(admin: &Thorium, id: Uuid) -> Result<ModelEndpointEntity, Error> {
    // only an endpoint has endpoint metadata
    match admin.entities.get(id).await?.metadata {
        EntityMetadata::ModelEndpoint(meta) => Ok(meta),
        other => panic!("expected a model endpoint, got {other:?}"),
    }
}

/// Read a credential's metadata
///
/// # Arguments
///
/// * `admin` - The admin client
/// * `id` - The credential
async fn credential_of(admin: &Thorium, id: Uuid) -> Result<ModelCredentialEntity, Error> {
    // only a credential has credential metadata
    match admin.entities.get(id).await?.metadata {
        EntityMetadata::ModelCredential(meta) => Ok(meta),
        other => panic!("expected a model credential, got {other:?}"),
    }
}

/// Get the names of the kinds of a ban list's bans, sorted
///
/// # Arguments
///
/// * `bans` - The ban list
fn ban_names(bans: &HashMap<Uuid, ModelBan>) -> Vec<&'static str> {
    // name each ban by its kind
    let mut names = bans
        .values()
        .map(|ban| ban.ban_kind.name())
        .collect::<Vec<&str>>();
    names.sort_unstable();
    names
}

/// Get a catalog entry of an endpoint
///
/// # Arguments
///
/// * `meta` - The endpoint
/// * `model_id` - The entry's model id
fn entry_of<'a>(meta: &'a ModelEndpointEntity, model_id: &str) -> &'a ModelCatalogEntry {
    meta.catalog_entry(model_id)
        .unwrap_or_else(|| panic!("{model_id} isn't in the catalog"))
}

/// Pop every queued model refresh and process it
///
/// # Arguments
///
/// * `harness` - The test's harness
async fn refresh_pass(harness: &mut Harness) -> Result<AiWorkerStats, Error> {
    // refreshes have no maturity delay, so one pop gets them all
    let opts = EventPopOpts::default().limit(1000);
    let events = harness
        .admin
        .events
        .pop(EventType::ModelRefresh, &opts)
        .await?;
    harness.process(events).await
}

/// Build a model refresh event the way the API queues it
///
/// # Arguments
///
/// * `user` - The user who caused the refresh
/// * `target` - What to refresh
fn refresh_event(user: &str, target: ModelRefreshTarget) -> Event {
    // a fresh top level event
    Event {
        id: Uuid::new_v4(),
        timestamp: Utc::now(),
        parent: None,
        user: user.to_owned(),
        data: EventData::ModelRefresh(ModelRefreshEvent {
            target,
            reason: ModelRefreshReason::Manual,
        }),
        depth: 0,
    }
}

/// Build the refresh target of a group endpoint
///
/// # Arguments
///
/// * `group` - The endpoint's group
/// * `endpoint` - The endpoint
fn group_target(group: &str, endpoint: Uuid) -> ModelRefreshTarget {
    ModelRefreshTarget::GroupEndpoint {
        groups: vec![group.to_owned()],
        endpoint,
    }
}

/// Send an update of a user's own model settings
///
/// # Arguments
///
/// * `client` - The client of the user to update
/// * `update` - The model settings update
async fn set_models(client: &Thorium, update: UserModelSettingsUpdate) -> Result<(), Error> {
    // wrap the model update in a user settings update
    let ai = AiSettingsUpdate::default().models(update);
    let settings = UserSettingsUpdate::default().ai(ai);
    client
        .users
        .update_self(UserUpdate::default().settings(settings))
        .await?;
    Ok(())
}

/// Get a user's own model settings
///
/// # Arguments
///
/// * `client` - The client of the user to read
async fn own_models(client: &Thorium) -> Result<UserModelSettings, Error> {
    // read the caller's settings
    let info = client.users.info().await?;
    Ok(info.settings.ai.map_or_default(|ai| ai.models))
}

/// A refresh of an endpoint the deployment no longer allows is cleared without any request to
/// it, whether the endpoint is banned for it yet or not
#[tokio::test]
async fn deallowed_endpoints_are_never_contacted() -> Result<(), Error> {
    // europa's endpoint, refreshed once while it's allowed
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = model_scene(&admin).await?;
    scene.mock.reply(
        "GET",
        "/v1/models",
        MockReply::json(anthropic_listing(&[("claude-a", 200_000, 64_000)])),
    );
    let credential = api_key(&scene.europa.client, &scene.group, "sk-deallowed").await?;
    let provider = anthropic_on(&scene.mock);
    let endpoint = endpoint_with(
        &scene.europa.client,
        &scene.group,
        provider.clone(),
        Some(credential),
        |req| req,
    )
    .await?;
    refresh_pass(&mut harness).await?;
    assert_eq!(scene.mock.requests().len(), 1);
    scene.mock.clear_requests();
    // a refresh queued before the deployment stops allowing it, with no scan to ban it
    scene
        .europa
        .client
        .entities
        .refresh_model_endpoint(endpoint)
        .await?;
    let entry = AllowedEndpoint::for_provider(&provider);
    generators::update_model_whitelist(
        ModelsConfUpdate::default().remove_endpoint(entry.clone()),
        false,
    )
    .await?;
    // is cleared without contacting it
    let stats = refresh_pass(&mut harness).await?;
    assert_eq!(stats.events_processed, 1);
    assert!(scene.mock.requests().is_empty());
    let left = events::queue_state(EventType::ModelRefresh).await?;
    assert!(left.queued.is_empty() && left.in_flight.is_empty());
    // a scan bans it, and allowing it again without a scan leaves the ban
    generators::update_model_whitelist(ModelsConfUpdate::default(), true).await?;
    generators::update_model_whitelist(ModelsConfUpdate::default().add_endpoint(entry), false)
        .await?;
    assert_eq!(
        ban_names(&endpoint_of(&admin, endpoint).await?.bans),
        vec!["NotAllowedByDeployment"]
    );
    // a refresh of the banned endpoint is cleared without contacting it either
    harness
        .process(vec![refresh_event(
            &scene.europa.username,
            group_target(&scene.group, endpoint),
        )])
        .await?;
    assert!(scene.mock.requests().is_empty());
    Ok(())
}

/// A refresh pass that runs past its deadline ends at the deadline and leaves its unfinished
/// refreshes in flight, so workflow graph events handed to the worker in the same batch are
/// built and cleared once the deadline passes
#[tokio::test]
async fn refresh_pass_deadline_bounds_its_batch() -> Result<(), Error> {
    // a keyless gateway that never answers
    let harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = model_scene(&admin).await?;
    scene.mock.reply("GET", "/stuck/models", MockReply::hang());
    let stuck = ModelProvider::OpenAiCompatible {
        base_url: format!("{}/stuck", scene.mock.url()),
        extra_headers: BTreeMap::new(),
        key_header: None,
    };
    endpoint_with(&scene.europa.client, &scene.group, stuck, None, |req| req).await?;
    // a workflow edit queued beside its refresh
    let groups = [scene.group.as_str()];
    let lead = agent(&admin, "lead", &groups, |req| req).await?;
    let flow = workflow(&scene.europa.client, "beside-stall", &groups, lead.id).await?;
    mature().await;
    let opts = EventPopOpts::default().limit(1000);
    let mut popped = admin.events.pop(EventType::ModelRefresh, &opts).await?;
    popped.extend(admin.events.pop(EventType::WorkflowGraph, &opts).await?);
    // one pass with a three second refresh deadline builds the instance and leaves the refresh
    let mut worker = new_worker(&admin).refresh_limits(RefreshLimits {
        pass: Duration::from_secs(3),
        ..RefreshLimits::default()
    });
    let started = Instant::now();
    let mut stats = AiWorkerStats::default();
    EventWorkerMutableSupport::process(&mut worker, popped, &mut stats).await?;
    let took = started.elapsed();
    assert!(took < Duration::from_secs(10), "took {took:?}");
    assert_eq!(instances(&admin, flow.id).await?.len(), 1);
    let left = events::queue_state(EventType::ModelRefresh).await?;
    assert_eq!(left.in_flight.len(), 1);
    assert!(worker.take_retry_ts().await.is_some());
    Ok(())
}

/// Refreshes of one endpoint queued by the same user in one batch list it once and clear every
/// one of them
#[tokio::test]
async fn duplicate_refreshes_list_once() -> Result<(), Error> {
    // europa's endpoint, refreshed once
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = model_scene(&admin).await?;
    scene.mock.reply(
        "GET",
        "/v1/models",
        MockReply::json(anthropic_listing(&[("claude-a", 200_000, 64_000)])),
    );
    let credential = api_key(&scene.europa.client, &scene.group, "sk-dupes").await?;
    endpoint_with(
        &scene.europa.client,
        &scene.group,
        anthropic_on(&scene.mock),
        Some(credential),
        |req| req,
    )
    .await?;
    refresh_pass(&mut harness).await?;
    scene.mock.clear_requests();
    // three rotations of its credential queue three refreshes
    for secret in ["sk-dupes-1", "sk-dupes-2", "sk-dupes-3"] {
        update(
            &scene.europa.client,
            credential,
            EntityMetadataUpdate::model_credential().credential_secret(secret),
        )
        .await?;
    }
    // one listing finishes all three
    let stats = refresh_pass(&mut harness).await?;
    assert_eq!(stats.events_processed, 3);
    assert_eq!(scene.mock.requests().len(), 1);
    let left = events::queue_state(EventType::ModelRefresh).await?;
    assert!(left.queued.is_empty() && left.in_flight.is_empty());
    Ok(())
}

/// The AI worker pops refreshes before workflow graph events, and the other workers keep their
/// one kind
#[test]
fn refreshes_pop_first() {
    // the AI worker's kinds, in pop order
    assert_eq!(
        AiWorker::event_kinds(),
        vec![EventType::ModelRefresh, EventType::WorkflowGraph]
    );
    // the other workers are unchanged
    assert_eq!(
        ReactionTriggerWorker::event_kinds(),
        vec![EventType::ReactionTrigger]
    );
    assert_eq!(
        SigmaRuleWorker::event_kinds(),
        vec![EventType::SigmaScannableResults]
    );
}

/// A refresh adds every listed model with its limits, leaves manual limits and disabled ids
/// alone, bans a model that disappears without deleting it, clears the ban when it returns, and
/// clears its events under their own kind; members then configure profiles and their own keys
#[tokio::test]
// one scenario builds the shared fixtures every step of it checks
#[allow(clippy::too_many_lines)]
async fn refresh_fills_catalog() -> Result<(), Error> {
    // europa's endpoint has a manual model, a disabled one, and a group key
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = model_scene(&admin).await?;
    scene.mock.reply(
        "GET",
        "/v1/models",
        MockReply::json(anthropic_listing(&[
            ("claude-a", 200_000, 64_000),
            ("claude-b", 100_000, 8_192),
        ])),
    );
    let secret = format!("sk-ant-{}", scene.suffix);
    let credential = api_key(&scene.europa.client, &scene.group, &secret).await?;
    let mut manual = ModelCatalogEntryRequest::new("claude-manual");
    manual.context_window = Some(1_000);
    manual.max_output_tokens = Some(100);
    let endpoint = endpoint_with(
        &scene.europa.client,
        &scene.group,
        anthropic_on(&scene.mock),
        Some(credential),
        |req| req.model(manual).disable("claude-b"),
    )
    .await?;
    // the refresh its creation queued adds both listed models
    let stats = refresh_pass(&mut harness).await?;
    assert_eq!(stats.events_processed, 1);
    assert_eq!(stats.models_added, 2);
    let meta = endpoint_of(&admin, endpoint).await?;
    let ids = meta
        .catalog
        .iter()
        .map(|entry| entry.model_id.as_str())
        .collect::<Vec<&str>>();
    assert_eq!(ids, vec!["claude-a", "claude-b", "claude-manual"]);
    let added = entry_of(&meta, "claude-a");
    assert_eq!(added.limits.context_window, Some(200_000));
    assert_eq!(added.limits.max_output_tokens, Some(64_000));
    assert!(matches!(
        added.limits.source,
        LimitsSource::Refreshed { .. }
    ));
    assert!(matches!(added.source, CatalogEntrySource::Refreshed { .. }));
    let kept = entry_of(&meta, "claude-manual");
    assert_eq!(kept.limits.context_window, Some(1_000));
    assert_eq!(kept.limits.source, LimitsSource::Manual);
    assert_eq!(
        meta.disabled_models,
        BTreeSet::from(["claude-b".to_owned()])
    );
    assert_eq!(meta.refresh.state, ModelRefreshState::Succeeded);
    // the listing went out with the group key and the endpoint's version
    let listing = scene.mock.requests_to("/v1/models");
    assert_eq!(listing.len(), 1);
    assert_eq!(listing[0].headers["x-api-key"], secret);
    assert_eq!(listing[0].headers["anthropic-version"], "2023-06-01");
    // the event was cleared under its own kind
    let left = events::queue_state(EventType::ModelRefresh).await?;
    assert!(left.queued.is_empty() && left.in_flight.is_empty());
    // a model that disappears is banned and stays in the catalog
    scene.mock.reply(
        "GET",
        "/v1/models",
        MockReply::json(anthropic_listing(&[("claude-a", 200_000, 64_000)])),
    );
    scene
        .europa
        .client
        .entities
        .refresh_model_endpoint(endpoint)
        .await?;
    let stats = refresh_pass(&mut harness).await?;
    assert_eq!(stats.bans_added, 1);
    let meta = endpoint_of(&admin, endpoint).await?;
    assert_eq!(meta.catalog.len(), 3);
    assert_eq!(
        ban_names(&entry_of(&meta, "claude-b").bans),
        vec!["ModelRemoved"]
    );
    assert!(entry_of(&meta, "claude-manual").bans.is_empty());
    // its return clears the ban
    scene.mock.reply(
        "GET",
        "/v1/models",
        MockReply::json(anthropic_listing(&[
            ("claude-a", 200_000, 64_000),
            ("claude-b", 100_000, 8_192),
        ])),
    );
    scene
        .europa
        .client
        .entities
        .refresh_model_endpoint(endpoint)
        .await?;
    let stats = refresh_pass(&mut harness).await?;
    assert_eq!(stats.bans_cleared, 1);
    let meta = endpoint_of(&admin, endpoint).await?;
    assert!(entry_of(&meta, "claude-b").bans.is_empty());
    assert_eq!(
        meta.disabled_models,
        BTreeSet::from(["claude-b".to_owned()])
    );
    // a member adds a profile on a refreshed model and their own key for the endpoint
    generators::model_profile(
        &[scene.group.as_str()],
        ModelProfileEntityRequest::new(endpoint, "claude-a", ModelApi::AnthropicMessages),
        &scene.io.client,
    )
    .await?;
    set_models(
        &scene.io.client,
        UserModelSettingsUpdate::default().credential(
            endpoint,
            UserCredentialRef::new(CredentialKind::ApiKey, Some(ModelSecret::from("sk-io"))),
        ),
    )
    .await?;
    // the member can't refresh it
    assert_eq!(
        scene
            .io
            .client
            .entities
            .refresh_model_endpoint(endpoint)
            .await
            .unwrap_err()
            .status()
            .map(|status| status.as_u16()),
        Some(401)
    );
    // an endpoint the deployment doesn't allow is refused
    let unlisted = ModelProvider::Anthropic {
        base_url: Some(format!("http://{}.unlisted.local", scene.suffix)),
        version: "2023-06-01".to_owned(),
        betas: BTreeSet::new(),
        workspace_id: None,
    };
    let refused = scene
        .europa
        .client
        .entities
        .create(generators::gen_model_entity(
            &[scene.group.as_str()],
            EntityMetadataRequest::ModelEndpoint(ModelEndpointEntityRequest::new(unlisted)),
        ))
        .await;
    assert_eq!(
        refused.unwrap_err().status().map(|status| status.as_u16()),
        Some(400)
    );
    Ok(())
}

/// Listing failures become bans on the credential or endpoint, a later success clears each,
/// an automatic ban cleared by hand comes back while its cause remains, rotating a rejected key
/// refreshes it, and bans a refresh never registers are never cleared by one
#[tokio::test]
// one scenario builds the shared fixtures every step of it checks
#[allow(clippy::too_many_lines)]
async fn refresh_failures_become_bans() -> Result<(), Error> {
    // europa's endpoint uses a key the provider rejects
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = model_scene(&admin).await?;
    let rejected = r#"{"error": {"message": "invalid x-api-key"}}"#;
    scene
        .mock
        .reply("GET", "/v1/models", MockReply::status(401, rejected));
    let credential = api_key(&scene.europa.client, &scene.group, "sk-bad").await?;
    let endpoint = endpoint_with(
        &scene.europa.client,
        &scene.group,
        anthropic_on(&scene.mock),
        Some(credential),
        |req| req,
    )
    .await?;
    let refresh = || async {
        scene
            .europa
            .client
            .entities
            .refresh_model_endpoint(endpoint)
            .await
            .map(|_| ())
    };
    // the rejection bans the credential with the provider's status and message
    let stats = refresh_pass(&mut harness).await?;
    assert_eq!(stats.listing_failures, 1);
    assert_eq!(stats.bans_added, 1);
    let cred = credential_of(&admin, credential).await?;
    let ban = cred
        .bans
        .values()
        .next()
        .cloned()
        .expect("a credential ban");
    assert_eq!(
        ban.ban_kind,
        ModelBanKind::CredentialRejected {
            status: 401,
            message: "invalid x-api-key".to_owned()
        }
    );
    // an owner clearing it while the key is still rejected only lasts until the next refresh
    update(
        &scene.europa.client,
        credential,
        EntityMetadataUpdate::model_credential()
            .model_bans_update(ModelBanUpdate::default().remove_ban(ban.id)),
    )
    .await?;
    assert!(credential_of(&admin, credential).await?.bans.is_empty());
    refresh().await?;
    refresh_pass(&mut harness).await?;
    assert_eq!(
        ban_names(&credential_of(&admin, credential).await?.bans),
        vec!["CredentialRejected"]
    );
    // rotating the key clears the ban and queues a refresh that succeeds
    scene.mock.reply(
        "GET",
        "/v1/models",
        MockReply::json(anthropic_listing(&[("claude-a", 200_000, 64_000)])),
    );
    update(
        &scene.europa.client,
        credential,
        EntityMetadataUpdate::model_credential().credential_secret("sk-good"),
    )
    .await?;
    assert!(credential_of(&admin, credential).await?.bans.is_empty());
    let stats = refresh_pass(&mut harness).await?;
    assert_eq!(stats.events_processed, 1);
    assert_eq!(
        endpoint_of(&admin, endpoint).await?.refresh.state,
        ModelRefreshState::Succeeded
    );
    // an expired key is its own ban, cleared by the next success
    scene.mock.reply(
        "GET",
        "/v1/models",
        MockReply::status(403, r#"{"error": {"message": "This API key has expired"}}"#),
    );
    refresh().await?;
    refresh_pass(&mut harness).await?;
    assert_eq!(
        ban_names(&credential_of(&admin, credential).await?.bans),
        vec!["CredentialExpired"]
    );
    // bans a refresh never registers stay through every refresh
    update(
        &admin,
        endpoint,
        EntityMetadataUpdate::model_endpoint()
            .model_bans_update(ModelBanUpdate::default().add_ban(ModelBan::generic("paused"))),
    )
    .await?;
    // a server error bans the endpoint and leaves the catalog without removal bans
    scene
        .mock
        .reply("GET", "/v1/models", MockReply::status(503, "unavailable"));
    refresh().await?;
    refresh_pass(&mut harness).await?;
    let meta = endpoint_of(&admin, endpoint).await?;
    assert_eq!(
        ban_names(&meta.bans),
        vec!["EndpointUnreachable", "Generic"]
    );
    assert_eq!(meta.catalog.len(), 1);
    assert!(entry_of(&meta, "claude-a").bans.is_empty());
    // an owner clearing it while the endpoint is still down only lasts until the next refresh
    let unreachable = meta
        .bans
        .values()
        .find(|ban| ban.ban_kind.name() == "EndpointUnreachable")
        .map(|ban| ban.id)
        .unwrap();
    update(
        &scene.europa.client,
        endpoint,
        EntityMetadataUpdate::model_endpoint()
            .model_bans_update(ModelBanUpdate::default().remove_ban(unreachable)),
    )
    .await?;
    // a body that isn't a listing is unreachable too, never an empty listing
    scene
        .mock
        .reply("GET", "/v1/models", MockReply::json(r#"{"models": 3}"#));
    refresh().await?;
    refresh_pass(&mut harness).await?;
    let meta = endpoint_of(&admin, endpoint).await?;
    assert!(ban_names(&meta.bans).contains(&"EndpointUnreachable"));
    assert!(entry_of(&meta, "claude-a").bans.is_empty());
    assert!(
        meta.refresh
            .error
            .as_deref()
            .is_some_and(|error| error.contains("data"))
    );
    // a success clears the refresh failures and keeps the other bans
    scene.mock.reply(
        "GET",
        "/v1/models",
        MockReply::json(anthropic_listing(&[("claude-a", 200_000, 64_000)])),
    );
    refresh().await?;
    refresh_pass(&mut harness).await?;
    let meta = endpoint_of(&admin, endpoint).await?;
    assert_eq!(ban_names(&meta.bans), vec!["Generic"]);
    assert!(credential_of(&admin, credential).await?.bans.is_empty());
    // a refused connection is unreachable
    let closed = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = closed.local_addr()?.port();
    drop(closed);
    let gone = ModelProvider::Anthropic {
        base_url: Some(format!("http://127.0.0.1:{port}")),
        version: "2023-06-01".to_owned(),
        betas: BTreeSet::new(),
        workspace_id: None,
    };
    generators::allow_model_endpoint(&gone).await?;
    update(
        &scene.europa.client,
        endpoint,
        EntityMetadataUpdate::model_endpoint().endpoint_provider(gone),
    )
    .await?;
    refresh_pass(&mut harness).await?;
    let meta = endpoint_of(&admin, endpoint).await?;
    assert!(ban_names(&meta.bans).contains(&"EndpointUnreachable"));
    assert_eq!(meta.refresh.state, ModelRefreshState::Failed);
    Ok(())
}

/// A credential whose expiry has passed is banned without calling the provider, and rotating it
/// clears the ban, with the queued refresh succeeding or banning it again by its new expiry
#[tokio::test]
async fn expired_credentials_skip_the_provider() -> Result<(), Error> {
    // europa's credential expired yesterday
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = model_scene(&admin).await?;
    scene.mock.reply(
        "GET",
        "/v1/models",
        MockReply::json(anthropic_listing(&[("claude-a", 200_000, 64_000)])),
    );
    let mut req = ModelCredentialEntityRequest::new(
        CredentialKind::ApiKey,
        Some(ModelSecret::from("sk-old")),
    );
    req.expires = Some(Utc::now() - chrono::Duration::days(1));
    let credential = scene
        .europa
        .client
        .entities
        .create(generators::gen_model_entity(
            &[scene.group.as_str()],
            EntityMetadataRequest::ModelCredential(req),
        ))
        .await?
        .id;
    endpoint_with(
        &scene.europa.client,
        &scene.group,
        anthropic_on(&scene.mock),
        Some(credential),
        |req| req,
    )
    .await?;
    // the refresh bans it without a listing request
    refresh_pass(&mut harness).await?;
    assert_eq!(
        ban_names(&credential_of(&admin, credential).await?.bans),
        vec!["CredentialExpired"]
    );
    assert!(scene.mock.requests().is_empty());
    // rotating it with a later expiry clears the ban and the refresh succeeds
    update(
        &scene.europa.client,
        credential,
        EntityMetadataUpdate::model_credential()
            .credential_secret("sk-new")
            .credential_expires(Utc::now() + chrono::Duration::days(30)),
    )
    .await?;
    assert!(credential_of(&admin, credential).await?.bans.is_empty());
    refresh_pass(&mut harness).await?;
    assert!(credential_of(&admin, credential).await?.bans.is_empty());
    assert_eq!(scene.mock.requests().len(), 1);
    // rotating it with an expiry still past clears the ban, which the refresh adds back
    update(
        &scene.europa.client,
        credential,
        EntityMetadataUpdate::model_credential()
            .credential_secret("sk-stale")
            .credential_expires(Utc::now() - chrono::Duration::hours(1)),
    )
    .await?;
    assert!(credential_of(&admin, credential).await?.bans.is_empty());
    refresh_pass(&mut harness).await?;
    assert_eq!(
        ban_names(&credential_of(&admin, credential).await?.bans),
        vec!["CredentialExpired"]
    );
    assert_eq!(scene.mock.requests().len(), 1);
    Ok(())
}

/// An endpoint without a group credential is listed with the triggering user's own key; with
/// neither nothing is recorded; refreshes read the endpoint as their user; and a failing user
/// refresh notifies that user until a later refresh clears it
#[tokio::test]
// one scenario builds the shared fixtures every step of it checks
#[allow(clippy::too_many_lines)]
async fn user_credentials_refresh() -> Result<(), Error> {
    // an endpoint with no group credential, and a user outside its group
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = model_scene(&admin).await?;
    let outsider = moon_user(&admin, "himalia", &scene.suffix).await?;
    god_group(
        &admin,
        "vesta",
        &scene.suffix,
        &GroupRoles::default().user(&outsider.username),
    )
    .await?;
    scene.mock.reply(
        "GET",
        "/v1/models",
        MockReply::json(anthropic_listing(&[("claude-a", 200_000, 64_000)])),
    );
    let endpoint = endpoint_with(
        &scene.europa.client,
        &scene.group,
        anthropic_on(&scene.mock),
        None,
        |req| req,
    )
    .await?;
    // with no group key and no key of europa's, nothing is listed or recorded
    let stats = refresh_pass(&mut harness).await?;
    assert_eq!(stats.events_processed, 1);
    assert!(scene.mock.requests().is_empty());
    let meta = endpoint_of(&admin, endpoint).await?;
    assert_eq!(meta.refresh.state, ModelRefreshState::Never);
    assert_eq!(meta.catalog.len(), 0);
    // europa's own key refreshes her credential without touching the group catalog
    set_models(
        &scene.europa.client,
        UserModelSettingsUpdate::default().credential(
            endpoint,
            UserCredentialRef::new(CredentialKind::ApiKey, Some(ModelSecret::from("sk-europa"))),
        ),
    )
    .await?;
    refresh_pass(&mut harness).await?;
    assert_eq!(scene.mock.requests().len(), 1);
    assert_eq!(endpoint_of(&admin, endpoint).await?.catalog.len(), 0);
    // her refresh of the endpoint lists with her key and fills its catalog
    scene
        .europa
        .client
        .entities
        .refresh_model_endpoint(endpoint)
        .await?;
    refresh_pass(&mut harness).await?;
    let listing = scene.mock.requests_to("/v1/models");
    assert_eq!(listing.len(), 2);
    assert_eq!(listing[1].headers["x-api-key"], "sk-europa");
    let meta = endpoint_of(&admin, endpoint).await?;
    assert_eq!(meta.refresh.state, ModelRefreshState::Succeeded);
    assert!(meta.catalog_entry("claude-a").is_some());
    // a member with no key and a user who can't see the endpoint refresh nothing
    events::push(&[
        refresh_event(&scene.io.username, group_target(&scene.group, endpoint)),
        refresh_event(&outsider.username, group_target(&scene.group, endpoint)),
    ])
    .await?;
    let stats = refresh_pass(&mut harness).await?;
    assert_eq!(stats.events_processed, 2);
    assert_eq!(scene.mock.requests().len(), 2);
    // a rejected own key bans her credential and notifies her
    scene.mock.reply(
        "GET",
        "/v1/models",
        MockReply::status(401, r#"{"error": {"message": "invalid x-api-key"}}"#),
    );
    let notes_before = notes(&scene.europa).await?.len();
    let user_target = ModelRefreshTarget::UserEndpoint {
        user: scene.europa.username.clone(),
        endpoint,
    };
    events::push(&[refresh_event(&scene.europa.username, user_target.clone())]).await?;
    refresh_pass(&mut harness).await?;
    let settings = own_models(&scene.europa.client).await?;
    assert_eq!(
        ban_names(&settings.credentials[&endpoint].bans),
        vec!["CredentialRejected"]
    );
    let after = notes(&scene.europa).await?;
    assert_eq!(after.len(), notes_before + 1);
    // a later successful refresh clears the ban and deletes the notification
    scene.mock.reply(
        "GET",
        "/v1/models",
        MockReply::json(anthropic_listing(&[("claude-a", 200_000, 64_000)])),
    );
    events::push(&[refresh_event(&scene.europa.username, user_target)]).await?;
    refresh_pass(&mut harness).await?;
    let settings = own_models(&scene.europa.client).await?;
    assert!(settings.credentials[&endpoint].bans.is_empty());
    assert_eq!(notes(&scene.europa).await?.len(), notes_before);
    // only admins send an endpoint's report
    let report = ModelRefreshReport::listed(vec![ListedModel::new("claude-z")]);
    let refused = update(
        &scene.europa.client,
        endpoint,
        EntityMetadataUpdate::model_endpoint().refresh_report(report),
    )
    .await;
    assert_eq!(
        refused.unwrap_err().status().map(|status| status.as_u16()),
        Some(401)
    );
    Ok(())
}

/// Refreshes of deleted endpoints, of users with no credentials, and of disabled users are
/// cleared with nothing recorded; a user target sends one report per endpoint; per-user clients
/// are built once on first use, dropped when a user is disabled, and rebuilt on retry; and a
/// report the API fails stays in flight and is sent once on the next pass after a reset
#[tokio::test]
// one scenario builds the shared fixtures every step of it checks
#[allow(clippy::too_many_lines)]
async fn refresh_skips_and_retries() -> Result<(), Error> {
    // europa's endpoint, made before any of the refreshes below
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = model_scene(&admin).await?;
    scene.mock.reply(
        "GET",
        "/v1/models",
        MockReply::json(anthropic_listing(&[("claude-a", 200_000, 64_000)])),
    );
    let credential = api_key(&scene.europa.client, &scene.group, "sk-group").await?;
    let endpoint = endpoint_with(
        &scene.europa.client,
        &scene.group,
        anthropic_on(&scene.mock),
        Some(credential),
        |req| req,
    )
    .await?;
    // europa's first refresh builds her client once, and her second reuses it
    let stats = refresh_pass(&mut harness).await?;
    assert_eq!(stats.user_lookups, 1);
    assert!(harness.worker.has_user_client(&scene.europa.username));
    scene
        .europa
        .client
        .entities
        .refresh_model_endpoint(endpoint)
        .await?;
    let stats = refresh_pass(&mut harness).await?;
    assert_eq!(stats.user_lookups, 0);
    assert_eq!(scene.mock.requests().len(), 2);
    // a report the API fails stays in flight with a retry time
    test_utilities::faults::fail_before_entity_update(endpoint);
    scene
        .europa
        .client
        .entities
        .refresh_model_endpoint(endpoint)
        .await?;
    let stats = refresh_pass(&mut harness).await?;
    assert_eq!(stats.events_processed, 0);
    let left = events::queue_state(EventType::ModelRefresh).await?;
    assert_eq!(left.in_flight.len(), 1);
    assert!(harness.worker.take_retry_ts().await.is_some());
    // after the reset the next pass rebuilds europa's client and sends the report once
    admin.events.reset_all(EventType::ModelRefresh).await?;
    let stats = refresh_pass(&mut harness).await?;
    assert_eq!(stats.events_processed, 1);
    assert_eq!(stats.user_lookups, 1);
    let left = events::queue_state(EventType::ModelRefresh).await?;
    assert!(left.queued.is_empty() && left.in_flight.is_empty());
    assert_eq!(
        endpoint_of(&admin, endpoint).await?.refresh.state,
        ModelRefreshState::Succeeded
    );
    // a deleted endpoint's refresh is cleared with no request
    let gone = endpoint_with(
        &scene.europa.client,
        &scene.group,
        anthropic_on(&scene.mock),
        Some(credential),
        |req| req,
    )
    .await?;
    events::discard(EventType::ModelRefresh).await?;
    admin.entities.delete(gone).await?;
    events::push(&[refresh_event(
        &scene.europa.username,
        group_target(&scene.group, gone),
    )])
    .await?;
    let before = scene.mock.requests().len();
    let stats = refresh_pass(&mut harness).await?;
    assert_eq!(stats.events_processed, 1);
    assert_eq!(scene.mock.requests().len(), before);
    // a user target with keys for two of their own endpoints sends two reports
    let mut own = Vec::new();
    for name in ["first", "second"] {
        let provider = ModelProvider::OpenAiCompatible {
            base_url: format!("{}/{name}", scene.mock.url()),
            extra_headers: BTreeMap::new(),
            key_header: None,
        };
        generators::allow_model_endpoint(&provider).await?;
        scene.mock.reply(
            "GET",
            &format!("/{name}/models"),
            MockReply::json(format!(r#"{{"data": [{{"id": "{name}-model"}}]}}"#)),
        );
        let id = Uuid::new_v4();
        set_models(
            &scene.europa.client,
            UserModelSettingsUpdate::default()
                .endpoint(id, UserModelEndpoint::new(name, provider))
                .credential(
                    id,
                    UserCredentialRef::new(
                        CredentialKind::ApiKey,
                        Some(ModelSecret::from("sk-own")),
                    ),
                ),
        )
        .await?;
        own.push(id);
    }
    events::discard(EventType::ModelRefresh).await?;
    events::push(&[refresh_event(
        &scene.europa.username,
        ModelRefreshTarget::User {
            user: scene.europa.username.clone(),
        },
    )])
    .await?;
    refresh_pass(&mut harness).await?;
    let settings = own_models(&scene.europa.client).await?;
    for (id, name) in own.iter().zip(["first", "second"]) {
        let endpoint = &settings.endpoints[id];
        assert_eq!(endpoint.refresh.state, ModelRefreshState::Succeeded);
        assert_eq!(endpoint.catalog[0].model_id, format!("{name}-model"));
    }
    // a user with no credentials refreshes nothing
    let before = scene.mock.requests().len();
    events::push(&[refresh_event(
        &scene.io.username,
        ModelRefreshTarget::User {
            user: scene.io.username.clone(),
        },
    )])
    .await?;
    let stats = refresh_pass(&mut harness).await?;
    assert_eq!(stats.events_processed, 1);
    assert_eq!(scene.mock.requests().len(), before);
    // a user created after the worker started gets a client on first use
    let callisto = moon_user(&admin, "callisto", &scene.suffix).await?;
    let roles = GroupRoles::default().manager(&callisto.username);
    let mars = god_group(&admin, "mars", &scene.suffix, &roles).await?;
    let theirs = endpoint_with(
        &callisto.client,
        &mars,
        anthropic_on(&scene.mock),
        Some(api_key(&callisto.client, &mars, "sk-callisto").await?),
        |req| req,
    )
    .await?;
    let stats = refresh_pass(&mut harness).await?;
    assert_eq!(stats.user_lookups, 1);
    // a user update disabling them drops their client and their next refresh is skipped
    let mut disabled = admin.users.get(&callisto.username).await?;
    disabled.role = UserRole::Disabled;
    harness
        .worker
        .apply_cache_update(WorkerCacheUpdates::Users(HashMap::from([(
            callisto.username.clone(),
            disabled,
        )])));
    assert!(!harness.worker.has_user_client(&callisto.username));
    let before = scene.mock.requests().len();
    events::push(&[refresh_event(
        &callisto.username,
        group_target(&mars, theirs),
    )])
    .await?;
    let stats = refresh_pass(&mut harness).await?;
    assert_eq!(stats.events_processed, 1);
    assert_eq!(scene.mock.requests().len(), before);
    // a user disabled in Thorium is skipped the first time the worker sees them
    let elara = moon_user(&admin, "elara", &scene.suffix).await?;
    let disable = UserUpdate {
        role: Some(UserRole::Disabled),
        ..UserUpdate::default()
    };
    admin.users.update(&elara.username, disable).await?;
    events::push(&[refresh_event(
        &elara.username,
        group_target(&scene.group, endpoint),
    )])
    .await?;
    let stats = refresh_pass(&mut harness).await?;
    assert_eq!(stats.events_processed, 1);
    assert_eq!(stats.user_lookups, 1);
    assert_eq!(scene.mock.requests().len(), before);
    Ok(())
}

/// Bedrock, Vertex, and Azure endpoints sign in against mock endpoints, and a refused token
/// exchange rejects the credential
#[tokio::test]
// one scenario builds the shared fixtures every step of it checks
#[allow(clippy::too_many_lines)]
async fn refresh_signs_in() -> Result<(), Error> {
    // mock Bedrock, Vertex, Azure, and sign-in endpoints
    let harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = model_scene(&admin).await?;
    let mock = &scene.mock;
    let mut worker = new_worker(&admin).auth_hosts(AuthHosts {
        sts: None,
        entra: mock.url(),
        iam_credentials: mock.url(),
        gcp_token: format!("{}/token", mock.url()),
    });
    mock.reply(
        "GET",
        "/foundation-models",
        MockReply::json(r#"{"modelSummaries": [{"modelId": "anthropic.claude"}]}"#),
    );
    mock.reply(
        "GET",
        "/inference-profiles",
        MockReply::json(r#"{"inferenceProfileSummaries": []}"#),
    );
    mock.reply(
        "POST",
        "/token",
        MockReply::json(r#"{"access_token": "gcp-token"}"#),
    );
    for publisher in ["google", "anthropic"] {
        mock.reply(
            "GET",
            &format!("/v1beta1/publishers/{publisher}/models"),
            MockReply::json(format!(
                r#"{{"publisherModels": [{{"name": "publishers/{publisher}/models/{publisher}-model"}}]}}"#
            )),
        );
    }
    mock.reply(
        "POST",
        "/tenant-1/oauth2/v2.0/token",
        MockReply::json(r#"{"access_token": "entra-token"}"#),
    );
    mock.reply(
        "GET",
        "/openai/v1/models",
        MockReply::json(r#"{"data": [{"id": "gpt-x"}]}"#),
    );
    // a Bedrock access key with a session token
    let mut req = ModelCredentialEntityRequest::new(
        CredentialKind::AwsAccessKey {
            access_key_id: "AKIATEST".to_owned(),
        },
        Some(ModelSecret::from("aws-secret")),
    );
    req.session_token = Some(ModelSecret::from("aws-session"));
    let aws = scene
        .europa
        .client
        .entities
        .create(generators::gen_model_entity(
            &[scene.group.as_str()],
            EntityMetadataRequest::ModelCredential(req),
        ))
        .await?
        .id;
    let bedrock = endpoint_with(
        &scene.europa.client,
        &scene.group,
        ModelProvider::Bedrock {
            region: "us-west-2".to_owned(),
            endpoint_kind: BedrockEndpointKind::Runtime,
            endpoint_url: Some(mock.url()),
            workspace_id: None,
        },
        Some(aws),
        |req| req,
    )
    .await?;
    // a Vertex service account whose tokens come from the mock
    let file = format!(
        r#"{{"type": "service_account", "client_email": "lister@thorium-test.iam.gserviceaccount.com", "private_key": {key:?}, "token_uri": "{url}/token"}}"#,
        key = TEST_KEY,
        url = mock.url()
    );
    let gcp = generators::model_credential(
        &[scene.group.as_str()],
        CredentialKind::GcpCredentialsJson {
            impersonate_service_account: None,
        },
        Some(&file),
        &scene.europa.client,
    )
    .await?
    .id;
    let vertex = endpoint_with(
        &scene.europa.client,
        &scene.group,
        ModelProvider::Vertex {
            project_id: Some("thorium-test".to_owned()),
            location: VertexLocation::Region("us-east5".to_owned()),
            endpoint_url: Some(mock.url()),
        },
        Some(gcp),
        |req| req,
    )
    .await?;
    // an Azure Entra client secret
    let entra = generators::model_credential(
        &[scene.group.as_str()],
        CredentialKind::AzureEntraClient {
            tenant_id: "tenant-1".to_owned(),
            client_id: "client-1".to_owned(),
            auth: AzureClientAuth::ClientSecret,
        },
        Some("entra-secret"),
        &scene.europa.client,
    )
    .await?
    .id;
    let azure = endpoint_with(
        &scene.europa.client,
        &scene.group,
        ModelProvider::AzureOpenAi {
            endpoint: mock.url(),
            api_mode: AzureApiMode::V1,
            entra_scope: None,
        },
        Some(entra),
        |req| req,
    )
    .await?;
    // one pass refreshes all three
    let opts = EventPopOpts::default().limit(1000);
    let popped = admin.events.pop(EventType::ModelRefresh, &opts).await?;
    let mut stats = AiWorkerStats::default();
    EventWorkerMutableSupport::process(&mut worker, popped, &mut stats).await?;
    assert_eq!(stats.events_processed, 3);
    // Bedrock's listing was signed for its region with the session token
    let signed = &mock.requests_to("/foundation-models")[0];
    assert!(signed.headers["authorization"].starts_with("AWS4-HMAC-SHA256 Credential=AKIATEST/"));
    assert!(signed.headers["authorization"].contains("/us-west-2/bedrock/aws4_request"));
    assert_eq!(signed.headers["x-amz-security-token"], "aws-session");
    assert!(
        endpoint_of(&admin, bedrock)
            .await?
            .catalog_entry("anthropic.claude")
            .is_some()
    );
    // Vertex listed with the exchanged token
    assert_eq!(
        mock.requests_to("/v1beta1/publishers/google/models")[0].headers["authorization"],
        "Bearer gcp-token"
    );
    assert!(
        endpoint_of(&admin, vertex)
            .await?
            .catalog_entry("anthropic-model")
            .is_some()
    );
    // Azure listed with the Entra token
    assert_eq!(
        mock.requests_to("/openai/v1/models")[0].headers["authorization"],
        "Bearer entra-token"
    );
    assert!(
        endpoint_of(&admin, azure)
            .await?
            .catalog_entry("gpt-x")
            .is_some()
    );
    // a refused token exchange rejects the credential
    mock.reply(
        "POST",
        "/tenant-1/oauth2/v2.0/token",
        MockReply::status(401, r#"{"error": "invalid_client"}"#),
    );
    scene
        .europa
        .client
        .entities
        .refresh_model_endpoint(azure)
        .await?;
    let popped = admin.events.pop(EventType::ModelRefresh, &opts).await?;
    EventWorkerMutableSupport::process(&mut worker, popped, &mut stats).await?;
    assert_eq!(
        ban_names(&credential_of(&admin, entra).await?.bans),
        vec!["CredentialRejected"]
    );
    Ok(())
}

/// Twelve refreshes against a provider that answers each in a second run at most five at once
/// and finish in about three seconds, and a provider that never answers is unreachable after
/// the listing timeout without holding up the batch's other refreshes or the worker
#[tokio::test]
// one scenario builds the shared fixtures every step of it checks
#[allow(clippy::too_many_lines)]
async fn refresh_bounds() -> Result<(), Error> {
    // twelve keyless gateway endpoints on a provider that takes a second per listing
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = model_scene(&admin).await?;
    scene.mock.reply(
        "GET",
        "/slow/models",
        MockReply::json(r#"{"data": [{"id": "local"}]}"#).delayed(Duration::from_secs(1)),
    );
    let slow = ModelProvider::OpenAiCompatible {
        base_url: format!("{}/slow", scene.mock.url()),
        extra_headers: BTreeMap::new(),
        key_header: None,
    };
    for _ in 0..12 {
        endpoint_with(
            &scene.europa.client,
            &scene.group,
            slow.clone(),
            None,
            |req| req,
        )
        .await?;
    }
    // they run at most five at a time
    let started = Instant::now();
    let stats = refresh_pass(&mut harness).await?;
    let took = started.elapsed();
    assert_eq!(stats.events_processed, 12);
    assert_eq!(scene.mock.requests_to("/slow/models").len(), 12);
    assert!(
        scene.mock.max_in_flight() <= 5,
        "{} at once",
        scene.mock.max_in_flight()
    );
    assert!(scene.mock.max_in_flight() >= 2);
    assert!(
        took >= Duration::from_millis(2_900) && took < Duration::from_secs(8),
        "took {took:?}"
    );
    // a provider that never answers is unreachable after the timeout, and its neighbor finishes
    scene.mock.reply("GET", "/stuck/models", MockReply::hang());
    scene.mock.reply(
        "GET",
        "/quick/models",
        MockReply::json(r#"{"data": [{"id": "quick"}]}"#),
    );
    let mut ids = Vec::new();
    for path in ["stuck", "quick"] {
        let provider = ModelProvider::OpenAiCompatible {
            base_url: format!("{}/{path}", scene.mock.url()),
            extra_headers: BTreeMap::new(),
            key_header: None,
        };
        ids.push(
            endpoint_with(&scene.europa.client, &scene.group, provider, None, |req| {
                req
            })
            .await?,
        );
    }
    let started = Instant::now();
    let stats = refresh_pass(&mut harness).await?;
    let took = started.elapsed();
    assert_eq!(stats.events_processed, 2);
    assert!(
        took >= Duration::from_secs(29) && took < Duration::from_secs(40),
        "took {took:?}"
    );
    let stuck = endpoint_of(&admin, ids[0]).await?;
    assert_eq!(ban_names(&stuck.bans), vec!["EndpointUnreachable"]);
    assert!(
        stuck
            .refresh
            .error
            .as_deref()
            .is_some_and(|error| error.contains("timed out"))
    );
    assert_eq!(
        endpoint_of(&admin, ids[1]).await?.refresh.state,
        ModelRefreshState::Succeeded
    );
    // the worker still builds instances right after
    let groups = [scene.group.as_str()];
    let lead = agent(&admin, "lead", &groups, |req| req).await?;
    let flow = workflow(&scene.europa.client, "after-stall", &groups, lead.id).await?;
    harness.run().await?;
    assert_eq!(instances(&admin, flow.id).await?.len(), 1);
    Ok(())
}

/// Editing a toolset's read only flag or selector, or including another tool, builds its next
/// instance recorded as caused by the editor; an edit that leaves the result unchanged builds
/// nothing
#[tokio::test]
async fn toolset_edits_build_instances() -> Result<(), Error> {
    // europa builds a toolset that includes one tool
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = crew(&admin).await?;
    let groups = [scene.group.as_str()];
    let (first, first_r1) = generators::tool(
        &groups,
        "1.0.0",
        &spec_with_effects(&[ToolEffect::WritesThorium]),
        &admin,
    )
    .await?;
    let (second, second_r1) =
        generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    let req = ToolSetEntityRequest::default().tool(first.id);
    let kit = toolset(&scene.europa.client, "kit", &groups, req).await?;
    let stats = harness.run().await?;
    let built = set_instances(&admin, kit.id).await?;
    assert_eq!(built.len(), 1);
    assert_eq!(member_ids(&built[0]), vec![first_r1.id]);
    assert_eq!(set_meta(&built[0]).triggered_by, scene.europa.username);
    assert_eq!(
        set_meta(&built[0]).trigger,
        ToolSetInstanceTrigger::ToolSetUpdated
    );
    assert_eq!(built[0].submitter, admin.users.info().await?.username);
    assert_eq!(stats.tool_set_instances_created, 1);
    // callisto turns on read only, which leaves out the tool that writes
    update(
        &scene.callisto.client,
        kit.id,
        set_update(|update| {
            if let EntityMetadataUpdate::ToolSet { read_only, .. } = update {
                *read_only = Some(true);
            }
        }),
    )
    .await?;
    harness.run().await?;
    let built = set_instances(&admin, kit.id).await?;
    assert_eq!(built.len(), 2);
    let r2 = set_meta(&built[1]);
    assert_eq!(r2.revision, 2);
    assert_eq!(r2.previous, Some(built[0].id));
    assert_eq!(r2.members, Vec::new());
    assert_eq!(r2.triggered_by, scene.callisto.username);
    // europa includes another tool, which joins at r3
    update(
        &scene.europa.client,
        kit.id,
        set_update(|update| {
            if let EntityMetadataUpdate::ToolSet { add_tools, .. } = update {
                add_tools.push(second.id);
            }
        }),
    )
    .await?;
    harness.run().await?;
    let built = set_instances(&admin, kit.id).await?;
    assert_eq!(built.len(), 3);
    assert_eq!(member_ids(&built[2]), vec![second_r1.id]);
    assert_eq!(set_meta(&built[2]).triggered_by, scene.europa.username);
    // turning automatic instances off and on again leaves the result unchanged and builds nothing
    for auto in [false, true] {
        update(
            &scene.europa.client,
            kit.id,
            set_update(|update| {
                if let EntityMetadataUpdate::ToolSet { auto_instance, .. } = update {
                    *auto_instance = Some(auto);
                }
            }),
        )
        .await?;
    }
    let stats = harness.run().await?;
    assert_eq!(set_instances(&admin, kit.id).await?.len(), 3);
    assert_eq!(stats.tool_set_instances_created, 0);
    assert_eq!(stats.tool_set_skipped, 1);
    Ok(())
}

/// Get the tool instances a workflow instance gives its agents, sorted
///
/// # Arguments
///
/// * `entity` - The workflow instance
fn tool_ids(entity: &Entity) -> Vec<Uuid> {
    // each given tool's instance, leaving out the harnesses the agents run in, sorted
    let mut ids = instance_meta(entity)
        .tools
        .iter()
        .filter(|tool| tool.kind != ToolKind::Harness)
        .map(|tool| tool.tool_instance)
        .collect::<Vec<Uuid>>();
    ids.sort();
    ids
}

/// Publishing a member tool the toolset follows at latest builds the toolset's next instance and
/// queues exactly one toolset update; processing it rebuilds the workflow that follows the
/// toolset at latest, builds no toolset instance, leaves the queue empty, and tells the publisher,
/// while the workflow that pins the old toolset instance gets nothing
#[tokio::test]
// one scenario follows a publish through both phases
#[allow(clippy::too_many_lines)]
async fn member_publish_reaches_workflows_through_toolset() -> Result<(), Error> {
    // a toolset that follows a tool, a workflow following the toolset, and one pinning its r1
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let (tool, tool_r1) =
        generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    let req = ToolSetEntityRequest::default().tool(tool.id);
    let kit = toolset(&scene.europa.client, "kit", &groups, req).await?;
    harness.run().await?;
    let kit_r1 = set_instances(&admin, kit.id).await?[0].id;
    let follower = agent(&scene.europa.client, "follower", &groups, |req| {
        req.tool_set(kit.id)
    })
    .await?;
    let pinner = agent(&scene.europa.client, "pinner", &groups, |req| {
        req.tool_set(kit_r1)
    })
    .await?;
    let follows = workflow(&scene.europa.client, "follows", &groups, follower.id).await?;
    let pins = workflow(&scene.europa.client, "pins", &groups, pinner.id).await?;
    harness.run().await?;
    let follows_r1 = &instances(&admin, follows.id).await?[0];
    assert!(
        instance_meta(follows_r1).is_ready(),
        "{:?}",
        instance_meta(follows_r1).status
    );
    assert_eq!(tool_ids(follows_r1), vec![tool_r1.id]);
    assert_eq!(instances(&admin, pins.id).await?.len(), 1);
    // europa publishes a new version of the tool
    scene
        .europa
        .client
        .entities
        .update(
            tool.id,
            generators::gen_tool_publish("2.0.0", &generators::gen_agentic_spec()),
        )
        .await?;
    let latest = latest_of(&admin, tool.id).await?;
    let stats = harness.run().await?;
    // the toolset got r2 with the new version, and exactly one toolset update is queued
    let built = set_instances(&admin, kit.id).await?;
    assert_eq!(built.len(), 2);
    assert_eq!(member_ids(&built[1]), vec![latest]);
    assert_eq!(
        set_meta(&built[1]).trigger,
        ToolSetInstanceTrigger::ToolUpdated {
            tool: tool.id,
            tool_instance: latest
        }
    );
    assert_eq!(stats.tool_set_instances_created, 1);
    assert_eq!(stats.instances_created, 0);
    assert_eq!(
        waiting(&admin).await?,
        vec![WorkflowGraphChange::ToolSetUpdated {
            tool_set: kit.id,
            tool_set_instance: built[1].id
        }]
    );
    // processing it rebuilds only the follower, builds no toolset instance, and queues nothing
    let stats = harness.run().await?;
    assert_eq!(stats.tool_set_instances_created, 0);
    assert_eq!(stats.instances_created, 1);
    let rebuilt = instances(&admin, follows.id).await?;
    assert_eq!(rebuilt.len(), 2);
    let r2 = instance_meta(&rebuilt[1]);
    assert_eq!(tool_ids(&rebuilt[1]), vec![latest]);
    assert_eq!(r2.agents[0].tool_sets, vec![built[1].id]);
    assert_eq!(r2.triggered_by, scene.europa.username);
    assert_eq!(
        r2.trigger,
        WorkflowInstanceTrigger::ToolSetUpdated {
            tool_set: kit.id,
            tool_set_instance: built[1].id
        }
    );
    assert_eq!(instances(&admin, pins.id).await?.len(), 1);
    assert_eq!(waiting(&admin).await?, Vec::new());
    let left = harness.leftover().await?;
    assert!(
        left.queued.is_empty() && left.in_flight.is_empty(),
        "{left:?}"
    );
    // europa was told about the toolset build and the workflow build it led to
    let found = notes(&scene.europa).await?;
    assert!(
        found
            .iter()
            .any(|(_, msg)| msg.contains("`kit`") && msg.contains("#r2")),
        "{found:?}"
    );
    assert!(
        found
            .iter()
            .any(|(_, msg)| msg.contains("`follows`") && msg.contains("#r2")),
        "{found:?}"
    );
    Ok(())
}

/// A toolset with automatic instances off gets no instance from its edits, so a workflow that
/// follows it is Invalid; a manual request builds its first Ready instance, whose toolset update
/// rebuilds the workflow as Ready
#[tokio::test]
async fn manual_toolset_instance_fixes_workflow() -> Result<(), Error> {
    // a manual-only toolset and a workflow whose agent follows it
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let (tool, tool_r1) =
        generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    let req = ToolSetEntityRequest::default()
        .tool(tool.id)
        .auto_instance(false);
    let kit = toolset(&scene.europa.client, "manual-kit", &groups, req).await?;
    let lead = agent(&scene.europa.client, "lead", &groups, |req| {
        req.tool_set(kit.id)
    })
    .await?;
    let flow = workflow(&scene.europa.client, "waits", &groups, lead.id).await?;
    let stats = harness.run().await?;
    // the toolset has no instance and the workflow is invalid without one
    assert_eq!(set_instances(&admin, kit.id).await?.len(), 0);
    assert_eq!(stats.tool_set_instances_created, 0);
    let built = instances(&admin, flow.id).await?;
    assert_eq!(built.len(), 1);
    assert!(!instance_meta(&built[0]).is_ready());
    // a manual request builds the toolset's first instance
    scene
        .europa
        .client
        .entities
        .request_tool_set_instance(kit.id)
        .await?;
    harness.run().await?;
    let kit_built = set_instances(&admin, kit.id).await?;
    assert_eq!(kit_built.len(), 1);
    assert_eq!(
        set_meta(&kit_built[0]).trigger,
        ToolSetInstanceTrigger::Manual
    );
    // its toolset update rebuilds the workflow as Ready with the member
    harness.run().await?;
    let built = instances(&admin, flow.id).await?;
    assert_eq!(built.len(), 2);
    assert!(
        instance_meta(&built[1]).is_ready(),
        "{:?}",
        instance_meta(&built[1]).status
    );
    assert_eq!(tool_ids(&built[1]), vec![tool_r1.id]);
    Ok(())
}

/// A toolset rebuild that ends Invalid queues no toolset update, so the workflow following the
/// toolset keeps its instance on the last Ready one, and the user who caused it is told why
#[tokio::test]
async fn invalid_toolset_instance_keeps_workflows() -> Result<(), Error> {
    // a toolset following a tool, and a workflow following the toolset
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let (tool, tool_r1) =
        generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    let req = ToolSetEntityRequest::default().tool(tool.id);
    let kit = toolset(&scene.europa.client, "fragile", &groups, req).await?;
    let lead = agent(&scene.europa.client, "lead", &groups, |req| {
        req.tool_set(kit.id)
    })
    .await?;
    let flow = workflow(&scene.europa.client, "steady", &groups, lead.id).await?;
    harness.run().await?;
    harness.run().await?;
    let before = instances(&admin, flow.id).await?.len();
    // europa deprecates the tool's only instance, so the toolset's latest edge has nothing
    scene
        .europa
        .client
        .entities
        .update(tool_r1.id, generators::gen_tool_deprecate(true))
        .await?;
    let stats = harness.run().await?;
    // the toolset got an Invalid r2 and nothing was queued for workflows
    let built = set_instances(&admin, kit.id).await?;
    assert_eq!(built.len(), 2);
    assert!(!set_meta(&built[1]).is_ready());
    assert_eq!(stats.tool_set_invalid, 1);
    assert_eq!(waiting(&admin).await?, Vec::new());
    // the workflow keeps its instance, and the toolset still resolves to the Ready r1
    harness.run().await?;
    assert_eq!(instances(&admin, flow.id).await?.len(), before);
    let root = admin.entities.get(kit.id).await?;
    match &root.metadata {
        EntityMetadata::ToolSet(meta) => assert_eq!(meta.latest_instance, Some(built[0].id)),
        other => panic!("expected a toolset, got {other:?}"),
    }
    // europa was told the build is invalid and why
    let found = notes(&scene.europa).await?;
    assert!(
        found
            .iter()
            .any(|(level, msg)| *level == NotificationLevel::Error
                && msg.contains("`fragile`")
                && msg.contains("#r2")
                && msg.contains("no latest instance")),
        "{found:?}"
    );
    Ok(())
}

/// A new tool that already carries a toolset's selector tag joins the toolset's next instance
/// when it's published
#[tokio::test]
async fn new_tagged_tool_joins_selector() -> Result<(), Error> {
    // a toolset selecting toolset=ghidra with no matching tools yet
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let req = ToolSetEntityRequest::default().selector(ToolSetSelector::new("toolset", "ghidra"));
    let kit = toolset(&scene.europa.client, "ghidra", &groups, req).await?;
    harness.run().await?;
    let built = set_instances(&admin, kit.id).await?;
    assert_eq!(built.len(), 1);
    assert_eq!(set_meta(&built[0]).members, Vec::new());
    // europa publishes a new tool that carries the tag
    let req = generators::gen_tool(&groups, "1.0.0", &generators::gen_agentic_spec())
        .tag("toolset", "ghidra");
    let created = scene.europa.client.entities.create(req).await?;
    let instance = created.tool_instance.expect("no tool instance");
    harness.run().await?;
    // the tool joined the toolset as a selected member
    let built = set_instances(&admin, kit.id).await?;
    assert_eq!(built.len(), 2);
    assert_eq!(member_ids(&built[1]), vec![instance]);
    assert_eq!(
        set_meta(&built[1]).members[0].inclusion,
        ToolSetInclusion::Selector
    );
    Ok(())
}

/// Adding a user tag to a tool queues one tag change with the pair and the tool's groups, and the
/// toolset selecting that pair gets the tool as a member; removing it rebuilds the toolset
/// without the tool; tagging an agent or a tool instance queues nothing
#[tokio::test]
async fn tool_tag_changes_rebuild_selector_toolsets() -> Result<(), Error> {
    // a toolset selecting family=parser and an untagged tool
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let (tool, tool_r1) =
        generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    let req = ToolSetEntityRequest::default().selector(ToolSetSelector::new("family", "parser"));
    let kit = toolset(&scene.europa.client, "parsers", &groups, req).await?;
    let lead = agent(&scene.europa.client, "lead", &groups, |req| req).await?;
    harness.run().await?;
    harness.run().await?;
    let first = set_instances(&admin, kit.id).await?;
    assert_eq!(set_meta(&first[0]).members, Vec::new());
    // europa tags the tool, which queues one tag change in the tool's groups
    let status = tag_entity(&scene.europa, tool.id, false, &groups, "family", "parser").await?;
    assert_eq!(status, 204);
    let queued = events::queued(&admin, EventType::WorkflowGraph).await?;
    assert_eq!(queued.len(), 1, "{queued:?}");
    let graph = events::graph_event(&queued[0]).expect("not a graph event");
    assert_eq!(graph.groups, vec![scene.group.clone()]);
    assert_eq!(queued[0].user, scene.europa.username);
    let pair = BTreeMap::from([("family".to_owned(), BTreeSet::from(["parser".to_owned()]))]);
    assert_eq!(
        graph.change,
        WorkflowGraphChange::ToolTagsChanged {
            tool: tool.id,
            tags: pair
        }
    );
    // the toolset gets the tool as a selected member
    harness.run().await?;
    let built = set_instances(&admin, kit.id).await?;
    assert_eq!(built.len(), 2);
    assert_eq!(member_ids(&built[1]), vec![tool_r1.id]);
    assert_eq!(set_meta(&built[1]).triggered_by, scene.europa.username);
    harness.run().await?;
    // removing the tag rebuilds it without the tool
    let status = tag_entity(&scene.europa, tool.id, true, &groups, "family", "parser").await?;
    assert_eq!(status, 204);
    harness.run().await?;
    let built = set_instances(&admin, kit.id).await?;
    assert_eq!(built.len(), 3);
    assert_eq!(set_meta(&built[2]).members, Vec::new());
    harness.run().await?;
    // tagging an agent or a tool instance queues nothing
    assert_eq!(waiting(&admin).await?, Vec::new());
    let status = tag_entity(&scene.europa, lead.id, false, &groups, "family", "parser").await?;
    assert_eq!(status, 204);
    let status = tag_entity(
        &scene.europa,
        tool_r1.id,
        false,
        &groups,
        "family",
        "parser",
    )
    .await?;
    assert_eq!(status, 204);
    assert_eq!(waiting(&admin).await?, Vec::new());
    Ok(())
}

/// Adding a group to a toolset queues one toolset change, widens its instances in the same
/// request, and builds its next instance in both groups, where a tool tagged in only the first
/// group is left out naming the second; removing the group brings the tool back
#[tokio::test]
// one scenario walks a group add and its removal
#[allow(clippy::too_many_lines)]
async fn toolset_group_changes_rebuild() -> Result<(), Error> {
    // a tool in minerva and mars tagged toolset=ghidra only in minerva, and a toolset in minerva
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let suffix = generators::name_suffix();
    let europa = moon_user(&admin, "europa", &suffix).await?;
    let roles = GroupRoles::default().user(&europa.username);
    let minerva = god_group(&admin, "minerva", &suffix, &roles).await?;
    let mars = god_group(&admin, "mars", &suffix, &roles).await?;
    let both = [minerva.as_str(), mars.as_str()];
    let (tool, tool_r1) =
        generators::tool(&both, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    let status = tag_entity(&europa, tool.id, false, &[&minerva], "toolset", "ghidra").await?;
    assert_eq!(status, 204);
    let req = ToolSetEntityRequest::default().selector(ToolSetSelector::new("toolset", "ghidra"));
    let kit = toolset(&europa.client, "ghidra", &[&minerva], req).await?;
    harness.run().await?;
    harness.run().await?;
    let built = set_instances(&admin, kit.id).await?;
    assert_eq!(member_ids(&built[0]), vec![tool_r1.id]);
    // adding mars queues one toolset change and widens r1 right away
    europa
        .client
        .entities
        .update(kit.id, EntityUpdate::default().group(&mars))
        .await?;
    assert_eq!(
        waiting(&admin).await?,
        vec![WorkflowGraphChange::ToolSetChanged { tool_set: kit.id }]
    );
    let widened = admin.entities.get(built[0].id).await?;
    let mut widened_groups = widened.groups.clone();
    widened_groups.sort();
    let mut sorted_both = vec![minerva.clone(), mars.clone()];
    sorted_both.sort();
    assert_eq!(widened_groups, sorted_both);
    // r2 is in both groups and leaves the tool out for missing the tag in mars
    harness.run().await?;
    let built = set_instances(&admin, kit.id).await?;
    assert_eq!(built.len(), 2);
    let r2 = set_meta(&built[1]);
    assert_eq!(r2.tool_set_groups, sorted_both);
    assert_eq!(r2.members, Vec::new());
    assert_eq!(r2.excluded.len(), 1);
    assert_eq!(r2.excluded[0].tool, tool.id);
    assert_eq!(
        r2.excluded[0].reason,
        ToolSetExclusionReason::NotTaggedInAllGroups
    );
    assert_eq!(r2.excluded[0].missing_groups, vec![mars.clone()]);
    harness.run().await?;
    // removing mars queues a toolset change and r3 has the tool again
    europa
        .client
        .entities
        .update(kit.id, EntityUpdate::default().remove_group(&mars))
        .await?;
    assert_eq!(
        waiting(&admin).await?,
        vec![WorkflowGraphChange::ToolSetChanged { tool_set: kit.id }]
    );
    harness.run().await?;
    let built = set_instances(&admin, kit.id).await?;
    assert_eq!(built.len(), 3);
    assert_eq!(member_ids(&built[2]), vec![tool_r1.id]);
    Ok(())
}

/// While a toolset's automatic instances are off, a member publish and an include edit are
/// processed and cleared without an instance; turning them back on builds one instance that
/// reflects both
#[tokio::test]
async fn toolset_auto_instance_off_then_on() -> Result<(), Error> {
    // a toolset following one tool, with its first instance
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let (tool, _) =
        generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    let (other, other_r1) =
        generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    let req = ToolSetEntityRequest::default().tool(tool.id);
    let kit = toolset(&scene.europa.client, "paused", &groups, req).await?;
    harness.run().await?;
    // automatic instances go off
    let auto = |on: bool| {
        set_update(|update| {
            if let EntityMetadataUpdate::ToolSet { auto_instance, .. } = update {
                *auto_instance = Some(on);
            }
        })
    };
    update(&scene.europa.client, kit.id, auto(false)).await?;
    harness.run().await?;
    // a publish and an include edit are cleared without an instance
    scene
        .europa
        .client
        .entities
        .update(
            tool.id,
            generators::gen_tool_publish("2.0.0", &generators::gen_agentic_spec()),
        )
        .await?;
    update(
        &scene.europa.client,
        kit.id,
        set_update(|update| {
            if let EntityMetadataUpdate::ToolSet { add_tools, .. } = update {
                add_tools.push(other.id);
            }
        }),
    )
    .await?;
    let stats = harness.run().await?;
    assert_eq!(stats.tool_set_instances_created, 0);
    assert_eq!(set_instances(&admin, kit.id).await?.len(), 1);
    let left = harness.leftover().await?;
    assert!(
        left.queued.is_empty() && left.in_flight.is_empty(),
        "{left:?}"
    );
    // turning them on builds one instance with both changes
    update(&scene.europa.client, kit.id, auto(true)).await?;
    harness.run().await?;
    let built = set_instances(&admin, kit.id).await?;
    assert_eq!(built.len(), 2);
    let latest = latest_of(&admin, tool.id).await?;
    let mut expected = vec![latest, other_r1.id];
    expected.sort();
    assert_eq!(member_ids(&built[1]), expected);
    Ok(())
}

/// Clearing a toolset's selector queues a toolset change and its next instance has no selector,
/// no selected members, and no selector tag; a different selector then matches only its own pair
#[tokio::test]
async fn clear_and_replace_selector() -> Result<(), Error> {
    // two tools tagged with different pairs and a toolset selecting the first
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let req = generators::gen_tool(&groups, "1.0.0", &generators::gen_agentic_spec())
        .tag("family", "parser");
    let parser = admin
        .entities
        .create(req)
        .await?
        .tool_instance
        .expect("no instance");
    let req = generators::gen_tool(&groups, "1.0.0", &generators::gen_agentic_spec())
        .tag("suite", "radare");
    let radare = admin
        .entities
        .create(req)
        .await?
        .tool_instance
        .expect("no instance");
    let req = ToolSetEntityRequest::default().selector(ToolSetSelector::new("family", "parser"));
    let kit = toolset(&scene.europa.client, "switching", &groups, req).await?;
    harness.run().await?;
    harness.run().await?;
    let built = set_instances(&admin, kit.id).await?;
    assert_eq!(member_ids(&built[0]), vec![parser]);
    assert_eq!(
        tag_values(&built[0], TOOL_SET_SELECTOR_TAG),
        vec!["family=parser"]
    );
    // clearing the selector leaves r2 with no selector, members, or selector tag
    update(
        &scene.europa.client,
        kit.id,
        set_update(|update| {
            if let EntityMetadataUpdate::ToolSet { clear_selector, .. } = update {
                *clear_selector = Some(true);
            }
        }),
    )
    .await?;
    assert_eq!(
        waiting(&admin).await?,
        vec![WorkflowGraphChange::ToolSetChanged { tool_set: kit.id }]
    );
    harness.run().await?;
    let built = set_instances(&admin, kit.id).await?;
    assert_eq!(built.len(), 2);
    assert_eq!(set_meta(&built[1]).selector, None);
    assert_eq!(set_meta(&built[1]).members, Vec::new());
    assert_eq!(
        tag_values(&built[1], TOOL_SET_SELECTOR_TAG),
        Vec::<String>::new()
    );
    harness.run().await?;
    // a different selector matches only its own pair
    update(
        &scene.europa.client,
        kit.id,
        set_update(|update| {
            if let EntityMetadataUpdate::ToolSet { selector, .. } = update {
                *selector = Some(ToolSetSelector::new("suite", "radare"));
            }
        }),
    )
    .await?;
    harness.run().await?;
    let built = set_instances(&admin, kit.id).await?;
    assert_eq!(built.len(), 3);
    assert_eq!(member_ids(&built[2]), vec![radare]);
    Ok(())
}

/// Five edits of one toolset by two users in one batch build exactly one instance recorded as
/// caused by the last editor, who alone is told
#[tokio::test]
async fn toolset_edits_coalesce() -> Result<(), Error> {
    // a toolset with its first instance, editable by europa and callisto
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = crew(&admin).await?;
    let groups = [scene.group.as_str()];
    let mut tools = Vec::new();
    for _ in 0..4 {
        tools.push(
            generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), &admin).await?,
        );
    }
    let req = ToolSetEntityRequest::default().tool(tools[0].0.id);
    let kit = toolset(&admin, "busy", &groups, req).await?;
    harness.run().await?;
    harness.run().await?;
    let europa_before = notes(&scene.europa).await?.len();
    // europa makes four edits and callisto one
    let include = |tool: Uuid| {
        set_update(move |update| {
            if let EntityMetadataUpdate::ToolSet { add_tools, .. } = update {
                add_tools.push(tool);
            }
        })
    };
    let read_only = |on: bool| {
        set_update(move |update| {
            if let EntityMetadataUpdate::ToolSet { read_only, .. } = update {
                *read_only = Some(on);
            }
        })
    };
    update(&scene.europa.client, kit.id, include(tools[1].0.id)).await?;
    update(&scene.europa.client, kit.id, include(tools[2].0.id)).await?;
    update(&scene.europa.client, kit.id, read_only(true)).await?;
    update(&scene.europa.client, kit.id, read_only(false)).await?;
    update(&scene.callisto.client, kit.id, include(tools[3].0.id)).await?;
    let stats = harness.run().await?;
    // exactly one new instance, caused by callisto, with every included tool
    let built = set_instances(&admin, kit.id).await?;
    assert_eq!(built.len(), 2);
    assert_eq!(stats.tool_set_instances_created, 1);
    assert_eq!(set_meta(&built[1]).triggered_by, scene.callisto.username);
    let mut expected = tools
        .iter()
        .map(|(_, instance)| instance.id)
        .collect::<Vec<Uuid>>();
    expected.sort();
    assert_eq!(member_ids(&built[1]), expected);
    // callisto was told once and europa not at all
    let told = notes(&scene.callisto).await?;
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(
        told[0].1.contains("`busy`") && told[0].1.contains("#r2"),
        "{told:?}"
    );
    assert_eq!(notes(&scene.europa).await?.len(), europa_before);
    Ok(())
}

/// Manual toolset instance requests: a user who can edit only one of the toolset's groups is
/// refused with no event queued, an unknown toolset is not found, and a request when nothing
/// changed builds nothing and tells only the requester that the newest instance is current
#[tokio::test]
// one scenario walks every request in turn
#[allow(clippy::too_many_lines)]
async fn manual_toolset_requests() -> Result<(), Error> {
    // a toolset in minerva and mars; europa and ganymede edit both, callisto only minerva
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let suffix = generators::name_suffix();
    let europa = moon_user(&admin, "europa", &suffix).await?;
    let ganymede = moon_user(&admin, "ganymede", &suffix).await?;
    let callisto = moon_user(&admin, "callisto", &suffix).await?;
    let minerva = god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default()
            .user(&europa.username)
            .user(&ganymede.username)
            .user(&callisto.username),
    )
    .await?;
    let mars = god_group(
        &admin,
        "mars",
        &suffix,
        &GroupRoles::default()
            .user(&europa.username)
            .user(&ganymede.username)
            .monitor(&callisto.username),
    )
    .await?;
    let both = [minerva.as_str(), mars.as_str()];
    let (tool, _) =
        generators::tool(&both, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    let req = ToolSetEntityRequest::default().tool(tool.id);
    let kit = toolset(&europa.client, "requested", &both, req).await?;
    harness.run().await?;
    harness.run().await?;
    let r1 = set_instances(&admin, kit.id).await?[0].id;
    let europa_before = notes(&europa).await?.len();
    // callisto can't edit mars, so the request is refused and nothing is queued
    let refused = callisto
        .client
        .entities
        .request_tool_set_instance(kit.id)
        .await
        .err()
        .and_then(|err| err.status())
        .map(|status| status.as_u16());
    assert_eq!(refused, Some(401));
    assert_eq!(waiting(&admin).await?, Vec::new());
    // an unknown toolset isn't found
    let missing = europa
        .client
        .entities
        .request_tool_set_instance(Uuid::new_v4())
        .await
        .err()
        .and_then(|err| err.status())
        .map(|status| status.as_u16());
    assert_eq!(missing, Some(404));
    assert_eq!(waiting(&admin).await?, Vec::new());
    // ganymede's request when nothing changed builds nothing and tells only ganymede
    ganymede
        .client
        .entities
        .request_tool_set_instance(kit.id)
        .await?;
    let stats = harness.run().await?;
    assert_eq!(set_instances(&admin, kit.id).await?.len(), 1);
    assert_eq!(stats.tool_set_skipped, 1);
    let told = notes(&ganymede).await?;
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(told[0].0, NotificationLevel::Info);
    assert!(told[0].1.contains(&r1.to_string()), "{told:?}");
    assert_eq!(notes(&europa).await?.len(), europa_before);
    Ok(())
}

/// A tool instance a toolset pins can't be deleted until the toolset drops the include, which
/// queues its edge change, and the next instance lacks the tool; a toolset instance an agent
/// pins can't be deleted until the agent drops the edge, so the workflow's next instance no
/// longer uses it; a plain member who didn't submit a toolset instance can't delete it
#[tokio::test]
// one scenario walks both deletes and the refusal
#[allow(clippy::too_many_lines)]
async fn deleting_what_a_toolset_references() -> Result<(), Error> {
    // a toolset pinning one tool instance and following another tool
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = crew(&admin).await?;
    let groups = [scene.group.as_str()];
    let (_, pinned) =
        generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    let (kept, kept_r1) =
        generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    let req = ToolSetEntityRequest::default()
        .tool(pinned.id)
        .tool(kept.id);
    let kit = toolset(&scene.europa.client, "pins", &groups, req).await?;
    harness.run().await?;
    let r1 = set_instances(&admin, kit.id).await?[0].id;
    // an agent pins r1 in a workflow
    let lead = agent(&scene.europa.client, "lead", &groups, |req| {
        req.tool_set(r1)
    })
    .await?;
    let flow = workflow(&scene.europa.client, "pinned", &groups, lead.id).await?;
    harness.run().await?;
    let flow_r1 = &instances(&admin, flow.id).await?[0];
    assert_eq!(instance_meta(flow_r1).agents[0].tool_sets, vec![r1]);
    // a plain member who didn't submit r1 can't delete it
    let refused = scene
        .callisto
        .client
        .entities
        .delete(r1)
        .await
        .err()
        .and_then(|err| err.status())
        .map(|status| status.as_u16());
    assert_eq!(refused, Some(401));
    // the pinned tool instance can't be deleted while the toolset includes it
    let refused = admin
        .entities
        .delete(pinned.id)
        .await
        .err()
        .and_then(|err| err.status())
        .map(|status| status.as_u16());
    assert_eq!(refused, Some(409));
    // dropping the include queues its edge change, and the instance then deletes
    update(
        &scene.europa.client,
        kit.id,
        set_update(|update| {
            if let EntityMetadataUpdate::ToolSet { remove_tools, .. } = update {
                remove_tools.push(pinned.id);
            }
        }),
    )
    .await?;
    admin.entities.delete(pinned.id).await?;
    let queued = waiting(&admin).await?;
    assert!(
        queued.contains(&WorkflowGraphChange::EdgeChanged {
            source: kit.id,
            target: pinned.id,
            kind: AssociationKind::Includes
        }),
        "{queued:?}"
    );
    harness.run().await?;
    let built = set_instances(&admin, kit.id).await?;
    assert_eq!(built.len(), 2);
    assert_eq!(member_ids(&built[1]), vec![kept_r1.id]);
    harness.run().await?;
    // r1 can't be deleted while the agent pins it
    let refused = admin
        .entities
        .delete(r1)
        .await
        .err()
        .and_then(|err| err.status())
        .map(|status| status.as_u16());
    assert_eq!(refused, Some(409));
    // dropping the agent's edge to it queues its edge change, and r1 then deletes
    update(
        &scene.europa.client,
        lead.id,
        agent_update(|update| {
            if let EntityMetadataUpdate::Agent {
                remove_tool_sets, ..
            } = update
            {
                remove_tool_sets.push(r1);
            }
        }),
    )
    .await?;
    admin.entities.delete(r1).await?;
    let queued = waiting(&admin).await?;
    assert!(
        queued.contains(&WorkflowGraphChange::EdgeChanged {
            source: lead.id,
            target: r1,
            kind: AssociationKind::Calls
        }),
        "{queued:?}"
    );
    harness.run().await?;
    let rebuilt = instances(&admin, flow.id).await?;
    assert_eq!(rebuilt.len(), 2);
    assert_eq!(
        instance_meta(&rebuilt[1]).agents[0].tool_sets,
        Vec::<Uuid>::new()
    );
    Ok(())
}

/// With read only on, a member's new version that declares an effect is left out of the
/// toolset's next instance with a warning and the workflow following the toolset loses it; a
/// later version with no effects brings it back
#[tokio::test]
// one scenario follows two publishes through both phases
#[allow(clippy::too_many_lines)]
async fn effects_change_on_publish() -> Result<(), Error> {
    // a read only toolset following a tool with no effects, and a workflow following it
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let (tool, tool_r1) =
        generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    let req = ToolSetEntityRequest::default()
        .tool(tool.id)
        .read_only(true);
    let kit = toolset(&scene.europa.client, "careful", &groups, req).await?;
    let lead = agent(&scene.europa.client, "lead", &groups, |req| {
        req.tool_set(kit.id)
    })
    .await?;
    let flow = workflow(&scene.europa.client, "careful-flow", &groups, lead.id).await?;
    harness.run().await?;
    harness.run().await?;
    let built = instances(&admin, flow.id).await?;
    let before = built.len();
    assert_eq!(tool_ids(built.last().unwrap()), vec![tool_r1.id]);
    // a version that writes outside Thorium is left out with a warning
    scene
        .europa
        .client
        .entities
        .update(
            tool.id,
            generators::gen_tool_publish(
                "2.0.0",
                &spec_with_effects(&[ToolEffect::WritesExternal]),
            ),
        )
        .await?;
    harness.run().await?;
    let sets = set_instances(&admin, kit.id).await?;
    let r2 = set_meta(sets.last().unwrap());
    assert_eq!(r2.members, Vec::new());
    assert_eq!(r2.excluded.len(), 1);
    assert_eq!(r2.excluded[0].reason, ToolSetExclusionReason::NotReadOnly);
    assert_ne!(r2.warnings, Vec::<String>::new());
    harness.run().await?;
    let built = instances(&admin, flow.id).await?;
    assert_eq!(built.len(), before + 1);
    assert_eq!(tool_ids(built.last().unwrap()), Vec::<Uuid>::new());
    // a version with no effects brings it back
    scene
        .europa
        .client
        .entities
        .update(
            tool.id,
            generators::gen_tool_publish("3.0.0", &generators::gen_agentic_spec()),
        )
        .await?;
    let latest = latest_of(&admin, tool.id).await?;
    harness.run().await?;
    harness.run().await?;
    let sets = set_instances(&admin, kit.id).await?;
    assert_eq!(member_ids(sets.last().unwrap()), vec![latest]);
    let built = instances(&admin, flow.id).await?;
    assert_eq!(built.len(), before + 2);
    assert_eq!(tool_ids(built.last().unwrap()), vec![latest]);
    Ok(())
}

/// Rebuilding an unchanged toolset is skipped, but swapping an included tool for another whose
/// latest instance has the same content builds a new instance following the new tool, and its
/// toolset update rebuilds the workflows that follow the toolset
#[tokio::test]
async fn toolset_dedupe_follows_member_ids() -> Result<(), Error> {
    // a toolset following one tool, a twin tool with the same spec, and a workflow
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let spec = generators::gen_agentic_spec();
    let (tool, tool_r1) = generators::tool(&groups, "1.0.0", &spec, &admin).await?;
    let (twin, twin_r1) = generators::tool(&groups, "1.0.0", &spec, &admin).await?;
    let req = ToolSetEntityRequest::default().tool(tool.id);
    let kit = toolset(&scene.europa.client, "twins", &groups, req).await?;
    let lead = agent(&scene.europa.client, "lead", &groups, |req| {
        req.tool_set(kit.id)
    })
    .await?;
    let flow = workflow(&scene.europa.client, "twin-flow", &groups, lead.id).await?;
    harness.run().await?;
    harness.run().await?;
    let flows_before = instances(&admin, flow.id).await?.len();
    // a manual rebuild of the unchanged toolset is skipped
    scene
        .europa
        .client
        .entities
        .request_tool_set_instance(kit.id)
        .await?;
    let stats = harness.run().await?;
    assert_eq!(stats.tool_set_skipped, 1);
    let built = set_instances(&admin, kit.id).await?;
    assert_eq!(built.len(), 1);
    // swapping the tool for its twin keeps the digest but builds r2 following the twin
    update(
        &scene.europa.client,
        kit.id,
        set_update(|update| {
            if let EntityMetadataUpdate::ToolSet {
                add_tools,
                remove_tools,
                ..
            } = update
            {
                add_tools.push(twin.id);
                remove_tools.push(tool.id);
            }
        }),
    )
    .await?;
    harness.run().await?;
    let built = set_instances(&admin, kit.id).await?;
    assert_eq!(built.len(), 2);
    assert_eq!(set_meta(&built[1]).digest, set_meta(&built[0]).digest);
    assert_eq!(member_ids(&built[0]), vec![tool_r1.id]);
    assert_eq!(member_ids(&built[1]), vec![twin_r1.id]);
    assert_eq!(
        tag_values(&built[1], TOOL_SET_FOLLOWS_TOOL_TAG),
        vec![twin.id.to_string()]
    );
    assert_eq!(
        waiting(&admin).await?,
        vec![WorkflowGraphChange::ToolSetUpdated {
            tool_set: kit.id,
            tool_set_instance: built[1].id
        }]
    );
    // the workflow following the toolset gets a new revision using r2
    harness.run().await?;
    let flows = instances(&admin, flow.id).await?;
    assert_eq!(flows.len(), flows_before + 1);
    assert_eq!(
        instance_meta(flows.last().unwrap()).agents[0].tool_sets,
        vec![built[1].id]
    );
    Ok(())
}

/// Deleting a toolset while its edit is queued clears the events without an instance or a
/// notification, its instances stay readable, and a later pass reads nothing for it
#[tokio::test]
async fn deleted_toolset_clears_its_events() -> Result<(), Error> {
    // a built toolset edited and then deleted before the worker runs
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let (tool, _) =
        generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    let req = ToolSetEntityRequest::default().tool(tool.id);
    let kit = toolset(&scene.europa.client, "doomed", &groups, req).await?;
    harness.run().await?;
    harness.run().await?;
    let r1 = set_instances(&admin, kit.id).await?[0].id;
    let before = notes(&scene.europa).await?.len();
    update(
        &scene.europa.client,
        kit.id,
        set_update(|update| {
            if let EntityMetadataUpdate::ToolSet { read_only, .. } = update {
                *read_only = Some(true);
            }
        }),
    )
    .await?;
    admin.entities.delete(kit.id).await?;
    let stats = harness.run().await?;
    // nothing is left queued or in flight, nothing was created, and nobody was told
    let left = harness.leftover().await?;
    assert!(
        left.queued.is_empty() && left.in_flight.is_empty(),
        "{left:?}"
    );
    assert_eq!(stats.tool_set_instances_created, 0);
    assert_eq!(notes(&scene.europa).await?.len(), before);
    // the old instance is still readable, and the next pass reads nothing
    assert_eq!(admin.entities.get(r1).await?.id, r1);
    let next = harness.run().await?;
    assert_eq!(next, AiWorkerStats::default());
    Ok(())
}

/// A toolset build that ends Ready tells the user who caused it and a manual requester once
/// each, one that ends Invalid tells its user why, and the workflow build that follows a Ready
/// toolset instance sends its own notification
#[tokio::test]
// one scenario walks a ready build, an invalid build, and the workflow build in turn
#[allow(clippy::too_many_lines)]
async fn toolset_notifications() -> Result<(), Error> {
    // a toolset, a workflow following it, a spare tool, and a tool with no latest instance
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = crew(&admin).await?;
    let groups = [scene.group.as_str()];
    let (first, _) =
        generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    let (spare, _) =
        generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    let (bare, bare_r1) =
        generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    admin
        .entities
        .update(bare_r1.id, generators::gen_tool_deprecate(true))
        .await?;
    let req = ToolSetEntityRequest::default().tool(first.id);
    let kit = toolset(&admin, "announced", &groups, req).await?;
    let lead = agent(&admin, "lead", &groups, |req| req.tool_set(kit.id)).await?;
    let flow = workflow(&admin, "announced-flow", &groups, lead.id).await?;
    harness.run().await?;
    harness.run().await?;
    let flows_before = instances(&admin, flow.id).await?.len();
    // callisto asks for an instance and then europa includes the spare tool
    scene
        .callisto
        .client
        .entities
        .request_tool_set_instance(kit.id)
        .await?;
    update(
        &scene.europa.client,
        kit.id,
        set_update(|update| {
            if let EntityMetadataUpdate::ToolSet { add_tools, .. } = update {
                add_tools.push(spare.id);
            }
        }),
    )
    .await?;
    let stats = harness.run().await?;
    // r2 is ready and europa and callisto were each told once
    let built = set_instances(&admin, kit.id).await?;
    assert_eq!(built.len(), 2);
    assert_eq!(set_meta(&built[1]).triggered_by, scene.europa.username);
    for user in [&scene.europa, &scene.callisto] {
        let told = notes(user).await?;
        assert_eq!(told.len(), 1, "{told:?}");
        assert_eq!(told[0].0, NotificationLevel::Info);
        assert!(told[0].1.contains("`announced`"), "{told:?}");
        assert!(told[0].1.contains("#r2"), "{told:?}");
        assert!(told[0].1.contains(&built[1].id.to_string()), "{told:?}");
    }
    assert_eq!(stats.notifications_sent, 2);
    // the workflow build that follows tells europa about the workflow
    harness.run().await?;
    assert_eq!(instances(&admin, flow.id).await?.len(), flows_before + 1);
    let told = notes(&scene.europa).await?;
    assert_eq!(told.len(), 2, "{told:?}");
    assert!(
        told.iter().any(|(_, msg)| msg.contains("`announced-flow`")),
        "{told:?}"
    );
    // including the bare tool makes r3 invalid, and europa is told why
    update(
        &scene.europa.client,
        kit.id,
        set_update(|update| {
            if let EntityMetadataUpdate::ToolSet { add_tools, .. } = update {
                add_tools.push(bare.id);
            }
        }),
    )
    .await?;
    harness.run().await?;
    let told = notes(&scene.europa).await?;
    assert_eq!(told.len(), 3, "{told:?}");
    assert!(
        told.iter()
            .any(|(level, msg)| *level == NotificationLevel::Error
                && msg.contains("`announced`")
                && msg.contains("#r3")
                && msg.contains("no latest instance")),
        "{told:?}"
    );
    assert_eq!(notes(&scene.callisto).await?.len(), 1);
    Ok(())
}

/// Creating a read only toolset with an explicit tool and a selector builds a first instance
/// with the expected members and exclusions and tells its creator; attaching it to an agent gives
/// the agent's workflow a new revision with the members among the agent's tools
#[tokio::test]
// one scenario walks the whole user flow
#[allow(clippy::too_many_lines)]
async fn toolset_flow_end_to_end() -> Result<(), Error> {
    // an explicit tool, two tools tagged toolset=ghidra (one that writes), and a workflow
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let (explicit, explicit_r1) =
        generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    let req = generators::gen_tool(&groups, "1.0.0", &generators::gen_agentic_spec())
        .tag("toolset", "ghidra");
    let reader = admin
        .entities
        .create(req)
        .await?
        .tool_instance
        .expect("no instance");
    let req = generators::gen_tool(
        &groups,
        "1.0.0",
        &spec_with_effects(&[ToolEffect::WritesThorium]),
    )
    .tag("toolset", "ghidra");
    let writer = admin.entities.create(req).await?;
    let (own, own_r1) =
        generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    let lead = agent(&scene.europa.client, "lead", &groups, |req| {
        req.tool(own.id)
    })
    .await?;
    let flow = workflow(&scene.europa.client, "reversing", &groups, lead.id).await?;
    harness.run().await?;
    // europa creates the read only toolset
    let req = ToolSetEntityRequest::default()
        .tool(explicit.id)
        .selector(ToolSetSelector::new("toolset", "ghidra"))
        .read_only(true);
    let kit = toolset(&scene.europa.client, "ghidra-read-only", &groups, req).await?;
    harness.run().await?;
    let built = set_instances(&admin, kit.id).await?;
    assert_eq!(built.len(), 1);
    let r1 = set_meta(&built[0]);
    let mut expected = vec![explicit_r1.id, reader];
    expected.sort();
    assert_eq!(member_ids(&built[0]), expected);
    assert_eq!(r1.excluded.len(), 1);
    assert_eq!(r1.excluded[0].tool, writer.id);
    assert_eq!(r1.excluded[0].reason, ToolSetExclusionReason::NotReadOnly);
    let told = notes(&scene.europa).await?;
    assert!(
        told.iter()
            .any(|(level, msg)| *level == NotificationLevel::Info
                && msg.contains("`ghidra-read-only`")
                && msg.contains("#r1")
                && msg.contains("Ready")),
        "{told:?}"
    );
    // attaching the toolset to the agent gives the workflow a revision with the members
    let flows_before = instances(&admin, flow.id).await?.len();
    update(
        &scene.europa.client,
        lead.id,
        agent_update(|update| {
            if let EntityMetadataUpdate::Agent { add_tool_sets, .. } = update {
                add_tool_sets.push(kit.id);
            }
        }),
    )
    .await?;
    harness.run().await?;
    let flows = instances(&admin, flow.id).await?;
    assert_eq!(flows.len(), flows_before + 1);
    let mut expected = vec![explicit_r1.id, reader, own_r1.id];
    expected.sort();
    assert_eq!(tool_ids(flows.last().unwrap()), expected);
    assert_eq!(
        instance_meta(flows.last().unwrap()).agents[0].tool_sets,
        vec![built[0].id]
    );
    Ok(())
}

/// Publishing a new harness version gives the workflow whose entry agent follows the harness at
/// latest `#r2` running in the new instance, while the workflow pinned to the first instance gets
/// nothing
#[tokio::test]
async fn harness_publish_rebuilds_followers_only() -> Result<(), Error> {
    // one workflow's entry agent follows the harness at latest and one pins its first instance
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let (runtime, runtime_r1) = generators::tool(
        &groups,
        "1.0.0",
        &generators::gen_harness_spec(),
        &scene.europa.client,
    )
    .await?;
    let follower = agent(&scene.europa.client, "follower", &groups, |req| {
        req.harness(runtime.id)
    })
    .await?;
    let pinner = agent(&scene.europa.client, "pinner", &groups, |req| {
        req.harness(runtime_r1.id)
    })
    .await?;
    let follows = workflow(&scene.europa.client, "follows", &groups, follower.id).await?;
    let pins = workflow(&scene.europa.client, "pins", &groups, pinner.id).await?;
    harness.run().await?;
    // both first builds run in the first harness instance
    for flow in [&follows, &pins] {
        let built = instances(&admin, flow.id).await?;
        assert_eq!(built.len(), 1, "{}", flow.name);
        let meta = instance_meta(&built[0]);
        assert!(meta.is_ready(), "{:?}", meta.status);
        assert_eq!(meta.agents[0].harness, Some(runtime_r1.id));
        assert_eq!(
            meta.tool(runtime_r1.id).map(|tool| tool.kind),
            Some(ToolKind::Harness)
        );
    }
    // europa publishes a new harness version
    scene
        .europa
        .client
        .entities
        .update(
            runtime.id,
            generators::gen_tool_publish("2.0.0", &generators::gen_harness_spec()),
        )
        .await?;
    let runtime_r2 = latest_of(&admin, runtime.id).await?;
    assert_ne!(runtime_r2, runtime_r1.id);
    harness.run().await?;
    // the follower has r2 in the new instance and the pinner is unchanged
    let rebuilt = instances(&admin, follows.id).await?;
    assert_eq!(rebuilt.len(), 2);
    let r2 = instance_meta(&rebuilt[1]);
    assert!(r2.is_ready(), "{:?}", r2.status);
    assert_eq!(r2.agents[0].harness, Some(runtime_r2));
    assert!(r2.tool(runtime_r1.id).is_none());
    assert!(
        matches!(r2.trigger, WorkflowInstanceTrigger::ToolUpdated { tool, .. } if tool == runtime.id)
    );
    assert_eq!(instances(&admin, pins.id).await?.len(), 1);
    Ok(())
}

/// Building a workflow for the first time gives a Ready `#r1` in the catalog's groups with exactly
/// its agents and tools; the editing user is in `triggered_by` and has one Info notification
/// naming it, the handler's account submitted it, and the catalog's latest instance is `#r1`
#[tokio::test]
async fn first_build_is_ready_and_announced() -> Result<(), Error> {
    // europa builds a workflow whose agent follows a tool at latest
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let (tool, tool_r1) =
        generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    let lead = agent(&scene.europa.client, "lead", &groups, |req| {
        req.tool(tool.id)
    })
    .await?;
    let flow = workflow(&scene.europa.client, "triage", &groups, lead.id).await?;
    // the worker builds it
    let stats = harness.run().await?;
    println!("stats: {stats:?}");
    let built = instances(&admin, flow.id).await?;
    assert_eq!(built.len(), 1);
    let r1 = &built[0];
    let meta = instance_meta(r1);
    assert_eq!(meta.revision, 1);
    assert!(meta.is_ready(), "{:?}", meta.status);
    assert_eq!(r1.groups, vec![scene.group.clone()]);
    assert_eq!(
        meta.agents.iter().map(|a| a.agent).collect::<Vec<Uuid>>(),
        vec![lead.id]
    );
    assert_eq!(
        meta.tools
            .iter()
            .filter(|t| t.kind != ToolKind::Harness)
            .map(|t| t.tool_instance)
            .collect::<Vec<Uuid>>(),
        vec![tool_r1.id]
    );
    assert!(meta.agents[0].harness.is_some());
    // the editor caused it and the handler's account submitted it
    assert_eq!(meta.triggered_by, scene.europa.username);
    assert_eq!(meta.trigger, WorkflowInstanceTrigger::WorkflowUpdated);
    assert_eq!(r1.submitter, admin.users.info().await?.username);
    // the catalog's latest instance is r1
    let catalog = admin.entities.get(flow.id).await?;
    assert_eq!(catalog_meta(&catalog).latest_instance, Some(r1.id));
    // the editor has one Info notification naming the catalog, the revision, and the instance
    let found = notes(&scene.europa).await?;
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, NotificationLevel::Info);
    assert!(found[0].1.contains("`triage`"), "{}", found[0].1);
    assert!(found[0].1.contains("#r1"), "{}", found[0].1);
    assert!(found[0].1.contains(&r1.id.to_string()), "{}", found[0].1);
    assert_eq!(stats.instances_created, 1);
    assert_eq!(stats.notifications_sent, 1);
    Ok(())
}

/// Publishing a new tool version moves its latest instance and queues `ToolUpdated`: the workflow
/// that follows the tool at latest gets `#r2` with the new instance and its publisher is told,
/// while a workflow that pins the old instance gets nothing
#[tokio::test]
async fn publish_rebuilds_followers_only() -> Result<(), Error> {
    // one workflow follows the tool at latest and one pins its first instance
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let (tool, tool_r1) =
        generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    let follower = agent(&scene.europa.client, "follower", &groups, |req| {
        req.tool(tool.id)
    })
    .await?;
    let pinner = agent(&scene.europa.client, "pinner", &groups, |req| {
        req.tool(tool_r1.id)
    })
    .await?;
    let follows = workflow(&scene.europa.client, "follows", &groups, follower.id).await?;
    let pins = workflow(&scene.europa.client, "pins", &groups, pinner.id).await?;
    harness.run().await?;
    // europa publishes a new version
    scene
        .europa
        .client
        .entities
        .update(
            tool.id,
            generators::gen_tool_publish("2.0.0", &generators::gen_agentic_spec()),
        )
        .await?;
    let tool = admin.entities.get(tool.id).await?;
    let EntityMetadata::Tool(tool_meta) = &tool.metadata else {
        panic!("not a tool");
    };
    let latest = tool_meta.latest.expect("no latest");
    assert_ne!(latest, tool_r1.id);
    let queued = events::graph_changes(&admin, tool.id).await?;
    assert!(
        queued.iter().any(|event| matches!(
            events::graph_event(event).map(|graph| &graph.change),
            Some(WorkflowGraphChange::ToolUpdated { .. })
        )),
        "no ToolUpdated was queued"
    );
    harness.run().await?;
    // the follower has r2 with the new instance and the pinner is unchanged
    let rebuilt = instances(&admin, follows.id).await?;
    assert_eq!(rebuilt.len(), 2);
    let r2 = instance_meta(&rebuilt[1]);
    assert_eq!(r2.revision, 2);
    assert!(r2.tool(latest).is_some());
    assert_eq!(r2.triggered_by, scene.europa.username);
    assert!(
        matches!(r2.trigger, WorkflowInstanceTrigger::ToolUpdated { tool: t, .. } if t == tool.id)
    );
    assert_eq!(instances(&admin, pins.id).await?.len(), 1);
    // the publisher was told about r2
    let found = notes(&scene.europa).await?;
    assert!(
        found
            .iter()
            .any(|(_, msg)| msg.contains("`follows`") && msg.contains("#r2")),
        "{found:?}"
    );
    Ok(())
}

/// Editing an agent's tool edges gives a new revision to every workflow containing it, whether
/// it's an orchestrated agent or a sub-agent reached only through delegation
#[tokio::test]
async fn agent_edge_edit_rebuilds_every_workflow() -> Result<(), Error> {
    // a shared agent, orchestrated by one workflow and delegated to in another
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let (tool, _) =
        generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    let (extra, _) =
        generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    let shared = agent(&scene.europa.client, "shared", &groups, |req| {
        req.tool(tool.id)
    })
    .await?;
    let lead = agent(&scene.europa.client, "lead", &groups, |req| {
        req.sub_agent(shared.id)
    })
    .await?;
    let direct = workflow(&scene.europa.client, "direct", &groups, shared.id).await?;
    let nested = workflow(&scene.europa.client, "nested", &groups, lead.id).await?;
    harness.run().await?;
    // europa gives the shared agent another tool
    let add = agent_update(|update| {
        if let EntityMetadataUpdate::Agent { add_tools, .. } = update {
            add_tools.push(extra.id);
        }
    });
    update(&scene.europa.client, shared.id, add).await?;
    harness.run().await?;
    // both workflows have r2
    for flow in [&direct, &nested] {
        let built = instances(&admin, flow.id).await?;
        assert_eq!(built.len(), 2, "{} wasn't rebuilt", flow.name);
        assert!(instance_meta(&built[1]).tools.len() >= 2);
    }
    Ok(())
}

/// A burst of edits by io and then europa in one batch makes one new revision triggered by
/// europa, with one notification to europa and none to io
#[tokio::test]
async fn burst_of_edits_coalesces() -> Result<(), Error> {
    // io and europa both edit one workflow
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let io = moon_user(&admin, "io", &scene.suffix).await?;
    let roles = GroupRoles::default()
        .user(&io.username)
        .user(&scene.europa.username);
    let group = god_group(&admin, "juno", &scene.suffix, &roles).await?;
    let groups = [group.as_str()];
    let lead = agent(&admin, "lead", &groups, |req| req).await?;
    let flow = workflow(&admin, "busy", &groups, lead.id).await?;
    harness.run().await?;
    // five edits by io, then five by europa
    for max in 1..=5 {
        update(&io.client, flow.id, limit_update(max)).await?;
    }
    for max in 6..=10 {
        update(&scene.europa.client, flow.id, limit_update(max)).await?;
    }
    let stats = harness.run().await?;
    // one new revision, europa's, and only europa was told
    let built = instances(&admin, flow.id).await?;
    assert_eq!(built.len(), 2);
    assert_eq!(instance_meta(&built[1]).triggered_by, scene.europa.username);
    assert_eq!(notes(&scene.europa).await?.len(), 1);
    assert_eq!(notes(&io).await?, Vec::new());
    assert_eq!(stats.events_processed, 10);
    assert_eq!(stats.instances_created, 1);
    Ok(())
}

/// Processing the same events again, as after a crash, creates no second instance
#[tokio::test]
async fn reprocessing_creates_no_duplicate() -> Result<(), Error> {
    // a workflow and the events its creation queued
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let lead = agent(&admin, "lead", &groups, |req| req).await?;
    let flow = workflow(&scene.europa.client, "again", &groups, lead.id).await?;
    let popped = harness.pop().await?;
    // process them twice
    let first = harness.process(popped.clone()).await?;
    let second = harness.process(popped).await?;
    // only the first made an instance and the second was a dedupe skip that told nobody
    assert_eq!(instances(&admin, flow.id).await?.len(), 1);
    assert_eq!(first.instances_created, 1);
    assert_eq!(second.instances_created, 0);
    assert_eq!(second.skipped, 1);
    assert_eq!(notes(&scene.europa).await?.len(), 1);
    Ok(())
}

/// A workflow whose only agent follows a tool with no latest instance gets an Invalid `#r1` and
/// its creator one Error notification listing the errors, with no latest instance; more events
/// while the graph stays broken create nothing; publishing the tool gives a Ready `#r2`, found
/// through the tag on the Invalid `#r1`
#[tokio::test]
async fn broken_workflow_is_rebuilt_once_fixed() -> Result<(), Error> {
    // a tool whose only instance is deprecated, followed by the workflow's only agent
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let (tool, tool_r1) =
        generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    admin
        .entities
        .update(tool_r1.id, generators::gen_tool_deprecate(true))
        .await?;
    let lead = agent(&scene.europa.client, "lead", &groups, |req| {
        req.tool(tool.id)
    })
    .await?;
    let flow = workflow(&scene.europa.client, "broken", &groups, lead.id).await?;
    let stats = harness.run().await?;
    // r1 is invalid, tagged with the tool it follows, and the catalog has no latest instance
    let built = instances(&admin, flow.id).await?;
    assert_eq!(built.len(), 1);
    let WorkflowInstanceStatus::Invalid { errors } = &instance_meta(&built[0]).status else {
        panic!("r1 should be invalid");
    };
    assert_eq!(
        tag_values(&built[0], FOLLOWS_TOOL_TAG),
        vec![tool.id.to_string()]
    );
    let catalog = admin.entities.get(flow.id).await?;
    assert_eq!(catalog_meta(&catalog).latest_instance, None);
    assert_eq!(stats.invalid, 1);
    // the creator has one Error notification listing every error
    let found = notes(&scene.europa).await?;
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, NotificationLevel::Error);
    for error in errors {
        assert!(
            found[0].1.contains(error.as_str()),
            "{} lacks {error}",
            found[0].1
        );
    }
    // more events while the graph is unchanged create nothing
    events::push(&[
        graph_event(
            &scene.europa.username,
            &groups,
            WorkflowGraphChange::WorkflowUpdated { workflow: flow.id },
        ),
        graph_event(
            &scene.europa.username,
            &groups,
            WorkflowGraphChange::AgentUpdated { agent: lead.id },
        ),
    ])
    .await?;
    let stats = harness.run().await?;
    assert_eq!(instances(&admin, flow.id).await?.len(), 1);
    assert_eq!(stats.skipped, 1);
    // publishing the tool rebuilds the workflow as Ready
    admin
        .entities
        .update(
            tool.id,
            generators::gen_tool_publish("2.0.0", &generators::gen_agentic_spec()),
        )
        .await?;
    harness.run().await?;
    let built = instances(&admin, flow.id).await?;
    assert_eq!(built.len(), 2);
    assert!(instance_meta(&built[1]).is_ready());
    let catalog = admin.entities.get(flow.id).await?;
    assert_eq!(catalog_meta(&catalog).latest_instance, Some(built[1].id));
    Ok(())
}

/// With automatic instances off, edits build nothing but a manual request does
#[tokio::test]
async fn auto_instance_off_builds_only_on_request() -> Result<(), Error> {
    // a workflow that doesn't instance automatically
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let lead = agent(&admin, "lead", &groups, |req| req).await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(lead.id)
        .entry_agent(lead.id)
        .auto_instance(false);
    let flow = workflow_with(&scene.europa.client, "manual", &groups, req).await?;
    update(&scene.europa.client, flow.id, limit_update(2)).await?;
    harness.run().await?;
    assert!(instances(&admin, flow.id).await?.is_empty());
    // a manual request builds it and tells the requester
    scene
        .europa
        .client
        .entities
        .request_instance(flow.id)
        .await?;
    harness.run().await?;
    let built = instances(&admin, flow.id).await?;
    assert_eq!(built.len(), 1);
    assert_eq!(
        instance_meta(&built[0]).trigger,
        WorkflowInstanceTrigger::Manual
    );
    assert_eq!(notes(&scene.europa).await?.len(), 1);
    Ok(())
}

/// Revisions count up without gaps, each names the one before it, and the catalog's latest
/// instance is its newest Ready one
#[tokio::test]
async fn revisions_are_contiguous() -> Result<(), Error> {
    // a workflow edited twice, then made invalid
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let lead = agent(&admin, "lead", &groups, |req| req).await?;
    let flow = workflow(&scene.europa.client, "counted", &groups, lead.id).await?;
    harness.run().await?;
    update(&scene.europa.client, flow.id, limit_update(3)).await?;
    harness.run().await?;
    let clear = workflow_update(|update| {
        if let EntityMetadataUpdate::WorkflowCatalog {
            clear_entry_agent, ..
        } = update
        {
            *clear_entry_agent = Some(true);
        }
    });
    update(&scene.europa.client, flow.id, clear).await?;
    harness.run().await?;
    // r1 and r2 are ready and r3 isn't
    let built = instances(&admin, flow.id).await?;
    let revisions = built
        .iter()
        .map(|entity| instance_meta(entity).revision)
        .collect::<Vec<u64>>();
    assert_eq!(revisions, vec![1, 2, 3]);
    assert_eq!(instance_meta(&built[0]).previous, None);
    assert_eq!(instance_meta(&built[1]).previous, Some(built[0].id));
    assert_eq!(instance_meta(&built[2]).previous, Some(built[1].id));
    assert!(!instance_meta(&built[2]).is_ready());
    let catalog = admin.entities.get(flow.id).await?;
    assert_eq!(catalog_meta(&catalog).latest_instance, Some(built[1].id));
    assert_eq!(catalog_meta(&catalog).newest_instance, Some(built[2].id));
    Ok(())
}

/// Dropping the entry agent and then deleting it, removing its `Orchestrates` edge, and clearing
/// the entry agent each give an Invalid revision naming the missing entry agent, and the latest
/// instance stays on the Ready one before it; an entry agent a workflow orchestrates can't be
/// deleted
#[tokio::test]
async fn losing_the_entry_agent_invalidates() -> Result<(), Error> {
    // three ready workflows, each with an entry agent and a second agent
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let mut flows = Vec::new();
    for name in ["deleted", "removed", "cleared"] {
        let lead = agent(&admin, &format!("{name}-lead"), &groups, |req| req).await?;
        let helper = agent(&admin, &format!("{name}-helper"), &groups, |req| req).await?;
        let req = WorkflowCatalogEntityRequest::default()
            .agent(lead.id)
            .agent(helper.id)
            .entry_agent(lead.id);
        let flow = workflow_with(&scene.europa.client, name, &groups, req).await?;
        flows.push((flow, lead));
    }
    harness.run().await?;
    // lose each entry agent a different way; an agent a workflow orchestrates can't be deleted,
    // so the first workflow drops its entry agent and the agent is then deleted
    let refused = admin
        .entities
        .delete(flows[0].1.id)
        .await
        .err()
        .and_then(|err| err.status())
        .map(|status| status.as_u16());
    assert_eq!(refused, Some(409));
    let remove_first = workflow_update(|update| {
        if let EntityMetadataUpdate::WorkflowCatalog { remove_agents, .. } = update {
            remove_agents.push(flows[0].1.id);
        }
    });
    update(&scene.europa.client, flows[0].0.id, remove_first).await?;
    admin.entities.delete(flows[0].1.id).await?;
    let remove = workflow_update(|update| {
        if let EntityMetadataUpdate::WorkflowCatalog { remove_agents, .. } = update {
            remove_agents.push(flows[1].1.id);
        }
    });
    update(&scene.europa.client, flows[1].0.id, remove).await?;
    let clear = workflow_update(|update| {
        if let EntityMetadataUpdate::WorkflowCatalog {
            clear_entry_agent, ..
        } = update
        {
            *clear_entry_agent = Some(true);
        }
    });
    update(&scene.europa.client, flows[2].0.id, clear).await?;
    harness.run().await?;
    // each has an invalid r2 naming the entry agent, and latest stays on r1
    for (flow, lead) in &flows {
        let built = instances(&admin, flow.id).await?;
        assert_eq!(built.len(), 2, "{} wasn't rebuilt", flow.name);
        let WorkflowInstanceStatus::Invalid { errors } = &instance_meta(&built[1]).status else {
            panic!("{} r2 should be invalid", flow.name);
        };
        assert!(
            errors.iter().any(|error| error.contains("entry agent")),
            "{}: {errors:?}",
            flow.name
        );
        if flow.name != "cleared" {
            assert!(
                errors
                    .iter()
                    .any(|error| error.contains(&lead.id.to_string())),
                "{}: {errors:?}",
                flow.name
            );
        }
        let catalog = admin.entities.get(flow.id).await?;
        assert_eq!(catalog_meta(&catalog).latest_instance, Some(built[0].id));
    }
    Ok(())
}

/// Removing an orchestrated agent drops it from the next instance unless another agent in the
/// workflow still delegates to it, and a later edit of the dropped agent doesn't rebuild the
/// workflow, whose newest instance no longer uses it
#[tokio::test]
async fn removed_agents_leave_unless_delegated() -> Result<(), Error> {
    // the entry agent delegates to one of the two other orchestrated agents
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let kept = agent(&admin, "kept", &groups, |req| req).await?;
    let dropped = agent(&admin, "dropped", &groups, |req| req).await?;
    let lead = agent(&admin, "lead", &groups, |req| req.sub_agent(kept.id)).await?;
    let req = WorkflowCatalogEntityRequest::default()
        .agent(lead.id)
        .agent(kept.id)
        .agent(dropped.id)
        .entry_agent(lead.id);
    let flow = workflow_with(&scene.europa.client, "pruned", &groups, req).await?;
    harness.run().await?;
    // both other agents are removed from the catalog
    let remove = workflow_update(|update| {
        if let EntityMetadataUpdate::WorkflowCatalog { remove_agents, .. } = update {
            remove_agents.extend([kept.id, dropped.id]);
        }
    });
    update(&scene.europa.client, flow.id, remove).await?;
    harness.run().await?;
    // the delegated agent stays and the other is gone
    let built = instances(&admin, flow.id).await?;
    assert_eq!(built.len(), 2);
    let agents = instance_meta(&built[1])
        .agents
        .iter()
        .map(|a| a.agent)
        .collect::<BTreeSet<Uuid>>();
    assert_eq!(agents, BTreeSet::from([lead.id, kept.id]));
    // the removed agent is still tagged on r1 but not on the newest instance, so editing it
    // rebuilds nothing
    update(&admin, dropped.id, config_update("dropped_v2")).await?;
    let stats = harness.run().await?;
    assert_eq!(stats.tag_lookups, 1);
    assert_eq!(instances(&admin, flow.id).await?.len(), 2);
    Ok(())
}

/// Editing the config of a sub-agent reached only through delegation rebuilds the workflow,
/// found through its instance's agent tag
#[tokio::test]
async fn sub_agent_edit_rebuilds() -> Result<(), Error> {
    // the entry agent delegates to a sub-agent the catalog doesn't name
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let sub = agent(&admin, "sub", &groups, |req| req).await?;
    let lead = agent(&admin, "lead", &groups, |req| req.sub_agent(sub.id)).await?;
    let flow = workflow(&scene.europa.client, "deep", &groups, lead.id).await?;
    harness.run().await?;
    let r1 = &instances(&admin, flow.id).await?[0];
    assert!(tag_values(r1, INSTANCE_AGENT_TAG).contains(&sub.id.to_string()));
    // the sub-agent's config changes
    update(&admin, sub.id, config_update("sub_v2")).await?;
    let stats = harness.run().await?;
    // the workflow got r2 with the new config
    let built = instances(&admin, flow.id).await?;
    assert_eq!(built.len(), 2);
    let r2 = instance_meta(&built[1]);
    assert_eq!(
        r2.agent(sub.id)
            .and_then(|a| a.config.function_name.clone()),
        Some("sub_v2".to_owned())
    );
    assert_eq!(stats.tag_lookups, 1);
    Ok(())
}

/// Deleting a catalog while its edit is queued clears the event without an instance or a
/// notification, and the catalog's instances stay readable
#[tokio::test]
async fn deleted_catalog_clears_its_events() -> Result<(), Error> {
    // a built workflow edited and then deleted before the worker runs
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let lead = agent(&admin, "lead", &groups, |req| req).await?;
    let flow = workflow(&scene.europa.client, "doomed", &groups, lead.id).await?;
    harness.run().await?;
    let r1 = instances(&admin, flow.id).await?[0].id;
    let before = notes(&scene.europa).await?.len();
    update(&scene.europa.client, flow.id, limit_update(4)).await?;
    admin.entities.delete(flow.id).await?;
    let stats = harness.run().await?;
    // nothing is left queued or in flight, nothing was created, and nobody was told
    let left = harness.leftover().await?;
    assert!(
        left.queued.is_empty() && left.in_flight.is_empty(),
        "{left:?}"
    );
    assert_eq!(stats.instances_created, 0);
    assert_eq!(notes(&scene.europa).await?.len(), before);
    // the old instance is still readable
    assert_eq!(admin.entities.get(r1).await?.id, r1);
    Ok(())
}

/// A pinned tool instance can't be deleted until the agent drops its edge to it, and the next
/// instance doesn't snapshot the deleted instance
#[tokio::test]
async fn deleted_pin_leaves_the_next_instance() -> Result<(), Error> {
    // an agent that pins one tool instance and follows another tool at latest
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let (_, pinned) =
        generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    let (kept, _) =
        generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    let lead = agent(&admin, "lead", &groups, |req| {
        req.tool(pinned.id).tool(kept.id)
    })
    .await?;
    let flow = workflow(&scene.europa.client, "pinned", &groups, lead.id).await?;
    harness.run().await?;
    // the pinned instance can't be deleted while the agent pins it
    let refused = admin
        .entities
        .delete(pinned.id)
        .await
        .err()
        .and_then(|err| err.status())
        .map(|status| status.as_u16());
    assert_eq!(refused, Some(409));
    // the agent drops its edge to it, and the instance is then deleted
    update(
        &admin,
        lead.id,
        agent_update(|update| {
            if let EntityMetadataUpdate::Agent { remove_tools, .. } = update {
                remove_tools.push(pinned.id);
            }
        }),
    )
    .await?;
    admin.entities.delete(pinned.id).await?;
    let EntityMetadata::Agent(meta) = admin.entities.get(lead.id).await?.metadata else {
        panic!("not an agent");
    };
    assert!(meta.pinned_tools.iter().all(|tool| tool.id != pinned.id));
    harness.run().await?;
    // the next revision no longer lists the deleted instance, and with its edge gone the rest
    // of the graph is still ready
    let built = instances(&admin, flow.id).await?;
    assert_eq!(built.len(), 2);
    let r2 = instance_meta(&built[1]);
    assert!(r2.tool(pinned.id).is_none());
    assert!(r2.is_ready(), "{:?}", r2.status);
    Ok(())
}

/// Dedupe compares the digest and the referenced ids: replacing an agent with an identical clone
/// makes a new revision tagged with the clone, and a later edit to the clone rebuilds it;
/// splitting a shared sub-agent into two identical copies makes a revision tagged with both;
/// switching from following a tool at latest to pinning that same instance makes a revision
/// without the follow tag
#[tokio::test]
async fn dedupe_follows_referenced_ids() -> Result<(), Error> {
    // a workflow whose two agents share a sub-agent and whose entry agent follows a tool
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let (tool, tool_r1) =
        generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    let config = generators::gen_agent_config("worker");
    let make = |name: &'static str| {
        let config = config.clone();
        let admin = admin.clone();
        let groups = groups.map(str::to_owned);
        async move {
            let groups = groups.iter().map(String::as_str).collect::<Vec<&str>>();
            let resp = admin
                .entities
                .create(generators::gen_agent(
                    name,
                    &groups,
                    AgentEntityRequest::new(config),
                ))
                .await?;
            admin.entities.get(resp.id).await
        }
    };
    let shared = make("shared").await?;
    let left = agent(&admin, "left", &groups, |req| req.sub_agent(shared.id)).await?;
    let lead = agent(&admin, "lead", &groups, |req| {
        req.sub_agent(shared.id).sub_agent(left.id).tool(tool.id)
    })
    .await?;
    let flow = workflow(&scene.europa.client, "clones", &groups, lead.id).await?;
    harness.run().await?;
    // split the shared sub-agent: left delegates to an identical copy instead
    let copy = make("copy").await?;
    let swap = agent_update(|update| {
        if let EntityMetadataUpdate::Agent {
            add_sub_agents,
            remove_sub_agents,
            ..
        } = update
        {
            add_sub_agents.push(copy.id);
            remove_sub_agents.push(shared.id);
        }
    });
    update(&admin, left.id, swap).await?;
    harness.run().await?;
    let built = instances(&admin, flow.id).await?;
    assert_eq!(built.len(), 2);
    let tagged = tag_values(&built[1], INSTANCE_AGENT_TAG);
    assert!(tagged.contains(&shared.id.to_string()) && tagged.contains(&copy.id.to_string()));
    // a later edit to the copy rebuilds the workflow
    update(&admin, copy.id, config_update("copy_v2")).await?;
    harness.run().await?;
    assert_eq!(instances(&admin, flow.id).await?.len(), 3);
    // pin the tool's current instance instead of following it
    let pin = agent_update(|update| {
        if let EntityMetadataUpdate::Agent {
            add_tools,
            remove_tools,
            ..
        } = update
        {
            add_tools.push(tool_r1.id);
            remove_tools.push(tool.id);
        }
    });
    update(&admin, lead.id, pin).await?;
    harness.run().await?;
    let built = instances(&admin, flow.id).await?;
    assert_eq!(built.len(), 4);
    assert_eq!(
        tag_values(&built[3], FOLLOWS_TOOL_TAG),
        Vec::<String>::new()
    );
    assert!(
        instance_meta(&built[3])
            .tool(tool_r1.id)
            .is_some_and(|t| t.pinned)
    );
    // the same graph again builds nothing
    events::push(&[graph_event(
        &scene.europa.username,
        &groups,
        WorkflowGraphChange::WorkflowUpdated { workflow: flow.id },
    )])
    .await?;
    let stats = harness.run().await?;
    assert_eq!(stats.skipped, 1);
    assert_eq!(instances(&admin, flow.id).await?.len(), 4);
    Ok(())
}

/// Replacing the entry agent with an identical clone makes a new revision tagged with the clone
#[tokio::test]
async fn identical_clone_gets_its_own_revision() -> Result<(), Error> {
    // a workflow and an identical copy of its entry agent
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let config = generators::gen_agent_config("twin");
    let mut twins = Vec::new();
    for name in ["original", "clone"] {
        let resp = admin
            .entities
            .create(generators::gen_agent(
                name,
                &groups,
                AgentEntityRequest::new(config.clone()),
            ))
            .await?;
        twins.push(resp.id);
    }
    let flow = workflow(&scene.europa.client, "twins", &groups, twins[0]).await?;
    harness.run().await?;
    // swap the entry agent for its clone
    let swap = workflow_update(|update| {
        if let EntityMetadataUpdate::WorkflowCatalog {
            add_agents,
            remove_agents,
            entry_agent,
            ..
        } = update
        {
            add_agents.push(twins[1]);
            remove_agents.push(twins[0]);
            *entry_agent = Some(twins[1]);
        }
    });
    update(&scene.europa.client, flow.id, swap).await?;
    harness.run().await?;
    // the digest is the same but the clone gets its own revision and tag
    let built = instances(&admin, flow.id).await?;
    assert_eq!(built.len(), 2);
    assert_eq!(
        instance_meta(&built[0]).digest,
        instance_meta(&built[1]).digest
    );
    assert_eq!(
        tag_values(&built[1], INSTANCE_AGENT_TAG),
        vec![twins[1].to_string()]
    );
    // an edit to the clone rebuilds the workflow
    update(&admin, twins[1], config_update("twin_v2")).await?;
    harness.run().await?;
    assert_eq!(instances(&admin, flow.id).await?.len(), 3);
    Ok(())
}

/// Dedupe compares only the newest instance: deprecating a followed tool's only instance makes
/// an Invalid `#r2`, un-deprecating it makes a Ready `#r3` with `#r1`'s key, and more events
/// while the graph is unchanged create nothing
#[tokio::test]
async fn dedupe_compares_only_the_newest() -> Result<(), Error> {
    // a ready workflow whose agent follows a tool with one instance
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let (tool, tool_r1) =
        generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    let lead = agent(&admin, "lead", &groups, |req| req.tool(tool.id)).await?;
    let flow = workflow(&scene.europa.client, "flip", &groups, lead.id).await?;
    harness.run().await?;
    // deprecate and then restore the tool's only instance
    admin
        .entities
        .update(tool_r1.id, generators::gen_tool_deprecate(true))
        .await?;
    harness.run().await?;
    admin
        .entities
        .update(tool_r1.id, generators::gen_tool_deprecate(false))
        .await?;
    harness.run().await?;
    // r1 ready, r2 invalid, r3 ready with r1's digest
    let built = instances(&admin, flow.id).await?;
    let ready = built
        .iter()
        .map(|entity| instance_meta(entity).is_ready())
        .collect::<Vec<bool>>();
    assert_eq!(ready, vec![true, false, true]);
    assert_eq!(
        instance_meta(&built[0]).digest,
        instance_meta(&built[2]).digest
    );
    // nothing more while the graph is unchanged
    events::push(&[graph_event(
        &scene.europa.username,
        &groups,
        WorkflowGraphChange::ToolUpdated {
            tool: tool.id,
            tool_instance: tool_r1.id,
        },
    )])
    .await?;
    harness.run().await?;
    assert_eq!(instances(&admin, flow.id).await?.len(), 3);
    Ok(())
}

/// The worker instances a workflow in two groups with its admin client after a change by a user
/// in only one of them: `triggered_by` is that user and the handler's account submitted it;
/// lookups stay in the event's groups, so a workflow in a third group that uses the agent isn't
/// touched; and a manual request from a user who can edit only one of the groups is refused
#[tokio::test]
async fn admin_instances_across_groups() -> Result<(), Error> {
    // io is only in minerva; the workflow and its agent are in minerva and mars, and a second
    // workflow in vesta alone uses the same agent
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let suffix = generators::name_suffix();
    let io = moon_user(&admin, "io", &suffix).await?;
    let minerva = god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().user(&io.username),
    )
    .await?;
    let mars = god_group(&admin, "mars", &suffix, &GroupRoles::default()).await?;
    let vesta = god_group(&admin, "vesta", &suffix, &GroupRoles::default()).await?;
    let all = [minerva.as_str(), mars.as_str(), vesta.as_str()];
    let pair = [minerva.as_str(), mars.as_str()];
    let lead = agent(&admin, "lead", &all, |req| req).await?;
    let shared = workflow(&admin, "shared", &pair, lead.id).await?;
    let elsewhere = workflow(&admin, "elsewhere", &[vesta.as_str()], lead.id).await?;
    harness.run().await?;
    // an agent change by io carrying only minerva and mars
    events::push(&[graph_event(
        &io.username,
        &pair,
        WorkflowGraphChange::WorkflowUpdated {
            workflow: shared.id,
        },
    )])
    .await?;
    update(&admin, lead.id, config_update("lead_v2")).await?;
    let mut popped = harness.pop().await?;
    for event in &mut popped {
        if let EventData::WorkflowGraph(graph) = &mut event.data
            && matches!(graph.change, WorkflowGraphChange::AgentUpdated { .. })
        {
            graph.groups = pair.map(str::to_owned).to_vec();
            event.user.clone_from(&io.username);
        }
    }
    let stats = harness.process(popped).await?;
    println!("stats: {stats:?}");
    // the shared workflow was built for io by the handler's account, and the other wasn't
    let built = instances(&admin, shared.id).await?;
    assert_eq!(built.len(), 2);
    assert_eq!(instance_meta(&built[1]).triggered_by, io.username);
    assert_eq!(built[1].submitter, admin.users.info().await?.username);
    assert_eq!(instances(&admin, elsewhere.id).await?.len(), 1);
    // io can't ask for an instance of a workflow they can only edit in one group
    let refused = io.client.entities.request_instance(shared.id).await;
    assert_eq!(
        refused
            .err()
            .and_then(|err| err.status())
            .map(|s| s.as_u16()),
        Some(401)
    );
    Ok(())
}

/// A disabled user's change still builds the instance, recording them in `triggered_by`
#[tokio::test]
async fn disabled_user_still_triggers() -> Result<(), Error> {
    // europa edits a workflow and is then disabled
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let lead = agent(&admin, "lead", &groups, |req| req).await?;
    let flow = workflow(&scene.europa.client, "orphan", &groups, lead.id).await?;
    let disable = UserUpdate {
        role: Some(UserRole::Disabled),
        ..UserUpdate::default()
    };
    admin.users.update(&scene.europa.username, disable).await?;
    harness.run().await?;
    // the instance records europa
    let built = instances(&admin, flow.id).await?;
    assert_eq!(built.len(), 1);
    assert_eq!(instance_meta(&built[0]).triggered_by, scene.europa.username);
    Ok(())
}

/// Build notifications: a manual request coalesced with another user's change tells both once; a
/// dedupe-skipped manual request tells its requester the newest instance is current while a
/// skipped change tells nobody; a 100-event batch for one workflow writes one notification; and
/// a notification that can't be written is counted while the events clear and the instance stays
#[tokio::test]
async fn build_notifications() -> Result<(), Error> {
    // io and europa can both edit one workflow
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let io = moon_user(&admin, "io", &scene.suffix).await?;
    let roles = GroupRoles::default()
        .user(&io.username)
        .user(&scene.europa.username);
    let group = god_group(&admin, "juno", &scene.suffix, &roles).await?;
    let groups = [group.as_str()];
    let lead = agent(&admin, "lead", &groups, |req| req).await?;
    let flow = workflow(&admin, "noisy", &groups, lead.id).await?;
    harness.run().await?;
    // europa asks for an instance and io then edits, in the same batch
    scene
        .europa
        .client
        .entities
        .request_instance(flow.id)
        .await?;
    update(&io.client, flow.id, limit_update(2)).await?;
    harness.run().await?;
    let built = instances(&admin, flow.id).await?;
    assert_eq!(instance_meta(&built[1]).triggered_by, io.username);
    assert_eq!(notes(&io).await?.len(), 1);
    assert_eq!(notes(&scene.europa).await?.len(), 1);
    // europa asks again with nothing changed and is told it's current; io's no-op event tells
    // nobody
    scene
        .europa
        .client
        .entities
        .request_instance(flow.id)
        .await?;
    let stats = harness.run().await?;
    assert_eq!(stats.skipped, 1);
    let europa_notes = notes(&scene.europa).await?;
    assert_eq!(europa_notes.len(), 2);
    assert!(
        europa_notes
            .iter()
            .any(|(level, msg)| *level == NotificationLevel::Info && msg.contains("current"))
    );
    events::push(&[graph_event(
        &io.username,
        &groups,
        WorkflowGraphChange::WorkflowUpdated { workflow: flow.id },
    )])
    .await?;
    harness.run().await?;
    assert_eq!(notes(&io).await?.len(), 1);
    // a hundred edits by io in one batch write one notification
    for max in 0..100 {
        update(&io.client, flow.id, limit_update(10 + max)).await?;
    }
    let stats = harness.run().await?;
    assert_eq!(stats.events_processed, 100);
    assert_eq!(stats.notifications_sent, 1);
    assert_eq!(notes(&io).await?.len(), 2);
    // io edits and is then deleted, so their notification can't be written
    update(&io.client, flow.id, limit_update(500)).await?;
    admin.users.delete(&io.username).await?;
    let before = instances(&admin, flow.id).await?.len();
    let stats = harness.run().await?;
    assert_eq!(stats.notification_failures, 1);
    assert_eq!(instances(&admin, flow.id).await?.len(), before + 1);
    let left = harness.leftover().await?;
    assert!(
        left.queued.is_empty() && left.in_flight.is_empty(),
        "{left:?}"
    );
    Ok(())
}

/// Building an instance queues no further workflow graph events and changes none of the
/// entities it read
#[tokio::test]
async fn instancing_does_not_loop() -> Result<(), Error> {
    // a workflow with an agent that follows a tool
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let (tool, tool_r1) =
        generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    let lead = agent(&admin, "lead", &groups, |req| req.tool(tool.id)).await?;
    let flow = workflow(&scene.europa.client, "still", &groups, lead.id).await?;
    let read = [flow.id, lead.id, tool.id, tool_r1.id];
    // snapshot the stored rows of everything the build reads
    mature().await;
    let mut before = Vec::new();
    for id in read {
        before.push(test_utilities::entity_rows(id).await?);
    }
    harness.run().await?;
    // the build happened, nothing was queued, and the stored rows are the same
    assert_eq!(instances(&admin, flow.id).await?.len(), 1);
    let queued = events::queued(&admin, EventType::WorkflowGraph).await?;
    assert!(queued.is_empty(), "{queued:?}");
    for (id, rows) in read.iter().zip(before) {
        let after = test_utilities::entity_rows(*id).await?;
        // the API records the new instance on the catalog, so only its graph fields are compared
        if *id != flow.id {
            assert_eq!(after, rows, "{id} changed");
        }
    }
    let catalog = admin.entities.get(flow.id).await?;
    let (old, new) = (catalog_meta(&flow), catalog_meta(&catalog));
    assert_eq!(old.entry_agent, new.entry_agent);
    assert_eq!(old.auto_instance, new.auto_instance);
    assert_eq!(old.limits, new.limits);
    assert_eq!(old.modified, new.modified);
    Ok(())
}

/// Two workers processing the same mature edit at once make exactly one new revision, and both
/// finish the event
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_workers_make_one_revision() -> Result<(), Error> {
    // a built workflow and an edit both workers see
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let lead = agent(&admin, "lead", &groups, |req| req).await?;
    let flow = workflow(&scene.europa.client, "contested", &groups, lead.id).await?;
    harness.run().await?;
    update(&scene.europa.client, flow.id, limit_update(7)).await?;
    let popped = harness.pop().await?;
    // two workers process the same events at once
    let mut first = new_worker(&admin);
    let mut second = new_worker(&admin);
    let (mut first_stats, mut second_stats) = (AiWorkerStats::default(), AiWorkerStats::default());
    let (one, two) = tokio::join!(
        EventWorkerMutableSupport::process(&mut first, popped.clone(), &mut first_stats),
        EventWorkerMutableSupport::process(&mut second, popped, &mut second_stats),
    );
    one?;
    two?;
    // exactly one new revision and both finished the event
    assert_eq!(instances(&admin, flow.id).await?.len(), 2);
    assert_eq!(
        first_stats.instances_created + second_stats.instances_created,
        1
    );
    assert_eq!(first_stats.events_processed, 1);
    assert_eq!(second_stats.events_processed, 1);
    let left = harness.leftover().await?;
    assert!(left.in_flight.is_empty(), "{left:?}");
    Ok(())
}

/// The environment variable naming the test Scylla's Prometheus metrics URL
const SCYLLA_METRICS_KEY: &str = "THORIUM_TEST_SCYLLA_METRICS";

/// Count the CQL requests the test Scylla has served, over every shard and request kind
///
/// Returns none when `THORIUM_TEST_SCYLLA_METRICS` doesn't name a metrics URL, or it can't be
/// read. Comparing counts taken before and after some work shows how many Scylla requests it
/// cost the API.
async fn scylla_requests() -> Option<u64> {
    // read the metrics page the variable names
    let url = std::env::var(SCYLLA_METRICS_KEY).ok()?;
    let client = reqwest::Client::builder().no_proxy().build().ok()?;
    let body = client.get(&url).send().await.ok()?.text().await.ok()?;
    // add up the served request counters
    let total = body
        .lines()
        .filter(|line| line.starts_with("scylla_transport_cql_requests_count{"))
        .filter_map(|line| line.rsplit(' ').next()?.parse::<f64>().ok())
        .sum::<f64>();
    // the counters are whole numbers stored as floats
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(total as u64)
}

/// Submit a copy of a workflow's newest instance as its next revision, as a build that found a
/// change would
///
/// # Arguments
///
/// * `admin` - The admin client
/// * `workflow` - The workflow
async fn next_revision(admin: &Thorium, workflow: Uuid) -> Result<Uuid, Error> {
    // copy the newest instance and point the copy at it
    let catalog = admin.entities.get(workflow).await?;
    let meta = catalog_meta(&catalog);
    let newest = meta.newest_instance.expect("the workflow has an instance");
    let mut instance = instance_meta(&admin.entities.get(newest).await?).clone();
    instance.revision = meta.highest_revision + 1;
    instance.previous = Some(newest);
    // submit it in the workflow's groups
    let metadata = thorium::models::WorkflowInstanceEntityRequest::new(&instance)?;
    let req = thorium::models::EntityRequest::new(
        "workflow-instance",
        thorium::models::EntityMetadataRequest::WorkflowInstance(metadata),
        instance.workflow_groups.clone(),
    );
    Ok(admin.entities.create(req).await?.id)
}

/// What one `ToolUpdated` event cost the worker and the API
#[derive(Debug)]
struct LookupCost {
    /// The revisions the workflow had when its tool was published
    depth: usize,
    /// The groups the tool, agent, and workflow are in
    width: usize,
    /// The tag lookups the worker made
    tag_lookups: usize,
    /// The entities the worker read by id
    graph_reads: usize,
    /// The CQL requests Scylla served, when its metrics can be read
    scylla: Option<u64>,
    /// The commands Redis ran
    redis: u64,
}

/// Build a workflow at some revision depth in some groups, publish its tool, and measure what
/// the worker's pass over the `ToolUpdated` event costs
///
/// # Arguments
///
/// * `harness` - The test's worker and admin client
/// * `scene` - The test's users and group
/// * `god` - The god the measurement's groups are named after
/// * `depth` - How many revisions the workflow has before the publish
/// * `width` - How many groups everything is in
async fn lookup_cost(
    harness: &mut Harness,
    scene: &Scene,
    god: &str,
    depth: usize,
    width: usize,
) -> Result<LookupCost, Error> {
    // a tool, an agent calling it, and a workflow, all in the groups
    let admin = harness.admin.clone();
    let mut names = Vec::with_capacity(width);
    for index in 0..width {
        let name = format!("{god}{depth}d{width}w{index}");
        names.push(god_group(&admin, &name, &scene.suffix, &GroupRoles::default()).await?);
    }
    let groups = names.iter().map(String::as_str).collect::<Vec<&str>>();
    let (tool, _) =
        generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    let caller = agent(&admin, &format!("deep-{depth}-{width}"), &groups, |req| {
        req.tool(tool.id)
    })
    .await?;
    let flow = workflow(&admin, &format!("deep-{depth}-{width}"), &groups, caller.id).await?;
    harness.run().await?;
    // more revisions that all follow the tool
    for _ in 1..depth {
        next_revision(&admin, flow.id).await?;
    }
    // publish the tool and measure the pass that rebuilds the workflow
    admin
        .entities
        .update(
            tool.id,
            generators::gen_tool_publish("2.0.0", &generators::gen_agentic_spec()),
        )
        .await?;
    let popped = harness.pop().await?;
    let scylla_before = scylla_requests().await;
    let redis_before = events::redis_commands().await?;
    let stats = harness.process(popped).await?;
    let redis_after = events::redis_commands().await?;
    let scylla_after = scylla_requests().await;
    assert_eq!(stats.instances_created, 1, "{stats:?}");
    // one command reads the count itself
    Ok(LookupCost {
        depth,
        width,
        tag_lookups: stats.tag_lookups,
        graph_reads: stats.graph_reads,
        scylla: scylla_before
            .zip(scylla_after)
            .map(|(before, after)| after - before),
        redis: redis_after - redis_before - 1,
    })
}

/// Lookup cost by workflow count: one `ToolUpdated` event reaching fifty workflows at their first
/// revision pairs their instances with one tag lookup and one read per workflow, never a listing
/// per workflow
#[tokio::test]
async fn lookup_cost_across_many_workflows() -> Result<(), Error> {
    // fifty workflows whose one agent calls a tool, each built once
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let (tool, _) =
        generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
    let caller = agent(&admin, "wide", &groups, |req| req.tool(tool.id)).await?;
    for index in 0..50 {
        workflow(&admin, &format!("wide-{index}"), &groups, caller.id).await?;
    }
    harness.run().await?;
    // publishing the tool rebuilds all fifty
    admin
        .entities
        .update(
            tool.id,
            generators::gen_tool_publish("2.0.0", &generators::gen_agentic_spec()),
        )
        .await?;
    let popped = harness.pop().await?;
    let redis_before = events::redis_commands().await?;
    let stats = harness.process(popped).await?;
    let redis_after = events::redis_commands().await?;
    println!(
        "ToolUpdated for 50 workflows at one revision: {} lookups, {} reads by id, {} Redis commands",
        stats.tag_lookups,
        stats.graph_reads,
        redis_after - redis_before - 1
    );
    assert_eq!(stats.instances_created, 50, "{stats:?}");
    assert!(stats.tag_lookups <= 4, "{stats:?}");
    Ok(())
}

/// Lookup cost by revision depth and group width: one `ToolUpdated` event for a workflow whose
/// instances follow the tool costs the same reads by id at revision depths 1, 10, and 50 and at
/// most one more lookup once older revisions match, and the Scylla requests the API serves for it
/// (counted server-side when the metrics URL is set) grow by at most a few id-listing pages; the
/// same counts are recorded for 1, 5, and 20 groups
#[tokio::test]
async fn lookup_cost_by_depth_and_width() -> Result<(), Error> {
    // measure each depth in one group, then each width at one revision
    let mut harness = Harness::new().await?;
    let scene = scene(&harness.admin).await?;
    let mut by_depth = Vec::with_capacity(3);
    for depth in [1, 10, 50] {
        by_depth.push(lookup_cost(&mut harness, &scene, "vesta", depth, 1).await?);
    }
    let mut by_width = Vec::with_capacity(3);
    for width in [1, 5, 20] {
        by_width.push(lookup_cost(&mut harness, &scene, "ceres", 1, width).await?);
    }
    for cost in by_depth.iter().chain(&by_width) {
        println!(
            "ToolUpdated at depth {} in {} groups: {} lookups, {} reads by id, {:?} Scylla requests, {} Redis commands",
            cost.depth, cost.width, cost.tag_lookups, cost.graph_reads, cost.scylla, cost.redis
        );
    }
    // the worker's reads by id don't grow with the revisions, and its lookups only gain the one
    // listing that pairs the older matched revisions with their workflow
    assert!(
        by_depth
            .iter()
            .all(|cost| cost.tag_lookups <= by_depth[0].tag_lookups + 1
                && cost.graph_reads == by_depth[0].graph_reads),
        "{by_depth:#?}"
    );
    // nor do the API's Scylla requests, beyond a few listing pages, when they're counted
    if let (Some(shallow), Some(deep)) = (by_depth[0].scylla, by_depth[2].scylla) {
        assert!(deep <= shallow + 20, "{by_depth:#?}");
    }
    Ok(())
}

/// Throughput: 10,000 events for 100 workflows build at most 100 workflows per batch and write at
/// most one notification per new instance; a burst of immature events is popped and put back with
/// unchanged scores on each idle poll, none is processed early, and the Redis commands each poll
/// costs are recorded; the lookups and reads one tool change causes are recorded for a tool in 1,
/// 5, and 20 groups and don't grow with the group count
#[tokio::test]
// one scenario measures every part of the worker's throughput
#[allow(clippy::too_many_lines)]
async fn throughput() -> Result<(), Error> {
    // 100 workflows, each built once
    let mut harness = Harness::new().await?;
    let admin = harness.admin.clone();
    let scene = scene(&admin).await?;
    let groups = [scene.group.as_str()];
    let lead = agent(&admin, "lead", &groups, |req| req).await?;
    let mut flows = Vec::with_capacity(100);
    for index in 0..100 {
        flows.push(workflow(&admin, &format!("bulk-{index}"), &groups, lead.id).await?);
    }
    events::discard(EventType::WorkflowGraph).await?;
    // 10,000 events, 100 per workflow
    let user = scene.europa.username.clone();
    let mut queued = Vec::with_capacity(10_000);
    for round in 0..100 {
        for flow in &flows {
            let change = if round == 99 {
                WorkflowGraphChange::ManualInstance { workflow: flow.id }
            } else {
                WorkflowGraphChange::WorkflowUpdated { workflow: flow.id }
            };
            queued.push(graph_event(&user, &groups, change));
        }
    }
    events::push(&queued).await?;
    // every batch builds each workflow at most once
    let mut batches = 0;
    let mut total = AiWorkerStats::default();
    loop {
        let popped = harness.pop().await?;
        if popped.is_empty() {
            break;
        }
        let stats = harness.process(popped).await?;
        assert!(stats.instances_created + stats.skipped <= 100, "{stats:?}");
        total += stats;
        batches += 1;
    }
    println!("10,000 events in {batches} batches: {total:?}");
    assert_eq!(total.events_processed, 10_000);
    assert_eq!(total.instances_created, 100);
    assert!(total.notifications_sent <= total.instances_created + total.skipped);
    // a burst of 1,000 immature events is put back untouched on every idle poll
    let burst = (0..1000)
        .map(|_| {
            graph_event(
                &user,
                &groups,
                WorkflowGraphChange::WorkflowUpdated {
                    workflow: flows[0].id,
                },
            )
        })
        .collect::<Vec<Event>>();
    events::push(&burst).await?;
    let before = harness.leftover().await?;
    let opts = EventPopOpts::default().limit(1000);
    let mut per_poll = Vec::new();
    for _ in 0..3 {
        let start = events::redis_commands().await?;
        let popped = admin.events.pop(EventType::WorkflowGraph, &opts).await?;
        let end = events::redis_commands().await?;
        assert!(popped.is_empty(), "an immature event was popped");
        // one command reads the count itself
        per_poll.push(end - start - 1);
        assert_eq!(harness.leftover().await?.queued, before.queued);
    }
    println!(
        "Redis commands per idle poll with 1,000 immature events: {per_poll:?} (about {} per second at one poll every 3 s)",
        per_poll.iter().max().copied().unwrap_or(0) / 3
    );
    assert!(
        per_poll.iter().all(|count| *count / 3 <= 700),
        "{per_poll:?}"
    );
    events::discard(EventType::WorkflowGraph).await?;
    // the lookups and reads one tool change causes for a tool in 1, 5, and 20 groups
    let mut costs = Vec::new();
    for width in [1_usize, 5, 20] {
        let mut names = Vec::with_capacity(width);
        for index in 0..width {
            let god = format!("ceres{width}x{index}");
            names.push(god_group(&admin, &god, &scene.suffix, &GroupRoles::default()).await?);
        }
        let wide = names.iter().map(String::as_str).collect::<Vec<&str>>();
        let (tool, _) =
            generators::tool(&wide, "1.0.0", &generators::gen_agentic_spec(), &admin).await?;
        let user_of_tool = agent(&admin, &format!("wide-{width}"), &wide, |req| {
            req.tool(tool.id)
        })
        .await?;
        workflow(&admin, &format!("wide-{width}"), &wide, user_of_tool.id).await?;
        harness.run().await?;
        admin
            .entities
            .update(
                tool.id,
                generators::gen_tool_publish("2.0.0", &generators::gen_agentic_spec()),
            )
            .await?;
        let stats = harness.run().await?;
        assert_eq!(stats.instances_created, 1, "{stats:?}");
        costs.push((width, stats.tag_lookups, stats.graph_reads));
    }
    println!("(groups, tag lookups, reads by id) per ToolUpdated: {costs:?}");
    assert!(costs.iter().all(|(_, lookups, _)| *lookups == costs[0].1));
    assert!(costs.iter().all(|(_, _, reads)| *reads == costs[0].2));
    Ok(())
}
