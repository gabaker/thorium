//! Seeds the data of Agentic Workflows through the API
//!
//! The seed creates one of each kind the feature adds: an Image tool, a container tool that runs
//! it, a harness tool, an Agentic tool, a toolset and its instance, a model credential holding a
//! secret, an endpoint that uses it, a profile, an agent, a workflow catalog and its instance, and
//! a workflow run that carries a run token. It also records a finished job's runtime in the shared
//! runtimes so the backup has something to hold for them. A pipeline whose stages run tool
//! instances comes from the broad seed's runtime step.

use super::names::Names;
use std::collections::BTreeMap;
use thorium::Thorium;
use thorium::models::{
    AgentEntityRequest, AllowedEndpoint, CredentialKind, Entity, EntityMetadata,
    EntityMetadataRequest, ImageScaler, ModelApi, ModelCatalogEntryRequest,
    ModelCredentialEntityRequest, ModelEndpointEntityRequest, ModelProfileEntityRequest,
    ModelProvider, ModelSecret, ModelsConfUpdate, RunInputs, SystemSettingsUpdate,
    SystemSettingsUpdateParams, ToolMechanism, ToolSetEntityRequest, WorkflowCatalogEntityRequest,
    WorkflowRunRequest,
};
use thorium::test_utilities::{generators, runs};
use uuid::Uuid;

/// The secret the seeded model credential holds, which must survive a backup and restore and
/// never show up in any output
pub const CREDENTIAL_SECRET: &str = "sk-fixture-credential-secret";

/// The model id the seeded endpoint's catalog holds
const MODEL_ID: &str = "fixture-model";

/// What the agentic seed created, for tests that need to find it again
#[derive(Debug, Clone)]
pub struct Agentic {
    /// The seeded tool's first instance
    pub tool_instance: Uuid,
    /// The seeded harness tool's first instance
    pub harness_instance: Uuid,
    /// The seeded model credential
    pub credential: Uuid,
    /// The seeded model endpoint
    pub endpoint: Uuid,
    /// The seeded model profile
    pub profile: Uuid,
    /// The seeded workflow instance
    pub workflow_instance: Uuid,
    /// The reaction of the seeded workflow run, which holds a run token
    pub run: Uuid,
    /// The seeded run's token
    pub run_token: String,
}

/// Seeds one of each kind Agentic Workflows adds, and a run with its run token
///
/// # Arguments
///
/// * `client` - An admin client
/// * `names` - The names for this seed's resources
/// * `group` - The group to create everything in
/// * `sample` - The sha256 of a sample in the group for the run to work on
///
/// # Panics
///
/// Panics if the API refuses any of the seed's requests.
pub async fn seed(client: &Thorium, names: &Names, group: &str, sample: &str) -> Agentic {
    // everything is created in the one group
    let groups = [group];
    // the model entities: a credential with a secret, an endpoint that uses it, and a profile
    let (credential, endpoint, profile) = models(client, &groups).await;
    // an Image tool, a container tool that runs it, and a harness tool, each with its first instance
    let (_, tool_instance) = runs::image_tool(&groups, ImageScaler::K8s, client)
        .await
        .expect("failed to create an image tool");
    let (_, container) = runs::container_tool(&groups, tool_instance.id, client)
        .await
        .expect("failed to create a container tool");
    let (_, harness) = runs::harness_tool(&groups, &runs::gen_model_harness_spec(), None, client)
        .await
        .expect("failed to create a harness tool");
    // an Agentic tool, the only kind a toolset includes, and a toolset of it with its first instance
    let (_, native) = generators::tool(&groups, "1.0.0", &generators::gen_native_spec(), client)
        .await
        .expect("failed to create an agentic tool");
    toolset(client, names, &groups, &native).await;
    // an agent that calls the tool, a workflow with it as entry agent, and the workflow's instance
    let (workflow_instance, run, run_token) =
        workflow(client, names, &groups, &container, &harness, sample).await;
    // average the finished jobs' runtimes into the shared runtimes
    client
        .jobs
        .update_workload_runtimes()
        .await
        .expect("failed to average the workload runtimes");
    Agentic {
        tool_instance: tool_instance.id,
        harness_instance: harness.id,
        credential,
        endpoint,
        profile,
        workflow_instance,
        run,
        run_token,
    }
}

