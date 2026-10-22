//! Helpers for testing tool runs and workflow runs

use std::collections::BTreeMap;
use uuid::Uuid;

use super::generators;
use crate::models::{
    AgentConfig, AgentEntityRequest, AgenticTool, AgenticToolBacking, ContainerToolBacking,
    CredentialType, Entity, EntityMetadata, EntityMetadataRequest, EntityRequest, HarnessEnv,
    ImageScaler, ModelApi, ModelAuthSource, ModelCatalogEntryRequest, ModelEndpointEntityRequest,
    ModelMatcher, ModelProvider, ToolEntityRequest, ToolMechanism, ToolSpec,
    WorkflowCatalogEntityRequest, WorkflowInstanceEntity,
};
use crate::{Error, Thorium};

/// A ready workflow and the entities its first instance snapshots
pub struct RunWorkflow {
    /// The workflow catalog
    pub workflow: Entity,
    /// The workflow's submitted, ready instance
    pub instance: Entity,
    /// The workflow's entry agent
    pub agent: Entity,
    /// The harness tool instance the entry agent runs in
    pub harness: Entity,
    /// The tool instances the entry agent calls
    pub tools: Vec<Entity>,
    /// The keyless model endpoint in the workflow's groups its agents resolve to
    pub endpoint: Entity,
}

impl RunWorkflow {
    /// Get the id of the workflow instance
    #[must_use]
    pub fn id(&self) -> Uuid {
        self.instance.id
    }
}

/// Generate an agentic tool spec backed by an Image tool instance
///
/// # Arguments
///
/// * `image` - The Image tool instance the tool runs
#[must_use]
pub fn gen_container_spec(image: Uuid) -> ToolSpec {
    // start from a random agentic tool and back it with the image
    let ToolSpec::Agentic(tool) = generators::gen_agentic_spec() else {
        unreachable!("an agentic spec is always an Agentic tool spec")
    };
    ToolSpec::Agentic(AgenticTool {
        backing: AgenticToolBacking::Container(ContainerToolBacking {
            tool_instance: image,
            ..ContainerToolBacking::default()
        }),
        ..tool
    })
}

/// Create an Image tool that runs in an environment, returning the tool and its first instance
///
/// # Arguments
///
/// * `groups` - The groups the tool should be in
/// * `scaler` - The scaler whose environment the tool runs in
/// * `client` - The client to create the tool with
pub async fn image_tool(
    groups: &[&str],
    scaler: ImageScaler,
    client: &Thorium,
) -> Result<(Entity, Entity), Error> {
    // publish an Image tool at 1.0.0 in that environment
    generators::tool(
        groups,
        "1.0.0",
        &generators::gen_image_spec_for(scaler),
        client,
    )
    .await
}

/// Create a container tool backed by an Image tool instance, returning the tool and its first
/// instance
///
/// # Arguments
///
/// * `groups` - The groups the tool should be in
/// * `image` - The Image tool instance the tool runs
/// * `client` - The client to create the tool with
pub async fn container_tool(
    groups: &[&str],
    image: Uuid,
    client: &Thorium,
) -> Result<(Entity, Entity), Error> {
    // publish a container tool at 1.0.0
    generators::tool(groups, "1.0.0", &gen_container_spec(image), client).await
}

/// Create a ready workflow whose one agent calls some tools, and submit its first instance
///
/// Each tool is bound with the mechanism given for it.
///
/// # Arguments
///
/// * `name` - The name of the workflow
/// * `groups` - The groups to create everything in
/// * `tools` - The tool instances the agent calls and the mechanism each is bound to
/// * `client` - The client to create everything with
pub async fn ready_workflow(
    name: &str,
    groups: &[&str],
    tools: &[(&Entity, ToolMechanism)],
    client: &Thorium,
) -> Result<RunWorkflow, Error> {
    // build the workflow in a boxed future so callers' futures stay small
    Box::pin(ready_workflow_inner(name, groups, tools, client)).await
}

/// Create a ready workflow whose one agent calls some tools, and submit its first instance
///
/// # Arguments
///
/// * `name` - The name of the workflow
/// * `groups` - The groups to create everything in
/// * `tools` - The tool instances the agent calls and the mechanism each is bound to
/// * `client` - The client to create everything with
async fn ready_workflow_inner(
    name: &str,
    groups: &[&str],
    tools: &[(&Entity, ToolMechanism)],
    client: &Thorium,
) -> Result<RunWorkflow, Error> {
    // a model the agent resolves to and a harness for the agent to run in
    let endpoint = keyless_endpoint(groups, &[TEST_MODEL], client).await?;
    let (_, harness) = harness_tool(groups, &gen_model_harness_spec(), None, client).await?;
    // an agent that calls each tool by its tool root
    let mut req = AgentEntityRequest::new(generators::gen_agent_config("lead"));
    req.tools = tools
        .iter()
        .map(|(tool, _)| match &tool.metadata {
            EntityMetadata::ToolInstance(instance) => instance.tool,
            _ => tool.id,
        })
        .collect();
    let agent_name = format!("lead-{}", generators::gen_string(8).to_lowercase());
    let resp = client
        .entities
        .create(generators::gen_agent(&agent_name, groups, req))
        .await?;
    let agent = client.entities.get(resp.id).await?;
    // a workflow with that agent as its entry agent
    let workflow_req = WorkflowCatalogEntityRequest::default()
        .agent(agent.id)
        .entry_agent(agent.id)
        .auto_instance(false);
    let resp = client
        .entities
        .create(generators::gen_workflow(name, groups, workflow_req))
        .await?;
    let workflow = client.entities.get(resp.id).await?;
    // submit its first instance
    let entities = tools.iter().map(|(tool, _)| (*tool).clone()).collect();
    let instance = submit_instance(&workflow, 1, None, &agent, &harness, tools, client).await?;
    Ok(RunWorkflow {
        workflow,
        instance,
        agent,
        harness,
        tools: entities,
        endpoint,
    })
}

/// Submit a ready instance of a workflow whose one agent calls some tools
///
/// # Arguments
///
/// * `workflow` - The workflow catalog
/// * `revision` - The instance's revision
/// * `previous` - The instance this one supersedes
/// * `agent` - The entry agent
/// * `harness` - The harness tool instance the agent runs in
/// * `tools` - The tool instances the agent calls and the mechanism each is bound to
/// * `client` - The client to submit the instance with
pub async fn submit_instance(
    workflow: &Entity,
    revision: u64,
    previous: Option<Uuid>,
    agent: &Entity,
    harness: &Entity,
    tools: &[(&Entity, ToolMechanism)],
    client: &Thorium,
) -> Result<Entity, Error> {
    // snapshot the agent with its harness and tools
    let mut given = tools
        .iter()
        .map(|(tool, _)| *tool)
        .collect::<Vec<&Entity>>();
    given.push(harness);
    let mut instance =
        generators::gen_workflow_instance(workflow, revision, previous, &[(agent, &given)]);
    // bind each tool with its mechanism
    rebind(&mut instance, tools)?;
    let resp = client
        .entities
        .create(generators::gen_workflow_instance_req(&instance))
        .await?;
    client.entities.get(resp.id).await
}

/// Bind each of an instance's tools with a mechanism and recompute its digests
///
/// # Arguments
///
/// * `instance` - The instance to rebind
/// * `tools` - The tool instances and the mechanism each is bound to
fn rebind(
    instance: &mut WorkflowInstanceEntity,
    tools: &[(&Entity, ToolMechanism)],
) -> Result<(), Error> {
    // set each binding's mechanism
    for agent in &mut instance.agents {
        for binding in &mut agent.bindings {
            if let Some((_, mechanism)) = tools
                .iter()
                .find(|(tool, _)| tool.id == binding.tool_instance)
            {
                binding.mechanism = *mechanism;
            }
        }
    }
    // bindings are part of the digest
    instance.digest = instance
        .compute_digest(&BTreeMap::new())
        .map_err(|err| Error::new(err.to_string()))?;
    Ok(())
}