/// Creates a model credential holding a secret, an endpoint that authenticates with it, and a
/// profile of one of the endpoint's models
///
/// # Arguments
///
/// * `client` - An admin client
/// * `groups` - The groups to create the entities in
///
/// # Panics
///
/// Panics if the API refuses any of the requests.
async fn models(client: &Thorium, groups: &[&str]) -> (Uuid, Uuid, Uuid) {
    // a uniquely addressed gateway, allowed on the deployment's whitelist
    let provider = ModelProvider::OpenAiCompatible {
        base_url: format!(
            "https://{}.gateway.local/v1",
            generators::gen_string(10).to_lowercase()
        ),
        extra_headers: BTreeMap::new(),
        key_header: None,
    };
    let allow = SystemSettingsUpdate::default()
        .models(ModelsConfUpdate::default().add_endpoint(AllowedEndpoint::for_provider(&provider)));
    client
        .system
        .update_settings(&allow, &SystemSettingsUpdateParams::default().no_scan())
        .await
        .expect("failed to allow the model endpoint");
    // the credential, whose secret the backup must keep
    let secret = ModelCredentialEntityRequest::new(
        CredentialKind::ApiKey,
        Some(ModelSecret::from(CREDENTIAL_SECRET)),
    );
    let credential = client
        .entities
        .create(generators::gen_model_entity(
            groups,
            EntityMetadataRequest::ModelCredential(secret),
        ))
        .await
        .expect("failed to create a model credential")
        .id;
    // the endpoint with a one model catalog
    let mut entry = ModelCatalogEntryRequest::new(MODEL_ID);
    entry.context_window = Some(200_000);
    entry.max_output_tokens = Some(8_192);
    let request = ModelEndpointEntityRequest::new(provider)
        .credential(credential)
        .model(entry);
    let endpoint = client
        .entities
        .create(generators::gen_model_entity(
            groups,
            EntityMetadataRequest::ModelEndpoint(request),
        ))
        .await
        .expect("failed to create a model endpoint")
        .id;
    // the profile of the endpoint's model
    let request = ModelProfileEntityRequest::new(endpoint, MODEL_ID, ModelApi::OpenAiChat);
    let profile = client
        .entities
        .create(generators::gen_model_entity(
            groups,
            EntityMetadataRequest::ModelProfile(request),
        ))
        .await
        .expect("failed to create a model profile")
        .id;
    (credential, endpoint, profile)
}

/// Creates a toolset that includes a tool instance, and its first instance
///
/// # Arguments
///
/// * `client` - An admin client
/// * `names` - The names for this seed's resources
/// * `groups` - The groups to create the toolset in
/// * `member` - The tool instance the toolset includes
///
/// # Panics
///
/// Panics if the API refuses any of the requests.
async fn toolset(client: &Thorium, names: &Names, groups: &[&str], member: &Entity) {
    // the toolset pins the tool instance
    let request = ToolSetEntityRequest::default().tool(member.id);
    let created = client
        .entities
        .create(generators::gen_toolset(
            &names.user("toolset"),
            groups,
            request,
        ))
        .await
        .expect("failed to create a toolset");
    let tool_set = client
        .entities
        .get(created.id)
        .await
        .expect("failed to read the toolset");
    // its first ready instance holds the tool instance as a member
    let instance = generators::gen_toolset_instance(&tool_set, 1, None, &[member]);
    client
        .entities
        .create(generators::gen_toolset_instance_req(&instance))
        .await
        .expect("failed to create a toolset instance");
}

/// Creates an agent that calls a tool, a workflow catalog with it as the entry agent, the
/// workflow's first instance, and a run of the instance with its run token
///
/// # Arguments
///
/// * `client` - An admin client
/// * `names` - The names for this seed's resources
/// * `groups` - The groups to create the entities in
/// * `tool` - The Agentic tool instance the agent calls
/// * `harness` - The harness tool instance the agent runs in
/// * `sample` - The sha256 of the sample the run works on
///
/// # Panics
///
/// Panics if the API refuses any of the requests.
async fn workflow(
    client: &Thorium,
    names: &Names,
    groups: &[&str],
    tool: &Entity,
    harness: &Entity,
    sample: &str,
) -> (Uuid, Uuid, String) {
    // the agent calls the tool by its root
    let EntityMetadata::ToolInstance(instance) = &tool.metadata else {
        panic!("{} is not a tool instance", tool.id);
    };
    let mut request = AgentEntityRequest::new(generators::gen_agent_config("lead"));
    request.tools = vec![instance.tool];
    let agent = client
        .entities
        .create(generators::gen_agent(&names.user("lead"), groups, request))
        .await
        .expect("failed to create an agent");
    let agent = client
        .entities
        .get(agent.id)
        .await
        .expect("failed to read the agent");
    // the workflow catalog with the agent as its entry agent
    let request = WorkflowCatalogEntityRequest::default()
        .agent(agent.id)
        .entry_agent(agent.id)
        .auto_instance(false);
    let workflow = client
        .entities
        .create(generators::gen_workflow(
            &names.user("triage"),
            groups,
            request,
        ))
        .await
        .expect("failed to create a workflow");
    let workflow = client
        .entities
        .get(workflow.id)
        .await
        .expect("failed to read the workflow");
    // its first instance binds the tool through the Thorium API
    let tools = [(tool, ToolMechanism::ThoriumApi)];
    let instance = runs::submit_instance(&workflow, 1, None, &agent, harness, &tools, client)
        .await
        .expect("failed to submit a workflow instance");
    // a run of the instance, which holds a run token while it lives
    let created = client
        .reactions
        .run_workflow(
            instance.id,
            &WorkflowRunRequest::new(RunInputs::default().sample(sample)),
        )
        .await
        .expect("failed to run the workflow");
    let token = client
        .reactions
        .run_token(groups[0], created.id)
        .await
        .expect("failed to read the run's token")
        .token;
    (instance.id, created.id, token)
}