/// Proceed or fail a reaction through its handle route, as its creator
///
/// # Arguments
///
/// * `client` - The client of the reaction's creator, for its host
/// * `auth` - The creator's authorization header
/// * `group` - The group the reaction is in
/// * `id` - The id of the reaction
/// * `cmd` - The handle command (`proceed` or `fail`)
pub async fn handle_reaction(
    client: &Thorium,
    auth: &str,
    group: &str,
    id: Uuid,
    cmd: &str,
) -> Result<(), Error> {
    // send the handle request with the creator's auth
    let resp = reqwest::Client::new()
        .post(format!(
            "{}/api/reactions/handle/{group}/{id}/{cmd}",
            client.host
        ))
        .header("authorization", auth)
        .send()
        .await?;
    // a refused handle is an error carrying its status
    if !resp.status().is_success() {
        return Err(Error::new(format!(
            "handling reaction {id} with {cmd} returned {}",
            resp.status()
        )));
    }
    Ok(())
}

/// The model every test workflow's agents resolve to through the keyless test endpoint
pub const TEST_MODEL: &str = "thorium-test-model";

/// Generate a Harness tool spec whose harness can call the common model protocols with API keys
/// and runs every agent on one model
#[must_use]
pub fn gen_model_harness_spec() -> ToolSpec {
    // start from the standard harness and add its model capabilities
    let ToolSpec::Harness(mut harness) = generators::gen_harness_spec() else {
        unreachable!("a harness spec is always a Harness tool spec")
    };
    harness.capabilities.model_apis = [
        ModelApi::OpenAiChat,
        ModelApi::OpenAiResponses,
        ModelApi::AnthropicMessages,
    ]
    .into_iter()
    .collect();
    harness.capabilities.credential_kinds = [CredentialType::ApiKey].into_iter().collect();
    ToolSpec::Harness(harness)
}

/// Create a harness tool and its first instance, with the env values it exports
///
/// # Arguments
///
/// * `groups` - The groups the tool should be in
/// * `spec` - The harness spec of the first instance
/// * `env` - The model values the harness exports, if any
/// * `client` - The client to create the tool with
pub async fn harness_tool(
    groups: &[&str],
    spec: &ToolSpec,
    env: Option<&HarnessEnv>,
    client: &Thorium,
) -> Result<(Entity, Entity), Error> {
    // build the tool request with its env
    let mut meta = ToolEntityRequest::new("1.0.0", spec)?;
    if let Some(env) = env {
        meta = meta.harness_env(env)?;
    }
    let req = EntityRequest::new(
        format!("harness{}", generators::gen_string(12).to_lowercase()),
        EntityMetadataRequest::Tool(meta),
        groups.to_vec(),
    );
    // create it and read the tool and its first instance back
    let resp = client.entities.create(req).await?;
    let tool = client.entities.get(resp.id).await?;
    let instance_id = resp
        .tool_instance
        .ok_or_else(|| Error::new("A tool create returned no tool instance".to_owned()))?;
    let instance = client.entities.get(instance_id).await?;
    Ok((tool, instance))
}

/// Create a keyless `OpenAI` compatible model endpoint serving some models in some groups
///
/// # Arguments
///
/// * `groups` - The groups the endpoint should be in
/// * `models` - The model ids its catalog lists
/// * `client` - The client to create the endpoint with
pub async fn keyless_endpoint(
    groups: &[&str],
    models: &[&str],
    client: &Thorium,
) -> Result<Entity, Error> {
    // a uniquely addressed gateway that needs no key
    let provider = ModelProvider::OpenAiCompatible {
        base_url: format!(
            "https://{}.gateway.local/v1",
            generators::gen_string(10).to_lowercase()
        ),
        extra_headers: BTreeMap::new(),
        key_header: None,
    };
    let mut req = ModelEndpointEntityRequest::new(provider);
    for model in models {
        let mut entry = ModelCatalogEntryRequest::new(*model);
        entry.context_window = Some(200_000);
        entry.max_output_tokens = Some(8_192);
        req = req.model(entry);
    }
    generators::model_endpoint(groups, req, client).await
}

/// One agent of a test workflow: its config and the auth source on its root
pub struct ModelAgent {
    /// The agent's config
    pub config: AgentConfig,
    /// The levels the agent may take model auth from, if limited
    pub model_auth: Option<ModelAuthSource>,
}

impl ModelAgent {
    /// An agent that accepts some models in priority order
    ///
    /// # Arguments
    ///
    /// * `models` - The models it accepts
    #[must_use]
    pub fn new(models: &[&str]) -> Self {
        // a delegable agent with an exact matcher per model
        let mut config = generators::gen_agent_config(&format!(
            "agent_{}",
            generators::gen_string(8).to_lowercase()
        ));
        config.models = models
            .iter()
            .map(|model| ModelMatcher {
                model_id: (*model).to_owned(),
                ..ModelMatcher::default()
            })
            .collect();
        ModelAgent {
            config,
            model_auth: None,
        }
    }
}

/// A ready workflow of several agents sharing one harness
pub struct ModelWorkflow {
    /// The workflow catalog
    pub workflow: Entity,
    /// The workflow's submitted, ready instance
    pub instance: Entity,
    /// The workflow's agents, the entry agent first
    pub agents: Vec<Entity>,
    /// The harness tool
    pub harness_tool: Entity,
    /// The harness tool instance every agent runs in
    pub harness: Entity,
}

impl ModelWorkflow {
    /// Get the id of the workflow instance
    #[must_use]
    pub fn id(&self) -> Uuid {
        self.instance.id
    }
}

/// Create a ready workflow whose agents share one harness and call no tools
///
/// # Arguments
///
/// * `name` - The name of the workflow
/// * `groups` - The groups to create everything in
/// * `agents` - The workflow's agents, the entry agent first
/// * `workflow_auth` - The levels the workflow allows model auth from, if limited
/// * `harness` - The harness spec and the env it exports
/// * `client` - The client to create everything with
pub async fn model_workflow(
    name: &str,
    groups: &[&str],
    agents: Vec<ModelAgent>,
    workflow_auth: Option<ModelAuthSource>,
    harness: (&ToolSpec, Option<&HarnessEnv>),
    client: &Thorium,
) -> Result<ModelWorkflow, Error> {
    // the harness every agent runs in
    let (harness_tool, harness) = harness_tool(groups, harness.0, harness.1, client).await?;
    // create each agent
    let mut created = Vec::with_capacity(agents.len());
    for agent in agents {
        let mut req = AgentEntityRequest::new(agent.config);
        req.model_auth = agent.model_auth;
        let agent_name = format!("agent-{}", generators::gen_string(8).to_lowercase());
        let resp = client
            .entities
            .create(generators::gen_agent(&agent_name, groups, req))
            .await?;
        created.push(client.entities.get(resp.id).await?);
    }
    // a workflow with the first agent as its entry agent
    let mut workflow_req = WorkflowCatalogEntityRequest::default()
        .entry_agent(created[0].id)
        .auto_instance(false);
    for agent in &created {
        workflow_req = workflow_req.agent(agent.id);
    }
    if let Some(auth) = workflow_auth {
        workflow_req = workflow_req.model_auth(auth);
    }
    let resp = client
        .entities
        .create(generators::gen_workflow(name, groups, workflow_req))
        .await?;
    let workflow = client.entities.get(resp.id).await?;
    // submit an instance where every agent runs in the harness
    let given = [&harness];
    let snapshot = created
        .iter()
        .map(|agent| (agent, &given[..]))
        .collect::<Vec<(&Entity, &[&Entity])>>();
    let instance = generators::gen_workflow_instance(&workflow, 1, None, &snapshot);
    let resp = client
        .entities
        .create(generators::gen_workflow_instance_req(&instance))
        .await?;
    let instance = client.entities.get(resp.id).await?;
    Ok(ModelWorkflow {
        workflow,
        instance,
        agents: created,
        harness_tool,
        harness,
    })
}
