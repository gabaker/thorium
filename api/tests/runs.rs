//! Tests the tool run and workflow run routes

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};
use thorium::client::ResultsClient;
use thorium::models::{
    AgentEntityRequest, AgentProvenance, AiSettingsUpdate, AllowedEndpoint, Buffer, CredentialKind,
    DEFAULT_PIPELINE_SLA, Entity, EntityKinds, EntityListOpts, EntityMetadata,
    EntityMetadataRequest, EntityMetadataUpdate, EntityUpdate, EventTrigger, FileDownloadOpts,
    GenericJob, GenericJobArgs, GenericJobOpts, GroupAllowed, GroupAllowedUpdate, GroupRequest,
    GroupUpdate, GroupUsersUpdate, HarnessAgentFile, HarnessEnv, HarnessEnvVar, HarnessRunFile,
    HarnessToolEntry, ImageScaler, JobResets, JobStatus, ModelApi, ModelAuthLevel, ModelAuthSource,
    ModelBan, ModelBanKind, ModelBanUpdate, ModelCatalogEntryRequest, ModelConfigRef,
    ModelCredentialEntityRequest, ModelEndpointEntity, ModelEndpointEntityRequest,
    ModelProfileEntityRequest, ModelProvider, ModelProviderKind, ModelRejectReason,
    ModelResolutionError, ModelSecret, ModelsConfUpdate, OutputDisplayType, OutputRequest,
    PipelineRequest, PipelineUpdate, Pools, Reaction, ReactionCacheFileUpdate, ReactionCacheUpdate,
    ReactionCursorOpts, ReactionModelBinding, ReactionRequest, ReactionSource, ReactionStatus,
    ReactionUpdate, Repo, RepoDependencyRequest, Resources, ResultGetParams, RunInputs,
    RunProvenance, Sample, SampleRequest, ScopedToken, ScopedTokenRequest, ScopedTokenRole,
    StageLogsAdd, TagRequest, ToolAnnotations, ToolInvocation, ToolMechanism, ToolProvenance,
    ToolRunRequest, ToolSetEntityRequest, ToolSpec, UserCredentialRef, UserModelEndpoint,
    UserModelSettingsUpdate, UserRole, UserSettingsUpdate, UserUpdate, WorkerDeleteMap,
    WorkerRegistrationList, WorkerStatus, WorkerUpdate, WorkflowResultKey, WorkflowRunRequest,
    WorkloadEnvironment,
};
use thorium::models::{
    Association, AssociationDeleteRequest, AssociationKind, AssociationRequest, AssociationTarget,
    Directionality, ToolSetSelector,
};
use thorium::test_utilities::generators::{self, GroupRoles, TestUser};
use thorium::test_utilities::runs::{self, RunWorkflow};
use thorium::test_utilities::{self, assert_status, faults};
use thorium::{Error, Thorium, is};
use uuid::Uuid;

/// A developer for every scaler
const DEVELOPER: UserRole = UserRole::Developer {
    k8s: true,
    bare_metal: true,
    windows: true,
    external: true,
    kvm: true,
};

/// A developer for K8s only
const K8S_DEVELOPER: UserRole = UserRole::Developer {
    k8s: true,
    bare_metal: false,
    windows: false,
    external: false,
    kvm: false,
};

/// A runner and the group they own
struct Runner {
    /// The admin client
    admin: Thorium,
    /// The runner
    user: TestUser,
    /// The group the runner owns
    group: String,
    /// The suffix of this test's readable names
    suffix: String,
}

/// Create a developer named after a moon and a group named after a god that they own
///
/// # Arguments
///
/// * `moon` - The moon the runner is named after
/// * `god` - The god the group is named after
async fn runner(moon: &str, god: &str) -> Result<Runner, Error> {
    // a developer for every scaler who owns one group
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let name = generators::readable_name(moon, &suffix);
    let user = generators::named_user_with_role(&admin, &name, DEVELOPER).await?;
    let group = generators::god_group(
        &admin,
        god,
        &suffix,
        &GroupRoles::default().owner(&user.username),
    )
    .await?;
    Ok(Runner {
        admin,
        user,
        group,
        suffix,
    })
}

/// Upload a random file to a group and get its SHA256
///
/// # Arguments
///
/// * `group` - The group to upload the file to
/// * `client` - The client to upload with
async fn sample(group: &str, client: &Thorium) -> Result<String, Error> {
    // upload a random file
    let req: SampleRequest = generators::gen_sample(group);
    Ok(client.files.create(req).await?.sha256)
}

/// Get the ids of the reactions a user created in a group
///
/// # Arguments
///
/// * `user` - The user who created the reactions
/// * `group` - The group to list
async fn reactions_of(user: &TestUser, group: &str) -> Result<Vec<String>, Error> {
    // every reaction is tagged with its creator
    let opts = ReactionCursorOpts::default().group(group).limit(1000);
    let mut cursor = user
        .client
        .reactions
        .list_tag_new(&user.username, &opts)
        .await?;
    let mut ids = std::mem::take(&mut cursor.data);
    while !cursor.exhausted() {
        cursor.refill().await?;
        ids.append(&mut cursor.data);
    }
    Ok(ids)
}

/// Get a reaction's only job as an admin
///
/// # Arguments
///
/// * `admin` - The admin client
/// * `reaction` - The reaction
async fn only_job(admin: &Thorium, reaction: &Reaction) -> Result<thorium::models::RawJob, Error> {
    // runs have exactly one stage with one job
    is!(reaction.jobs.len(), 1);
    admin.jobs.get(reaction.jobs[0]).await
}

/// Read a reaction buffer as JSON
///
/// # Arguments
///
/// * `client` - The client to read with
/// * `reaction` - The reaction
/// * `name` - The name of the buffer
async fn buffer<T: serde::de::DeserializeOwned>(
    client: &Thorium,
    reaction: &Reaction,
    name: &str,
) -> Result<T, Error> {
    // download the ephemeral file and parse it
    let raw = client
        .reactions
        .download_ephemeral(&reaction.group, &reaction.id, name)
        .await?;
    serde_json::from_slice(&raw).map_err(|err| Error::new(err.to_string()))
}

/// Upload a result under a tool name the way the thorium-agent does for a run
///
/// # Arguments
///
/// * `client` - The client to upload with
/// * `sha256` - The file the result is for
/// * `tool` - The tool name to store the result under
async fn agent_result(client: &Thorium, sha256: &str, tool: &str) -> Result<(), Error> {
    // store a result named after the run's result key
    let req = OutputRequest::new(
        sha256.to_owned(),
        tool,
        format!("{{\"answer\": \"{tool}\"}}"),
        OutputDisplayType::Json,
    );
    client.files.create_result(req).await?;
    Ok(())
}

/// A ready workflow named after a god, with one Image-backed container tool bound with the CLI
///
/// # Arguments
///
/// * `runner` - The runner whose group the workflow is in
/// * `name` - The workflow's name
async fn workflow(runner: &Runner, name: &str) -> Result<RunWorkflow, Error> {
    // an Image tool and a container tool that runs it
    let groups = [runner.group.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let (_, boxed) = runs::container_tool(&groups, image.id, &runner.admin).await?;
    runs::ready_workflow(
        name,
        &groups,
        &[(&boxed, ToolMechanism::Cli)],
        &runner.admin,
    )
    .await
}

/// Get a connection to the test API's Redis database
async fn redis_conn() -> redis::aio::MultiplexedConnection {
    // reuse the harness's pool so the connection selects the configured database
    let pool = test_utilities::get_redis_client(&test_utilities::CONF).await;
    let pooled = pool.get().await.expect("a Redis connection");
    pooled.clone()
}

/// Get the namespace of the test API's keys
fn ns() -> &'static str {
    // every key is under the configured namespace
    &test_utilities::CONF.thorium.namespace
}

/// Read a reaction's data hash straight from Redis
///
/// # Arguments
///
/// * `group` - The group the reaction is in
/// * `id` - The id of the reaction
async fn reaction_hash(group: &str, id: Uuid) -> HashMap<String, String> {
    // read the whole hash the reaction is stored in
    let key = format!("{}:reaction_data:{group}:{id}", ns());
    redis::cmd("hgetall")
        .arg(key)
        .query_async(&mut redis_conn().await)
        .await
        .expect("the reaction's data hash")
}

/// Get the names of the run tokens a user holds, read straight from Redis
///
/// # Arguments
///
/// * `username` - The user whose tokens to read
async fn stored_run_tokens(username: &str) -> Vec<String> {
    // list every scoped token name in the user's hash
    let key = format!("{}:scoped_tokens:{username}", ns());
    let names: Vec<String> = redis::cmd("hkeys")
        .arg(key)
        .query_async(&mut redis_conn().await)
        .await
        .expect("the user's scoped token names");
    names
        .into_iter()
        .filter(|name| name.starts_with("run-"))
        .collect()
}

/// Get the names of the run tokens a user lists with their full token
///
/// # Arguments
///
/// * `user` - The user whose tokens to list
async fn run_tokens(user: &TestUser) -> Result<Vec<String>, Error> {
    // list the user's scoped tokens and keep the run tokens
    let tokens = user.client.users.list_scoped_tokens().await?;
    Ok(sorted_strings(
        tokens
            .into_iter()
            .map(|token| token.name)
            .filter(|name| name.starts_with("run-"))
            .collect(),
    ))
}

/// Read a run's token through the admin run-token route
///
/// # Arguments
///
/// * `admin` - The admin client
/// * `reaction` - The run's reaction
async fn run_token_of(admin: &Thorium, reaction: &Reaction) -> Result<ScopedToken, Error> {
    // the scaler and reactor read run tokens this way
    admin
        .reactions
        .run_token(&reaction.group, reaction.id)
        .await
}

/// Build a client that authenticates with a token value
///
/// # Arguments
///
/// * `admin` - The admin client, for its host
/// * `token` - The token value to authenticate with
async fn token_client(admin: &Thorium, token: &str) -> Result<Thorium, Error> {
    // build a client with the raw token
    Thorium::build(&admin.host).token(token).build().await
}

/// Register a K8s worker as an admin, the way the scaler does
///
/// # Arguments
///
/// * `admin` - The admin client
/// * `user` - The user the worker runs as
/// * `group` - The group the worker serves
/// * `tool_instance` - The tool instance the worker runs
/// * `run` - The run the worker serves, if it's a per-run worker
async fn register_worker(
    admin: &Thorium,
    user: &str,
    group: &str,
    tool_instance: Uuid,
    run: Option<Uuid>,
) -> Result<String, Error> {
    // a uniquely named worker on the test node
    let name = generators::gen_string(24);
    let list = WorkerRegistrationList::default().add(
        "cluster0",
        "node0",
        &name,
        user,
        group,
        tool_instance,
        run,
        Resources::default(),
        Pools::Deadline,
    );
    admin
        .system
        .register_workers(ImageScaler::K8s, &list)
        .await?;
    Ok(name)
}

/// Claim up to a limit of a tool instance's jobs for a worker
///
/// # Arguments
///
/// * `client` - The client to claim with
/// * `group` - The group to claim in
/// * `tool_instance` - The tool instance whose jobs to claim
/// * `worker` - The worker to claim for
async fn claim(
    client: &Thorium,
    group: &str,
    tool_instance: Uuid,
    worker: &str,
) -> Result<Vec<GenericJob>, Error> {
    // claim up to five jobs at once
    client
        .jobs
        .claim_workload(group, tool_instance, "cluster0", "node0", worker, 5)
        .await
}

/// Download a file to a scratch path and remove it again
///
/// # Arguments
///
/// * `client` - The client to download with
/// * `sha256` - The file to download
async fn downloads(client: &Thorium, sha256: &str) -> Result<(), Error> {
    // download into a unique scratch path
    let path = std::env::temp_dir().join(format!("run-token-{}", Uuid::new_v4()));
    let mut opts = FileDownloadOpts::default();
    client.files.download(sha256, &path, &mut opts).await?;
    tokio::fs::remove_file(&path).await?;
    Ok(())
}

/// Upload a result with a client and get the groups it landed in
///
/// # Arguments
///
/// * `client` - The client to upload with
/// * `admin` - The admin client to read the result back with
/// * `sha256` - The file the result is for
/// * `groups` - The groups to name, if any
async fn result_groups(
    client: &Thorium,
    admin: &Thorium,
    sha256: &str,
    groups: &[&str],
) -> Result<Vec<String>, Error> {
    // store a result under a unique tool name
    let tool = format!("probe-{}", generators::gen_string(8).to_lowercase());
    let req = OutputRequest::new(
        sha256.to_owned(),
        &tool,
        "{\"seen\": true}",
        OutputDisplayType::Json,
    )
    .groups(groups.to_vec());
    client.files.create_result(req).await?;
    // read where it landed
    let params = ResultGetParams::default().tool(&tool);
    let results = admin.files.get_results(sha256, &params).await?;
    let mut landed = results
        .results
        .get(&tool)
        .and_then(|outputs| outputs.first())
        .map_or_else(Vec::new, |output| output.groups.clone());
    landed.sort();
    Ok(landed)
}

/// Upload a file with fixed content to some groups and get its SHA256
///
/// # Arguments
///
/// * `content` - The file's content
/// * `groups` - The groups to upload it to
/// * `client` - The client to upload with
async fn sample_in(content: &str, groups: &[&str], client: &Thorium) -> Result<String, Error> {
    // upload the content to every group
    let req = SampleRequest::new_buffer(Buffer::new(content), groups.to_vec());
    Ok(client.files.create(req).await?.sha256)
}

/// Create a one stage pipeline whose stage runs a tool instance
///
/// # Arguments
///
/// * `group` - The group of the pipeline
/// * `instance` - The tool instance the stage runs
/// * `client` - The client to create the pipeline with
async fn pipe_for(group: &str, instance: Uuid, client: &Thorium) -> Result<String, Error> {
    // a single stage pinned to the instance
    let name = generators::gen_string(20).to_lowercase();
    let req = PipelineRequest::new(group, &name, serde_json::json!([["harvest"]]))
        .stage_tool("harvest", instance);
    client.pipelines.create(&req).await?;
    Ok(name)
}

/// Sort a list of strings
///
/// # Arguments
///
/// * `values` - The values to sort
fn sorted_strings<T: Into<String>>(values: Vec<T>) -> Vec<String> {
    // own and sort the values
    let mut values = values.into_iter().map(Into::into).collect::<Vec<String>>();
    values.sort();
    values
}

/// Remove a user from a group's owners and users
///
/// # Arguments
///
/// * `admin` - The admin client
/// * `group` - The group to leave
/// * `user` - The user leaving it
async fn leave(admin: &Thorium, group: &str, user: &str) -> Result<(), Error> {
    // drop the user from every role they may hold
    let update = GroupUpdate::default()
        .users(GroupUsersUpdate::default().direct_remove(user))
        .owners(GroupUsersUpdate::default().direct_remove(user));
    admin.groups.update(group, &update).await?;
    Ok(())
}

/// Start a workflow run as the runner and build a client with its run token
///
/// # Arguments
///
/// * `runner` - The runner
/// * `workflow` - The workflow to run
/// * `inputs` - The run's inputs
async fn start_run(
    runner: &Runner,
    workflow: &RunWorkflow,
    inputs: &RunInputs,
) -> Result<(Reaction, Thorium), Error> {
    // start the run and read its token
    let client = &runner.user.client.reactions;
    let created = client
        .run_workflow(workflow.id(), &WorkflowRunRequest::new(inputs.clone()))
        .await?;
    let run = client.get(&runner.group, created.id).await?;
    let token = run_token_of(&runner.admin, &run).await?;
    let scoped = token_client(&runner.admin, &token.token).await?;
    Ok((run, scoped))
}

/// Check that a user holds no run token and has no reaction beyond the ones they had before
///
/// # Arguments
///
/// * `user` - The user to check
/// * `groups` - The groups whose reactions to list
/// * `before` - The reactions the user had in each group before
async fn nothing_left(
    user: &TestUser,
    groups: &[&str],
    before: &[Vec<String>],
) -> Result<(), Error> {
    // no run token is stored or listed
    is!(stored_run_tokens(&user.username).await.len(), 0);
    is!(run_tokens(user).await?.len(), 0);
    // and no reaction was added in any group
    let mut now = Vec::with_capacity(groups.len());
    for group in groups {
        now.push(reactions_of(user, group).await?);
    }
    is!(now, before.to_vec());
    Ok(())
}

/// The secret of the group credential in the model tests
const GROUP_SECRET: &str = "sk-group-key-for-model-runs";

/// The secret of a runner's own credential in the model tests
const USER_SECRET: &str = "sk-runner-key-for-model-runs";

/// The secret a group credential is rotated to in the model tests
const ROTATED_SECRET: &str = "sk-rotated-group-key-for-model-runs";

/// Build an `OpenAI` provider at a unique url
fn openai_at_random() -> ModelProvider {
    // a unique url so whitelist changes never touch another test's endpoints
    ModelProvider::OpenAi {
        base_url: Some(format!(
            "https://{}.openai.local/v1",
            generators::gen_string(10).to_lowercase()
        )),
        organization: None,
        project: None,
    }
}

/// Build a keyless `OpenAI` compatible provider at a unique url
fn gateway_at_random() -> ModelProvider {
    // a unique url so whitelist changes never touch another test's endpoints
    ModelProvider::OpenAiCompatible {
        base_url: format!(
            "https://{}.gateway.local/v1",
            generators::gen_string(10).to_lowercase()
        ),
        extra_headers: BTreeMap::new(),
        key_header: None,
    }
}

/// Build a catalog entry with known limits
///
/// # Arguments
///
/// * `model_id` - The model id
fn catalog_entry(model_id: &str) -> ModelCatalogEntryRequest {
    // a model with known limits
    let mut entry = ModelCatalogEntryRequest::new(model_id);
    entry.context_window = Some(200_000);
    entry.max_output_tokens = Some(8_192);
    entry
}

/// Create a model endpoint serving some models
///
/// # Arguments
///
/// * `runner` - The runner whose group the endpoint is in
/// * `name` - The endpoint's name
/// * `provider` - The endpoint's provider
/// * `credential` - The group credential it authenticates with
/// * `models` - The models its catalog lists
async fn model_endpoint(
    runner: &Runner,
    name: &str,
    provider: ModelProvider,
    credential: Option<Uuid>,
    models: &[&str],
) -> Result<Entity, Error> {
    // build the endpoint with its catalog
    let mut req = ModelEndpointEntityRequest::new(provider);
    if let Some(credential) = credential {
        req = req.credential(credential);
    }
    for model in models {
        req = req.model(catalog_entry(model));
    }
    // allow it and create it with a readable name
    generators::allow_model_endpoint(&req.provider).await?;
    let mut entity = generators::gen_model_entity(
        &[runner.group.as_str()],
        EntityMetadataRequest::ModelEndpoint(req),
    );
    entity.name = format!("{name}-{}", runner.suffix);
    let resp = runner.admin.entities.create(entity).await?;
    runner.admin.entities.get(resp.id).await
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

/// Add a manual ban to a model endpoint, profile, or credential as an admin
///
/// # Arguments
///
/// * `admin` - The admin client
/// * `entity` - The model entity
/// * `msg` - Why it's banned
async fn ban_model_entity(admin: &Thorium, entity: &Entity, msg: &str) -> Result<Uuid, Error> {
    // build the update for the entity's kind
    let ban = ModelBan::generic(msg);
    let id = ban.id;
    let base = match entity.kind {
        EntityKinds::ModelEndpoint => EntityMetadataUpdate::model_endpoint(),
        EntityKinds::ModelProfile => EntityMetadataUpdate::model_profile(),
        _ => EntityMetadataUpdate::model_credential(),
    };
    let update = base.model_bans_update(ModelBanUpdate::default().add_ban(ban));
    admin
        .entities
        .update(entity.id, EntityUpdate::default().metadata(update))
        .await?;
    Ok(id)
}

/// Clear a ban from a model endpoint, profile, or credential as an admin
///
/// # Arguments
///
/// * `admin` - The admin client
/// * `entity` - The model entity
/// * `ban` - The ban to clear
async fn clear_model_ban(admin: &Thorium, entity: &Entity, ban: Uuid) -> Result<(), Error> {
    // build the update for the entity's kind
    let base = match entity.kind {
        EntityKinds::ModelEndpoint => EntityMetadataUpdate::model_endpoint(),
        EntityKinds::ModelProfile => EntityMetadataUpdate::model_profile(),
        _ => EntityMetadataUpdate::model_credential(),
    };
    let update = base.model_bans_update(ModelBanUpdate::default().remove_ban(ban));
    admin
        .entities
        .update(entity.id, EntityUpdate::default().metadata(update))
        .await?;
    Ok(())
}

/// Get the model bindings a workflow run recorded
///
/// # Arguments
///
/// * `run` - The run's reaction
fn bindings_of(run: &Reaction) -> Vec<ReactionModelBinding> {
    // only workflow runs have bindings
    match &run.source {
        ReactionSource::WorkflowRun(source) => source.models.clone(),
        _ => Vec::new(),
    }
}

/// Start a workflow run of an instance as the runner and read its reaction back
///
/// # Arguments
///
/// * `runner` - The runner
/// * `instance` - The workflow instance to run
/// * `inputs` - The run's inputs
async fn run_instance(
    runner: &Runner,
    instance: Uuid,
    inputs: &RunInputs,
) -> Result<Reaction, Error> {
    // start the run and read it back
    let client = &runner.user.client.reactions;
    let created = client
        .run_workflow(instance, &WorkflowRunRequest::new(inputs.clone()))
        .await?;
    client.get(&runner.group, created.id).await
}

/// Start a workflow run with a raw request and get its status and JSON body
///
/// # Arguments
///
/// * `runner` - The runner
/// * `instance` - The workflow instance to run
/// * `inputs` - The run's inputs
async fn raw_run(
    runner: &Runner,
    instance: Uuid,
    inputs: &RunInputs,
) -> Result<(u16, serde_json::Value), Error> {
    // send the run request as the runner
    let resp = reqwest::Client::new()
        .post(format!(
            "{}/api/workflows/instances/{instance}/runs",
            runner.admin.host
        ))
        .header("authorization", runner.user.auth_header())
        .json(&WorkflowRunRequest::new(inputs.clone()))
        .send()
        .await?;
    let status = resp.status().as_u16();
    let body = resp.json::<serde_json::Value>().await?;
    Ok((status, body))
}

/// Get a raw response body as a user
///
/// # Arguments
///
/// * `runner` - The runner whose host to use
/// * `auth` - The authorization header to send
/// * `path` - The path to get
async fn raw_get(runner: &Runner, auth: &str, path: &str) -> Result<String, Error> {
    // get the path and read its body as text
    let resp = reqwest::Client::new()
        .get(format!("{}{path}", runner.admin.host))
        .header("authorization", auth)
        .send()
        .await?;
    Ok(resp.text().await?)
}

/// List the workflow instances in a group
///
/// # Arguments
///
/// * `admin` - The admin client
/// * `group` - The group to list
async fn instances_in(admin: &Thorium, group: &str) -> Result<Vec<Uuid>, Error> {
    // list the workflow instances in the group
    let opts = EntityListOpts::default()
        .groups(vec![group.to_owned()])
        .kind(EntityKinds::WorkflowInstance)
        .limit(1_000);
    let mut found = admin
        .entities
        .list(&opts)
        .await?
        .data
        .into_iter()
        .map(|line| line.id)
        .collect::<Vec<Uuid>>();
    found.sort();
    found.dedup();
    Ok(found)
}

/// A K8s image reference pinned by digest
const PINNED_IMAGE: &str = "registry.local/sundial@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Generate an Image tool spec whose K8s workload runs an image pinned by digest
fn pinned_image_spec() -> ToolSpec {
    // a K8s Image tool with its image pinned
    let ToolSpec::Image(mut workload) = generators::gen_image_spec_for(ImageScaler::K8s) else {
        unreachable!("an image spec is always an Image tool spec")
    };
    if let WorkloadEnvironment::K8s(k8s) = &mut workload.environment {
        PINNED_IMAGE.clone_into(&mut k8s.image);
    }
    ToolSpec::Image(workload)
}

/// Get the image a tool instance's workload runs as its spec writes it, if it runs one
///
/// # Arguments
///
/// * `entity` - The tool instance entity
fn spec_image(entity: &Entity) -> Option<String> {
    // only K8s and Windows workloads of Image and Harness tools run an image
    let workload = match instance_spec(entity)? {
        ToolSpec::Image(workload) => workload,
        ToolSpec::Harness(harness) => harness.workload,
        ToolSpec::Agentic(_) => return None,
    };
    match workload.environment {
        WorkloadEnvironment::K8s(k8s) => Some(k8s.image),
        WorkloadEnvironment::Windows(windows) => Some(windows.image),
        _ => None,
    }
}

/// Build the provenance a tool instance's record should produce
///
/// # Arguments
///
/// * `entity` - The tool instance entity as the API returns it
fn expected_tool(entity: &Entity) -> ToolProvenance {
    // copy the record's root, kind, version, hash, and image
    let EntityMetadata::ToolInstance(meta) = &entity.metadata else {
        panic!("{} isn't a tool instance", entity.name);
    };
    ToolProvenance {
        tool: meta.tool,
        tool_instance: entity.id,
        name: entity.name.clone(),
        kind: meta.spec.kind(),
        version: meta.version.to_string(),
        spec_hash: meta.spec_hash.clone(),
        image: spec_image(entity),
    }
}

/// Build the provenance of some tool instances, sorted by id
///
/// # Arguments
///
/// * `entities` - The tool instance entities
fn expected_tools(entities: &[&Entity]) -> Vec<ToolProvenance> {
    // describe each instance and sort them by id
    let mut tools = entities
        .iter()
        .map(|entity| expected_tool(entity))
        .collect::<Vec<ToolProvenance>>();
    tools.sort_by_key(|tool| tool.tool_instance);
    tools
}

/// Get the provenance of a tool run's reaction
///
/// # Arguments
///
/// * `reaction` - The reaction
fn tool_provenance(reaction: &Reaction) -> Result<RunProvenance, Error> {
    // only tool runs have a tool source
    match &reaction.source {
        ReactionSource::Tool { provenance, .. } => Ok(provenance.clone()),
        other => Err(Error::new(format!("not a tool run: {other:?}"))),
    }
}

/// A workflow run's reaction records every agent with its hashes and every tool instance the run
/// runs or may call (the harness, each bound tool, and each image a container tool pins) with
/// its root, kind, version, hash, and image as written, sorted by id, in the reaction's one read
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn workflow_run_records_provenance() -> Result<(), Error> {
    // a tagged image and a pinned image behind container tools, and an agentic API tool
    let runner = runner("adrastea", "saturn").await?;
    let groups = [runner.group.as_str()];
    let (_, tagged) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let (_, pinned) =
        generators::tool(&groups, "1.0.0", &pinned_image_spec(), &runner.admin).await?;
    let (_, cli) = runs::container_tool(&groups, tagged.id, &runner.admin).await?;
    let (_, boxed) = runs::container_tool(&groups, pinned.id, &runner.admin).await?;
    let (_, api) = generators::tool(
        &groups,
        "1.0.0",
        &generators::gen_agentic_spec(),
        &runner.admin,
    )
    .await?;
    let triage = Box::pin(runs::ready_workflow(
        "Triage",
        &groups,
        &[
            (&cli, ToolMechanism::Cli),
            (&boxed, ToolMechanism::Mcp),
            (&api, ToolMechanism::ThoriumApi),
        ],
        &runner.admin,
    ))
    .await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let client = &runner.user.client.reactions;
    let created = client
        .run_workflow(
            triage.id(),
            &WorkflowRunRequest::new(RunInputs::default().sample(&sha256)),
        )
        .await?;
    // one read of the reaction carries the whole provenance
    let reaction = client.get(&runner.group, created.id).await?;
    let ReactionSource::WorkflowRun(run) = &reaction.source else {
        return Err(Error::new(format!(
            "not a workflow run: {:?}",
            reaction.source
        )));
    };
    // every agent of the instance with its hashes from the instance record
    let EntityMetadata::WorkflowInstance(instance) = &triage.instance.metadata else {
        return Err(Error::new("not a workflow instance".to_owned()));
    };
    let mut agents = instance
        .agents
        .iter()
        .map(|agent| AgentProvenance {
            agent: agent.agent,
            name: agent.name.clone(),
            config_hash: agent.config_hash.clone(),
            toolset_digest: agent.toolset_digest.clone(),
        })
        .collect::<Vec<AgentProvenance>>();
    agents.sort_by_key(|agent| agent.agent);
    is!(agents.is_empty(), false);
    is!(run.provenance.agents.clone(), agents);
    // the harness, every bound tool, and both pinned images, matching their records
    let expected = expected_tools(&[&triage.harness, &cli, &boxed, &api, &tagged, &pinned]);
    is!(run.provenance.tools.clone(), expected);
    // each tool the instance lists matches its instanced record
    for tool in &run.provenance.tools {
        if let Some(instanced) = instance
            .tools
            .iter()
            .find(|instanced| instanced.tool_instance == tool.tool_instance)
        {
            is!(instanced.tool, tool.tool);
            is!(instanced.kind, tool.kind);
            is!(instanced.version.clone(), tool.version.clone());
            is!(instanced.spec_hash.clone(), tool.spec_hash.clone());
        }
    }
    // a tag stays a tag, a pinned reference stays pinned, and agentic tools have no image
    let image_of = |id: Uuid| run.provenance.tool(id).and_then(|tool| tool.image.clone());
    let tag = image_of(tagged.id).unwrap_or_default();
    let pinned_tag = tag.contains("@sha256:");
    is!(pinned_tag, false);
    is!(Some(tag.clone()), spec_image(&tagged));
    is!(image_of(pinned.id), Some(PINNED_IMAGE.to_owned()));
    is!(image_of(triage.harness.id).is_some(), true);
    is!(image_of(triage.harness.id), spec_image(&triage.harness));
    for agentic in [&cli, &boxed, &api] {
        is!(image_of(agentic.id), None::<String>);
    }
    // the harness and entry agent are found by the ids the run already holds
    is!(run.provenance.tool(run.harness).is_some(), true);
    is!(
        run.provenance
            .agents
            .iter()
            .any(|agent| agent.agent == run.agent),
        true
    );
    // the stored source holds the same provenance
    let stored = reaction_hash(&runner.group, reaction.id).await;
    let source: ReactionSource =
        serde_json::from_str(&stored["source"]).map_err(|err| Error::new(err.to_string()))?;
    is!(source.provenance(), &run.provenance);
    Ok(())
}

/// A tool run records the tool instance it runs, plus the Image tool instance a container tool
/// pins, with their images as written; a harness's tool call records its container tool and
/// image too
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn tool_run_records_provenance() -> Result<(), Error> {
    // Image tools in three environments and a container tool pinning the K8s one
    let runner = runner("thebe", "pluto").await?;
    let groups = [runner.group.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let (_, windows) = runs::image_tool(&groups, ImageScaler::Windows, &runner.admin).await?;
    let (_, bare) = runs::image_tool(&groups, ImageScaler::BareMetal, &runner.admin).await?;
    let (_, boxed) = runs::container_tool(&groups, image.id, &runner.admin).await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let inputs = RunInputs::default().sample(&sha256);
    let client = &runner.user.client.reactions;
    // an Image tool run records only its instance, with its image or none
    for entity in [&image, &windows, &bare] {
        let created = client
            .run_tool(entity.id, &ToolRunRequest::new(inputs.clone()))
            .await?;
        let reaction = client.get(&runner.group, created.id).await?;
        let provenance = tool_provenance(&reaction)?;
        is!(provenance.agents.len(), 0);
        is!(provenance.tools.clone(), expected_tools(&[entity]));
    }
    let windows_image = spec_image(&windows);
    is!(windows_image.is_some(), true);
    is!(spec_image(&bare), None::<String>);
    // a container tool run records the container tool and the image it pins
    let created = client
        .run_tool(boxed.id, &ToolRunRequest::new(inputs.clone()))
        .await?;
    let reaction = client.get(&runner.group, created.id).await?;
    let provenance = tool_provenance(&reaction)?;
    is!(provenance.tools.clone(), expected_tools(&[&boxed, &image]));
    is!(
        provenance
            .tool(image.id)
            .and_then(|tool| tool.image.clone()),
        spec_image(&image)
    );
    is!(
        provenance
            .tool(boxed.id)
            .and_then(|tool| tool.image.clone()),
        None::<String>
    );
    // a harness's tool call of the container tool records the same
    let triage = Box::pin(runs::ready_workflow(
        "Triage",
        &groups,
        &[(&boxed, ToolMechanism::Cli)],
        &runner.admin,
    ))
    .await?;
    let (run, scoped) = start_run(&runner, &triage, &inputs).await?;
    let call = scoped
        .reactions
        .run_tool(boxed.id, &ToolRunRequest::new(RunInputs::default()))
        .await?;
    let call = client.get(&runner.group, call.id).await?;
    is!(call.parent, Some(run.id));
    is!(
        tool_provenance(&call)?.tools,
        expected_tools(&[&boxed, &image])
    );
    Ok(())
}

/// A pipeline reaction records the tool instance of every planned stage, including a stage that
/// follows its tool's latest instance, built from the reads its plan resolution made, so a bulk
/// create reads each tool instance once
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn pipeline_reaction_records_provenance() -> Result<(), Error> {
    // a pipeline with a pinned K8s stage, a stage following a tool, and a pinned Windows stage
    let runner = runner("metis", "ceres").await?;
    let groups = [runner.group.as_str()];
    let (_, plant) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let (water_tool, water) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let (_, harvest) = runs::image_tool(&groups, ImageScaler::Windows, &runner.admin).await?;
    let name = generators::gen_string(20).to_lowercase();
    let req = PipelineRequest::new(
        &runner.group,
        &name,
        serde_json::json!([["plant", "water"], ["harvest"]]),
    )
    .stage_tool("plant", plant.id)
    .stage_tool("water", water_tool.id)
    .stage_tool("harvest", harvest.id);
    runner.user.client.pipelines.create(&req).await?;
    let expected = expected_tools(&[&plant, &water, &harvest]);
    // a single reaction records every planned stage's tool instance
    let client = &runner.user.client.reactions;
    let created = client
        .create(&ReactionRequest::new(&runner.group, &name))
        .await?;
    let reaction = client.get(&runner.group, created.id).await?;
    let ReactionSource::Pipeline { provenance, .. } = &reaction.source else {
        return Err(Error::new(format!(
            "not a pipeline reaction: {:?}",
            reaction.source
        )));
    };
    is!(provenance.agents.len(), 0);
    is!(provenance.tools.clone(), expected.clone());
    let recorded = provenance
        .tools
        .iter()
        .map(|tool| tool.tool_instance)
        .collect::<Vec<Uuid>>();
    is!(recorded, reaction.plan.tool_instances());
    // a bulk create of three records the same on each and reads each instance only once
    let ids = [plant.id, water.id, harvest.id];
    let before = ids
        .iter()
        .map(|id| test_utilities::faults::entity_reads(*id))
        .collect::<Vec<u64>>();
    let reqs = (0..3)
        .map(|_| ReactionRequest::new(&runner.group, &name))
        .collect::<Vec<ReactionRequest>>();
    let bulk = client.create_bulk(&reqs).await?;
    is!(bulk.created.len(), 3);
    let after = ids
        .iter()
        .map(|id| test_utilities::faults::entity_reads(*id))
        .collect::<Vec<u64>>();
    for (before, after) in before.iter().zip(&after) {
        is!(after - before, 1);
    }
    for id in &bulk.created {
        let reaction = client.get(&runner.group, *id).await?;
        is!(reaction.source.provenance().tools.clone(), expected.clone());
    }
    Ok(())
}

/// When a run ends, it expires the run queues of the images its tool calls ran, not only the
/// queues of its own plan
#[tokio::test]
async fn run_end_expires_tool_call_queues() -> Result<(), Error> {
    // a workflow run whose harness calls a container tool once, and the test node
    let runner = runner("leda", "vesta").await?;
    generators::node("cluster0", "node0", Resources::default(), &runner.admin).await?;
    let groups = [runner.group.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let (_, boxed) = runs::container_tool(&groups, image.id, &runner.admin).await?;
    let triage = Box::pin(runs::ready_workflow(
        "Triage",
        &groups,
        &[(&boxed, ToolMechanism::Cli)],
        &runner.admin,
    ))
    .await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let (run, scoped) = start_run(&runner, &triage, &RunInputs::default().sample(&sha256)).await?;
    scoped
        .reactions
        .run_tool(boxed.id, &ToolRunRequest::new(RunInputs::default()))
        .await?;
    // the call's job is claimed by the run's worker for the image
    let user = runner.user.username.as_str();
    let worker =
        register_worker(&runner.admin, user, &runner.group, image.id, Some(run.id)).await?;
    is!(
        claim(&scoped, &runner.group, image.id, &worker)
            .await?
            .len(),
        1
    );
    // the call runs on the run's queue for the image, which doesn't expire yet
    let queue = format!(
        "{}:run_queue:{}:{}:{}:Running",
        ns(),
        runner.group,
        run.id,
        image.id
    );
    let ttl = |key: String| async move {
        let ttl: i64 = redis::cmd("ttl")
            .arg(key)
            .query_async(&mut redis_conn().await)
            .await
            .expect("a ttl");
        ttl
    };
    is!(ttl(queue.clone()).await, -1);
    // ending the run expires the image's run queue along with its own
    let auth = runner.user.auth_header();
    runs::handle_reaction(&runner.user.client, &auth, &runner.group, run.id, "fail").await?;
    assert!(ttl(queue).await > 0, "the tool call's run queue has no ttl");
    generators::delete_worker(&worker, &runner.admin).await?;
    Ok(())
}

/// The client reads the entities a reaction's provenance names in order, a user outside the
/// reaction's group reads neither the reaction nor its entities, and a reaction stored without
/// a provenance reads with an empty one and sends no request
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn provenance_entities_follow_provenance() -> Result<(), Error> {
    // a workflow calling a container tool, and a pipeline running the tool's image
    let runner = runner("sinope", "mercury").await?;
    let groups = [runner.group.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let (_, boxed) = runs::container_tool(&groups, image.id, &runner.admin).await?;
    let triage = Box::pin(runs::ready_workflow(
        "Triage",
        &groups,
        &[(&boxed, ToolMechanism::Cli)],
        &runner.admin,
    ))
    .await?;
    let pipeline = pipe_for(&runner.group, image.id, &runner.user.client).await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let inputs = RunInputs::default().sample(&sha256);
    let client = &runner.user.client.reactions;
    // a workflow run's entities are its instance, then each tool in provenance order
    let created = client
        .run_workflow(triage.id(), &WorkflowRunRequest::new(inputs.clone()))
        .await?;
    let run = client.get(&runner.group, created.id).await?;
    let entities = client.provenance_entities(&run).await?;
    let mut expected = vec![triage.id()];
    expected.extend(
        run.source
            .provenance()
            .tools
            .iter()
            .map(|tool| tool.tool_instance),
    );
    is!(expected.len(), 4);
    is!(
        entities
            .iter()
            .map(|entity| entity.id)
            .collect::<Vec<Uuid>>(),
        expected
    );
    is!(entities[0].kind, EntityKinds::WorkflowInstance);
    // a tool run's entities are the container tool and its image
    let created = client
        .run_tool(boxed.id, &ToolRunRequest::new(inputs.clone()))
        .await?;
    let tool_run = client.get(&runner.group, created.id).await?;
    let entities = client.provenance_entities(&tool_run).await?;
    let mut ids = vec![boxed.id, image.id];
    ids.sort();
    is!(
        entities
            .iter()
            .map(|entity| entity.id)
            .collect::<Vec<Uuid>>(),
        ids
    );
    // a pipeline reaction's entities are its planned stages' tool instances
    let created = client
        .create(&ReactionRequest::new(&runner.group, &pipeline).samples(vec![sha256.clone()]))
        .await?;
    let reaction = client.get(&runner.group, created.id).await?;
    let entities = client.provenance_entities(&reaction).await?;
    is!(
        entities
            .iter()
            .map(|entity| entity.id)
            .collect::<Vec<Uuid>>(),
        reaction.plan.tool_instances()
    );
    // a user in another group can't read the run or any entity it names
    let outsider = generators::moon_user(&runner.admin, "carpo", &runner.suffix).await?;
    generators::god_group(
        &runner.admin,
        "bacchus",
        &runner.suffix,
        &GroupRoles::default().owner(&outsider.username),
    )
    .await?;
    let hidden = outsider.client.reactions.get(&runner.group, run.id).await;
    is!(hidden.is_err(), true);
    let leaked = outsider.client.reactions.provenance_entities(&run).await;
    let Err(err) = leaked else {
        return Err(Error::new(
            "an outsider read a run's provenance entities".to_owned(),
        ));
    };
    is!(err.status().map(|code| code.as_u16()), Some(404));
    // a reaction stored before provenance was recorded reads with an empty one
    let key = format!("{}:reaction_data:{}:{}", ns(), runner.group, reaction.id);
    let old = serde_json::json!({ "Pipeline": { "name": pipeline } }).to_string();
    let _: () = redis::cmd("hset")
        .arg(&key)
        .arg("source")
        .arg(old)
        .query_async(&mut redis_conn().await)
        .await
        .expect("store an older source");
    let read = client.get(&runner.group, reaction.id).await?;
    is!(read.source.provenance().is_empty(), true);
    // reading its entities sends no request at all
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a local listener");
    let addr = listener.local_addr().expect("the listener's address");
    let accepted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = accepted.clone();
    let server = tokio::spawn(async move {
        while listener.accept().await.is_ok() {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    });
    let offline = thorium::client::Reactions::new(
        &format!("http://{addr}"),
        "token",
        &reqwest::Client::new(),
    );
    let entities = offline.provenance_entities(&read).await?;
    is!(entities.len(), 0);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    is!(accepted.load(std::sync::atomic::Ordering::SeqCst), 0);
    server.abort();
    Ok(())
}

/// A workflow run binds each agent to the model it resolved to, records the bindings without
/// secrets on the reaction and in its run file, and the admin run models read delivers them
/// with their current secrets and the entry agent's env; a banned credential can't be
/// delivered, its refusal naming the ban's kind but not its detail, and the next run binds the
/// runner's own key
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn run_models_bound_and_delivered() -> Result<(), Error> {
    // a group OpenAI endpoint with a group key, the runner's own key for it, and a workflow
    let runner = runner("thebe", "janus").await?;
    let groups = [runner.group.as_str()];
    let group_cred = generators::model_credential(
        &groups,
        CredentialKind::ApiKey,
        Some(GROUP_SECRET),
        &runner.admin,
    )
    .await?;
    let endpoint = model_endpoint(
        &runner,
        "corn-openai",
        openai_at_random(),
        Some(group_cred.id),
        &["gpt-5"],
    )
    .await?;
    let user_cred = UserCredentialRef::new(CredentialKind::ApiKey, Some(USER_SECRET.into()));
    set_models(
        &runner.user.client,
        UserModelSettingsUpdate::default().credential(endpoint.id, user_cred.clone()),
    )
    .await?;
    let env = HarnessEnv {
        export: [HarnessEnvVar::OpenAiApiKey, HarnessEnvVar::ModelId]
            .into_iter()
            .collect(),
        names: BTreeMap::new(),
    };
    let flow = runs::model_workflow(
        "Triage",
        &groups,
        vec![runs::ModelAgent::new(&["gpt-5"])],
        None,
        (&runs::gen_model_harness_spec(), Some(&env)),
        &runner.admin,
    )
    .await?;
    let agent = flow.agents[0].id;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let inputs = RunInputs::default().sample(&sha256);
    // the run binds the agent to the group endpoint and key at the group level
    let run = run_instance(&runner, flow.id(), &inputs).await?;
    let first = bindings_of(&run);
    is!(
        first,
        vec![ReactionModelBinding {
            agent,
            priority: Some(0),
            level: ModelAuthLevel::Group,
            endpoint: ModelConfigRef::Group(endpoint.id),
            model_id: "gpt-5".to_owned(),
            api: ModelApi::OpenAiChat,
            profile: None,
            credential: Some(ModelConfigRef::Group(group_cred.id)),
        }]
    );
    // the run file carries the same choices
    let run_file: HarnessRunFile = buffer(&runner.user.client, &run, "run.json").await?;
    is!(run_file.models, first.clone());
    // no response holds a secret
    let auth = runner.user.auth_header();
    let get = raw_get(
        &runner,
        &auth,
        &format!("/api/reactions/{}/{}", run.group, run.id),
    )
    .await?;
    let listed = raw_get(
        &runner,
        &auth,
        &format!("/api/reactions/tag/{}/WorkflowRun/details/", run.group),
    )
    .await?;
    let raw_file = runner
        .user
        .client
        .reactions
        .download_ephemeral(&run.group, &run.id, "run.json")
        .await?;
    for body in [get, listed, String::from_utf8_lossy(&raw_file).to_string()] {
        is!(
            body.contains(GROUP_SECRET) || body.contains(USER_SECRET),
            false
        );
    }
    // the admin reads the models with the group key and the entry agent's env
    let models = runner
        .admin
        .reactions
        .run_models(&run.group, run.id)
        .await?;
    let entry = &models.models.agents[&agent];
    is!(entry.model_id.clone(), "gpt-5".to_owned());
    is!(entry.api, ModelApi::OpenAiChat);
    is!(entry.provider.kind(), ModelProviderKind::OpenAi);
    let Some(credential) = &entry.credential else {
        return Err(Error::new("the bound key wasn't delivered".to_owned()));
    };
    is!(credential.id, ModelConfigRef::Group(group_cred.id));
    is!(
        credential
            .secrets
            .primary
            .as_ref()
            .map(|secret| secret.expose().to_owned()),
        Some(GROUP_SECRET.to_owned())
    );
    is!(
        models.env.keys().cloned().collect::<Vec<String>>(),
        vec!["OPENAI_API_KEY".to_owned(), "THORIUM_MODEL".to_owned()]
    );
    is!(models.env["OPENAI_API_KEY"].expose(), GROUP_SECRET);
    is!(models.env["THORIUM_MODEL"].expose(), "gpt-5");
    // the runner and the run's own token can't read them
    assert_status(
        runner
            .user
            .client
            .reactions
            .run_models(&run.group, run.id)
            .await,
        401,
        "the runner read the run's models",
    )?;
    let token = run_token_of(&runner.admin, &run).await?;
    let scoped = token_client(&runner.admin, &token.token).await?;
    assert_status(
        scoped.reactions.run_models(&run.group, run.id).await,
        401,
        "the run's token read the run's models",
    )?;
    // a direct tool run has no model bindings
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let created = runner
        .user
        .client
        .reactions
        .run_tool(image.id, &ToolRunRequest::new(inputs.clone()))
        .await?;
    assert_status(
        runner
            .admin
            .reactions
            .run_models(&runner.group, created.id)
            .await,
        404,
        "a tool run had model bindings",
    )?;
    // a pipeline reaction and an unknown reaction have none either
    let pipe = pipe_for(&runner.group, image.id, &runner.user.client).await?;
    let pipeline = runner
        .user
        .client
        .reactions
        .create(&ReactionRequest::new(&runner.group, &pipe).sample(&sha256))
        .await?;
    assert_status(
        runner
            .admin
            .reactions
            .run_models(&runner.group, pipeline.id)
            .await,
        404,
        "a pipeline reaction had model bindings",
    )?;
    assert_status(
        runner
            .admin
            .reactions
            .run_models(&runner.group, Uuid::new_v4())
            .await,
        404,
        "an unknown reaction had model bindings",
    )?;
    // a rotated group key is delivered at the next read
    runner
        .admin
        .entities
        .update(
            group_cred.id,
            EntityUpdate::default().metadata(
                EntityMetadataUpdate::model_credential().credential_secret(ROTATED_SECRET),
            ),
        )
        .await?;
    let models = runner
        .admin
        .reactions
        .run_models(&run.group, run.id)
        .await?;
    is!(models.env["OPENAI_API_KEY"].expose(), ROTATED_SECRET);
    // a banned group key can't be delivered, and the refusal (which the scaler copies into the
    // run's stage logs) names the ban's kind but never its detail
    let cred_ban = ban_model_entity(&runner.admin, &group_cred, "revoked key sk-...wxyz").await?;
    let msg = conflict_msg(
        runner.admin.reactions.run_models(&run.group, run.id).await,
        "a banned key was delivered",
    )?;
    is!(
        msg.contains(&format!(
            "model credential {} is banned (Generic)",
            group_cred.id
        )),
        true
    );
    is!(msg.contains("revoked"), false);
    is!(msg.contains("wxyz"), false);
    // the next run binds the runner's own key
    is!(
        bindings_of(&runner.user.client.reactions.get(&run.group, run.id).await?),
        first.clone()
    );
    let second = run_instance(&runner, flow.id(), &inputs).await?;
    let bound = bindings_of(&second);
    is!(bound.len(), 1);
    is!(bound[0].level, ModelAuthLevel::User);
    is!(bound[0].endpoint, ModelConfigRef::Group(endpoint.id));
    is!(
        bound[0].credential,
        Some(ModelConfigRef::User(user_cred.id))
    );
    // both runs are of the same instance digest
    let digest = |reaction: &Reaction| match &reaction.source {
        ReactionSource::WorkflowRun(source) => source.digest.clone(),
        _ => String::new(),
    };
    is!(digest(&run), digest(&second));
    let models = runner
        .admin
        .reactions
        .run_models(&second.group, second.id)
        .await?;
    is!(models.env["OPENAI_API_KEY"].expose(), USER_SECRET);
    // an endpoint ban leaves the second run's models deliverable
    ban_model_entity(&runner.admin, &endpoint, "maintenance").await?;
    runner
        .admin
        .reactions
        .run_models(&second.group, second.id)
        .await?;
    // clearing the key's ban lets the first run's models be delivered with the current secret
    clear_model_ban(&runner.admin, &group_cred, cred_ban).await?;
    let models = runner
        .admin
        .reactions
        .run_models(&run.group, run.id)
        .await?;
    is!(models.env["OPENAI_API_KEY"].expose(), ROTATED_SECRET);
    // once the run's token is deleted it has no models to read
    runner
        .user
        .client
        .users
        .delete_scoped_token(&format!("run-{}", second.id))
        .await?;
    assert_status(
        runner
            .admin
            .reactions
            .run_models(&second.group, second.id)
            .await,
        404,
        "a run without a token had models",
    )?;
    Ok(())
}

/// Get the message of a run models read the API refused with `409`
///
/// # Arguments
///
/// * `result` - The run models read
/// * `context` - What a success or another status would mean
fn conflict_msg<T>(result: Result<T, Error>, context: &str) -> Result<String, Error> {
    // only a `409` carries the refusal
    match result {
        Ok(_) => Err(Error::new(format!("{context}: the read succeeded"))),
        Err(error) if error.status() == Some(reqwest::StatusCode::CONFLICT) => {
            Ok(error.msg().unwrap_or_default())
        }
        Err(error) => Err(Error::new(format!("{context}: {error}"))),
    }
}

/// A queued run's models aren't delivered once its endpoint leaves the deployment's allowed
/// list, before any scan bans it, or while it carries the `NotAllowedByDeployment` ban a scan
/// writes, and are delivered again once it's allowed again; the run keeps its bindings, and the
/// next run binds an endpoint the deployment still allows
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn run_models_refuse_endpoints_the_deployment_no_longer_allows() -> Result<(), Error> {
    // a keyless gateway serving one model and a workflow whose agent calls it
    let runner = runner("carme", "terminus").await?;
    let groups = [runner.group.as_str()];
    let gone_provider = gateway_at_random();
    let gone = model_endpoint(&runner, "a-gone", gone_provider.clone(), None, &["m"]).await?;
    let flow = runs::model_workflow(
        "Allowed",
        &groups,
        vec![runs::ModelAgent::new(&["m"])],
        None,
        (&runs::gen_model_harness_spec(), None),
        &runner.admin,
    )
    .await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let inputs = RunInputs::default().sample(&sha256);
    // the run is queued bound to the gateway, and its models are delivered
    let run = run_instance(&runner, flow.id(), &inputs).await?;
    let bound = bindings_of(&run);
    is!(bound[0].endpoint, ModelConfigRef::Group(gone.id));
    runner
        .admin
        .reactions
        .run_models(&run.group, run.id)
        .await?;
    // removing the gateway from the allowed list stops delivery before any scan bans it
    let entry = AllowedEndpoint::for_provider(&gone_provider);
    generators::update_model_whitelist(
        ModelsConfUpdate::default().remove_endpoint(entry.clone()),
        false,
    )
    .await?;
    let msg = conflict_msg(
        runner.admin.reactions.run_models(&run.group, run.id).await,
        "an endpoint off the allowed list was delivered",
    )?;
    is!(
        msg.contains(&format!(
            "model endpoint {} is no longer on the deployment's allowed endpoint list",
            gone.id
        )),
        true
    );
    // the run keeps its bindings
    is!(
        bindings_of(&runner.user.client.reactions.get(&run.group, run.id).await?),
        bound.clone()
    );
    // allowing it again delivers the run's models
    generators::update_model_whitelist(
        ModelsConfUpdate::default().add_endpoint(entry.clone()),
        false,
    )
    .await?;
    runner
        .admin
        .reactions
        .run_models(&run.group, run.id)
        .await?;
    // the ban a scan writes stops delivery too, even while the list allows the endpoint (an admin
    // adds it here, since a scan reads every endpoint and would skew the read counts other tests
    // in this binary take)
    let scan_ban = ModelBan::new(ModelBanKind::NotAllowedByDeployment);
    let scan_ban_id = scan_ban.id;
    let update = EntityMetadataUpdate::model_endpoint()
        .model_bans_update(ModelBanUpdate::default().add_ban(scan_ban));
    runner
        .admin
        .entities
        .update(gone.id, EntityUpdate::default().metadata(update))
        .await?;
    let msg = conflict_msg(
        runner.admin.reactions.run_models(&run.group, run.id).await,
        "an endpoint the scan banned was delivered",
    )?;
    is!(
        msg.contains(&format!(
            "model endpoint {} is banned (NotAllowedByDeployment)",
            gone.id
        )),
        true
    );
    // the next run binds a gateway the deployment still allows, and gets its models
    let kept = model_endpoint(&runner, "b-kept", gateway_at_random(), None, &["m"]).await?;
    let next = run_instance(&runner, flow.id(), &inputs).await?;
    is!(
        bindings_of(&next)[0].endpoint,
        ModelConfigRef::Group(kept.id)
    );
    runner
        .admin
        .reactions
        .run_models(&next.group, next.id)
        .await?;
    // clearing the ban delivers the first run's models again
    clear_model_ban(&runner.admin, &gone, scan_ban_id).await?;
    runner
        .admin
        .reactions
        .run_models(&run.group, run.id)
        .await?;
    Ok(())
}

/// A run that can't resolve its agents' models is refused with `400` before anything is
/// created: its text lists every allowed model with a reason per level, its body parses as a
/// `ModelResolutionError`, a harness running every agent on one model refuses agents on
/// different models, and bans, rotations, and disabled ids create no instance
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn runs_refused_for_models() -> Result<(), Error> {
    // a banned Anthropic endpoint serving claude and no endpoint serving gpt-5
    let runner = runner("pasiphae", "volturnus").await?;
    let groups = [runner.group.as_str()];
    let cred = generators::model_credential(
        &groups,
        CredentialKind::ApiKey,
        Some(GROUP_SECRET),
        &runner.admin,
    )
    .await?;
    let provider = ModelProvider::Anthropic {
        base_url: Some(format!(
            "https://{}.anthropic.local",
            generators::gen_string(10).to_lowercase()
        )),
        version: "2023-06-01".to_owned(),
        betas: std::collections::BTreeSet::new(),
        workspace_id: None,
    };
    let endpoint = model_endpoint(
        &runner,
        "corn-anthropic",
        provider,
        Some(cred.id),
        &["claude"],
    )
    .await?;
    ban_model_entity(&runner.admin, &endpoint, "maintenance").await?;
    // two agents that can't be resolved
    let flow = runs::model_workflow(
        "Triage",
        &groups,
        vec![
            runs::ModelAgent::new(&["claude", "gpt-5"]),
            runs::ModelAgent::new(&["gpt-5"]),
        ],
        None,
        (&runs::gen_model_harness_spec(), None),
        &runner.admin,
    )
    .await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let inputs = RunInputs::default().sample(&sha256);
    let before = vec![reactions_of(&runner.user, &runner.group).await?];
    let instances = instances_in(&runner.admin, &runner.group).await?;
    // the run is refused with every agent's reasons in agent order
    let (status, body) = raw_run(&runner, flow.id(), &inputs).await?;
    is!(status, 400);
    let error: ModelResolutionError = serde_json::from_value(body.clone())?;
    let EntityMetadata::WorkflowInstance(instance) = &flow.instance.metadata else {
        return Err(Error::new("not a workflow instance".to_owned()));
    };
    let order = instance
        .agents
        .iter()
        .map(|agent| agent.agent)
        .collect::<Vec<Uuid>>();
    is!(
        error
            .agents
            .iter()
            .map(|agent| agent.agent)
            .collect::<Vec<Uuid>>(),
        order
    );
    // the lead agent's candidates are both models at every level, in order
    let lead = error
        .agents
        .iter()
        .find(|agent| agent.agent == flow.agents[0].id)
        .ok_or_else(|| Error::new("the lead agent wasn't reported".to_owned()))?;
    let tried = lead
        .candidates
        .iter()
        .map(|candidate| {
            (
                candidate
                    .matcher
                    .as_ref()
                    .map(|matcher| matcher.model_id.clone()),
                candidate.level,
            )
        })
        .collect::<Vec<_>>();
    is!(tried.len(), 8);
    is!(
        tried[0],
        (Some("claude".to_owned()), Some(ModelAuthLevel::Harness))
    );
    is!(
        tried[4],
        (Some("gpt-5".to_owned()), Some(ModelAuthLevel::Harness))
    );
    is!(
        lead.candidates[2].reason.clone(),
        ModelRejectReason::Banned(ModelBanKind::Generic("maintenance".to_owned()))
    );
    // the text lists every allowed model in order with a reason per level
    let text = body["error"].as_str().unwrap_or_default().to_owned();
    let lead_name = &flow.agents[0].name;
    for line in [
        format!("No usable model for agent \"{lead_name}\" in "),
        "Allowed models, in priority order:".to_owned(),
        "  1. claude".to_owned(),
        "     - harness, workflow: reserved, no auth source yet".to_owned(),
        format!(
            "     - group {}: endpoint \"corn-anthropic-{}\" is banned (Generic: maintenance)",
            runner.group, runner.suffix
        ),
        format!(
            "     - user: you have no usable credential for endpoint \"corn-anthropic-{}\"",
            runner.suffix
        ),
        "  2. gpt-5".to_owned(),
        "     - group ".to_owned(),
    ] {
        is!(text.contains(&line), true, line);
    }
    // nothing was created
    nothing_left(&runner.user, &groups, &before).await?;
    is!(
        instances_in(&runner.admin, &runner.group).await?,
        instances.clone()
    );
    // a harness that runs every agent on one model refuses agents on different models
    let gateway = runs::keyless_endpoint(&groups, &["one", "two"], &runner.admin).await?;
    let mixed = runs::model_workflow(
        "Mixed",
        &groups,
        vec![
            runs::ModelAgent::new(&["one"]),
            runs::ModelAgent::new(&["two"]),
        ],
        None,
        (&runs::gen_model_harness_spec(), None),
        &runner.admin,
    )
    .await?;
    let (status, body) = raw_run(&runner, mixed.id(), &inputs).await?;
    is!(status, 400);
    let error: ModelResolutionError = serde_json::from_value(body.clone())?;
    is!(error.agents.len(), 2);
    for agent in &error.agents {
        is!(agent.candidates.len(), 1);
        is!(
            agent.candidates[0].reason.clone(),
            ModelRejectReason::MixedModelsUnsupported
        );
        is!(
            agent.candidates[0].endpoint,
            Some(ModelConfigRef::Group(gateway.id))
        );
    }
    let text = body["error"].as_str().unwrap_or_default();
    for agent in &mixed.agents {
        is!(text.contains(&agent.name), true);
    }
    is!(text.contains("one") && text.contains("two"), true);
    nothing_left(&runner.user, &groups, &before).await?;
    // disabling a model on the group endpoint resolves it from the runner's own endpoint
    let instances = instances_in(&runner.admin, &runner.group).await?;
    let single = runs::model_workflow(
        "Single",
        &groups,
        vec![runs::ModelAgent::new(&["one"])],
        None,
        (&runs::gen_model_harness_spec(), None),
        &runner.admin,
    )
    .await?;
    let instances = {
        let mut all = instances;
        all.push(single.id());
        all.sort();
        all
    };
    let own = Uuid::new_v4();
    let own_provider = gateway_at_random();
    generators::allow_model_endpoint(&own_provider).await?;
    set_models(
        &runner.user.client,
        UserModelSettingsUpdate::default().endpoint(
            own,
            UserModelEndpoint::new("mine", own_provider).model(catalog_entry("one")),
        ),
    )
    .await?;
    runner
        .admin
        .entities
        .update(
            gateway.id,
            EntityUpdate::default()
                .metadata(EntityMetadataUpdate::model_endpoint().disable_model("one")),
        )
        .await?;
    let run = run_instance(&runner, single.id(), &inputs).await?;
    let bound = bindings_of(&run);
    is!(bound[0].level, ModelAuthLevel::User);
    is!(bound[0].endpoint, ModelConfigRef::User(own));
    // disabling made no notification, and bans and rotations made no instance
    let notes = runner
        .admin
        .entities
        .notifications::<ModelEndpointEntity>(gateway.id)
        .await?;
    is!(notes.len(), 0);
    runner
        .admin
        .entities
        .update(
            cred.id,
            EntityUpdate::default().metadata(
                EntityMetadataUpdate::model_credential().credential_secret(ROTATED_SECRET),
            ),
        )
        .await?;
    ban_model_entity(&runner.admin, &gateway, "down").await?;
    is!(instances_in(&runner.admin, &runner.group).await?, instances);
    Ok(())
}

/// Retrying a failed run resolves its models again: after the bound endpoint is banned the
/// retry binds the next one, with no usable endpoint left it's refused with nothing created,
/// and banning a run's bound endpoint leaves its bindings and models as they were
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn retry_resolves_models_again() -> Result<(), Error> {
    // two keyless endpoints serving one model, alpha first by name
    let runner = runner("carme", "portunus").await?;
    let groups = [runner.group.as_str()];
    let alpha = model_endpoint(&runner, "alpha", gateway_at_random(), None, &["m"]).await?;
    let beta = model_endpoint(&runner, "beta", gateway_at_random(), None, &["m"]).await?;
    let flow = runs::model_workflow(
        "Retry",
        &groups,
        vec![runs::ModelAgent::new(&["m"])],
        None,
        (&runs::gen_model_harness_spec(), None),
        &runner.admin,
    )
    .await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let inputs = RunInputs::default().sample(&sha256);
    let run = run_instance(&runner, flow.id(), &inputs).await?;
    is!(
        bindings_of(&run)[0].endpoint,
        ModelConfigRef::Group(alpha.id)
    );
    // banning the bound endpoint leaves the run's bindings and models unchanged
    let before_ban = runner
        .admin
        .reactions
        .run_models(&run.group, run.id)
        .await?;
    ban_model_entity(&runner.admin, &alpha, "removed").await?;
    is!(
        runner
            .admin
            .reactions
            .run_models(&run.group, run.id)
            .await?,
        before_ban
    );
    is!(
        bindings_of(&runner.user.client.reactions.get(&run.group, run.id).await?),
        bindings_of(&run)
    );
    // the failed run's retry binds the next endpoint with a new token
    runs::handle_reaction(
        &runner.user.client,
        &runner.user.auth_header(),
        &run.group,
        run.id,
        "fail",
    )
    .await?;
    let created = runner
        .user
        .client
        .reactions
        .retry_workflow_run(flow.id(), run.id)
        .await?;
    let retry = runner
        .user
        .client
        .reactions
        .get(&runner.group, created.id)
        .await?;
    is!(retry.parent, Some(run.id));
    is!(
        bindings_of(&retry)[0].endpoint,
        ModelConfigRef::Group(beta.id)
    );
    is!(
        run_tokens(&runner.user).await?,
        vec![format!("run-{}", retry.id)]
    );
    // with no usable endpoint left the retry is refused and creates nothing
    runs::handle_reaction(
        &runner.user.client,
        &runner.user.auth_header(),
        &retry.group,
        retry.id,
        "fail",
    )
    .await?;
    ban_model_entity(&runner.admin, &beta, "removed").await?;
    let before = vec![reactions_of(&runner.user, &runner.group).await?];
    assert_status(
        runner
            .user
            .client
            .reactions
            .retry_workflow_run(flow.id(), retry.id)
            .await,
        400,
        "a retry with no usable model was created",
    )?;
    nothing_left(&runner.user, &groups, &before).await?;
    // a tool run in a group with no model endpoint is created and binds nothing
    let bare = generators::god_group(
        &runner.admin,
        "fornax",
        &runner.suffix,
        &GroupRoles::default().owner(&runner.user.username),
    )
    .await?;
    let (_, image) = runs::image_tool(&[bare.as_str()], ImageScaler::K8s, &runner.admin).await?;
    let file = sample(&bare, &runner.user.client).await?;
    let created = runner
        .user
        .client
        .reactions
        .run_tool(
            image.id,
            &ToolRunRequest::new(RunInputs::default().sample(&file)),
        )
        .await?;
    let tool_run = runner.user.client.reactions.get(&bare, created.id).await?;
    is!(matches!(tool_run.source, ReactionSource::Tool { .. }), true);
    Ok(())
}

/// Resolution uses only the run group's endpoints and the runner's own credentials, skips an
/// endpoint off the allowed list even before a scan bans it, and never uses a credential whose
/// endpoint was deleted
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn resolution_scope() -> Result<(), Error> {
    // a runner in their own group and another group, and a workflow in both
    let runner = runner("ananke", "consus").await?;
    let other = generators::god_group(
        &runner.admin,
        "faunus",
        &runner.suffix,
        &GroupRoles::default().owner(&runner.user.username),
    )
    .await?;
    let both = [runner.group.as_str(), other.as_str()];
    let flow = runs::model_workflow(
        "Scope",
        &both,
        vec![runs::ModelAgent::new(&["m"])],
        None,
        (&runs::gen_model_harness_spec(), None),
        &runner.admin,
    )
    .await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let inputs = RunInputs::default().sample(&sha256);
    let request = WorkflowRunRequest::new(inputs.clone()).group(&runner.group);
    // an endpoint only in the other group is never used for a run in the runner's group
    runs::keyless_endpoint(&[other.as_str()], &["m"], &runner.admin).await?;
    assert_status(
        runner
            .user
            .client
            .reactions
            .run_workflow(flow.id(), &request)
            .await,
        400,
        "a run used an endpoint outside its run group",
    )?;
    // a group endpoint with no group key isn't served by another user's key
    let keyed = model_endpoint(&runner, "keyed", openai_at_random(), None, &["m"]).await?;
    let callisto = generators::named_user_with_role(
        &runner.admin,
        &generators::readable_name("callisto", &runner.suffix),
        UserRole::User,
    )
    .await?;
    runner
        .admin
        .groups
        .update(
            &runner.group,
            &GroupUpdate::default()
                .users(GroupUsersUpdate::default().direct_add(&callisto.username)),
        )
        .await?;
    set_models(
        &callisto.client,
        UserModelSettingsUpdate::default().credential(
            keyed.id,
            UserCredentialRef::new(CredentialKind::ApiKey, Some(USER_SECRET.into())),
        ),
    )
    .await?;
    assert_status(
        runner
            .user
            .client
            .reactions
            .run_workflow(flow.id(), &request)
            .await,
        400,
        "a run used another user's key",
    )?;
    // the runner's own key for it is used at the user level
    let mine = UserCredentialRef::new(CredentialKind::ApiKey, Some(USER_SECRET.into()));
    set_models(
        &runner.user.client,
        UserModelSettingsUpdate::default().credential(keyed.id, mine.clone()),
    )
    .await?;
    let created = runner
        .user
        .client
        .reactions
        .run_workflow(flow.id(), &request)
        .await?;
    let run = runner
        .user
        .client
        .reactions
        .get(&runner.group, created.id)
        .await?;
    is!(
        bindings_of(&run)[0].credential,
        Some(ModelConfigRef::User(mine.id))
    );
    // once that endpoint is deleted the runner's key for it is never used
    runner.admin.entities.delete(keyed.id).await?;
    assert_status(
        runner
            .user
            .client
            .reactions
            .run_workflow(flow.id(), &request)
            .await,
        400,
        "a run used a key whose endpoint was deleted",
    )?;
    // an endpoint whose allowed list entry was removed is skipped before any scan
    let gone_provider = gateway_at_random();
    let gone = model_endpoint(&runner, "a-gone", gone_provider.clone(), None, &["m"]).await?;
    let kept = model_endpoint(&runner, "b-kept", gateway_at_random(), None, &["m"]).await?;
    generators::update_model_whitelist(
        ModelsConfUpdate::default().remove_endpoint(AllowedEndpoint::for_provider(&gone_provider)),
        false,
    )
    .await?;
    let created = runner
        .user
        .client
        .reactions
        .run_workflow(flow.id(), &request)
        .await?;
    let run = runner
        .user
        .client
        .reactions
        .get(&runner.group, created.id)
        .await?;
    is!(
        bindings_of(&run)[0].endpoint,
        ModelConfigRef::Group(kept.id)
    );
    // the skipped endpoint has no ban yet
    let stored = runner.admin.entities.get(gone.id).await?;
    let EntityMetadata::ModelEndpoint(meta) = stored.metadata else {
        return Err(Error::new("not a model endpoint".to_owned()));
    };
    is!(meta.bans.len(), 0);
    Ok(())
}

/// Each run reads the live auth sources, so changing an agent's auth source changes the next
/// run with no new instance, and a harness with per-agent models runs agents on different
/// models with one binding each in the run file and the models it gets
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn resolution_reads_live_config() -> Result<(), Error> {
    // a runner with a keyless group endpoint and the runner's own endpoint, both serving m
    let runner = runner("sinope", "terminus").await?;
    let groups = [runner.group.as_str()];
    runs::keyless_endpoint(&groups, &["m", "n"], &runner.admin).await?;
    let own = Uuid::new_v4();
    let own_provider = gateway_at_random();
    generators::allow_model_endpoint(&own_provider).await?;
    set_models(
        &runner.user.client,
        UserModelSettingsUpdate::default().endpoint(
            own,
            UserModelEndpoint::new("mine", own_provider).model(catalog_entry("m")),
        ),
    )
    .await?;
    // an agent limited to the group level in a workflow limited to the user level
    let levels = |levels: &[ModelAuthLevel]| ModelAuthSource {
        levels: levels.iter().copied().collect(),
    };
    let mut limited = runs::ModelAgent::new(&["m"]);
    limited.model_auth = Some(levels(&[ModelAuthLevel::Group]));
    let flow = runs::model_workflow(
        "Live",
        &groups,
        vec![limited],
        Some(levels(&[ModelAuthLevel::User])),
        (&runs::gen_model_harness_spec(), None),
        &runner.admin,
    )
    .await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let inputs = RunInputs::default().sample(&sha256);
    let (status, body) = raw_run(&runner, flow.id(), &inputs).await?;
    is!(status, 400);
    let error: ModelResolutionError = serde_json::from_value(body)?;
    is!(
        error.agents[0]
            .candidates
            .iter()
            .all(|candidate| candidate.reason == ModelRejectReason::LevelNotAllowed),
        true
    );
    // widening the live agent's auth source changes the next run with no new instance
    let instances = instances_in(&runner.admin, &runner.group).await?;
    let update = EntityMetadataUpdate::Agent {
        config: None,
        model_auth: Some(levels(&[ModelAuthLevel::Group, ModelAuthLevel::User])),
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
    runner
        .admin
        .entities
        .update(flow.agents[0].id, EntityUpdate::default().metadata(update))
        .await?;
    let run = run_instance(&runner, flow.id(), &inputs).await?;
    is!(bindings_of(&run)[0].level, ModelAuthLevel::User);
    is!(bindings_of(&run)[0].endpoint, ModelConfigRef::User(own));
    is!(instances_in(&runner.admin, &runner.group).await?, instances);
    // a harness with per-agent models takes agents on different models
    let ToolSpec::Harness(mut per_agent) = runs::gen_model_harness_spec() else {
        return Err(Error::new("not a harness spec".to_owned()));
    };
    per_agent.capabilities.per_agent_models = true;
    let mixed = runs::model_workflow(
        "PerAgent",
        &groups,
        vec![runs::ModelAgent::new(&["m"]), runs::ModelAgent::new(&["n"])],
        None,
        (&ToolSpec::Harness(per_agent), None),
        &runner.admin,
    )
    .await?;
    let run = run_instance(&runner, mixed.id(), &inputs).await?;
    let bound = bindings_of(&run);
    is!(bound.len(), 2);
    let model_of = |agent: Uuid| {
        bound
            .iter()
            .find(|binding| binding.agent == agent)
            .map(|binding| binding.model_id.clone())
    };
    is!(model_of(mixed.agents[0].id), Some("m".to_owned()));
    is!(model_of(mixed.agents[1].id), Some("n".to_owned()));
    let run_file: HarnessRunFile = buffer(&runner.user.client, &run, "run.json").await?;
    is!(run_file.models, bound.clone());
    let models = runner
        .admin
        .reactions
        .run_models(&run.group, run.id)
        .await?;
    is!(models.models.agents.len(), 2);
    is!(
        models.models.agents[&mixed.agents[1].id].model_id.clone(),
        "n".to_owned()
    );
    Ok(())
}
/// An agent's auth source is read in the groups the agent is in now, so an agent moved out of the
/// groups its instance saw it in keeps its restriction
#[tokio::test]
async fn moved_agents_keep_their_auth_source() -> Result<(), Error> {
    // a runner with a keyless group endpoint and the runner's own endpoint, both serving m
    let runner = runner("callisto", "zeus").await?;
    let groups = [runner.group.as_str()];
    runs::keyless_endpoint(&groups, &["m"], &runner.admin).await?;
    let own = Uuid::new_v4();
    let own_provider = gateway_at_random();
    generators::allow_model_endpoint(&own_provider).await?;
    set_models(
        &runner.user.client,
        UserModelSettingsUpdate::default().endpoint(
            own,
            UserModelEndpoint::new("mine", own_provider).model(catalog_entry("m")),
        ),
    )
    .await?;
    // an agent limited to the user level
    let mut limited = runs::ModelAgent::new(&["m"]);
    limited.model_auth = Some(ModelAuthSource {
        levels: [ModelAuthLevel::User].into_iter().collect(),
    });
    let flow = runs::model_workflow(
        "Moved",
        &groups,
        vec![limited],
        None,
        (&runs::gen_model_harness_spec(), None),
        &runner.admin,
    )
    .await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let inputs = RunInputs::default().sample(&sha256);
    let run = run_instance(&runner, flow.id(), &inputs).await?;
    is!(bindings_of(&run)[0].level, ModelAuthLevel::User);
    // delete the catalog, which leaves its instance, and move the agent to another group
    let other = generators::god_group(
        &runner.admin,
        "hera",
        &runner.suffix,
        &GroupRoles::default(),
    )
    .await?;
    runner.admin.entities.delete(flow.workflow.id).await?;
    let moved = EntityUpdate::default()
        .group(&other)
        .remove_group(&runner.group);
    runner
        .admin
        .entities
        .update(flow.agents[0].id, moved)
        .await?;
    let agent = runner.admin.entities.get(flow.agents[0].id).await?;
    is!(agent.groups, vec![other.clone()]);
    // the next run of the instance still takes auth from the user level only
    let run = run_instance(&runner, flow.id(), &inputs).await?;
    is!(bindings_of(&run)[0].level, ModelAuthLevel::User);
    is!(bindings_of(&run)[0].endpoint, ModelConfigRef::User(own));
    Ok(())
}

/// Creating a run and reading its models read each endpoint and credential the agents bind
/// once per request, however many agents bind them
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn run_models_read_each_config_once() -> Result<(), Error> {
    // five agents binding one endpoint and its credential
    let runner = runner("lysithea", "pomona").await?;
    let groups = [runner.group.as_str()];
    let cred = generators::model_credential(
        &groups,
        CredentialKind::ApiKey,
        Some(GROUP_SECRET),
        &runner.admin,
    )
    .await?;
    let endpoint =
        model_endpoint(&runner, "shared", openai_at_random(), Some(cred.id), &["m"]).await?;
    let agents = (0..5).map(|_| runs::ModelAgent::new(&["m"])).collect();
    let flow = runs::model_workflow(
        "Five",
        &groups,
        agents,
        None,
        (&runs::gen_model_harness_spec(), None),
        &runner.admin,
    )
    .await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let inputs = RunInputs::default().sample(&sha256);
    // creating the run reads the endpoint and the credential once each
    let (endpoint_reads, cred_reads) = (
        faults::entity_reads(endpoint.id),
        faults::entity_reads(cred.id),
    );
    let run = run_instance(&runner, flow.id(), &inputs).await?;
    is!(bindings_of(&run).len(), 5);
    is!(faults::entity_reads(endpoint.id) - endpoint_reads, 1);
    is!(faults::entity_reads(cred.id) - cred_reads, 1);
    // reading its models reads them once each too
    let (endpoint_reads, cred_reads) = (
        faults::entity_reads(endpoint.id),
        faults::entity_reads(cred.id),
    );
    runner
        .admin
        .reactions
        .run_models(&run.group, run.id)
        .await?;
    is!(faults::entity_reads(endpoint.id) - endpoint_reads, 1);
    is!(faults::entity_reads(cred.id) - cred_reads, 1);
    // two agents with different credentials read each once
    let first = generators::model_credential(
        &groups,
        CredentialKind::ApiKey,
        Some(GROUP_SECRET),
        &runner.admin,
    )
    .await?;
    let second = generators::model_credential(
        &groups,
        CredentialKind::ApiKey,
        Some(USER_SECRET),
        &runner.admin,
    )
    .await?;
    model_endpoint(&runner, "one", openai_at_random(), Some(first.id), &["one"]).await?;
    model_endpoint(
        &runner,
        "two",
        openai_at_random(),
        Some(second.id),
        &["two"],
    )
    .await?;
    let ToolSpec::Harness(mut per_agent) = runs::gen_model_harness_spec() else {
        return Err(Error::new("not a harness spec".to_owned()));
    };
    per_agent.capabilities.per_agent_models = true;
    let pair = runs::model_workflow(
        "Pair",
        &groups,
        vec![
            runs::ModelAgent::new(&["one"]),
            runs::ModelAgent::new(&["two"]),
        ],
        None,
        (&ToolSpec::Harness(per_agent), None),
        &runner.admin,
    )
    .await?;
    let (first_reads, second_reads) = (
        faults::entity_reads(first.id),
        faults::entity_reads(second.id),
    );
    let run = run_instance(&runner, pair.id(), &inputs).await?;
    runner
        .admin
        .reactions
        .run_models(&run.group, run.id)
        .await?;
    is!(faults::entity_reads(first.id) - first_reads, 2);
    is!(faults::entity_reads(second.id) - second_reads, 2);
    Ok(())
}

/// Creating a workflow run mints `run-{reaction}` for the runner before its job is queued, a
/// direct tool run mints one scoped to its tool instance, a harness's tool call with the
/// workflow run's token mints none and records the parent's, and a pipeline reaction gets none;
/// a run token's value never rotates and its expiry is the run's SLA
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn run_tokens_minted_with_runs() -> Result<(), Error> {
    // a runner, a workflow calling a container tool by CLI and an agentic tool by the API, and a
    // file
    let runner = runner("kore", "pax").await?;
    let groups = [runner.group.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let (_, cli) = runs::container_tool(&groups, image.id, &runner.admin).await?;
    let (_, api) = generators::tool(
        &groups,
        "1.0.0",
        &generators::gen_agentic_spec(),
        &runner.admin,
    )
    .await?;
    let triage = runs::ready_workflow(
        "Triage",
        &groups,
        &[
            (&cli, ToolMechanism::Cli),
            (&api, ToolMechanism::ThoriumApi),
        ],
        &runner.admin,
    )
    .await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let inputs = RunInputs::default().sample(&sha256);
    let client = &runner.user.client.reactions;
    // a workflow run names its token and is its own run
    let created = client
        .run_workflow(triage.id(), &WorkflowRunRequest::new(inputs.clone()))
        .await?;
    let run = client.get(&runner.group, created.id).await?;
    let name = format!("run-{}", run.id);
    is!(name.len(), 40);
    is!(run.run_token.clone(), Some(name.clone()));
    is!(run.run, Some(run.id));
    // its job belongs to the run
    let job = only_job(&runner.admin, &run).await?;
    is!(job.run, Some(run.id));
    // the token acts as the runner with the user role in the run group until the run's SLA
    let token = run_token_of(&runner.admin, &run).await?;
    is!(token.name, name.clone());
    is!(token.owner, runner.user.username.clone());
    is!(token.role, ScopedTokenRole::User);
    is!(token.groups, vec![runner.group.clone()]);
    is!(
        token.expires.map(|expires| expires.timestamp_millis()),
        Some(run.sla.timestamp_millis())
    );
    is!(
        token.token_expiration.timestamp_millis(),
        run.sla.timestamp_millis()
    );
    // its scope is the run and every bound tool with the image the container tool pins
    let Some(scope) = token.run.clone() else {
        return Err(Error::new("the run token has no run scope".to_owned()));
    };
    is!(scope.reaction, run.id);
    is!(
        sorted_strings(
            scope
                .tool_instances
                .iter()
                .map(ToString::to_string)
                .collect()
        ),
        sorted_strings(vec![
            cli.id.to_string(),
            image.id.to_string(),
            api.id.to_string()
        ])
    );
    // reading the token again never rotates its value
    is!(
        run_token_of(&runner.admin, &run).await?.token,
        token.token.clone()
    );
    // the reaction's hash holds the token's name and its run
    let hash = reaction_hash(&runner.group, run.id).await;
    is!(hash.get("run_token").cloned(), Some(name.clone()));
    is!(
        hash.get("run").cloned(),
        Some(serde_json::to_string(&run.id)?)
    );
    is!(
        hash.contains_key("source") && hash.contains_key("plan"),
        true
    );
    // a direct tool run mints a token scoped to its tool instance and is its own run
    let created = client
        .run_tool(image.id, &ToolRunRequest::new(inputs.clone()))
        .await?;
    let direct = client.get(&runner.group, created.id).await?;
    is!(direct.run, Some(direct.id));
    is!(direct.run_token.clone(), Some(format!("run-{}", direct.id)));
    let direct_token = run_token_of(&runner.admin, &direct).await?;
    is!(
        direct_token
            .run
            .clone()
            .map(|scope| (scope.reaction, scope.tool_instances)),
        Some((direct.id, vec![image.id]))
    );
    // a tool call made with the workflow run's token mints nothing and records the parent's
    let before = run_tokens(&runner.user).await?;
    let scoped = token_client(&runner.admin, &token.token).await?;
    let created = scoped
        .reactions
        .run_tool(cli.id, &ToolRunRequest::new(inputs.clone()))
        .await?;
    let call = client.get(&runner.group, created.id).await?;
    is!(call.parent, Some(run.id));
    is!(call.run_token.clone(), Some(name.clone()));
    is!(call.run, Some(run.id));
    is!(run_tokens(&runner.user).await?, before);
    is!(only_job(&runner.admin, &call).await?.run, Some(run.id));
    let hash = reaction_hash(&runner.group, call.id).await;
    is!(hash.get("run_token").cloned(), Some(name.clone()));
    is!(
        hash.get("run").cloned(),
        Some(serde_json::to_string(&run.id)?)
    );
    // a pipeline reaction gets no token and no run
    let pipe = pipe_for(&runner.group, image.id, &runner.user.client).await?;
    let created = client
        .create(&ReactionRequest::new(&runner.group, &pipe).sample(&sha256))
        .await?;
    let pipeline = client.get(&runner.group, created.id).await?;
    is!(pipeline.run_token, None::<String>);
    is!(pipeline.run, None::<Uuid>);
    let hash = reaction_hash(&runner.group, pipeline.id).await;
    is!(
        hash.contains_key("source") && hash.contains_key("plan"),
        true
    );
    is!(
        hash.contains_key("run_token") || hash.contains_key("run"),
        false
    );
    is!(run_tokens(&runner.user).await?, before);
    Ok(())
}

/// A direct tool run or a workflow run whose SLA is longer than the deployment's token lifetime
/// (`thorium.auth.token_expire`) is refused with a `400` naming the limit before anything is
/// written, so a run token never outlives a user token; an SLA of exactly that lifetime is
/// allowed and its token expires with it, a run without one gets the default SLA, and a
/// harness's tool call made with the run's token keeps its own SLA because it mints no token
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn run_sla_bounded_by_token_expire() -> Result<(), Error> {
    // a runner, an image tool, a container tool pinning it, a workflow calling it, and a file
    let runner = runner("metis", "janus").await?;
    let groups = [runner.group.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let (_, cli) = runs::container_tool(&groups, image.id, &runner.admin).await?;
    let triage = runs::ready_workflow(
        "Triage",
        &groups,
        &[(&cli, ToolMechanism::Cli)],
        &runner.admin,
    )
    .await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let inputs = RunInputs::default().sample(&sha256);
    let client = &runner.user.client.reactions;
    // the longest SLA a run may have is the token lifetime the test API runs with
    let days = test_utilities::CONF.thorium.auth.token_expire;
    let max = u64::from(days) * 24 * 60 * 60;
    let before = vec![reactions_of(&runner.user, &runner.group).await?];
    // a workflow run with a longer SLA is refused, and the error names the limit
    let refused = client
        .run_workflow(
            triage.id(),
            &WorkflowRunRequest::new(inputs.clone()).sla(max + 1),
        )
        .await;
    let msg = refused
        .as_ref()
        .err()
        .and_then(Error::msg)
        .unwrap_or_default();
    assert_status(refused, 400, "a workflow run with an SLA past token_expire")?;
    is!(msg.contains("token_expire"), true, msg);
    is!(msg.contains(&max.to_string()), true, msg);
    // so is a direct tool run, even with the century-long SLA the generic bound allows
    let refused = client
        .run_tool(
            image.id,
            &ToolRunRequest::new(inputs.clone()).sla(3_000_000_000),
        )
        .await;
    assert_status(refused, 400, "a tool run with an SLA past token_expire")?;
    let refused = client
        .run_tool(image.id, &ToolRunRequest::new(inputs.clone()).sla(max + 1))
        .await;
    assert_status(refused, 400, "a tool run one second past token_expire")?;
    // neither refusal left a token or a reaction behind
    nothing_left(&runner.user, &groups, &before).await?;
    // a workflow run with an SLA of exactly the token lifetime is allowed
    let created = client
        .run_workflow(
            triage.id(),
            &WorkflowRunRequest::new(inputs.clone()).sla(max),
        )
        .await?;
    let run = client.get(&runner.group, created.id).await?;
    let token = run_token_of(&runner.admin, &run).await?;
    // its token expires with the run, no later than the token lifetime from now
    is!(
        token.expires.map(|expires| expires.timestamp_millis()),
        Some(run.sla.timestamp_millis())
    );
    let limit = chrono::Utc::now() + chrono::Duration::days(i64::from(days));
    is!((run.sla <= limit), true, run.sla);
    // so is a direct tool run
    let created = client
        .run_tool(image.id, &ToolRunRequest::new(inputs.clone()).sla(max))
        .await?;
    let direct = client.get(&runner.group, created.id).await?;
    let limit = chrono::Utc::now() + chrono::Duration::days(i64::from(days));
    is!((direct.sla <= limit), true, direct.sla);
    // a run with no SLA gets the default, which fits the default token lifetime
    let created = client
        .run_tool(image.id, &ToolRunRequest::new(inputs.clone()))
        .await?;
    let defaulted = client.get(&runner.group, created.id).await?;
    let default_left = (defaulted.sla - chrono::Utc::now()).num_seconds();
    let default_sla = i64::try_from(DEFAULT_PIPELINE_SLA.min(max)).unwrap_or(i64::MAX);
    is!(
        (default_sla - 600..=default_sla).contains(&default_left),
        true,
        default_left
    );
    // a harness's tool call mints nothing, so its SLA isn't bounded by the token lifetime
    let scoped = token_client(&runner.admin, &token.token).await?;
    let created = scoped
        .reactions
        .run_tool(cli.id, &ToolRunRequest::new(inputs.clone()).sla(max + 1))
        .await?;
    let call = client.get(&runner.group, created.id).await?;
    is!(call.run_token.clone(), run.run_token.clone());
    is!((call.sla > limit), true, call.sla);
    Ok(())
}

/// Build the authorization header for a raw token value
///
/// # Arguments
///
/// * `token` - The token value to authenticate with
fn token_auth(token: &str) -> String {
    // base64 encode the token like the client does
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(token.as_bytes());
    format!("token {encoded}")
}

/// Send a proceed or fail command for a reaction with an authorization header
///
/// # Arguments
///
/// * `admin` - The admin client, for its host
/// * `auth` - The authorization header to send
/// * `reaction` - The reaction to handle
/// * `cmd` - `proceed` or `fail`
async fn handle_as(
    admin: &Thorium,
    auth: &str,
    reaction: &Reaction,
    cmd: &str,
) -> Result<(), Error> {
    // send the handle request
    let resp = reqwest::Client::new()
        .post(format!(
            "{}/api/reactions/handle/{}/{}/{cmd}",
            admin.host, reaction.group, reaction.id
        ))
        .header("authorization", auth)
        .send()
        .await?;
    // a refused handle is an error carrying its status
    if !resp.status().is_success() {
        return Err(Error::from(resp));
    }
    Ok(())
}

/// Check that a job is in no queue or deadline stream and that its data expires
///
/// # Arguments
///
/// * `run` - The run the job belongs to
/// * `tool_instance` - The tool instance the job runs
/// * `job` - The job
async fn job_dequeued(run: &Reaction, tool_instance: Uuid, job: Uuid) -> Result<(), Error> {
    // the job is in none of the run's queues for its tool instance
    let mut conn = redis_conn().await;
    let id = job.to_string();
    for status in ["Created", "Running", "Sleeping"] {
        let queue = format!(
            "{}:run_queue:{}:{}:{tool_instance}:{status}",
            ns(),
            run.group,
            run.id
        );
        let members: Vec<String> = redis::cmd("zrange")
            .arg(&queue)
            .arg(0)
            .arg(-1)
            .query_async(&mut conn)
            .await
            .expect("a run queue");
        is!(
            members.iter().any(|member| member.contains(&id)),
            false,
            status
        );
    }
    // its deadline member is gone
    let deadlines: Vec<String> = redis::cmd("zrange")
        .arg(format!("{}:streams:system:K8s:deadlines", ns()))
        .arg(0)
        .arg(-1)
        .query_async(&mut conn)
        .await
        .expect("the K8s deadlines");
    is!(deadlines.iter().any(|member| member.contains(&id)), false);
    // and its data expires rather than living forever
    let ttl: i64 = redis::cmd("ttl")
        .arg(format!("{}:job_data:{job}", ns()))
        .query_async(&mut conn)
        .await
        .expect("the job data's ttl");
    is!((ttl != -1), true, ttl);
    Ok(())
}

/// Read the members of a sorted set in the test API's Redis that mention a job
///
/// # Arguments
///
/// * `key` - The sorted set's key without the namespace
/// * `job` - The job to look for
async fn members_with(key: &str, job: Uuid) -> Vec<String> {
    // read every member of the set
    let members: Vec<String> = redis::cmd("zrange")
        .arg(format!("{}:{key}", ns()))
        .arg(0)
        .arg(-1)
        .query_async(&mut redis_conn().await)
        .await
        .expect("a sorted set");
    // keep only the ones naming the job
    let id = job.to_string();
    members
        .into_iter()
        .filter(|member| member.contains(&id))
        .collect()
}

/// Resetting the claimed job of a run that already failed, as the scaler does once the job's
/// worker is gone, never queues it again and takes it out of the run's queues, the running
/// stream, and the deadline stream, while a running run's claimed job is reset as before
#[tokio::test]
async fn reset_skips_jobs_of_ended_runs() -> Result<(), Error> {
    // a runner with a workflow, and the test node
    let runner = runner("kore", "angerona").await?;
    generators::node("cluster0", "node0", Resources::default(), &runner.admin).await?;
    let triage = Box::pin(workflow(&runner, "Triage")).await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let inputs = RunInputs::default().sample(&sha256);
    let user = runner.user.username.as_str();
    let auth = runner.user.auth_header();
    let harness = triage.harness.id;
    for end in [true, false] {
        // a run whose harness job a per-run worker claimed
        let (run, scoped) = start_run(&runner, &triage, &inputs).await?;
        let worker =
            register_worker(&runner.admin, user, &runner.group, harness, Some(run.id)).await?;
        let claimed = claim(&scoped, &runner.group, harness, &worker).await?;
        is!(claimed.len(), 1);
        let job = claimed[0].id;
        let label = if end { "a failed run" } else { "a running run" };
        is!(
            members_with("streams:system:K8s:running", job).await.len(),
            1
        );
        // the run fails while its job is still claimed
        if end {
            runs::handle_reaction(&runner.user.client, &auth, &runner.group, run.id, "fail")
                .await?;
        }
        // the job is reset as if its worker were lost
        let resets = JobResets::with_capacity(ImageScaler::K8s, "Worker not found", 1).add(job);
        runner.admin.jobs.bulk_reset(&resets).await?;
        // a failed run's job is never queued again, and a running run's job is
        let created = format!("run_queue:{}:{}:{harness}:Created", run.group, run.id);
        let requeued = runner.admin.jobs.get(job).await?.status == JobStatus::Created;
        is!(requeued, !end, label);
        is!(members_with(&created, job).await.is_empty(), end, label);
        let deadlines = members_with("streams:system:K8s:deadlines", job).await;
        is!(deadlines.is_empty(), end, label);
        // either way the job leaves the running queue and stream, so it isn't reset again
        let running = format!("run_queue:{}:{}:{harness}:Running", run.group, run.id);
        is!(members_with(&running, job).await.is_empty(), true, label);
        is!(
            members_with("streams:system:K8s:running", job)
                .await
                .is_empty(),
            true,
            label
        );
        generators::delete_worker(&worker, &runner.admin).await?;
    }
    Ok(())
}

/// Failing or deleting a run fails its unfinished tool calls and removes their queued jobs and
/// deadlines along with the run's own, since nothing can claim them once the run's token is gone
#[tokio::test]
async fn ending_a_run_ends_its_tool_calls() -> Result<(), Error> {
    // a runner with a workflow calling a container tool
    let runner = runner("sponde", "bellona").await?;
    let groups = [runner.group.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let (_, cli) = runs::container_tool(&groups, image.id, &runner.admin).await?;
    let triage = Box::pin(runs::ready_workflow(
        "Triage",
        &groups,
        &[(&cli, ToolMechanism::Cli)],
        &runner.admin,
    ))
    .await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let inputs = RunInputs::default().sample(&sha256);
    let client = &runner.user.client.reactions;
    let auth = runner.user.auth_header();
    for end in ["fail", "delete"] {
        // a run with a queued tool call
        let (run, scoped) = start_run(&runner, &triage, &inputs).await?;
        let created = scoped
            .reactions
            .run_tool(cli.id, &ToolRunRequest::new(inputs.clone()))
            .await?;
        let call = client.get(&runner.group, created.id).await?;
        let harness_job = only_job(&runner.admin, &run).await?;
        let call_job = only_job(&runner.admin, &call).await?;
        // the runner ends the run
        if end == "fail" {
            runs::handle_reaction(&runner.user.client, &auth, &runner.group, run.id, "fail")
                .await?;
            job_dequeued(&run, triage.harness.id, harness_job.id).await?;
        } else {
            client.delete(&runner.group, &run.id).await?;
        }
        // the call failed and its job can't be claimed or spawned any more
        is!(
            client.get(&runner.group, call.id).await?.status,
            ReactionStatus::Failed,
            end
        );
        job_dequeued(&run, image.id, call_job.id).await?;
        // the run's token is gone
        assert_status(scoped.users.info().await, 401, "an ended run's token")?;
    }
    Ok(())
}

/// Deleting a run whose harness job finished while a tool call is still pending fails the call
/// without completing the run, so the deleted run is listed in no group status set
#[tokio::test]
async fn deleting_a_waiting_run_never_completes_it() -> Result<(), Error> {
    // a runner with a workflow calling a container tool, and the test node
    let runner = runner("praxidike", "concordia").await?;
    generators::node("cluster0", "node0", Resources::default(), &runner.admin).await?;
    let groups = [runner.group.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let (_, cli) = runs::container_tool(&groups, image.id, &runner.admin).await?;
    let triage = Box::pin(runs::ready_workflow(
        "Triage",
        &groups,
        &[(&cli, ToolMechanism::Cli)],
        &runner.admin,
    ))
    .await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let inputs = RunInputs::default().sample(&sha256);
    let client = &runner.user.client.reactions;
    // a run with a pending tool call whose harness job already finished
    let (run, scoped) = start_run(&runner, &triage, &inputs).await?;
    scoped
        .reactions
        .run_tool(cli.id, &ToolRunRequest::new(inputs.clone()))
        .await?;
    let user = runner.user.username.as_str();
    let worker = register_worker(
        &runner.admin,
        user,
        &runner.group,
        triage.harness.id,
        Some(run.id),
    )
    .await?;
    let claimed = claim(&scoped, &runner.group, triage.harness.id, &worker).await?;
    is!(claimed.len(), 1);
    scoped
        .jobs
        .proceed(&claimed[0], &StageLogsAdd::default(), 1, &worker)
        .await?;
    let waiting = client.get(&runner.group, run.id).await?;
    is!(waiting.status, ReactionStatus::Started);
    is!(waiting.current_stage_progress, waiting.current_stage_length);
    // the runner deletes the run
    client.delete(&runner.group, &run.id).await?;
    // the deleted run is in no group status set
    let mut conn = redis_conn().await;
    let id = run.id.to_string();
    for status in ["Created", "Started", "Completed", "Failed"] {
        let key = format!("{}:reactions_group:{}:{status}", ns(), runner.group);
        let score: Option<f64> = redis::cmd("zscore")
            .arg(&key)
            .arg(&id)
            .query_async(&mut conn)
            .await
            .expect("a group set score");
        is!(score.is_none(), true, status);
    }
    generators::delete_worker(&worker, &runner.admin).await?;
    Ok(())
}

/// A run's models are delivered only while it has a live token scoped to it, not once its runner
/// replaces the run's token with a plain token under the same name
#[tokio::test]
async fn run_models_refuse_reused_token_names() -> Result<(), Error> {
    // a runner with a workflow run
    let runner = runner("autonoe", "flora").await?;
    let triage = Box::pin(workflow(&runner, "Triage")).await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let inputs = RunInputs::default().sample(&sha256);
    let (run, _) = start_run(&runner, &triage, &inputs).await?;
    // the live run's models are delivered
    runner
        .admin
        .reactions
        .run_models(&run.group, run.id)
        .await?;
    // the runner deletes the run's token and creates a plain token under its name
    let name = format!("run-{}", run.id);
    runner.user.client.users.delete_scoped_token(&name).await?;
    runner
        .user
        .client
        .users
        .create_scoped_token(&ScopedTokenRequest::new(&name).group(&runner.group))
        .await?;
    // the run's models aren't delivered any more
    assert_status(
        runner.admin.reactions.run_models(&run.group, run.id).await,
        404,
        "a run's models under a plain token",
    )?;
    Ok(())
}

/// A run's token and models are not found while its runner can't edit in the run group, so the
/// run's workers are never spawned with a token every call refuses, and leaving one of the
/// input groups changes nothing
#[tokio::test]
async fn run_token_follows_run_group_editing() -> Result<(), Error> {
    // a runner with a workflow run on a file that is also in a group the runner is a user in
    let runner = runner("herse", "cardea").await?;
    let triage = Box::pin(workflow(&runner, "Triage")).await?;
    let user = runner.user.username.clone();
    let roles = GroupRoles::default().user(&user);
    let other = generators::god_group(&runner.admin, "faunus", &runner.suffix, &roles).await?;
    let content = generators::gen_string(2048);
    let groups = [runner.group.as_str(), other.as_str()];
    let sha256 = sample_in(&content, &groups, &runner.user.client).await?;
    let inputs = RunInputs::default().sample(&sha256);
    let (run, _) = start_run(&runner, &triage, &inputs).await?;
    let models = &runner.admin.reactions;
    // leaving an input group that isn't the run group still serves the run's token and models
    leave(&runner.admin, &other, &user).await?;
    run_token_of(&runner.admin, &run).await?;
    models.run_models(&run.group, run.id).await?;
    // a runner demoted to a monitor of the run group can't edit in it, so neither is found
    let demote = GroupUpdate::default()
        .owners(GroupUsersUpdate::default().direct_remove(&user))
        .monitors(GroupUsersUpdate::default().direct_add(&user));
    runner.admin.groups.update(&runner.group, &demote).await?;
    let token = run_token_of(&runner.admin, &run).await;
    assert_status(token, 404, "a run token of a monitor")?;
    let refused = models.run_models(&run.group, run.id).await;
    assert_status(refused, 404, "run models of a monitor")?;
    // a runner made a user of the run group again gets the run's token and models served
    let restore = GroupUpdate::default()
        .monitors(GroupUsersUpdate::default().direct_remove(&user))
        .users(GroupUsersUpdate::default().direct_add(&user));
    runner.admin.groups.update(&runner.group, &restore).await?;
    run_token_of(&runner.admin, &run).await?;
    models.run_models(&run.group, run.id).await?;
    // a runner removed from the run group gets neither
    leave(&runner.admin, &runner.group, &user).await?;
    let token = run_token_of(&runner.admin, &run).await;
    assert_status(token, 404, "a run token after leaving the run group")?;
    let refused = models.run_models(&run.group, run.id).await;
    assert_status(refused, 404, "run models after leaving the run group")?;
    Ok(())
}

/// The run-token route serves only a token scoped to the reaction's run, so a plain token the
/// runner creates under a deleted run token's name is never handed to the run's workers
#[tokio::test]
async fn run_token_route_refuses_reused_names() -> Result<(), Error> {
    // a runner with a workflow run and a tool call of it
    let runner = runner("callirrhoe", "silvanus").await?;
    let groups = [runner.group.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let (_, cli) = runs::container_tool(&groups, image.id, &runner.admin).await?;
    let triage = Box::pin(runs::ready_workflow(
        "Triage",
        &groups,
        &[(&cli, ToolMechanism::Cli)],
        &runner.admin,
    ))
    .await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let inputs = RunInputs::default().sample(&sha256);
    let (run, scoped) = start_run(&runner, &triage, &inputs).await?;
    let created = scoped
        .reactions
        .run_tool(cli.id, &ToolRunRequest::new(inputs.clone()))
        .await?;
    let call = runner
        .user
        .client
        .reactions
        .get(&runner.group, created.id)
        .await?;
    // the run and its call are served the run's own token
    for reaction in [&run, &call] {
        let token = run_token_of(&runner.admin, reaction).await?;
        is!(token.run.as_ref().map(|scope| scope.reaction), Some(run.id));
    }
    // the runner deletes the run's token and creates a plain token under its name
    let name = format!("run-{}", run.id);
    runner.user.client.users.delete_scoped_token(&name).await?;
    runner
        .user
        .client
        .users
        .create_scoped_token(&ScopedTokenRequest::new(&name).group(&runner.group))
        .await?;
    // the plain token is served for neither
    for reaction in [&run, &call] {
        assert_status(
            run_token_of(&runner.admin, reaction).await,
            404,
            "a plain token under a run token's name",
        )?;
    }
    Ok(())
}

/// Ending a run deletes only a token scoped to it, so a plain token the runner creates under a
/// deleted run token's name outlives the run's completion, failure, or deletion
#[tokio::test]
async fn run_end_keeps_reused_token_names() -> Result<(), Error> {
    // a runner with a workflow
    let runner = runner("eukelade", "picus").await?;
    let triage = Box::pin(workflow(&runner, "Triage")).await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let inputs = RunInputs::default().sample(&sha256);
    let auth = runner.user.auth_header();
    let users = &runner.user.client.users;
    for end in ["complete", "fail", "delete"] {
        // the runner deletes a live run's token and creates a plain token under its name
        let (run, _) = start_run(&runner, &triage, &inputs).await?;
        let name = format!("run-{}", run.id);
        users.delete_scoped_token(&name).await?;
        let req = ScopedTokenRequest::new(&name).group(&runner.group);
        let plain = users.create_scoped_token(&req).await?;
        // the run ends
        match end {
            "delete" => {
                runner
                    .user
                    .client
                    .reactions
                    .delete(&runner.group, &run.id)
                    .await?;
            }
            "fail" => {
                runs::handle_reaction(&runner.user.client, &auth, &runner.group, run.id, "fail")
                    .await?;
            }
            _ => {
                runs::handle_reaction(&runner.user.client, &auth, &runner.group, run.id, "proceed")
                    .await?;
            }
        }
        // the run ended and the plain token is still stored and still authenticates
        let ended = runner
            .user
            .client
            .reactions
            .get(&runner.group, run.id)
            .await;
        match end {
            "delete" => assert_status(ended, 404, "a deleted run")?,
            "fail" => {
                let status = ended?.status;
                is!(status, ReactionStatus::Failed);
            }
            _ => {
                let status = ended?.status;
                is!(status, ReactionStatus::Completed);
            }
        }
        is!(
            stored_run_tokens(&runner.user.username)
                .await
                .contains(&name),
            true
        );
        let kept = users.get_scoped_token(&name).await?;
        is!(kept.run.is_none(), true);
        let client = token_client(&runner.admin, &plain.token).await?;
        client.users.info().await?;
        // remove the plain token before the next case
        users.delete_scoped_token(&name).await?;
    }
    Ok(())
}

/// A run token's tool call is refused while the run group disallows reactions or the runner is
/// only a monitor of it, and is a sub run of the run again once both are restored
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn run_token_tool_calls_follow_group_rights() -> Result<(), Error> {
    // a runner with a workflow run calling a container tool
    let runner = runner("kale", "fides").await?;
    let groups = [runner.group.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let (_, cli) = runs::container_tool(&groups, image.id, &runner.admin).await?;
    let triage = Box::pin(runs::ready_workflow(
        "Triage",
        &groups,
        &[(&cli, ToolMechanism::Cli)],
        &runner.admin,
    ))
    .await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let inputs = RunInputs::default().sample(&sha256);
    let (run, scoped) = start_run(&runner, &triage, &inputs).await?;
    let req = ToolRunRequest::new(inputs.clone());
    let user = runner.user.username.clone();
    // a tool call in a group that allows reactions is a sub run of the run
    scoped.reactions.run_tool(cli.id, &req).await?;
    let before = reactions_of(&runner.user, &runner.group).await?;
    // a group that disallows reactions refuses the call and it creates nothing
    let disable = GroupUpdate {
        allowed: GroupAllowedUpdate::default().disable_reactions(),
        ..GroupUpdate::default()
    };
    runner.admin.groups.update(&runner.group, &disable).await?;
    let refused = scoped.reactions.run_tool(cli.id, &req).await;
    assert_status(refused, 401, "a tool call in a group without reactions")?;
    is!(reactions_of(&runner.user, &runner.group).await?, before);
    // allowing reactions again lets the call through
    let enable = GroupUpdate {
        allowed: GroupAllowedUpdate::default().enable_reactions(),
        ..GroupUpdate::default()
    };
    runner.admin.groups.update(&runner.group, &enable).await?;
    scoped.reactions.run_tool(cli.id, &req).await?;
    let before = reactions_of(&runner.user, &runner.group).await?;
    // a runner demoted to a monitor of the run group is refused and the call creates nothing
    let demote = GroupUpdate::default()
        .owners(GroupUsersUpdate::default().direct_remove(&user))
        .monitors(GroupUsersUpdate::default().direct_add(&user));
    runner.admin.groups.update(&runner.group, &demote).await?;
    let refused = scoped.reactions.run_tool(cli.id, &req).await;
    assert_status(refused, 401, "a monitor's tool call")?;
    is!(reactions_of(&runner.user, &runner.group).await?, before);
    // a runner made a user of the run group again gets a sub run of the run
    let restore = GroupUpdate::default()
        .monitors(GroupUsersUpdate::default().direct_remove(&user))
        .users(GroupUsersUpdate::default().direct_add(&user));
    runner.admin.groups.update(&runner.group, &restore).await?;
    let created = scoped.reactions.run_tool(cli.id, &req).await?;
    let call = runner
        .user
        .client
        .reactions
        .get(&runner.group, created.id)
        .await?;
    is!(call.parent, Some(run.id));
    Ok(())
}

/// Overriding a direct tool run's command needs the Developer role for the scaler of the Image
/// tool instance its job runs, when the run is created and when its reaction is updated
#[tokio::test]
async fn tool_run_overrides_need_the_image_scaler() -> Result<(), Error> {
    // a K8s developer and a BareMetal developer in one group
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let bare_metal_only = UserRole::Developer {
        k8s: false,
        bare_metal: true,
        windows: false,
        external: false,
        kvm: false,
    };
    let k8s = generators::named_user_with_role(
        &admin,
        &generators::readable_name("leda", &suffix),
        K8S_DEVELOPER,
    )
    .await?;
    let bare_metal = generators::named_user_with_role(
        &admin,
        &generators::readable_name("himalia", &suffix),
        bare_metal_only,
    )
    .await?;
    let roles = GroupRoles::default()
        .user(&k8s.username)
        .user(&bare_metal.username);
    let group = generators::god_group(&admin, "minerva", &suffix, &roles).await?;
    let groups = [group.as_str()];
    // a BareMetal Image tool and a container tool pinning it
    let (_, image) = runs::image_tool(&groups, ImageScaler::BareMetal, &admin).await?;
    let (_, boxed) = runs::container_tool(&groups, image.id, &admin).await?;
    let sha256 = sample(&group, &admin).await?;
    let cmd = vec![
        "sh".to_owned(),
        "-c".to_owned(),
        "echo overridden".to_owned(),
    ];
    let opts = GenericJobOpts::new(true, true, Some(cmd));
    let req = ToolRunRequest::new(RunInputs::default().sample(&sha256))
        .args(GenericJobArgs::default().opts(opts.clone()));
    let changed = GenericJobOpts::new(false, false, Some(vec!["/bin/true".to_owned()]));
    for id in [image.id, boxed.id] {
        // the K8s developer can't start the run with the override
        let refused = k8s.client.reactions.run_tool(id, &req).await;
        assert_status(
            refused,
            401,
            "a K8s developer's BareMetal run with an override",
        )?;
        // the BareMetal developer starts it and the override is stored on the image's stage
        let created = bare_metal.client.reactions.run_tool(id, &req).await?;
        let run = bare_metal.client.reactions.get(&group, created.id).await?;
        let stage = run.plan.stages[0][0].name.clone();
        is!(run.plan.stages[0][0].tool_instance, image.id);
        is!(run.args[&stage].opts.clone(), opts.clone());
        // the K8s developer can't change the override through an update of the run
        let update = ReactionUpdate::default().arg(
            &stage,
            thorium::models::GenericJobArgsUpdate::default().opts(changed.clone()),
        );
        let refused = k8s
            .client
            .reactions
            .update(&group, &created.id, &update)
            .await;
        assert_status(refused, 401, "a K8s developer's update of a BareMetal run")?;
        // the BareMetal developer can
        let updated = bare_metal
            .client
            .reactions
            .update(&group, &created.id, &update)
            .await?;
        is!(updated.args[&stage].opts.clone(), changed.clone());
    }
    Ok(())
}

/// A run token's tool call can't override the command, positionals, or kwargs of the image it
/// runs, while plain args still pass
#[tokio::test]
async fn run_token_tool_calls_refuse_overrides() -> Result<(), Error> {
    // a developer runner with a workflow run calling a container tool
    let runner = runner("arche", "liber").await?;
    let groups = [runner.group.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let (_, cli) = runs::container_tool(&groups, image.id, &runner.admin).await?;
    let triage = Box::pin(runs::ready_workflow(
        "Triage",
        &groups,
        &[(&cli, ToolMechanism::Cli)],
        &runner.admin,
    ))
    .await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let inputs = RunInputs::default().sample(&sha256);
    let (run, scoped) = start_run(&runner, &triage, &inputs).await?;
    let before = reactions_of(&runner.user, &runner.group).await?;
    // every kind of override in a tool call is refused and creates nothing
    let cmd = vec!["sh".to_owned(), "-c".to_owned(), "injected".to_owned()];
    for opts in [
        GenericJobOpts::new(false, false, Some(cmd)),
        GenericJobOpts::new(true, false, None),
        GenericJobOpts::new(false, true, None),
    ] {
        let req = ToolRunRequest::new(inputs.clone()).args(GenericJobArgs::default().opts(opts));
        for id in [cli.id, image.id] {
            let refused = scoped.reactions.run_tool(id, &req).await;
            assert_status(refused, 401, "a run token's tool call with an override")?;
        }
    }
    is!(reactions_of(&runner.user, &runner.group).await?, before);
    // a tool call with plain args is still a sub run of the run
    let args = GenericJobArgs::default().positionals(vec!["--quick"]);
    let created = scoped
        .reactions
        .run_tool(cli.id, &ToolRunRequest::new(inputs.clone()).args(args))
        .await?;
    let call = runner
        .user
        .client
        .reactions
        .get(&runner.group, created.id)
        .await?;
    is!(call.parent, Some(run.id));
    Ok(())
}

/// Nobody replaces or adds a run's generated files through a reaction update, not the runner,
/// the run's token, or an admin; other ephemeral files are still added
#[tokio::test]
async fn run_files_not_replaced_by_updates() -> Result<(), Error> {
    use base64::Engine as _;
    // a runner with a workflow run and a direct tool run
    let runner = runner("pasiphae", "vesta").await?;
    let groups = [runner.group.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let (_, cli) = runs::container_tool(&groups, image.id, &runner.admin).await?;
    let triage = Box::pin(runs::ready_workflow(
        "Triage",
        &groups,
        &[(&cli, ToolMechanism::Cli)],
        &runner.admin,
    ))
    .await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let inputs = RunInputs::default().sample(&sha256);
    let client = &runner.user.client.reactions;
    let (run, scoped) = start_run(&runner, &triage, &inputs).await?;
    let created = client
        .run_tool(image.id, &ToolRunRequest::new(inputs.clone()))
        .await?;
    let tool_run = client.get(&runner.group, created.id).await?;
    // the run's files as the run was created
    let names = ["run.json", "agent.json", "tools.json"];
    let mut before = Vec::with_capacity(names.len());
    for name in names {
        before.push(buffer::<serde_json::Value>(&runner.user.client, &run, name).await?);
    }
    // an update replacing any of them is refused for everyone
    let injected = base64::engine::general_purpose::STANDARD.encode(b"{\"injected\": true}");
    for name in names {
        let mut update = ReactionUpdate::default();
        update.ephemeral.insert(name.to_owned(), injected.clone());
        for updater in [&runner.user.client, &scoped, &runner.admin] {
            let refused = updater.reactions.update(&run.group, &run.id, &update).await;
            assert_status(refused, 401, "replacing a run's generated file")?;
        }
        // nor can one be added to a tool run
        let refused = client.update(&tool_run.group, &tool_run.id, &update).await;
        assert_status(refused, 401, "adding a generated file to a tool run")?;
    }
    // the run's files are unchanged and the tool run has none
    for (name, before) in names.iter().zip(&before) {
        is!(
            &buffer::<serde_json::Value>(&runner.user.client, &run, name).await?,
            before
        );
    }
    let stored = client.get(&tool_run.group, tool_run.id).await?;
    is!(stored.ephemeral.is_empty(), true);
    // another ephemeral file is still added to the run
    let mut update = ReactionUpdate::default();
    update.ephemeral.insert("notes.txt".to_owned(), injected);
    let updated = client.update(&run.group, &run.id, &update).await?;
    is!(updated.ephemeral.contains(&"notes.txt".to_owned()), true);
    Ok(())
}

/// A run token updates, proceeds, fails, and writes the cache of only its own run's reactions,
/// never the runner's other reactions or another run of the runner
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn run_token_reaction_writes_stay_in_run() -> Result<(), Error> {
    // a runner with two workflow runs and a pipeline reaction
    let runner = runner("eukelade", "terminus").await?;
    let groups = [runner.group.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let (_, cli) = runs::container_tool(&groups, image.id, &runner.admin).await?;
    let triage = Box::pin(runs::ready_workflow(
        "Triage",
        &groups,
        &[(&cli, ToolMechanism::Cli)],
        &runner.admin,
    ))
    .await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let inputs = RunInputs::default().sample(&sha256);
    let client = &runner.user.client.reactions;
    let (run, scoped) = start_run(&runner, &triage, &inputs).await?;
    let token = run_token_of(&runner.admin, &run).await?;
    let auth = token_auth(&token.token);
    let (other, _) = start_run(&runner, &triage, &inputs).await?;
    let pipe = pipe_for(&runner.group, image.id, &runner.user.client).await?;
    let created = client
        .create(&ReactionRequest::new(&runner.group, &pipe).sample(&sha256))
        .await?;
    let pipeline = client.get(&runner.group, created.id).await?;
    // the token can't update, proceed, fail, or write the cache of either reaction
    for outside in [&pipeline, &other] {
        let update = ReactionUpdate::default().tag("phobos");
        let refused = scoped
            .reactions
            .update(&outside.group, &outside.id, &update)
            .await;
        assert_status(refused, 401, "a run token's update outside its run")?;
        for cmd in ["proceed", "fail"] {
            let refused = handle_as(&runner.admin, &auth, outside, cmd).await;
            assert_status(refused, 401, "a run token's handle outside its run")?;
        }
        let cache = ReactionCacheUpdate::default().generic("deimos", "moon");
        let refused = scoped
            .reactions
            .update_cache(&outside.group, outside.id, &cache)
            .await;
        assert_status(refused, 401, "a run token's cache update outside its run")?;
        let files = ReactionCacheFileUpdate::default().buffer(Buffer::new("moon").name("moon"));
        let refused = scoped
            .reactions
            .update_cache_files(&outside.group, outside.id, files)
            .await;
        assert_status(refused, 401, "a run token's cache files outside its run")?;
        // the reaction is unchanged
        let stored = client.get(&outside.group, outside.id).await?;
        is!(stored.status, ReactionStatus::Created);
        is!(stored.current_stage, outside.current_stage);
        is!(
            stored.tags.contains(&"phobos".to_owned()),
            false,
            "outside tags"
        );
        let cache = client.get_cache(&outside.group, outside.id).await?;
        is!(
            cache.generic.is_empty() && cache.files.is_empty(),
            true,
            "outside cache"
        );
    }
    // the runner's full token still updates both
    let update = ReactionUpdate::default().tag("phobos");
    client
        .update(&pipeline.group, &pipeline.id, &update)
        .await?;
    client.update(&other.group, &other.id, &update).await?;
    // the token updates and writes the cache of its own run and of its tool calls
    let call = scoped
        .reactions
        .run_tool(cli.id, &ToolRunRequest::new(inputs.clone()))
        .await?;
    let call = client.get(&runner.group, call.id).await?;
    for own in [&run, &call] {
        let updated = scoped
            .reactions
            .update(&own.group, &own.id, &update)
            .await?;
        is!(
            updated.tags.contains(&"phobos".to_owned()),
            true,
            "own tags"
        );
        let cache = ReactionCacheUpdate::default().generic("deimos", "moon");
        scoped
            .reactions
            .update_cache(&own.group, own.id, &cache)
            .await?;
        let cache = client.get_cache(&own.group, own.id).await?;
        is!(
            cache.generic.get("deimos").map(String::as_str),
            Some("moon")
        );
    }
    // the token fails its own tool call
    handle_as(&runner.admin, &auth, &call, "fail").await?;
    is!(
        client.get(&call.group, call.id).await?.status,
        ReactionStatus::Failed
    );
    Ok(())
}

/// A run token deletes and adds stage logs to only its own run's reactions, never the runner's
/// other reactions or another run of the runner, while the runner's full token deletes them all
#[tokio::test]
async fn run_token_deletes_stay_in_run() -> Result<(), Error> {
    // a runner with two workflow runs and a pipeline reaction
    let runner = runner("thyone", "fortuna").await?;
    let groups = [runner.group.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let (_, cli) = runs::container_tool(&groups, image.id, &runner.admin).await?;
    let triage = Box::pin(runs::ready_workflow(
        "Triage",
        &groups,
        &[(&cli, ToolMechanism::Cli)],
        &runner.admin,
    ))
    .await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let inputs = RunInputs::default().sample(&sha256);
    let client = &runner.user.client.reactions;
    let (_, scoped) = start_run(&runner, &triage, &inputs).await?;
    let (other, other_scoped) = start_run(&runner, &triage, &inputs).await?;
    let pipe = pipe_for(&runner.group, image.id, &runner.user.client).await?;
    let created = client
        .create(&ReactionRequest::new(&runner.group, &pipe).sample(&sha256))
        .await?;
    let pipeline = client.get(&runner.group, created.id).await?;
    let mut logs = StageLogsAdd::default();
    logs.add("forged");
    // the token can't delete or add stage logs to either reaction
    for outside in [&pipeline, &other] {
        let refused = scoped.reactions.delete(&outside.group, &outside.id).await;
        assert_status(refused, 401, "a run token's delete outside its run")?;
        let refused = scoped
            .reactions
            .add_stage_logs(&outside.group, &outside.id, "harvest", &logs)
            .await;
        assert_status(refused, 401, "a run token's stage logs outside its run")?;
        // the reaction is still there
        client.get(&outside.group, outside.id).await?;
    }
    // the other run's token still works
    other_scoped.users.info().await?;
    // the token adds stage logs to and deletes its own tool call
    let call = scoped
        .reactions
        .run_tool(cli.id, &ToolRunRequest::new(inputs.clone()))
        .await?;
    scoped
        .reactions
        .add_stage_logs(&runner.group, &call.id, "harvest", &logs)
        .await?;
    scoped.reactions.delete(&runner.group, &call.id).await?;
    assert_status(
        client.get(&runner.group, call.id).await,
        404,
        "a deleted tool call",
    )?;
    // the runner's full token still deletes both
    for outside in [&pipeline, &other] {
        client.delete(&outside.group, &outside.id).await?;
        assert_status(
            client.get(&outside.group, outside.id).await,
            404,
            "a reaction the runner deleted",
        )?;
    }
    Ok(())
}

/// A run token runs only its run's tool instances, as sub runs of its run in the run's group
/// with the harness job's deadline, and can't start workflow runs, retries, or pipeline
/// reactions; a tool run's tags and SLA are its own
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn run_token_scope() -> Result<(), Error> {
    // a runner with two groups, a workflow in both calling a container tool, and an unbound tool
    let runner = runner("carme", "janus").await?;
    let second = generators::god_group(
        &runner.admin,
        "quirinus",
        &runner.suffix,
        &GroupRoles::default().owner(&runner.user.username),
    )
    .await?;
    let groups = [runner.group.as_str(), second.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let (_, cli) = runs::container_tool(&groups, image.id, &runner.admin).await?;
    let (_, unbound) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let triage = runs::ready_workflow(
        "Triage",
        &groups,
        &[(&cli, ToolMechanism::Cli)],
        &runner.admin,
    )
    .await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let inputs = RunInputs::default().sample(&sha256);
    let client = &runner.user.client.reactions;
    // a workflow run in the first group and its token
    let created = client
        .run_workflow(
            triage.id(),
            &WorkflowRunRequest::new(inputs.clone()).group(&runner.group),
        )
        .await?;
    let run = client.get(&runner.group, created.id).await?;
    let token = run_token_of(&runner.admin, &run).await?;
    let scoped = token_client(&runner.admin, &token.token).await?;
    // a tool run of an instance outside the run creates nothing, nor does the harness
    let reactions = (
        reactions_of(&runner.user, &runner.group).await?,
        reactions_of(&runner.user, &second).await?,
    );
    let tokens = run_tokens(&runner.user).await?;
    for outside in [unbound.id, triage.harness.id] {
        let refused = scoped
            .reactions
            .run_tool(outside, &ToolRunRequest::new(inputs.clone()))
            .await;
        assert_status(refused, 401, "a run token's tool run outside its run")?;
    }
    is!(
        (
            reactions_of(&runner.user, &runner.group).await?,
            reactions_of(&runner.user, &second).await?
        ),
        reactions
    );
    is!(run_tokens(&runner.user).await?, tokens.clone());
    // a direct tool run in the second group to name as another parent
    let other = client
        .run_tool(
            image.id,
            &ToolRunRequest::new(inputs.clone()).group(&second),
        )
        .await?;
    let other = client.get(&second, other.id).await?;
    let tokens = run_tokens(&runner.user).await?;
    // a tool call naming another group and parent is the run's sub run in the run's group
    let req = ToolRunRequest::new(inputs.clone())
        .group(&second)
        .parent(other.id)
        .tag("titan")
        .sla(600);
    let created = scoped.reactions.run_tool(cli.id, &req).await?;
    let call = client.get(&runner.group, created.id).await?;
    is!(call.parent, Some(run.id));
    is!(call.group, runner.group.clone());
    is!(call.tags.contains(&"titan".to_owned()), true);
    is!(run_tokens(&runner.user).await?, tokens);
    // its SLA is its own
    let left = (call.sla - chrono::Utc::now()).num_seconds();
    is!((500..=600).contains(&left), true, left);
    // its job has the harness job's deadline, and so does its deadline member
    let harness_job = only_job(&runner.admin, &run).await?;
    let call_job = only_job(&runner.admin, &call).await?;
    is!(
        call_job.deadline.timestamp(),
        harness_job.deadline.timestamp()
    );
    let members: Vec<(String, f64)> = redis::cmd("zrange")
        .arg(format!("{}:streams:system:K8s:deadlines", ns()))
        .arg(0)
        .arg(-1)
        .arg("WITHSCORES")
        .query_async(&mut redis_conn().await)
        .await
        .expect("the K8s deadlines");
    let score = |job: Uuid| {
        members
            .iter()
            .find(|(member, _)| member.contains(&job.to_string()))
            .map(|(_, score)| *score)
    };
    is!(score(call_job.id).is_some(), true);
    is!(score(call_job.id), score(harness_job.id));
    // the call is queued on the run's queue for its image
    let queue = format!(
        "{}:run_queue:{}:{}:{}:Created",
        ns(),
        runner.group,
        run.id,
        image.id
    );
    let queued: Vec<String> = redis::cmd("zrange")
        .arg(&queue)
        .arg(0)
        .arg(-1)
        .query_async(&mut redis_conn().await)
        .await
        .expect("the run queue");
    is!(
        queued
            .iter()
            .any(|member| member.contains(&call_job.id.to_string())),
        true
    );
    // a pipeline job queued after the run started sorts after the call
    let pipe = pipe_for(&runner.group, image.id, &runner.user.client).await?;
    let created = client
        .create(&ReactionRequest::new(&runner.group, &pipe).sample(&sha256))
        .await?;
    let pipeline = client.get(&runner.group, created.id).await?;
    let pipeline_job = only_job(&runner.admin, &pipeline).await?;
    is!((pipeline_job.deadline > call_job.deadline), true);
    // a user's tool run with no parent keeps the deadline its SLA and cost give
    let other_job = only_job(&runner.admin, &other).await?;
    is!((other_job.deadline < other.sla), true);
    // a direct tool run's SLA sets its token's expiry, and an omitted SLA uses the default
    let timed = client
        .run_tool(
            image.id,
            &ToolRunRequest::new(inputs.clone())
                .group(&runner.group)
                .sla(600),
        )
        .await?;
    let timed = client.get(&runner.group, timed.id).await?;
    let left = (timed.sla - chrono::Utc::now()).num_seconds();
    is!((500..=600).contains(&left), true, left);
    is!(
        run_token_of(&runner.admin, &timed)
            .await?
            .expires
            .map(|expires| expires.timestamp_millis()),
        Some(timed.sla.timestamp_millis())
    );
    let default_left = (other.sla - chrono::Utc::now()).num_seconds();
    let default_sla = i64::try_from(DEFAULT_PIPELINE_SLA).unwrap_or(i64::MAX);
    is!(
        (default_sla - 600..=default_sla).contains(&default_left),
        true
    );
    is!(
        run_token_of(&runner.admin, &other)
            .await?
            .expires
            .map(|expires| expires.timestamp_millis()),
        Some(other.sla.timestamp_millis())
    );
    // a run token can't start a workflow run, a retry, or a pipeline reaction
    let refused = scoped
        .reactions
        .run_workflow(triage.id(), &WorkflowRunRequest::new(inputs.clone()))
        .await;
    assert_status(refused, 401, "a workflow run with a run token")?;
    let refused = scoped
        .reactions
        .retry_workflow_run(triage.id(), run.id)
        .await;
    assert_status(refused, 401, "a retry with a run token")?;
    let req = ReactionRequest::new(&runner.group, &pipe).sample(&sha256);
    assert_status(
        scoped.reactions.create(&req).await,
        401,
        "a pipeline reaction with a run token",
    )?;
    let bulk = scoped.reactions.create_bulk(&[req]).await;
    let Err(err) = bulk else {
        return Err(Error::new(
            "a bulk pipeline reaction with a run token was accepted".to_owned(),
        ));
    };
    is!(err.status().map(|code| code.as_u16()), Some(401));
    Ok(())
}

/// A run token claims only for its run's workers, handles only its run's jobs as the claiming
/// worker, and reads, updates, and deletes only its run's workers; full tokens are unaffected
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn run_token_claims_and_handles() -> Result<(), Error> {
    // a runner with two workflow runs on one workflow, and the test node
    let runner = runner("lysithea", "mars").await?;
    generators::node("cluster0", "node0", Resources::default(), &runner.admin).await?;
    let groups = [runner.group.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let (_, cli) = runs::container_tool(&groups, image.id, &runner.admin).await?;
    let triage = runs::ready_workflow(
        "Triage",
        &groups,
        &[(&cli, ToolMechanism::Cli)],
        &runner.admin,
    )
    .await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let inputs = RunInputs::default().sample(&sha256);
    let client = &runner.user.client.reactions;
    let harness = triage.harness.id;
    let mut runs = Vec::new();
    for _ in 0..2 {
        let created = client
            .run_workflow(triage.id(), &WorkflowRunRequest::new(inputs.clone()))
            .await?;
        runs.push(client.get(&runner.group, created.id).await?);
    }
    let (first, second) = (&runs[0], &runs[1]);
    let token = run_token_of(&runner.admin, first).await?;
    let scoped = token_client(&runner.admin, &token.token).await?;
    // a worker for each run's harness and a shared worker
    let user = runner.user.username.as_str();
    let own = register_worker(&runner.admin, user, &runner.group, harness, Some(first.id)).await?;
    let others =
        register_worker(&runner.admin, user, &runner.group, harness, Some(second.id)).await?;
    let shared = register_worker(&runner.admin, user, &runner.group, harness, None).await?;
    // the token can't claim for another run's worker or a shared worker
    for worker in [&others, &shared] {
        let refused = claim(&scoped, &runner.group, harness, worker).await;
        assert_status(refused, 401, "a run token's claim for another worker")?;
    }
    // a shared worker never pops a run queue
    is!(
        claim(&runner.user.client, &runner.group, harness, &shared)
            .await?
            .len(),
        0
    );
    // the run's own worker pops only its run's job
    let jobs = claim(&scoped, &runner.group, harness, &own).await?;
    is!(jobs.len(), 1);
    is!(jobs[0].reaction, first.id);
    // the full token claims for the other run's worker
    let theirs = claim(&runner.user.client, &runner.group, harness, &others).await?;
    is!(theirs.len(), 1);
    is!(theirs[0].reaction, second.id);
    // a tool call of the run and its worker
    let created = scoped
        .reactions
        .run_tool(cli.id, &ToolRunRequest::new(inputs.clone()))
        .await?;
    let tool_worker =
        register_worker(&runner.admin, user, &runner.group, image.id, Some(first.id)).await?;
    let calls = claim(&scoped, &runner.group, image.id, &tool_worker).await?;
    is!(calls.len(), 1);
    is!(calls[0].reaction, created.id);
    let logs = StageLogsAdd::default();
    // naming another worker of the run is refused
    let refused = scoped.jobs.proceed(&calls[0], &logs, 1, &own).await;
    assert_status(refused, 401, "proceeding naming another worker")?;
    let refused = scoped.jobs.error(&calls[0].id, &logs, &own).await;
    assert_status(refused, 401, "failing naming another worker")?;
    // another run's claimed job is refused on every handle, even naming its real worker
    let real = runner.admin.system.get_worker(&others).await?.name;
    let refused = scoped.jobs.proceed(&theirs[0], &logs, 1, &real).await;
    assert_status(refused, 401, "proceeding another run's job")?;
    let refused = scoped.jobs.error(&theirs[0].id, &logs, &real).await;
    assert_status(refused, 401, "failing another run's job")?;
    let refused = scoped.jobs.sleep(&theirs[0].id, "nap").await;
    assert_status(refused, 401, "sleeping another run's job")?;
    let refused = scoped.jobs.checkpoint(&theirs[0], "mark").await;
    assert_status(refused, 401, "checkpointing another run's job")?;
    // the other run's job is unchanged
    let unchanged = runner.admin.jobs.get(theirs[0].id).await?;
    is!(unchanged.status, JobStatus::Running);
    is!(unchanged.args.contains("mark"), false);
    // the call is finished by its own worker, and the full token finishes the other run's job
    scoped
        .jobs
        .proceed(&calls[0], &logs, 1, &tool_worker)
        .await?;
    runner
        .user
        .client
        .jobs
        .proceed(&theirs[0], &logs, 1, &real)
        .await?;
    // another run's worker can't be read, updated, or deleted with the token
    let refused = scoped.system.get_worker(&others).await;
    assert_status(refused, 401, "reading another run's worker")?;
    let update = WorkerUpdate::new(WorkerStatus::Running);
    let refused = scoped.system.update_worker(&others, &update).await;
    assert_status(refused, 401, "updating another run's worker")?;
    for names in [vec![&others], vec![&own, &others], vec![&shared]] {
        let deletes = names
            .iter()
            .fold(WorkerDeleteMap::with_capacity(2), |map, name| {
                map.add(*name)
            });
        let refused = scoped
            .system
            .delete_workers(ImageScaler::K8s, &deletes)
            .await;
        assert_status(refused, 401, "deleting another run's worker")?;
    }
    for worker in [&others, &own, &shared] {
        is!(
            runner.admin.system.get_worker(worker).await?.name,
            (*worker).clone()
        );
    }
    // the run's own worker is read with the token
    is!(scoped.system.get_worker(&own).await?.run, Some(first.id));
    // whoami returns the run token itself and its groups, never the runner's token
    let info = scoped.users.info().await?;
    is!(info.token, token.token.clone());
    is!(info.groups, token.groups.clone());
    is!(
        serde_json::to_string(&info)?.contains(&runner.user.token),
        false
    );
    // the runner's worker in a group outside the token can't be deleted with it
    let elsewhere = generators::god_group(
        &runner.admin,
        "fortuna",
        &runner.suffix,
        &GroupRoles::default().owner(user),
    )
    .await?;
    let outside = register_worker(&runner.admin, user, &elsewhere, image.id, None).await?;
    let deletes = WorkerDeleteMap::with_capacity(1).add(&outside);
    let refused = scoped
        .system
        .delete_workers(ImageScaler::K8s, &deletes)
        .await;
    assert_status(refused, 401, "deleting a worker outside the token's groups")?;
    is!(
        runner.admin.system.get_worker(&outside).await?.name,
        outside.clone()
    );
    // the run deletes its own per-run worker at exit
    let deletes = WorkerDeleteMap::with_capacity(1).add(&tool_worker);
    scoped
        .system
        .delete_workers(ImageScaler::K8s, &deletes)
        .await?;
    assert_status(
        runner.admin.system.get_worker(&tool_worker).await,
        404,
        "a deleted worker",
    )?;
    // failing the run's own harness job as its worker ends the run and its token
    scoped.jobs.error(&jobs[0].id, &logs, &own).await?;
    assert_status(
        run_token_of(&runner.admin, first).await,
        404,
        "the token of a failed run",
    )?;
    assert_status(scoped.users.info().await, 401, "a failed run's token")?;
    for worker in [&own, &others, &shared, &outside] {
        generators::delete_worker(worker, &runner.admin).await?;
    }
    Ok(())
}

/// A run's token is deleted when its reaction completes, fails, or is deleted, never by its sub
/// reactions, and a run whose token is already gone still completes and deletes; deleting the
/// token ends the run's access, and a token past its SLA is deleted rather than rotated
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn run_token_lifecycle() -> Result<(), Error> {
    // a runner with a workflow, and the test node
    let runner = runner("praxidike", "pomona").await?;
    generators::node("cluster0", "node0", Resources::default(), &runner.admin).await?;
    let groups = [runner.group.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let (_, cli) = runs::container_tool(&groups, image.id, &runner.admin).await?;
    let triage = runs::ready_workflow(
        "Triage",
        &groups,
        &[(&cli, ToolMechanism::Cli)],
        &runner.admin,
    )
    .await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let inputs = RunInputs::default().sample(&sha256);
    let client = &runner.user.client.reactions;
    let user = runner.user.username.clone();
    let logs = StageLogsAdd::default();
    let auth = runner.user.auth_header();
    // a completed sub reaction leaves the token, and the completed run deletes it
    let (run, scoped) = start_run(&runner, &triage, &inputs).await?;
    let call = scoped
        .reactions
        .run_tool(cli.id, &ToolRunRequest::new(inputs.clone()))
        .await?;
    let tool_worker =
        register_worker(&runner.admin, &user, &runner.group, image.id, Some(run.id)).await?;
    let jobs = claim(&scoped, &runner.group, image.id, &tool_worker).await?;
    is!(jobs[0].reaction, call.id);
    scoped
        .jobs
        .proceed(&jobs[0], &logs, 1, &tool_worker)
        .await?;
    is!(
        client.get(&runner.group, call.id).await?.status,
        ReactionStatus::Completed
    );
    run_token_of(&runner.admin, &run).await?;
    let harness_worker = register_worker(
        &runner.admin,
        &user,
        &runner.group,
        triage.harness.id,
        Some(run.id),
    )
    .await?;
    let jobs = claim(&scoped, &runner.group, triage.harness.id, &harness_worker).await?;
    scoped
        .jobs
        .proceed(&jobs[0], &logs, 1, &harness_worker)
        .await?;
    is!(
        client.get(&runner.group, run.id).await?.status,
        ReactionStatus::Completed
    );
    assert_status(
        run_token_of(&runner.admin, &run).await,
        404,
        "a completed run's token",
    )?;
    assert_status(scoped.users.info().await, 401, "a completed run's token")?;
    // a failed sub reaction leaves the token, and the failed run deletes it
    let (run, scoped) = start_run(&runner, &triage, &inputs).await?;
    let call = scoped
        .reactions
        .run_tool(cli.id, &ToolRunRequest::new(inputs.clone()))
        .await?;
    runs::handle_reaction(&runner.user.client, &auth, &runner.group, call.id, "fail").await?;
    run_token_of(&runner.admin, &run).await?;
    scoped.users.info().await?;
    runs::handle_reaction(&runner.user.client, &auth, &runner.group, run.id, "fail").await?;
    assert_status(
        run_token_of(&runner.admin, &run).await,
        404,
        "a failed run's token",
    )?;
    assert_status(scoped.users.info().await, 401, "a failed run's token")?;
    // a deleted sub reaction leaves the token, and the deleted run deletes it
    let (run, scoped) = start_run(&runner, &triage, &inputs).await?;
    let call = scoped
        .reactions
        .run_tool(cli.id, &ToolRunRequest::new(inputs.clone()))
        .await?;
    client.delete(&runner.group, &call.id).await?;
    scoped.users.info().await?;
    let name = format!("run-{}", run.id);
    is!(stored_run_tokens(&user).await.contains(&name), true);
    client.delete(&runner.group, &run.id).await?;
    is!(stored_run_tokens(&user).await.contains(&name), false);
    assert_status(scoped.users.info().await, 401, "a deleted run's token")?;
    // the owner deletes a live run's token: the run's calls fail and its token read is a 404
    let (run, scoped) = start_run(&runner, &triage, &inputs).await?;
    let name = format!("run-{}", run.id);
    is!(run_tokens(&runner.user).await?.contains(&name), true);
    runner.user.client.users.delete_scoped_token(&name).await?;
    assert_status(scoped.users.info().await, 401, "a deleted token")?;
    assert_status(
        run_token_of(&runner.admin, &run).await,
        404,
        "a deleted token",
    )?;
    // a run whose token is gone still completes, and another still deletes, without error
    let worker = register_worker(
        &runner.admin,
        &user,
        &runner.group,
        triage.harness.id,
        Some(run.id),
    )
    .await?;
    let jobs = claim(&runner.admin, &runner.group, triage.harness.id, &worker).await?;
    runner
        .admin
        .jobs
        .proceed(&jobs[0], &logs, 1, &worker)
        .await?;
    is!(
        client.get(&runner.group, run.id).await?.status,
        ReactionStatus::Completed
    );
    let (run, _) = start_run(&runner, &triage, &inputs).await?;
    runner
        .user
        .client
        .users
        .delete_scoped_token(&format!("run-{}", run.id))
        .await?;
    client.delete(&runner.group, &run.id).await?;
    // a tool run's token past its SLA is deleted rather than rotated
    let created = client
        .run_tool(image.id, &ToolRunRequest::new(inputs.clone()).sla(5))
        .await?;
    let timed = client.get(&runner.group, created.id).await?;
    let token = run_token_of(&runner.admin, &timed).await?;
    let scoped = token_client(&runner.admin, &token.token).await?;
    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
    assert_status(scoped.users.info().await, 401, "an expired run token")?;
    is!(
        stored_run_tokens(&user)
            .await
            .contains(&format!("run-{}", timed.id)),
        false
    );
    for worker in [&tool_worker, &harness_worker, &worker] {
        generators::delete_worker(worker, &runner.admin).await?;
    }
    Ok(())
}

/// A run token holds the run group and the groups of the inputs the runner can see that the
/// runner is in; results and children uploaded with it land where the runner's own would, and
/// it follows the runner's membership and account at every request
#[tokio::test]
#[allow(clippy::too_many_lines, clippy::many_single_char_names)]
async fn run_token_groups() -> Result<(), Error> {
    // a runner who owns G, is a user in A, B, C, and D (no results), and monitors M; X is hidden
    let admin = test_utilities::admin_client().await?;
    generators::node("cluster0", "node0", Resources::default(), &admin).await?;
    let suffix = generators::name_suffix();
    let name = generators::readable_name("pasiphae", &suffix);
    let runner = generators::named_user_with_role(&admin, &name, DEVELOPER).await?;
    let user = runner.username.clone();
    let g = generators::god_group(
        &admin,
        "janus",
        &suffix,
        &GroupRoles::default().owner(&user),
    )
    .await?;
    let a = generators::god_group(
        &admin,
        "minerva",
        &suffix,
        &GroupRoles::default().user(&user),
    )
    .await?;
    let b =
        generators::god_group(&admin, "ceres", &suffix, &GroupRoles::default().user(&user)).await?;
    let c =
        generators::god_group(&admin, "vesta", &suffix, &GroupRoles::default().user(&user)).await?;
    let m = generators::god_group(
        &admin,
        "terminus",
        &suffix,
        &GroupRoles::default().monitor(&user),
    )
    .await?;
    let x = generators::god_group(&admin, "orcus", &suffix, &GroupRoles::default()).await?;
    let d = generators::named_group_with_roles(
        &generators::readable_name("diana", &suffix),
        &GroupRoles::default().user(&user),
        GroupAllowed {
            results: false,
            ..GroupAllowed::default()
        },
        &admin,
    )
    .await?;
    let (_, image) = runs::image_tool(&[g.as_str()], ImageScaler::K8s, &admin).await?;
    // a file in A and X and a repo in B
    let content = generators::gen_string(2048);
    let shared_file = sample_in(&content, &[&a, &x], &admin).await?;
    let repo = generators::repos(&b, 1, &admin).await?.remove(0);
    let hidden = sample(&x, &admin).await?;
    // a tool run on both gets a token in A, B, and G only
    let inputs = RunInputs::default()
        .sample(&shared_file)
        .repo(RepoDependencyRequest::new(&repo.url));
    let created = runner
        .client
        .reactions
        .run_tool(image.id, &ToolRunRequest::new(inputs))
        .await?;
    let run = runner.client.reactions.get(&g, created.id).await?;
    let token = run_token_of(&admin, &run).await?;
    is!(
        token.groups,
        sorted_strings(vec![a.clone(), b.clone(), g.clone()])
    );
    let scoped = token_client(&admin, &token.token).await?;
    // the token reads the input but not a file only in X, and can't upload results to X
    downloads(&scoped, &shared_file).await?;
    assert_status(scoped.files.get(&hidden).await, 404, "a file only in X")?;
    let refused = result_groups(&scoped, &admin, &shared_file, &[x.as_str()]).await;
    assert_status(refused, 401, "a result naming X")?;
    // results it lists for the input show none from X
    result_groups(&admin, &admin, &shared_file, &[x.as_str()]).await?;
    let listed = scoped
        .files
        .get_results(&shared_file, &ResultGetParams::default())
        .await?;
    let in_x = listed
        .results
        .values()
        .flatten()
        .any(|output| output.groups.contains(&x));
    is!(in_x, false);
    // a result uploaded with the token lands where the runner's own lands
    is!(
        result_groups(&scoped, &admin, &shared_file, &[]).await?,
        result_groups(&runner.client, &admin, &shared_file, &[]).await?
    );
    is!(
        result_groups(&scoped, &admin, &shared_file, &[]).await?,
        vec![a.clone()]
    );
    // a file in A, M, D, and G gets results in A and G only, and children in A and G
    let content = generators::gen_string(2048);
    let wide = sample_in(&content, &[&a, &m, &d, &g], &admin).await?;
    let created = runner
        .client
        .reactions
        .run_tool(
            image.id,
            &ToolRunRequest::new(RunInputs::default().sample(&wide)),
        )
        .await?;
    let wide_run = runner.client.reactions.get(&g, created.id).await?;
    let wide_token = run_token_of(&admin, &wide_run).await?;
    let wide_scoped = token_client(&admin, &wide_token.token).await?;
    is!(
        result_groups(&wide_scoped, &admin, &wide, &[]).await?,
        sorted_strings(vec![a.clone(), g.clone()])
    );
    let child = generators::gen_string(1024);
    let child_sha = sample_in(&child, &[&a, &g], &wide_scoped).await?;
    let stored = admin.files.get(&child_sha).await?;
    is!(
        sorted_strings(stored.groups().into_iter().map(ToOwned::to_owned).collect()),
        sorted_strings(vec![a.clone(), g.clone()])
    );
    // the file gaining C after minting doesn't add C to the token
    sample_in(&content, &[&c], &admin).await?;
    is!(
        run_token_of(&admin, &wide_run).await?.groups,
        wide_token.groups.clone()
    );
    // a run on a file only the runner's group A can see lands its results in A
    let only_a = sample(&a, &admin).await?;
    let created = runner
        .client
        .reactions
        .run_tool(
            image.id,
            &ToolRunRequest::new(RunInputs::default().sample(&only_a)),
        )
        .await?;
    let a_run = runner.client.reactions.get(&g, created.id).await?;
    is!(a_run.group, g.clone());
    let a_scoped = token_client(&admin, &run_token_of(&admin, &a_run).await?.token).await?;
    downloads(&a_scoped, &only_a).await?;
    is!(
        result_groups(&a_scoped, &admin, &only_a, &[]).await?,
        vec![a.clone()]
    );
    // once the runner leaves A the token can't read A's files or upload results naming A
    leave(&admin, &a, &user).await?;
    assert_status(
        a_scoped.files.get(&only_a).await,
        404,
        "a file of a group left",
    )?;
    let refused = result_groups(&wide_scoped, &admin, &wide, &[a.as_str()]).await;
    assert_status(refused, 401, "a result naming a group left")?;
    // once the runner leaves the run group the run's worker can't claim or proceed
    let worker = register_worker(&admin, &user, &g, image.id, Some(a_run.id)).await?;
    let jobs = claim(&a_scoped, &g, image.id, &worker).await?;
    is!(jobs.len(), 1);
    leave(&admin, &g, &user).await?;
    assert_status(
        claim(&a_scoped, &g, image.id, &worker).await,
        401,
        "a claim after leaving the run group",
    )?;
    let refused = a_scoped
        .jobs
        .proceed(&jobs[0], &StageLogsAdd::default(), 1, &worker)
        .await;
    assert_status(refused, 401, "a proceed after leaving the run group")?;
    // a disabled runner's token is refused on every call
    let disable = UserUpdate {
        password: None,
        email: None,
        role: Some(UserRole::Disabled),
        settings: None,
    };
    admin.users.update(&user, disable).await?;
    let refused = wide_scoped.files.get(&wide).await;
    let Err(err) = refused else {
        return Err(Error::new(
            "a disabled runner's token was accepted".to_owned(),
        ));
    };
    is!(err.status().map(|code| code.as_u16()), Some(401));
    is!(
        err.msg()
            .unwrap_or_default()
            .contains("This user has been disabled"),
        true
    );
    generators::delete_worker(&worker, &admin).await?;
    Ok(())
}

/// A tool call made with a workflow run's token on the run's input reaches exactly the run's
/// groups, and an analyst's run token reaches a hidden input group without acting as an analyst
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn run_token_shared_and_analyst_groups() -> Result<(), Error> {
    // a runner in G and A with a workflow in G and a file in A and G
    let runner = runner("eukelade", "saturn").await?;
    let a = generators::god_group(
        &runner.admin,
        "fides",
        &runner.suffix,
        &GroupRoles::default().user(&runner.user.username),
    )
    .await?;
    let triage = workflow(&runner, "Triage").await?;
    let content = generators::gen_string(2048);
    let input = sample_in(&content, &[&a, &runner.group], &runner.admin).await?;
    let inputs = RunInputs::default().sample(&input);
    let created = runner
        .user
        .client
        .reactions
        .run_workflow(triage.id(), &WorkflowRunRequest::new(inputs.clone()))
        .await?;
    let run = runner
        .user
        .client
        .reactions
        .get(&runner.group, created.id)
        .await?;
    let token = run_token_of(&runner.admin, &run).await?;
    is!(
        token.groups,
        sorted_strings(vec![a.clone(), runner.group.clone()])
    );
    let scoped = token_client(&runner.admin, &token.token).await?;
    // the call mints no token, records the run's, and reaches A and G with it
    let before = run_tokens(&runner.user).await?;
    let call = scoped
        .reactions
        .run_tool(triage.tools[0].id, &ToolRunRequest::new(inputs))
        .await?;
    let call = runner
        .user
        .client
        .reactions
        .get(&runner.group, call.id)
        .await?;
    is!(call.run_token, run.run_token.clone());
    is!(run_tokens(&runner.user).await?, before);
    downloads(&scoped, &input).await?;
    is!(
        result_groups(&scoped, &runner.admin, &input, &[]).await?,
        sorted_strings(vec![a.clone(), runner.group.clone()])
    );
    // an analyst in G but not X runs a tool in G on a file only in X
    let analyst = generators::named_user_with_role(
        &runner.admin,
        &generators::readable_name("arche", &runner.suffix),
        UserRole::Analyst,
    )
    .await?;
    let g = generators::god_group(
        &runner.admin,
        "liber",
        &runner.suffix,
        &GroupRoles::default().owner(&analyst.username),
    )
    .await?;
    let x = generators::god_group(
        &runner.admin,
        "orcus",
        &runner.suffix,
        &GroupRoles::default(),
    )
    .await?;
    let (_, image) = runs::image_tool(&[g.as_str()], ImageScaler::K8s, &runner.admin).await?;
    let hidden = sample(&x, &runner.admin).await?;
    let created = analyst
        .client
        .reactions
        .run_tool(
            image.id,
            &ToolRunRequest::new(RunInputs::default().sample(&hidden)),
        )
        .await?;
    let analyst_run = analyst.client.reactions.get(&g, created.id).await?;
    let analyst_token = run_token_of(&runner.admin, &analyst_run).await?;
    is!(
        analyst_token.groups,
        sorted_strings(vec![g.clone(), x.clone()])
    );
    let analyst_scoped = token_client(&runner.admin, &analyst_token.token).await?;
    // it acts as a user, downloads the file, and lands results where the analyst's own land
    is!(analyst_scoped.users.info().await?.role, UserRole::User);
    downloads(&analyst_scoped, &hidden).await?;
    is!(
        result_groups(&analyst_scoped, &runner.admin, &hidden, &[]).await?,
        result_groups(&analyst.client, &runner.admin, &hidden, &[]).await?
    );
    // and it can't pass a developer check
    let req = PipelineRequest::new(
        &g,
        generators::gen_string(16).to_lowercase(),
        serde_json::json!([["harvest"]]),
    )
    .stage_tool("harvest", image.id);
    assert_status(
        analyst_scoped.pipelines.create(&req).await,
        401,
        "a pipeline create with an analyst's run token",
    )?;
    Ok(())
}

/// An admin can start a run only in a group they're actually in, and their run token holds only
/// groups they're in
#[tokio::test]
async fn run_token_admin_groups() -> Result<(), Error> {
    // an admin who isn't in the tool's only group
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let boss = generators::named_user_with_role(
        &admin,
        &generators::readable_name("themisto", &suffix),
        UserRole::Admin,
    )
    .await?;
    let h = generators::god_group(&admin, "honos", &suffix, &GroupRoles::default()).await?;
    let (_, image) = runs::image_tool(&[h.as_str()], ImageScaler::K8s, &admin).await?;
    let sha256 = sample(&h, &admin).await?;
    let req = ToolRunRequest::new(RunInputs::default().sample(&sha256));
    // the run is refused and leaves nothing
    let refused = boss.client.reactions.run_tool(image.id, &req).await;
    let Err(err) = refused else {
        return Err(Error::new(
            "an admin started a run outside their groups".to_owned(),
        ));
    };
    is!(err.status().map(|code| code.as_u16()), Some(401));
    is!(
        err.msg()
            .unwrap_or_default()
            .contains("Admins must actually be in groups to scope a token to that group"),
        true
    );
    is!(stored_run_tokens(&boss.username).await.len(), 0);
    is!(reactions_of(&boss, &h).await?.len(), 0);
    // once the admin joins the group the run is created with a token in their groups only
    let update =
        GroupUpdate::default().users(GroupUsersUpdate::default().direct_add(&boss.username));
    admin.groups.update(&h, &update).await?;
    let created = boss.client.reactions.run_tool(image.id, &req).await?;
    let run = boss.client.reactions.get(&h, created.id).await?;
    let token = run_token_of(&admin, &run).await?;
    is!(token.groups, vec![h.clone()]);
    Ok(())
}

/// Every runner's run token acts as a user and starts its run's tool calls without a developer
/// check, even after the runner loses the developer role, while the runner's own user-role and
/// full tokens are still checked
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn run_token_roles() -> Result<(), Error> {
    // a developer, an analyst, an admin, and a K8s developer in one group with a K8s workflow
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let mut members = Vec::new();
    for (moon, role) in [
        ("io", DEVELOPER),
        ("europa", UserRole::Analyst),
        ("ganymede", UserRole::Admin),
        ("callisto", K8S_DEVELOPER),
    ] {
        let name = generators::readable_name(moon, &suffix);
        members.push(generators::named_user_with_role(&admin, &name, role).await?);
    }
    let roles = members.iter().fold(GroupRoles::default(), |roles, member| {
        roles.owner(&member.username)
    });
    let group = generators::god_group(&admin, "juno", &suffix, &roles).await?;
    let groups = [group.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &admin).await?;
    let (_, cli) = runs::container_tool(&groups, image.id, &admin).await?;
    let triage =
        runs::ready_workflow("Triage", &groups, &[(&cli, ToolMechanism::Cli)], &admin).await?;
    let sha256 = sample(&group, &admin).await?;
    let inputs = RunInputs::default().sample(&sha256);
    // each runner's token acts as a user and starts a tool call with no developer check
    let mut tokens = Vec::new();
    for member in &members {
        let created = member
            .client
            .reactions
            .run_workflow(triage.id(), &WorkflowRunRequest::new(inputs.clone()))
            .await?;
        let run = member.client.reactions.get(&group, created.id).await?;
        let scoped = token_client(&admin, &run_token_of(&admin, &run).await?.token).await?;
        is!(scoped.users.info().await?.role, UserRole::User);
        let call = scoped
            .reactions
            .run_tool(cli.id, &ToolRunRequest::new(inputs.clone()))
            .await?;
        is!(
            member
                .client
                .reactions
                .get(&group, call.id)
                .await?
                .jobs
                .len(),
            1
        );
        tokens.push(scoped);
    }
    // the developer's own user-role scoped token can't start a direct K8s tool run
    let own = members[0]
        .client
        .users
        .create_scoped_token(&ScopedTokenRequest::new(format!("own-{suffix}")).group(&group))
        .await?;
    let own = token_client(&admin, &own.token).await?;
    let refused = own
        .reactions
        .run_tool(image.id, &ToolRunRequest::new(inputs.clone()))
        .await;
    assert_status(refused, 401, "a user-role token's K8s tool run")?;
    // the K8s developer loses the role mid run: the run token's call still runs, their full
    // token's direct run doesn't
    let demote = UserUpdate {
        password: None,
        email: None,
        role: Some(UserRole::User),
        settings: None,
    };
    admin.users.update(&members[3].username, demote).await?;
    let call = tokens[3]
        .reactions
        .run_tool(cli.id, &ToolRunRequest::new(inputs.clone()))
        .await?;
    is!(
        members[3]
            .client
            .reactions
            .get(&group, call.id)
            .await?
            .jobs
            .len(),
        1
    );
    let refused = members[3]
        .client
        .reactions
        .run_tool(image.id, &ToolRunRequest::new(inputs))
        .await;
    assert_status(refused, 401, "a demoted developer's direct tool run")?;
    Ok(())
}

/// A run refused for any reason, or whose reaction can't be written, leaves no run token and no
/// reaction
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn run_token_none_on_refused_runs() -> Result<(), Error> {
    // a runner who owns two groups and monitors one, a closed group, and a plain user
    let runner = runner("hermippe", "vulcan").await?;
    let user = runner.user.username.clone();
    let second = generators::god_group(
        &runner.admin,
        "quirinus",
        &runner.suffix,
        &GroupRoles::default().owner(&user),
    )
    .await?;
    let plain = generators::moon_user(&runner.admin, "helike", &runner.suffix).await?;
    let watched = generators::god_group(
        &runner.admin,
        "terminus",
        &runner.suffix,
        &GroupRoles::default().monitor(&user).user(&plain.username),
    )
    .await?;
    let closed = generators::named_group_with_roles(
        &generators::readable_name("vesta", &runner.suffix),
        &GroupRoles::default().owner(&user),
        GroupAllowed {
            reactions: false,
            ..GroupAllowed::default()
        },
        &runner.admin,
    )
    .await?;
    let hidden_group = generators::god_group(
        &runner.admin,
        "pluto",
        &runner.suffix,
        &GroupRoles::default(),
    )
    .await?;
    let both = [runner.group.as_str(), second.as_str()];
    let (_, image) = runs::image_tool(&both, ImageScaler::K8s, &runner.admin).await?;
    let (_, boxed) = runs::container_tool(&both, image.id, &runner.admin).await?;
    let triage = runs::ready_workflow(
        "Triage",
        &both,
        &[(&boxed, ToolMechanism::Cli)],
        &runner.admin,
    )
    .await?;
    let (_, watched_only) =
        runs::image_tool(&[watched.as_str()], ImageScaler::K8s, &runner.admin).await?;
    let (_, watched_too) = runs::image_tool(
        &[runner.group.as_str(), watched.as_str()],
        ImageScaler::K8s,
        &runner.admin,
    )
    .await?;
    let (_, closed_tool) =
        runs::image_tool(&[closed.as_str()], ImageScaler::K8s, &runner.admin).await?;
    let mut invalid = generators::gen_workflow_instance(
        &triage.workflow,
        2,
        Some(triage.id()),
        &[(&triage.agent, &[&triage.tools[0], &triage.harness])],
    );
    invalid.status = thorium::models::WorkflowInstanceStatus::Invalid {
        errors: vec!["broken".to_owned()],
    };
    let invalid = runner
        .admin
        .entities
        .create(generators::gen_workflow_instance_req(&invalid))
        .await?
        .id;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let unseen = sample(&hidden_group, &runner.admin).await?;
    let inputs = RunInputs::default().sample(&sha256);
    let client = &runner.user.client.reactions;
    // what the runner has before any refusal
    let before = vec![
        reactions_of(&runner.user, &runner.group).await?,
        reactions_of(&runner.user, &second).await?,
    ];
    // no candidate group, a monitor's run group, and a group that disallows reactions
    let refused = client
        .run_tool(watched_only.id, &ToolRunRequest::new(inputs.clone()))
        .await;
    assert_status(refused, 401, "a run with no candidate group")?;
    nothing_left(&runner.user, &[&runner.group, &second], &before).await?;
    let refused = client
        .run_tool(
            watched_too.id,
            &ToolRunRequest::new(inputs.clone()).group(&watched),
        )
        .await;
    assert_status(refused, 401, "a run in a monitored group")?;
    nothing_left(&runner.user, &[&runner.group, &second], &before).await?;
    let refused = client
        .run_tool(closed_tool.id, &ToolRunRequest::new(inputs.clone()))
        .await;
    assert_status(refused, 401, "a run in a closed group")?;
    nothing_left(&runner.user, &[&runner.group, &second], &before).await?;
    // several candidates with none given, a non-ready instance, and no inputs
    let refused = client
        .run_workflow(triage.id(), &WorkflowRunRequest::new(inputs.clone()))
        .await;
    assert_status(refused, 400, "a run with several candidate groups")?;
    nothing_left(&runner.user, &[&runner.group, &second], &before).await?;
    let refused = client
        .run_workflow(
            invalid,
            &WorkflowRunRequest::new(inputs.clone()).group(&runner.group),
        )
        .await;
    assert_status(refused, 400, "a run of an Invalid instance")?;
    nothing_left(&runner.user, &[&runner.group, &second], &before).await?;
    let refused = client
        .run_tool(
            image.id,
            &ToolRunRequest::new(RunInputs::default()).group(&runner.group),
        )
        .await;
    assert_status(refused, 400, "a run without inputs")?;
    nothing_left(&runner.user, &[&runner.group, &second], &before).await?;
    // an input the runner can't see
    let refused = client
        .run_tool(
            image.id,
            &ToolRunRequest::new(RunInputs::default().sample(&unseen)).group(&runner.group),
        )
        .await;
    assert_status(refused, 401, "a run on an unseen file")?;
    nothing_left(&runner.user, &[&runner.group, &second], &before).await?;
    // a runner without the developer role
    let refused = plain
        .client
        .reactions
        .run_tool(
            watched_too.id,
            &ToolRunRequest::new(inputs.clone()).group(&watched),
        )
        .await;
    assert_status(refused, 401, "a run without the developer role")?;
    is!(stored_run_tokens(&plain.username).await.len(), 0);
    // a workflow run and a tool run whose reaction write fails leave no token
    test_utilities::faults::fail_reaction_write(&user);
    let failed = client
        .run_workflow(
            triage.id(),
            &WorkflowRunRequest::new(inputs.clone()).group(&runner.group),
        )
        .await;
    assert_status(failed, 500, "a workflow run whose write fails")?;
    nothing_left(&runner.user, &[&runner.group, &second], &before).await?;
    test_utilities::faults::fail_reaction_write(&user);
    let failed = client
        .run_tool(
            image.id,
            &ToolRunRequest::new(inputs.clone()).group(&runner.group),
        )
        .await;
    assert_status(failed, 500, "a tool run whose write fails")?;
    nothing_left(&runner.user, &[&runner.group, &second], &before).await?;
    // the same runs then succeed and mint their tokens
    client
        .run_tool(image.id, &ToolRunRequest::new(inputs).group(&runner.group))
        .await?;
    is!(run_tokens(&runner.user).await?.len(), 1);
    Ok(())
}

/// A run token is refused on every route that requires a full token, where the runner's full
/// token succeeds, and works on ordinary routes in its groups
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn run_token_full_token_routes() -> Result<(), Error> {
    // an admin runner who owns a group with a workflow, and a spare user to act on
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let boss = generators::named_user_with_role(
        &admin,
        &generators::readable_name("kalyke", &suffix),
        UserRole::Admin,
    )
    .await?;
    let spare = generators::moon_user(&admin, "chaldene", &suffix).await?;
    let group = generators::god_group(
        &admin,
        "apollo",
        &suffix,
        &GroupRoles::default().owner(&boss.username),
    )
    .await?;
    let groups = [group.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &admin).await?;
    let sha256 = sample(&group, &admin).await?;
    let created = boss
        .client
        .reactions
        .run_tool(
            image.id,
            &ToolRunRequest::new(RunInputs::default().sample(&sha256)),
        )
        .await?;
    let run = boss.client.reactions.get(&group, created.id).await?;
    let token = run_token_of(&admin, &run).await?;
    let run_auth = test_utilities::auth_header(&token.token);
    let full_auth = boss.auth_header();
    let new_group = Uuid::new_v4().simple().to_string();
    let backup = admin.system.backup().await?;
    // the 19 routes that require a full token, in an order the full token can replay
    let body = |value: serde_json::Value| Some(value);
    let routes: Vec<(reqwest::Method, String, Option<serde_json::Value>)> = vec![
        (
            reqwest::Method::POST,
            "/api/users/tokens/".to_owned(),
            body(
                serde_json::json!({"name": format!("plain-{suffix}"), "role": "User", "groups": [group], "expires": null}),
            ),
        ),
        (reqwest::Method::GET, "/api/users/tokens/".to_owned(), None),
        (
            reqwest::Method::GET,
            format!("/api/users/tokens/plain-{suffix}"),
            None,
        ),
        (
            reqwest::Method::PATCH,
            format!("/api/users/tokens/plain-{suffix}"),
            body(serde_json::json!({"clear_expires": true})),
        ),
        (
            reqwest::Method::DELETE,
            format!("/api/users/tokens/plain-{suffix}"),
            None,
        ),
        (
            reqwest::Method::POST,
            "/api/groups/".to_owned(),
            body(serde_json::to_value(GroupRequest::new(&new_group))?),
        ),
        (
            reqwest::Method::PATCH,
            format!("/api/groups/{new_group}"),
            body(serde_json::to_value(disable_files())?),
        ),
        (
            reqwest::Method::DELETE,
            format!("/api/groups/{new_group}"),
            None,
        ),
        (
            reqwest::Method::GET,
            format!("/api/users/user/{}", boss.username),
            None,
        ),
        (reqwest::Method::GET, "/api/users/details/".to_owned(), None),
        (reqwest::Method::GET, "/api/system/backup".to_owned(), None),
        (
            reqwest::Method::POST,
            "/api/reactions/bulk/by/user/".to_owned(),
            body(serde_json::json!({})),
        ),
        (
            reqwest::Method::PATCH,
            "/api/users/".to_owned(),
            body(serde_json::json!({"email": format!("{}@moons.test", boss.username)})),
        ),
        (
            reqwest::Method::PATCH,
            format!("/api/users/user/{}", spare.username),
            body(serde_json::json!({"email": format!("{}@moons.test", spare.username)})),
        ),
        (
            reqwest::Method::GET,
            format!("/api/users/logout/{}", spare.username),
            None,
        ),
        (
            reqwest::Method::DELETE,
            format!("/api/users/delete/{}", spare.username),
            None,
        ),
        (reqwest::Method::DELETE, "/api/users/image".to_owned(), None),
        (reqwest::Method::POST, "/api/users/image".to_owned(), None),
        (
            reqwest::Method::POST,
            "/api/system/restore".to_owned(),
            body(serde_json::to_value(&backup)?),
        ),
    ];
    is!(routes.len(), 19);
    let http = reqwest::Client::new();
    let send =
        |method: reqwest::Method, path: &str, json: Option<&serde_json::Value>, auth: &str| {
            let mut req = http
                .request(method.clone(), format!("{}{path}", admin.host))
                .header("authorization", auth);
            if let Some(json) = json {
                req = req.json(json);
            }
            // the icon upload reads a multipart form
            if path == "/api/users/image" && method == reqwest::Method::POST {
                let part = reqwest::multipart::Part::bytes(vec![0_u8; 8]).file_name("icon.png");
                req = req.multipart(reqwest::multipart::Form::new().part("image", part));
            }
            req.send()
        };
    // every one refuses the run token
    for (method, path, json) in &routes {
        let resp = send(method.clone(), path, json.as_ref(), &run_auth).await?;
        is!(resp.status().as_u16(), 401, format!("{method} {path}"));
    }
    // the full token succeeds on every one but the image routes, which need an image, and the
    // restore, which would overwrite the data of the tests running beside this one
    for (method, path, json) in routes.iter().take(16) {
        let resp = send(method.clone(), path, json.as_ref(), &full_auth).await?;
        is!(
            resp.status().is_success(),
            true,
            format!("{method} {path}: {}", resp.status())
        );
    }
    // ordinary routes in the run group work with the run token
    let scoped = token_client(&admin, &token.token).await?;
    let uploaded = sample(&group, &scoped).await?;
    let tags = TagRequest::<Sample>::default()
        .group(&group)
        .add("moon", "kalyke");
    scoped.files.tag(&uploaded, &tags).await?;
    let entity = generators::vendor_entity(&group, &scoped).await?;
    is!(entity.groups, vec![group.clone()]);
    Ok(())
}

/// Send a profile image request as a runner
///
/// Returns the response status and body.
///
/// # Arguments
///
/// * `runner` - The runner whose image is read, uploaded, or deleted
/// * `method` - The method of the request
/// * `auth` - The authorization header to send
/// * `image` - The image to upload, if the request uploads one
async fn profile_image(
    runner: &Runner,
    method: reqwest::Method,
    auth: &str,
    image: Option<&'static [u8]>,
) -> Result<(u16, Vec<u8>), Error> {
    // reads name the runner, and uploads and deletes act on the caller's own image
    let url = if method == reqwest::Method::GET {
        format!(
            "{}/api/users/user/{}/image",
            runner.admin.host, runner.user.username
        )
    } else {
        format!("{}/api/users/image", runner.admin.host)
    };
    let mut req = reqwest::Client::new()
        .request(method, url)
        .header("authorization", auth);
    // an upload sends the image as a PNG form field
    if let Some(image) = image {
        let part = reqwest::multipart::Part::bytes(image)
            .file_name("icon.png")
            .mime_str("image/png")?;
        req = req.multipart(reqwest::multipart::Form::new().part("image", part));
    }
    let resp = req.send().await?;
    let status = resp.status().as_u16();
    Ok((status, resp.bytes().await?.to_vec()))
}

/// A runner's full token uploads and deletes their profile image, and the token of their run
/// can neither replace nor delete it
#[tokio::test]
async fn run_token_cannot_change_the_profile_image() -> Result<(), Error> {
    // a runner with a live tool run, and its run token
    let runner = runner("elara", "luna").await?;
    let groups = [runner.group.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let created = runner
        .user
        .client
        .reactions
        .run_tool(
            image.id,
            &ToolRunRequest::new(RunInputs::default().sample(&sha256)),
        )
        .await?;
    let run = runner
        .user
        .client
        .reactions
        .get(&runner.group, created.id)
        .await?;
    let token = run_token_of(&runner.admin, &run).await?;
    let run_auth = test_utilities::auth_header(&token.token);
    let full_auth = runner.user.auth_header();
    // the runner has no image, and their full token uploads one
    let (status, _) = profile_image(&runner, reqwest::Method::GET, &full_auth, None).await?;
    is!(status, 404);
    let ours: &'static [u8] = b"THE-RUNNERS-IMAGE";
    let (status, _) = profile_image(&runner, reqwest::Method::POST, &full_auth, Some(ours)).await?;
    is!(status, 204);
    let (status, body) = profile_image(&runner, reqwest::Method::GET, &full_auth, None).await?;
    is!(status, 200);
    is!(body, ours.to_vec());
    // the run token can't replace or delete it, and the image is unchanged
    let theirs: &'static [u8] = b"A-RUN-TOKENS-IMAGE";
    let (status, _) =
        profile_image(&runner, reqwest::Method::POST, &run_auth, Some(theirs)).await?;
    is!(status, 401);
    let (status, _) = profile_image(&runner, reqwest::Method::DELETE, &run_auth, None).await?;
    is!(status, 401);
    let (status, body) = profile_image(&runner, reqwest::Method::GET, &full_auth, None).await?;
    is!(status, 200);
    is!(body, ours.to_vec());
    // the full token deletes it
    let (status, _) = profile_image(&runner, reqwest::Method::DELETE, &full_auth, None).await?;
    is!(status, 204);
    let (status, _) = profile_image(&runner, reqwest::Method::GET, &full_auth, None).await?;
    is!(status, 404);
    Ok(())
}

/// Build a group update that turns off file uploads
fn disable_files() -> GroupUpdate {
    // flip only the files flag
    GroupUpdate {
        allowed: GroupAllowedUpdate::default().disable_files(),
        ..GroupUpdate::default()
    }
}

/// With a run token the runner can't write tools, their instances, toolsets, agents, workflows,
/// model credentials, endpoints, profiles, legacy images, or pipelines, or run a pipeline; the
/// runner's full token still writes them, and entities holding data are not guarded
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn run_token_definition_writes() -> Result<(), Error> {
    // a K8s developer who owns G and is a user in A and C, with an Image tool and a pipeline
    let runner = runner("sponde", "faunus").await?;
    let user = runner.user.username.clone();
    let a = generators::god_group(
        &runner.admin,
        "fides",
        &runner.suffix,
        &GroupRoles::default().user(&user),
    )
    .await?;
    let c = generators::god_group(
        &runner.admin,
        "ceres",
        &runner.suffix,
        &GroupRoles::default().user(&user),
    )
    .await?;
    let g = runner.group.clone();
    let groups = [g.as_str()];
    let (tool, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.user.client).await?;
    let pipe = PipelineRequest::new(
        &g,
        generators::gen_string(16).to_lowercase(),
        serde_json::json!([["harvest"]]),
    )
    .stage_tool("harvest", tool.id);
    runner.user.client.pipelines.create(&pipe).await?;
    let toolset = runner
        .user
        .client
        .entities
        .create(generators::gen_toolset(
            &format!("kit-{}", runner.suffix),
            &groups,
            ToolSetEntityRequest::default(),
        ))
        .await?;
    let (_, cli) = runs::container_tool(&groups, image.id, &runner.admin).await?;
    let triage = runs::ready_workflow(
        "Triage",
        &groups,
        &[(&cli, ToolMechanism::Cli)],
        &runner.admin,
    )
    .await?;
    let credential = generators::model_credential(
        &groups,
        CredentialKind::ApiKey,
        Some(&format!("sk-{}", generators::gen_string(24).to_lowercase())),
        &runner.user.client,
    )
    .await?;
    // a workflow run in G on a file in A and G
    let content = generators::gen_string(2048);
    let input = sample_in(&content, &[&a, &g], &runner.admin).await?;
    let created = runner
        .user
        .client
        .reactions
        .run_workflow(
            triage.id(),
            &WorkflowRunRequest::new(RunInputs::default().sample(&input)),
        )
        .await?;
    let run = runner.user.client.reactions.get(&g, created.id).await?;
    let token = run_token_of(&runner.admin, &run).await?;
    is!(token.groups, sorted_strings(vec![a.clone(), g.clone()]));
    let scoped = token_client(&runner.admin, &token.token).await?;
    // a run token can't publish a tool version, create a tool or a direct tool instance,
    // deprecate an instance, or delete one, and nothing is written
    assert_status(
        scoped
            .entities
            .update(
                tool.id,
                generators::gen_tool_publish("2.0.0", &generators::gen_image_spec()),
            )
            .await,
        401,
        "a tool publish with a run token",
    )?;
    let unchanged = runner.admin.entities.get(tool.id).await?;
    let latest_before = match &unchanged.metadata {
        thorium::models::EntityMetadata::Tool(meta) => meta.latest,
        _ => None,
    };
    is!(latest_before, Some(image.id));
    assert_status(
        scoped
            .entities
            .create(generators::gen_tool(
                &groups,
                "1.0.0",
                &generators::gen_image_spec(),
            ))
            .await,
        401,
        "a tool create with a run token",
    )?;
    let direct = thorium::models::EntityRequest::new(
        "ignored",
        EntityMetadataRequest::ToolInstance(thorium::models::ToolInstanceEntityRequest::new(
            tool.id,
            "3.0.0",
            &generators::gen_image_spec(),
        )?),
        groups.to_vec(),
    );
    assert_status(
        scoped.entities.create(direct).await,
        401,
        "a direct tool instance create with a run token",
    )?;
    assert_status(
        scoped
            .entities
            .update(image.id, generators::gen_tool_deprecate(true))
            .await,
        401,
        "a tool instance deprecation with a run token",
    )?;
    assert_status(
        scoped.entities.delete(image.id).await,
        401,
        "a tool instance delete with a run token",
    )?;
    let kept = runner.admin.entities.get(image.id).await?;
    is!(
        matches!(
            &kept.metadata,
            thorium::models::EntityMetadata::ToolInstance(meta) if !meta.deprecated
        ),
        true
    );
    // the runner's full token publishes a new tool version, which becomes latest, and a
    // pipeline reaction then runs it
    runner
        .user
        .client
        .entities
        .update(
            tool.id,
            generators::gen_tool_publish("2.0.0", &generators::gen_image_spec()),
        )
        .await?;
    let published = runner.admin.entities.get(tool.id).await?;
    let latest = match &published.metadata {
        thorium::models::EntityMetadata::Tool(meta) => meta.latest,
        _ => None,
    };
    is!(latest.is_some() && latest != Some(image.id), true);
    let created = runner
        .user
        .client
        .reactions
        .create(&ReactionRequest::new(&g, &pipe.name).sample(&input))
        .await?;
    let reaction = runner.user.client.reactions.get(&g, created.id).await?;
    is!(Some(reaction.plan.stages[0][0].tool_instance), latest);
    // the run token can't change the pipeline's triggers or SLA or delete the pipeline
    let triggers = HashMap::from([("fresh".to_owned(), EventTrigger::NewSample)]);
    let refused = scoped
        .pipelines
        .update(
            &g,
            &pipe.name,
            &PipelineUpdate::default().triggers(triggers),
        )
        .await;
    assert_status(refused, 401, "a trigger change with a run token")?;
    let refused = scoped
        .pipelines
        .update(&g, &pipe.name, &PipelineUpdate::default().sla(1200))
        .await;
    assert_status(refused, 401, "an SLA change with a run token")?;
    assert_status(
        scoped.pipelines.delete(&g, &pipe.name).await,
        401,
        "a pipeline delete with a run token",
    )?;
    // the pipeline and its reaction are still there
    runner.user.client.pipelines.get(&g, &pipe.name).await?;
    runner.user.client.reactions.get(&g, created.id).await?;
    // a pipeline create, a stage change, and a pipeline reaction are refused
    let second = PipelineRequest::new(
        &g,
        generators::gen_string(16).to_lowercase(),
        serde_json::json!([["harvest"]]),
    )
    .stage_tool("harvest", tool.id);
    assert_status(
        scoped.pipelines.create(&second).await,
        401,
        "a pipeline create with a run token",
    )?;
    runner.user.client.pipelines.create(&second).await?;
    let stages = PipelineUpdate {
        order: Some(serde_json::json!([["harvest"], ["harvest"]])),
        ..PipelineUpdate::default()
    };
    assert_status(
        scoped.pipelines.update(&g, &second.name, &stages).await,
        401,
        "a stage change with a run token",
    )?;
    assert_status(
        scoped
            .reactions
            .create(&ReactionRequest::new(&g, &second.name))
            .await,
        401,
        "a pipeline reaction with a run token",
    )?;
    // a snapshot of an entity as the admin reads it, to show a refused write changed nothing
    let snapshot = |id: Uuid| {
        let admin = &runner.admin;
        async move {
            let entity = admin.entities.get(id).await?;
            Ok::<_, Error>(serde_json::to_value(&entity).expect("entities serialize"))
        }
    };
    // a toolset, the workflow's catalog, and a credential rotation are refused and change nothing
    let toolset_before = snapshot(toolset.id).await?;
    let selector = || EntityMetadataUpdate::ToolSet {
        read_only: None,
        selector: Some(thorium::models::ToolSetSelector::new("suite", "radare")),
        clear_selector: None,
        auto_instance: None,
        add_tools: Vec::new(),
        remove_tools: Vec::new(),
        extend_target_groups: false,
    };
    assert_status(
        scoped
            .entities
            .update(toolset.id, EntityUpdate::default().metadata(selector()))
            .await,
        401,
        "a toolset change with a run token",
    )?;
    is!(snapshot(toolset.id).await?, toolset_before);
    let catalog_before = snapshot(triage.workflow.id).await?;
    let entry = || EntityMetadataUpdate::WorkflowCatalog {
        entry_agent: Some(triage.agent.id),
        clear_entry_agent: None,
        auto_instance: None,
        limits: None,
        default_models: None,
        model_auth: None,
        add_agents: Vec::new(),
        remove_agents: Vec::new(),
        extend_target_groups: false,
    };
    assert_status(
        scoped
            .entities
            .update(
                triage.workflow.id,
                EntityUpdate::default().metadata(entry()),
            )
            .await,
        401,
        "a workflow catalog change with a run token",
    )?;
    is!(snapshot(triage.workflow.id).await?, catalog_before);
    let credential_before = snapshot(credential.id).await?;
    let rotated = format!("sk-{}", generators::gen_string(24).to_lowercase());
    let rotation = |secret: String| {
        EntityUpdate::default()
            .metadata(EntityMetadataUpdate::model_credential().credential_secret(secret))
    };
    assert_status(
        scoped
            .entities
            .update(credential.id, rotation(rotated.clone()))
            .await,
        401,
        "a credential rotation with a run token",
    )?;
    is!(snapshot(credential.id).await?, credential_before);
    // a toolset, an agent, a credential, an endpoint, and a profile can't be created, in the
    // token's groups or outside them, and a toolset or workflow can't be asked for an instance
    let kit = format!("kit2-{}", runner.suffix);
    assert_status(
        scoped
            .entities
            .create(generators::gen_toolset(
                &kit,
                &groups,
                ToolSetEntityRequest::default(),
            ))
            .await,
        401,
        "a toolset create with a run token",
    )?;
    let name = format!("lead-{}", generators::gen_string(8).to_lowercase());
    let req = AgentEntityRequest::new(generators::gen_agent_config("lead"));
    for target in [groups.as_slice(), &[c.as_str()][..]] {
        assert_status(
            scoped
                .entities
                .create(generators::gen_agent(&name, target, req.clone()))
                .await,
            401,
            "an agent create with a run token",
        )?;
    }
    let secret = ModelSecret::from(format!("sk-{}", generators::gen_string(24).to_lowercase()));
    let meta = ModelCredentialEntityRequest::new(CredentialKind::ApiKey, Some(secret));
    assert_status(
        scoped
            .entities
            .create(generators::gen_model_entity(
                &groups,
                EntityMetadataRequest::ModelCredential(meta),
            ))
            .await,
        401,
        "a credential create with a run token",
    )?;
    let endpoint = ModelEndpointEntityRequest::new(generators::gen_anthropic_provider());
    assert_status(
        scoped
            .entities
            .create(generators::gen_model_entity(
                &groups,
                EntityMetadataRequest::ModelEndpoint(endpoint),
            ))
            .await,
        401,
        "an endpoint create with a run token",
    )?;
    let profile =
        ModelProfileEntityRequest::new(Uuid::new_v4(), "a-model", ModelApi::AnthropicMessages);
    assert_status(
        scoped
            .entities
            .create(generators::gen_model_entity(
                &groups,
                EntityMetadataRequest::ModelProfile(profile),
            ))
            .await,
        401,
        "a profile create with a run token",
    )?;
    assert_status(
        scoped.entities.request_tool_set_instance(toolset.id).await,
        401,
        "a toolset instance request with a run token",
    )?;
    assert_status(
        scoped.entities.request_instance(triage.workflow.id).await,
        401,
        "a workflow instance request with a run token",
    )?;
    // an agent made with the full token can't be deleted with the run token
    let lead = runner
        .user
        .client
        .entities
        .create(generators::gen_agent(&name, &groups, req))
        .await?;
    assert_status(
        scoped.entities.delete(lead.id).await,
        401,
        "an agent delete with a run token",
    )?;
    runner.admin.entities.get(lead.id).await?;
    // a workflow instance can't be deleted with the run token either
    assert_status(
        scoped.entities.delete(triage.id()).await,
        401,
        "a workflow instance delete with a run token",
    )?;
    runner.admin.entities.get(triage.id()).await?;
    // nor a toolset instance, which an admin submits for the toolset
    let toolset_entity = runner.admin.entities.get(toolset.id).await?;
    let first = generators::gen_toolset_instance(&toolset_entity, 1, None, &[]);
    let made = runner
        .admin
        .entities
        .create(generators::gen_toolset_instance_req(&first))
        .await?;
    assert_status(
        scoped.entities.delete(made.id).await,
        401,
        "a toolset instance delete with a run token",
    )?;
    runner.admin.entities.get(made.id).await?;
    // an image delete is refused before the image is even looked up
    assert_status(
        scoped.images.delete(&g, "missing").await,
        401,
        "an image delete with a run token",
    )?;
    assert_status(
        runner.user.client.images.delete(&g, "missing").await,
        404,
        "an image delete with the full token",
    )?;
    // the runner's full token writes all of these: a toolset change, the catalog's entry
    // agent, a credential rotation, and a toolset
    runner
        .user
        .client
        .entities
        .update(toolset.id, EntityUpdate::default().metadata(selector()))
        .await?;
    runner
        .user
        .client
        .entities
        .update(
            triage.workflow.id,
            EntityUpdate::default().metadata(entry()),
        )
        .await?;
    runner
        .user
        .client
        .entities
        .update(credential.id, rotation(rotated))
        .await?;
    let created = runner
        .user
        .client
        .entities
        .create(generators::gen_toolset(
            &kit,
            &groups,
            ToolSetEntityRequest::default(),
        ))
        .await?;
    runner.admin.entities.get(created.id).await?;
    Ok(())
}

/// A retry of a failed workflow run mints its own token while the failed run's stays deleted;
/// a monitor's retry and a retry of an unknown reaction are refused
#[tokio::test]
async fn run_token_retry() -> Result<(), Error> {
    // a runner with a workflow, and a monitor of the runner's group
    let runner = runner("cyllene", "fides").await?;
    let triage = workflow(&runner, "Triage").await?;
    let monitor = generators::moon_user(&runner.admin, "mneme", &runner.suffix).await?;
    let update =
        GroupUpdate::default().monitors(GroupUsersUpdate::default().direct_add(&monitor.username));
    runner.admin.groups.update(&runner.group, &update).await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let client = &runner.user.client.reactions;
    // a failed run's token is deleted
    let created = client
        .run_workflow(
            triage.id(),
            &WorkflowRunRequest::new(RunInputs::default().sample(&sha256)),
        )
        .await?;
    let auth = runner.user.auth_header();
    runs::handle_reaction(
        &runner.user.client,
        &auth,
        &runner.group,
        created.id,
        "fail",
    )
    .await?;
    let failed = client.get(&runner.group, created.id).await?;
    assert_status(
        run_token_of(&runner.admin, &failed).await,
        404,
        "a failed run's token",
    )?;
    // the retry mints a token for itself and the failed run's stays deleted
    let retried = client.retry_workflow_run(triage.id(), failed.id).await?;
    let retry = client.get(&runner.group, retried.id).await?;
    is!(retry.parent, Some(failed.id));
    is!(retry.run_token.clone(), Some(format!("run-{}", retry.id)));
    is!(retry.run, Some(retry.id));
    let token = run_token_of(&runner.admin, &retry).await?;
    is!(
        token.run.as_ref().map(|scope| scope.reaction),
        Some(retry.id)
    );
    is!(
        stored_run_tokens(&runner.user.username).await,
        vec![format!("run-{}", retry.id)]
    );
    // a monitor of the run group can't retry it, and an unknown reaction isn't found
    let refused = monitor
        .client
        .reactions
        .retry_workflow_run(triage.id(), failed.id)
        .await;
    assert_status(refused, 401, "a monitor's retry")?;
    let refused = client.retry_workflow_run(triage.id(), Uuid::new_v4()).await;
    assert_status(refused, 404, "a retry of an unknown reaction")?;
    Ok(())
}

/// A tool run of an Image tool instance creates a `Tool` reaction with one stage running it on
/// samples, repos, or both; a run without inputs is refused; a deprecated instance still runs
#[tokio::test]
async fn tool_run_creates_reaction() -> Result<(), Error> {
    // a runner, an Image tool, a file, and a repo
    let runner = runner("io", "vulcan").await?;
    let groups = [runner.group.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let repo = generators::repos(&runner.group, 1, &runner.user.client)
        .await?
        .remove(0);
    let client = &runner.user.client.reactions;
    // samples only, repos only, and both are each accepted
    let inputs = [
        RunInputs::default().sample(&sha256),
        RunInputs::default().repo(RepoDependencyRequest::new(&repo.url)),
        RunInputs::default()
            .sample(&sha256)
            .repo(RepoDependencyRequest::new(&repo.url)),
    ];
    for input in inputs {
        let created = client
            .run_tool(image.id, &ToolRunRequest::new(input.clone()))
            .await?;
        let reaction = client.get(&runner.group, created.id).await?;
        // the reaction runs the tool instance as its only stage
        is!(
            matches!(
                reaction.source,
                ReactionSource::Tool { tool_instance, .. } if tool_instance == image.id
            ),
            true
        );
        is!(reaction.plan.stages.len(), 1);
        is!(reaction.plan.stages[0].len(), 1);
        is!(reaction.plan.stages[0][0].tool_instance, image.id);
        is!(reaction.group, runner.group.clone());
        is!(reaction.samples, input.samples);
        is!(reaction.repos.len(), input.repos.len());
        // its one job runs the tool instance
        let job = only_job(&runner.admin, &reaction).await?;
        is!(job.tool_instance, image.id);
        is!(job.creator, runner.user.username.clone());
    }
    // a run without files or repos is refused and creates nothing
    let before = reactions_of(&runner.user, &runner.group).await?;
    let empty = client
        .run_tool(image.id, &ToolRunRequest::new(RunInputs::default()))
        .await;
    assert_status(empty, 400, "a tool run without inputs")?;
    is!(
        reactions_of(&runner.user, &runner.group).await?.len(),
        before.len()
    );
    // a deprecated instance can still be run
    runner
        .admin
        .entities
        .update(image.id, generators::gen_tool_deprecate(true))
        .await?;
    let created = client
        .run_tool(
            image.id,
            &ToolRunRequest::new(RunInputs::default().sample(&sha256)),
        )
        .await?;
    let reaction = client.get(&runner.group, created.id).await?;
    is!(reaction.plan.stages[0][0].tool_instance, image.id);
    Ok(())
}

/// A run of a container tool plans the Image tool instance it pins, and runs of Thorium MCP,
/// native, and Harness tool instances are refused naming the kind, creating nothing
#[tokio::test]
async fn tool_run_kinds() -> Result<(), Error> {
    // a runner, an Image tool, a container tool pinning it, and three kinds that can't run
    let runner = runner("europa", "juno").await?;
    let groups = [runner.group.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let (_, boxed) = runs::container_tool(&groups, image.id, &runner.admin).await?;
    let (_, mcp) = generators::tool(
        &groups,
        "1.0.0",
        &generators::gen_agentic_spec(),
        &runner.admin,
    )
    .await?;
    let (_, native) = generators::tool(
        &groups,
        "1.0.0",
        &generators::gen_native_spec(),
        &runner.admin,
    )
    .await?;
    let (_, harness) = generators::tool(
        &groups,
        "1.0.0",
        &generators::gen_harness_spec(),
        &runner.admin,
    )
    .await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let req = ToolRunRequest::new(RunInputs::default().sample(&sha256));
    let client = &runner.user.client.reactions;
    // a container tool's run plans its pinned image and records the container tool
    let created = client.run_tool(boxed.id, &req).await?;
    let reaction = client.get(&runner.group, created.id).await?;
    is!(
        matches!(
            reaction.source,
            ReactionSource::Tool { tool_instance, .. } if tool_instance == boxed.id
        ),
        true
    );
    is!(reaction.plan.stages[0][0].tool_instance, image.id);
    is!(
        only_job(&runner.admin, &reaction).await?.tool_instance,
        image.id
    );
    // the kinds without a workload of their own are refused naming the kind
    let before = reactions_of(&runner.user, &runner.group).await?;
    for (entity, kind) in [
        (&mcp, "ThoriumMcp"),
        (&native, "HarnessNative"),
        (&harness, "Harness"),
    ] {
        let refused = client.run_tool(entity.id, &req).await;
        let Err(err) = refused else {
            return Err(Error::new(format!("a run of a {kind} tool was accepted")));
        };
        is!(err.status().map(|code| code.as_u16()), Some(400));
        let msg = err.msg().unwrap_or_default();
        is!(msg.contains(kind), true, format!("{kind}: {msg}"));
    }
    // nothing was created for any of them
    is!(reactions_of(&runner.user, &runner.group).await?, before);
    Ok(())
}

/// Tool runs of `BareMetal`, `Windows`, and `Kvm` tool instances are accepted and queue a job for
/// their tool instance, with no "not supported" refusal anywhere
#[tokio::test]
async fn tool_run_environments() -> Result<(), Error> {
    // a runner and a file
    let runner = runner("callisto", "neptune").await?;
    let groups = [runner.group.as_str()];
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let req = ToolRunRequest::new(RunInputs::default().sample(&sha256));
    for scaler in [
        ImageScaler::BareMetal,
        ImageScaler::Windows,
        ImageScaler::Kvm,
    ] {
        // a tool in this environment runs like any other
        let (_, image) = runs::image_tool(&groups, scaler, &runner.admin).await?;
        let created = runner
            .user
            .client
            .reactions
            .run_tool(image.id, &req)
            .await?;
        let reaction = runner
            .user
            .client
            .reactions
            .get(&runner.group, created.id)
            .await?;
        let job = only_job(&runner.admin, &reaction).await?;
        is!(job.tool_instance, image.id);
        is!(job.scaler, scaler);
    }
    Ok(())
}

/// Run inputs are checked as for reactions: an unseen file is a 401, a missing repo a 404, a
/// file seen only in another group is accepted, and a repo keeps its commitish
#[tokio::test]
async fn tool_run_inputs_authorized() -> Result<(), Error> {
    // a runner with a second group, a tool in the first, and an outsider's file
    let runner = runner("ganymede", "minerva").await?;
    let groups = [runner.group.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let other = generators::god_group(
        &runner.admin,
        "ceres",
        &runner.suffix,
        &GroupRoles::default().owner(&runner.user.username),
    )
    .await?;
    let hidden_group = generators::god_group(
        &runner.admin,
        "pluto",
        &runner.suffix,
        &GroupRoles::default(),
    )
    .await?;
    let hidden = sample(&hidden_group, &runner.admin).await?;
    let elsewhere = sample(&other, &runner.user.client).await?;
    let repo = generators::repos(&runner.group, 1, &runner.user.client)
        .await?
        .remove(0);
    let client = &runner.user.client.reactions;
    let before = reactions_of(&runner.user, &runner.group).await?;
    // a file the runner can't see in any group is refused
    let refused = client
        .run_tool(
            image.id,
            &ToolRunRequest::new(RunInputs::default().sample(&hidden)),
        )
        .await;
    assert_status(refused, 401, "a run on an unseen file")?;
    // a repo that doesn't exist is refused
    let missing = RepoDependencyRequest::new("provider.tld/nobody/nothing");
    let refused = client
        .run_tool(
            image.id,
            &ToolRunRequest::new(RunInputs::default().repo(missing)),
        )
        .await;
    assert_status(refused, 404, "a run on a missing repo")?;
    // neither refusal created anything
    is!(reactions_of(&runner.user, &runner.group).await?, before);
    // a file the runner sees only in another group is accepted
    let created = client
        .run_tool(
            image.id,
            &ToolRunRequest::new(RunInputs::default().sample(&elsewhere)),
        )
        .await?;
    let reaction = client.get(&runner.group, created.id).await?;
    is!(reaction.samples, vec![elsewhere.clone()]);
    // a repo with a commitish keeps it
    let pinned = RepoDependencyRequest::new(&repo.url).commitish("v1.2.3");
    let created = client
        .run_tool(
            image.id,
            &ToolRunRequest::new(RunInputs::default().repo(pinned)),
        )
        .await?;
    let reaction = client.get(&runner.group, created.id).await?;
    is!(reaction.repos[0].commitish.as_deref(), Some("v1.2.3"));
    Ok(())
}

/// A workflow run creates one `WorkflowRun` reaction running the entry agent's harness, with the
/// run's tags, its result key, and the run, agent, and tools files
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn workflow_run_creates_reaction() -> Result<(), Error> {
    // a runner, a ready workflow with a tool bound each way, and a file
    let runner = runner("amalthea", "mars").await?;
    let groups = [runner.group.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let (_, cli) = runs::container_tool(&groups, image.id, &runner.admin).await?;
    let (_, mcp_boxed) = runs::container_tool(&groups, image.id, &runner.admin).await?;
    let (_, api) = generators::tool(
        &groups,
        "1.0.0",
        &generators::gen_agentic_spec(),
        &runner.admin,
    )
    .await?;
    let (_, mcp) = generators::tool(
        &groups,
        "1.0.0",
        &generators::gen_agentic_spec(),
        &runner.admin,
    )
    .await?;
    let triage = runs::ready_workflow(
        "Triage",
        &groups,
        &[
            (&cli, ToolMechanism::Cli),
            (&mcp_boxed, ToolMechanism::Mcp),
            (&api, ToolMechanism::ThoriumApi),
            (&mcp, ToolMechanism::Mcp),
        ],
        &runner.admin,
    )
    .await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let client = &runner.user.client.reactions;
    // run it on the file
    let req = WorkflowRunRequest::new(RunInputs::default().sample(&sha256)).tag("moons");
    let created = client.run_workflow(triage.id(), &req).await?;
    let reaction = client.get(&runner.group, created.id).await?;
    // the reaction runs the workflow instance with its harness as the only stage
    let ReactionSource::WorkflowRun(run) = &reaction.source else {
        return Err(Error::new(format!(
            "not a workflow run: {:?}",
            reaction.source
        )));
    };
    is!(run.workflow_instance, triage.id());
    is!(run.agent, triage.agent.id);
    is!(run.harness, triage.harness.id);
    is!(
        run.result_key,
        WorkflowResultKey::build(&triage.instance.name, triage.id())
    );
    is!(run.result_key, format!("Triage#r1~{}", triage.id()));
    is!(run.digest, instance_digest(&triage.instance)?);
    is!(reaction.plan.stages.len(), 1);
    is!(reaction.plan.stages[0][0].tool_instance, triage.harness.id);
    // it's tagged as a run of its workflow and instance, with the cast's input and creator tags
    for tag in [
        "WorkflowRun".to_owned(),
        format!("Workflow={}", triage.workflow.id),
        format!("WorkflowInstance={}", triage.id()),
        sha256.clone(),
        runner.user.username.clone(),
        "moons".to_owned(),
    ] {
        is!(reaction.tags.contains(&tag), true, tag);
    }
    // its job runs the harness as part of the run
    let job = only_job(&runner.admin, &reaction).await?;
    is!(job.tool_instance, triage.harness.id);
    is!(job.run, Some(reaction.id));
    // the run file holds the run's inputs, identity, and limits
    let file: HarnessRunFile = buffer(&runner.user.client, &reaction, "run.json").await?;
    is!(file.reaction, reaction.id);
    is!(file.samples, vec![sha256.clone()]);
    is!(file.workflow_instance, triage.id());
    is!(file.digest, run.digest.clone());
    is!(file.agent, triage.agent.id);
    is!(file.max_result_bytes, 0);
    // and the entry agent's resolved model, as the reaction records it
    is!(file.models.len(), 1);
    is!(file.models[0].model_id.clone(), runs::TEST_MODEL.to_owned());
    is!(
        file.models[0].endpoint,
        ModelConfigRef::Group(triage.endpoint.id)
    );
    is!(file.models.clone(), bindings_of(&reaction));
    // the agent file holds the entry agent's snapshot with its bindings
    let agents: HarnessAgentFile = buffer(&runner.user.client, &reaction, "agent.json").await?;
    is!(agents.agent.agent, triage.agent.id);
    is!(agents.agent.bindings.len(), 4);
    is!(agents.sub_agents.len(), 0);
    // the tools file holds one entry per bound tool with its invocation
    let tools: BTreeMap<Uuid, HarnessToolEntry> =
        buffer(&runner.user.client, &reaction, "tools.json").await?;
    is!(tools.len(), 4);
    let ToolInvocation::Cli { command } = &tools[&cli.id].invocation else {
        return Err(Error::new("the CLI tool has no CLI invocation".to_owned()));
    };
    is!(
        command[..4].to_vec(),
        vec![
            "/opt/thorium/thorium-agent".to_owned(),
            "tool".to_owned(),
            "run".to_owned(),
            cli.id.to_string()
        ]
    );
    is!(command[4], "--timeout");
    let ToolInvocation::Mcp { server_url, tool } = &tools[&mcp_boxed.id].invocation else {
        return Err(Error::new(
            "the MCP container tool has no MCP invocation".to_owned(),
        ));
    };
    is!(server_url.ends_with("/api/mcp"), true);
    is!(tool, "run_tool");
    is!(
        tools[&mcp_boxed.id]
            .model_description
            .contains(&mcp_boxed.id.to_string()),
        true
    );
    let is_api = matches!(
        &tools[&api.id].invocation,
        ToolInvocation::ThoriumApi { tool_instance, .. } if *tool_instance == api.id
    );
    is!(is_api, true);
    let ToolInvocation::Mcp { tool, .. } = &tools[&mcp.id].invocation else {
        return Err(Error::new("the MCP tool has no MCP invocation".to_owned()));
    };
    is!(tool, "get_sample");
    // each entry carries the tool's full descriptor from its spec
    for entity in [&cli, &mcp_boxed, &api, &mcp] {
        let Some(ToolSpec::Agentic(spec)) = instance_spec(entity) else {
            return Err(Error::new(format!("{} isn't an agentic tool", entity.name)));
        };
        let entry = &tools[&entity.id];
        is!(entry.function_name, spec.function_name.clone());
        is!(entry.title, spec.title.clone());
        is!(entry.output_schema, spec.output_schema.clone());
        is!(entry.input_examples, spec.input_examples.clone());
        is!(entry.strict, spec.strict);
        is!(entry.loading, spec.loading);
        is!(entry.icons, spec.icons.clone());
        is!(entry.requires_approval, spec.requires_approval);
        is!(entry.annotations, ToolAnnotations::from(&spec));
        is!(entry.portability, spec.portability());
        is!(entry.timeout, spec.timeout);
    }
    // a renamed instance gives new runs the new key while earlier runs keep theirs
    runner
        .admin
        .entities
        .update(
            triage.id(),
            thorium::models::EntityUpdate::default().name("Triage Renamed#r1"),
        )
        .await?;
    let renamed = client.run_workflow(triage.id(), &req).await?;
    let renamed = client.get(&runner.group, renamed.id).await?;
    let renamed_key = format!("Triage Renamed#r1~{}", triage.id());
    is!(renamed.source.result_key(), Some(renamed_key.as_str()));
    let earlier = client.get(&runner.group, reaction.id).await?;
    let earlier_key = format!("Triage#r1~{}", triage.id());
    is!(earlier.source.result_key(), Some(earlier_key.as_str()));
    Ok(())
}

/// Get the spec of a tool instance entity
///
/// # Arguments
///
/// * `entity` - The tool instance entity
fn instance_spec(entity: &Entity) -> Option<ToolSpec> {
    // only tool instances have a spec
    match &entity.metadata {
        thorium::models::EntityMetadata::ToolInstance(meta) => Some(meta.spec.clone()),
        _ => None,
    }
}

/// Get the digest of a workflow instance entity
///
/// # Arguments
///
/// * `instance` - The workflow instance entity
fn instance_digest(instance: &Entity) -> Result<String, Error> {
    // only workflow instances have a digest
    match &instance.metadata {
        thorium::models::EntityMetadata::WorkflowInstance(meta) => Ok(meta.digest.clone()),
        _ => Err(Error::new("not a workflow instance".to_owned())),
    }
}

/// Workflow runs of an Invalid instance, without inputs, or of a hidden instance are refused; a
/// superseded Ready instance still runs under its own key
#[tokio::test]
async fn workflow_run_refusals() -> Result<(), Error> {
    // a runner with a ready workflow and an outsider
    let runner = runner("himalia", "apollo").await?;
    let triage = workflow(&runner, "Triage").await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let outsider = generators::moon_user(&runner.admin, "elara", &runner.suffix).await?;
    let req = WorkflowRunRequest::new(RunInputs::default().sample(&sha256));
    let client = &runner.user.client.reactions;
    let before = reactions_of(&runner.user, &runner.group).await?;
    // a run without inputs is refused
    let empty = client
        .run_workflow(triage.id(), &WorkflowRunRequest::new(RunInputs::default()))
        .await;
    assert_status(empty, 400, "a workflow run without inputs")?;
    // a user who can't see the instance gets a 404
    let hidden = outsider
        .client
        .reactions
        .run_workflow(triage.id(), &req)
        .await;
    assert_status(hidden, 404, "a workflow run of a hidden instance")?;
    // an Invalid instance is refused
    let mut invalid = generators::gen_workflow_instance(
        &triage.workflow,
        2,
        Some(triage.id()),
        &[(&triage.agent, &[&triage.tools[0], &triage.harness])],
    );
    invalid.status = thorium::models::WorkflowInstanceStatus::Invalid {
        errors: vec!["broken".to_owned()],
    };
    let invalid_id = runner
        .admin
        .entities
        .create(generators::gen_workflow_instance_req(&invalid))
        .await?
        .id;
    let refused = client.run_workflow(invalid_id, &req).await;
    assert_status(refused, 400, "a workflow run of an Invalid instance")?;
    // none of the refusals created anything
    is!(reactions_of(&runner.user, &runner.group).await?, before);
    // a superseded Ready instance still runs and records its own key
    let r2 = runs::submit_instance(
        &triage.workflow,
        3,
        Some(invalid_id),
        &triage.agent,
        &triage.harness,
        &[(&triage.tools[0], ToolMechanism::Cli)],
        &runner.admin,
    )
    .await?;
    is!(r2.name, "Triage#r3");
    let created = client.run_workflow(triage.id(), &req).await?;
    let reaction = client.get(&runner.group, created.id).await?;
    let key = format!("Triage#r1~{}", triage.id());
    is!(reaction.source.result_key(), Some(key.as_str()));
    Ok(())
}

/// A started or completed workflow run can't be retried, and a failed one can
#[tokio::test]
async fn workflow_run_retry_needs_failed_run() -> Result<(), Error> {
    // a runner with a workflow and three runs of it
    let runner = runner("kale", "terminus").await?;
    let triage = Box::pin(workflow(&runner, "Triage")).await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let client = &runner.user.client.reactions;
    let req = WorkflowRunRequest::new(RunInputs::default().sample(&sha256));
    let mut ids = Vec::with_capacity(3);
    for _ in 0..3 {
        ids.push(client.run_workflow(triage.id(), &req).await?.id);
    }
    let (started, completed, failed) = (ids[0], ids[1], ids[2]);
    // the first run starts when its harness job is claimed
    let user = &runner.user.username;
    let harness = triage.harness.id;
    let worker =
        register_worker(&runner.admin, user, &runner.group, harness, Some(started)).await?;
    is!(
        claim(&runner.admin, &runner.group, harness, &worker)
            .await?
            .len(),
        1
    );
    is!(
        client.get(&runner.group, started).await?.status,
        ReactionStatus::Started
    );
    // the second run completes when it proceeds past its only stage
    runs::handle_reaction(
        &runner.user.client,
        &runner.user.auth_header(),
        &runner.group,
        completed,
        "proceed",
    )
    .await?;
    is!(
        client.get(&runner.group, completed).await?.status,
        ReactionStatus::Completed
    );
    // neither can be retried
    for (id, context) in [
        (started, "a retry of a started run"),
        (completed, "a retry of a completed run"),
    ] {
        let refused = client.retry_workflow_run(triage.id(), id).await;
        assert_status(refused, 400, context)?;
    }
    // the third run is retried once it fails
    runs::handle_reaction(
        &runner.user.client,
        &runner.user.auth_header(),
        &runner.group,
        failed,
        "fail",
    )
    .await?;
    let retried = client.retry_workflow_run(triage.id(), failed).await?;
    is!(
        client.get(&runner.group, retried.id).await?.parent,
        Some(failed)
    );
    Ok(())
}

/// A workflow run is refused in a group its harness isn't in
#[tokio::test]
async fn workflow_run_harness_outside_run_group() -> Result<(), Error> {
    // a runner with a second group and a workflow in both
    let runner = runner("sinope", "pomona").await?;
    let second = generators::god_group(
        &runner.admin,
        "flora",
        &runner.suffix,
        &GroupRoles::default().owner(&runner.user.username),
    )
    .await?;
    let groups = [runner.group.as_str(), second.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let (_, boxed) = runs::container_tool(&groups, image.id, &runner.admin).await?;
    let triage = Box::pin(runs::ready_workflow(
        "Triage",
        &groups,
        &[(&boxed, ToolMechanism::Cli)],
        &runner.admin,
    ))
    .await?;
    // the harness tool leaves the second group
    let thorium::models::EntityMetadata::ToolInstance(harness) = &triage.harness.metadata else {
        return Err(Error::new("the harness is a tool instance".to_owned()));
    };
    runner
        .admin
        .entities
        .update(
            harness.tool,
            thorium::models::EntityUpdate::default().remove_group(&second),
        )
        .await?;
    // a run in the second group is refused and queues nothing
    let sha256 = sample(&second, &runner.user.client).await?;
    let req = WorkflowRunRequest::new(RunInputs::default().sample(&sha256)).group(&second);
    let refused = runner
        .user
        .client
        .reactions
        .run_workflow(triage.id(), &req)
        .await;
    assert_status(refused, 404, "a run whose harness isn't in the run group")?;
    is!(reactions_of(&runner.user, &second).await?.len(), 0);
    // a run in the first group is queued
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let req = WorkflowRunRequest::new(RunInputs::default().sample(&sha256)).group(&runner.group);
    runner
        .user
        .client
        .reactions
        .run_workflow(triage.id(), &req)
        .await?;
    Ok(())
}

/// Only a failed workflow run can be retried; the retry is a new run on the same inputs in the
/// same group, linked to the failed run as its parent
#[tokio::test]
async fn workflow_run_retry() -> Result<(), Error> {
    // a runner with a second group and a workflow in both
    let runner = runner("thebe", "diana").await?;
    let second = generators::god_group(
        &runner.admin,
        "faunus",
        &runner.suffix,
        &GroupRoles::default().owner(&runner.user.username),
    )
    .await?;
    let groups = [runner.group.as_str(), second.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let (_, boxed) = runs::container_tool(&groups, image.id, &runner.admin).await?;
    let triage = runs::ready_workflow(
        "Triage",
        &groups,
        &[(&boxed, ToolMechanism::Cli)],
        &runner.admin,
    )
    .await?;
    let sha256 = sample(&second, &runner.user.client).await?;
    let repo = generators::repos(&second, 1, &runner.user.client)
        .await?
        .remove(0);
    let client = &runner.user.client.reactions;
    // run it in the second group on a file and a repo
    let inputs = RunInputs::default()
        .sample(&sha256)
        .repo(RepoDependencyRequest::new(&repo.url).commitish("main"));
    let req = WorkflowRunRequest::new(inputs).group(&second).tag("moons");
    let first = client.run_workflow(triage.id(), &req).await?;
    // a run that hasn't failed can't be retried
    let refused = client.retry_workflow_run(triage.id(), first.id).await;
    assert_status(refused, 400, "a retry of a run that hasn't failed")?;
    // fail it, then retry it
    runs::handle_reaction(
        &runner.user.client,
        &runner.user.auth_header(),
        &second,
        first.id,
        "fail",
    )
    .await?;
    let failed = client.get(&second, first.id).await?;
    is!(failed.status, ReactionStatus::Failed);
    let retried = client.retry_workflow_run(triage.id(), first.id).await?;
    let retry = client.get(&second, retried.id).await?;
    // the retry is a new run of the same instance on the same inputs in the same group
    is!(retry.parent, Some(first.id));
    is!(retry.group, second.clone());
    is!(retry.samples, failed.samples.clone());
    is!(retry.repos[0].url, repo.url.clone());
    is!(retry.repos[0].commitish.as_deref(), Some("main"));
    let key = format!("Triage#r1~{}", triage.id());
    is!(retry.source.result_key(), Some(key.as_str()));
    is!(retry.tags.contains(&"moons".to_owned()), true);
    is!(retry.status, ReactionStatus::Created);
    // a reaction that isn't a run of this instance can't be retried through it
    let other = workflow(&runner, "Other").await?;
    let refused = client.retry_workflow_run(other.id(), first.id).await;
    assert_status(refused, 404, "a retry through another instance")?;
    Ok(())
}

/// Run reactions are listed by their instance, their workflow across revisions, and as
/// workflow runs, per group and across groups
#[tokio::test]
async fn workflow_run_tags() -> Result<(), Error> {
    // a runner with a workflow at two revisions, a tool, and a pipeline reaction
    let runner = runner("adrastea", "mercury").await?;
    let triage = workflow(&runner, "Triage").await?;
    let r2 = runs::submit_instance(
        &triage.workflow,
        2,
        Some(triage.id()),
        &triage.agent,
        &triage.harness,
        &[(&triage.tools[0], ToolMechanism::Cli)],
        &runner.admin,
    )
    .await?;
    let groups = [runner.group.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let client = &runner.user.client.reactions;
    let inputs = RunInputs::default().sample(&sha256);
    let first = client
        .run_workflow(triage.id(), &WorkflowRunRequest::new(inputs.clone()))
        .await?;
    let second = client
        .run_workflow(r2.id, &WorkflowRunRequest::new(inputs.clone()))
        .await?;
    let tool = client
        .run_tool(image.id, &ToolRunRequest::new(inputs))
        .await?;
    // list a tag in the runner's group
    let list = |tag: String| {
        let url = format!(
            "{}/api/reactions/tag/{}/{tag}/",
            runner.user.client.host, runner.group
        );
        let auth = runner.user.auth_header();
        async move {
            let resp: serde_json::Value = reqwest::Client::new()
                .get(url)
                .header("authorization", auth)
                .send()
                .await?
                .json()
                .await?;
            let mut names = resp["names"].as_array().map_or_else(Vec::new, |names| {
                names
                    .iter()
                    .filter_map(|name| name.as_str().map(ToOwned::to_owned))
                    .collect::<Vec<String>>()
            });
            names.sort();
            Ok::<Vec<String>, Error>(names)
        }
    };
    let sorted = |mut ids: Vec<String>| {
        ids.sort();
        ids
    };
    // the instance tag lists exactly that instance's runs
    is!(
        list(format!("WorkflowInstance={}", triage.id())).await?,
        vec![first.id.to_string()]
    );
    // the workflow tag lists the runs of every revision
    is!(
        list(format!("Workflow={}", triage.workflow.id)).await?,
        sorted(vec![first.id.to_string(), second.id.to_string()])
    );
    // the workflow run tag lists only workflow runs
    let runs = list("WorkflowRun".to_owned()).await?;
    is!(
        runs,
        sorted(vec![first.id.to_string(), second.id.to_string()])
    );
    is!(runs.contains(&tool.id.to_string()), false);
    // the cross group listing returns only ids
    let opts = ReactionCursorOpts::default().group(&runner.group);
    let cursor = client.list_tag_new("WorkflowRun", &opts).await?;
    let listed = sorted(cursor.data);
    is!(listed, runs);
    Ok(())
}

/// Workflow run results are stored under the instance's name and id, so two revisions of one
/// workflow and two workflows with the same name and revision each keep their own results on a file
#[tokio::test]
async fn workflow_run_result_keys() -> Result<(), Error> {
    // a runner with a workflow at two revisions and a second workflow of the same name
    let runner = runner("metis", "bacchus").await?;
    let triage = workflow(&runner, "Triage").await?;
    let r2 = runs::submit_instance(
        &triage.workflow,
        2,
        Some(triage.id()),
        &triage.agent,
        &triage.harness,
        &[(&triage.tools[0], ToolMechanism::Cli)],
        &runner.admin,
    )
    .await?;
    let twin = workflow(&runner, "Triage").await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let client = &runner.user.client.reactions;
    let req = WorkflowRunRequest::new(RunInputs::default().sample(&sha256));
    // run both revisions and the twin on one file
    let mut keys = Vec::new();
    for instance in [triage.id(), r2.id, twin.id()] {
        let created = client.run_workflow(instance, &req).await?;
        let reaction = client.get(&runner.group, created.id).await?;
        let key = reaction.source.result_key().unwrap_or_default().to_owned();
        keys.push(key);
    }
    is!(
        keys,
        vec![
            format!("Triage#r1~{}", triage.id()),
            format!("Triage#r2~{}", r2.id),
            format!("Triage#r1~{}", twin.id()),
        ]
    );
    // the twin shares the first workflow's name and revision but not its key
    assert_ne!(keys[0], keys[2]);
    // each key names the instance it came from and never holds a path separator
    for (key, instance) in keys.iter().zip([triage.id(), r2.id, twin.id()]) {
        let parsed = WorkflowResultKey::parse(key).expect("a workflow run's key");
        is!(parsed.instance, instance);
        assert!(!key.contains('/'), "{key}");
    }
    // the results of each run are stored and read back alone under their keys
    for key in &keys {
        agent_result(&runner.user.client, &sha256, key).await?;
    }
    for key in &keys {
        let params = ResultGetParams::default().tool(key);
        let results = runner
            .user
            .client
            .files
            .get_results(&sha256, &params)
            .await?;
        is!(
            results.results.keys().cloned().collect::<Vec<String>>(),
            vec![key.to_owned()]
        );
    }
    // a repo's results are stored and read back under a full key too
    let repo = generators::repos(&runner.group, 1, &runner.user.client)
        .await?
        .remove(0);
    let req = OutputRequest::<Repo>::new(
        repo.url.clone(),
        keys[0].as_str(),
        "{\"answer\": \"repo\"}",
        OutputDisplayType::Json,
    );
    runner.user.client.repos.create_result(req).await?;
    let params = ResultGetParams::default().tool(&keys[0]);
    let results = runner
        .user
        .client
        .repos
        .get_results(&repo.url, &params)
        .await?;
    is!(
        results.results.keys().cloned().collect::<Vec<String>>(),
        vec![keys[0].clone()]
    );
    Ok(())
}

/// Analysts, users, managers, and owners of the run group can start workflow runs and tool runs
/// there; monitors and groups that disallow reactions are refused, creating nothing
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn run_group_roles() -> Result<(), Error> {
    // one developer per group role, an analyst, and a monitor
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let mut members = Vec::new();
    for moon in ["io", "europa", "ganymede", "callisto", "amalthea"] {
        let name = generators::readable_name(moon, &suffix);
        let role = if moon == "amalthea" {
            UserRole::Analyst
        } else {
            DEVELOPER
        };
        members.push(generators::named_user_with_role(&admin, &name, role).await?);
    }
    let roles = GroupRoles::default()
        .owner(&members[0].username)
        .manager(&members[1].username)
        .user(&members[2].username)
        .monitor(&members[3].username)
        .user(&members[4].username);
    let group = generators::god_group(&admin, "janus", &suffix, &roles).await?;
    let closed_roles = GroupRoles::default().owner(&members[0].username);
    let closed_name = generators::readable_name("vesta", &suffix);
    let closed = generators::named_group_with_roles(
        &closed_name,
        &closed_roles,
        GroupAllowed {
            reactions: false,
            ..GroupAllowed::default()
        },
        &admin,
    )
    .await?;
    // a workflow and an Image tool in the open group, and another tool in the closed group
    let groups = [group.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &admin).await?;
    let (_, boxed) = runs::container_tool(&groups, image.id, &admin).await?;
    let triage =
        runs::ready_workflow("Triage", &groups, &[(&boxed, ToolMechanism::Cli)], &admin).await?;
    let (_, closed_image) = runs::image_tool(&[closed.as_str()], ImageScaler::K8s, &admin).await?;
    let sha256 = sample(&group, &admin).await?;
    let inputs = RunInputs::default().sample(&sha256);
    // owners, managers, users, and analysts each start both kinds of runs in the group
    for member in [&members[0], &members[1], &members[2], &members[4]] {
        let client = &member.client.reactions;
        let run = client
            .run_workflow(triage.id(), &WorkflowRunRequest::new(inputs.clone()))
            .await?;
        is!(client.get(&group, run.id).await?.group, group.clone());
        let run = client
            .run_tool(image.id, &ToolRunRequest::new(inputs.clone()))
            .await?;
        is!(client.get(&group, run.id).await?.group, group.clone());
    }
    // a monitor is refused both and creates nothing
    let monitor = &members[3];
    let refused = monitor
        .client
        .reactions
        .run_workflow(triage.id(), &WorkflowRunRequest::new(inputs.clone()))
        .await;
    assert_status(refused, 401, "a monitor's workflow run")?;
    let refused = monitor
        .client
        .reactions
        .run_tool(image.id, &ToolRunRequest::new(inputs.clone()))
        .await;
    assert_status(refused, 401, "a monitor's tool run")?;
    is!(reactions_of(monitor, &group).await?.len(), 0);
    // a group that disallows reactions refuses its owner's run and creates nothing
    let closed_sha = sample(&closed, &admin).await?;
    let refused = members[0]
        .client
        .reactions
        .run_tool(
            closed_image.id,
            &ToolRunRequest::new(RunInputs::default().sample(&closed_sha)),
        )
        .await;
    assert_status(refused, 401, "a run in a group that disallows reactions")?;
    is!(reactions_of(&members[0], &closed).await?.len(), 0);
    Ok(())
}

/// A run's job is created as the runner
#[tokio::test]
async fn run_jobs_as_runner() -> Result<(), Error> {
    // a runner with a workflow and an Image tool
    let runner = runner("sinope", "ops").await?;
    let triage = workflow(&runner, "Triage").await?;
    let groups = [runner.group.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let inputs = RunInputs::default().sample(&sha256);
    let client = &runner.user.client.reactions;
    // both kinds of runs create their job as the runner
    let workflow_run = client
        .run_workflow(triage.id(), &WorkflowRunRequest::new(inputs.clone()))
        .await?;
    let tool_run = client
        .run_tool(image.id, &ToolRunRequest::new(inputs))
        .await?;
    for id in [workflow_run.id, tool_run.id] {
        let reaction = client.get(&runner.group, id).await?;
        is!(reaction.creator, runner.user.username.clone());
        let job = only_job(&runner.admin, &reaction).await?;
        is!(job.creator, runner.user.username.clone());
    }
    Ok(())
}

/// The run group is the only candidate when there's one, a 401 with a reason per visible group
/// when there's none, a 400 listing them when there are several, and the requested group when
/// it's one of the instance's groups the runner can run in
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn run_group_selection() -> Result<(), Error> {
    // a runner who owns two groups and monitors a third, and a fourth group they aren't in
    let runner = runner("leda", "saturn").await?;
    let second = generators::god_group(
        &runner.admin,
        "quirinus",
        &runner.suffix,
        &GroupRoles::default().owner(&runner.user.username),
    )
    .await?;
    let watched = generators::god_group(
        &runner.admin,
        "terminus",
        &runner.suffix,
        &GroupRoles::default().monitor(&runner.user.username),
    )
    .await?;
    let hidden = generators::god_group(
        &runner.admin,
        "orcus",
        &runner.suffix,
        &GroupRoles::default(),
    )
    .await?;
    let outside = generators::god_group(
        &runner.admin,
        "fortuna",
        &runner.suffix,
        &GroupRoles::default().owner(&runner.user.username),
    )
    .await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let inputs = RunInputs::default().sample(&sha256);
    let client = &runner.user.client.reactions;
    // a tool in one runnable group, the monitored group, and the hidden group uses the one
    let (_, single) = runs::image_tool(
        &[runner.group.as_str(), watched.as_str(), hidden.as_str()],
        ImageScaler::K8s,
        &runner.admin,
    )
    .await?;
    let created = client
        .run_tool(single.id, &ToolRunRequest::new(inputs.clone()))
        .await?;
    is!(
        client.get(&runner.group, created.id).await?.group,
        runner.group.clone()
    );
    // a tool only in groups the runner can't run in is a 401 naming only the visible group
    let (_, none) = runs::image_tool(
        &[watched.as_str(), hidden.as_str()],
        ImageScaler::K8s,
        &runner.admin,
    )
    .await?;
    let refused = client
        .run_tool(none.id, &ToolRunRequest::new(inputs.clone()))
        .await;
    let Err(err) = refused else {
        return Err(Error::new(
            "a run with no candidate group was accepted".to_owned(),
        ));
    };
    is!(err.status().map(|code| code.as_u16()), Some(401));
    let msg = err.msg().unwrap_or_default();
    is!(msg.contains(&watched), true, msg.clone());
    is!(msg.contains(&hidden), false, msg);
    // a workflow in two runnable groups needs a group
    let groups = [runner.group.as_str(), second.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
    let (_, boxed) = runs::container_tool(&groups, image.id, &runner.admin).await?;
    let triage = runs::ready_workflow(
        "Triage",
        &groups,
        &[(&boxed, ToolMechanism::Cli)],
        &runner.admin,
    )
    .await?;
    let refused = client
        .run_workflow(triage.id(), &WorkflowRunRequest::new(inputs.clone()))
        .await;
    let Err(err) = refused else {
        return Err(Error::new(
            "a run with two candidate groups was accepted".to_owned(),
        ));
    };
    is!(err.status().map(|code| code.as_u16()), Some(400));
    let msg = err.msg().unwrap_or_default();
    is!(
        msg.contains(&runner.group) && msg.contains(&second),
        true,
        msg
    );
    // with a group it runs there
    let created = client
        .run_workflow(
            triage.id(),
            &WorkflowRunRequest::new(inputs.clone()).group(&second),
        )
        .await?;
    is!(client.get(&second, created.id).await?.group, second.clone());
    let created = client
        .run_tool(
            image.id,
            &ToolRunRequest::new(inputs.clone()).group(&runner.group),
        )
        .await?;
    is!(
        client.get(&runner.group, created.id).await?.group,
        runner.group.clone()
    );
    // a group that isn't one of the instance's is refused
    let refused = client
        .run_workflow(
            triage.id(),
            &WorkflowRunRequest::new(inputs.clone()).group(&outside),
        )
        .await;
    assert_status(refused, 400, "a run group that isn't the instance's")?;
    // a group the runner can't run in is refused
    let (_, watched_tool) = runs::image_tool(
        &[runner.group.as_str(), watched.as_str()],
        ImageScaler::K8s,
        &runner.admin,
    )
    .await?;
    let refused = client
        .run_tool(
            watched_tool.id,
            &ToolRunRequest::new(inputs.clone()).group(&watched),
        )
        .await;
    assert_status(refused, 401, "a run group the runner only monitors")?;
    // a tool run with a parent runs in the parent's group whatever group it names
    let parent = client
        .run_tool(
            image.id,
            &ToolRunRequest::new(inputs.clone()).group(&second),
        )
        .await?;
    let child = client
        .run_tool(
            image.id,
            &ToolRunRequest::new(inputs)
                .group(&runner.group)
                .parent(parent.id),
        )
        .await?;
    let child = client.get(&second, child.id).await?;
    is!(child.group, second);
    is!(child.parent, Some(parent.id));
    Ok(())
}

/// The runner must be a developer for every workload a run may spawn: the harness and the image
/// behind each container tool for a workflow run, and the image for a tool run
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn run_developer_checks() -> Result<(), Error> {
    // a K8s developer, a K8s and BareMetal developer, a BareMetal developer, a plain user, and
    // a developer without the External role, all in one group
    let admin = test_utilities::admin_client().await?;
    let suffix = generators::name_suffix();
    let bare_metal_only = UserRole::Developer {
        k8s: false,
        bare_metal: true,
        windows: false,
        external: false,
        kvm: false,
    };
    let k8s_and_bare_metal = UserRole::Developer {
        k8s: true,
        bare_metal: true,
        windows: false,
        external: false,
        kvm: false,
    };
    let mut members = Vec::new();
    for (moon, role) in [
        ("io", K8S_DEVELOPER),
        ("europa", k8s_and_bare_metal),
        ("ganymede", bare_metal_only),
        ("callisto", UserRole::User),
    ] {
        let name = generators::readable_name(moon, &suffix);
        members.push(generators::named_user_with_role(&admin, &name, role).await?);
    }
    let roles = members.iter().fold(GroupRoles::default(), |roles, member| {
        roles.user(&member.username)
    });
    let group = generators::god_group(&admin, "jupiter", &suffix, &roles).await?;
    let groups = [group.as_str()];
    // a K8s harness whose agent calls a container tool pinning a BareMetal image
    let (_, bare_metal) = runs::image_tool(&groups, ImageScaler::BareMetal, &admin).await?;
    let (_, boxed) = runs::container_tool(&groups, bare_metal.id, &admin).await?;
    let triage =
        runs::ready_workflow("Triage", &groups, &[(&boxed, ToolMechanism::Cli)], &admin).await?;
    let (_, external) = runs::image_tool(&groups, ImageScaler::External, &admin).await?;
    let (_, k8s) = runs::image_tool(&groups, ImageScaler::K8s, &admin).await?;
    let sha256 = sample(&group, &admin).await?;
    let inputs = RunInputs::default().sample(&sha256);
    let workflow_req = WorkflowRunRequest::new(inputs.clone());
    let tool_req = ToolRunRequest::new(inputs);
    // a K8s-only developer is refused, naming the BareMetal scaler
    let refused = members[0]
        .client
        .reactions
        .run_workflow(triage.id(), &workflow_req)
        .await;
    let Err(err) = refused else {
        return Err(Error::new(
            "a K8s-only developer ran a BareMetal tool".to_owned(),
        ));
    };
    is!(err.status().map(|code| code.as_u16()), Some(401));
    let msg = err.msg().unwrap_or_default();
    is!(msg.contains("BareMetal"), true, msg);
    // a K8s and BareMetal developer runs it
    let created = members[1]
        .client
        .reactions
        .run_workflow(triage.id(), &workflow_req)
        .await?;
    is!(
        members[1]
            .client
            .reactions
            .get(&group, created.id)
            .await?
            .group,
        group.clone()
    );
    // a runner who isn't a K8s developer is refused for the harness
    let refused = members[2]
        .client
        .reactions
        .run_workflow(triage.id(), &workflow_req)
        .await;
    let Err(err) = refused else {
        return Err(Error::new(
            "a BareMetal-only developer ran a K8s harness".to_owned(),
        ));
    };
    is!(err.status().map(|code| code.as_u16()), Some(401));
    is!(err.msg().unwrap_or_default().contains("K8s"), true);
    // an External tool needs the External role
    let refused = members[1]
        .client
        .reactions
        .run_tool(external.id, &tool_req)
        .await;
    assert_status(
        refused,
        401,
        "an External tool run without the External role",
    )?;
    // a plain user can't run a K8s tool
    let refused = members[3]
        .client
        .reactions
        .run_tool(k8s.id, &tool_req)
        .await;
    assert_status(refused, 401, "a plain user's K8s tool run")?;
    // none of the refused runners created anything
    for member in [&members[0], &members[2], &members[3]] {
        is!(reactions_of(member, &group).await?.len(), 0);
    }
    is!(reactions_of(&members[1], &group).await?.len(), 1);
    Ok(())
}

/// The environment variable that marks the child process running the logging scenario
///
/// Its value is the directory the child writes the secrets it used to.
const TRACE_CHILD_DIR: &str = "THORIUM_TRACE_CHILD_DIR";

/// The longest the logging scenario may take, in seconds
const TRACE_CHILD_SECONDS: u64 = 900;

/// A child process that is killed when the test that started it ends, however it ends
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    /// Kill and reap the child
    fn drop(&mut self) {
        // stop the child and wait for it so none is left running
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Run the scenario whose logs [`secrets_stay_out_of_logs`] checks
///
/// This only runs as a child of that test, which gives it a namespace, port, and log level of
/// its own so the API it starts logs everything to stdout.
#[tokio::test]
#[ignore = "run by secrets_stay_out_of_logs in a child process"]
async fn trace_scenario_child() -> Result<(), Error> {
    // only run when the parent test started this process
    let Ok(dir) = std::env::var(TRACE_CHILD_DIR) else {
        return Ok(());
    };
    // a runner with a group credential and endpoint, an own key for it, and a model workflow
    let runner = runner("themisto", "sol").await?;
    let groups = [runner.group.as_str()];
    let credential = generators::model_credential(
        &groups,
        CredentialKind::ApiKey,
        Some(GROUP_SECRET),
        &runner.admin,
    )
    .await?;
    let endpoint = model_endpoint(
        &runner,
        "logged-openai",
        openai_at_random(),
        Some(credential.id),
        &["gpt-5"],
    )
    .await?;
    let own = UserCredentialRef::new(CredentialKind::ApiKey, Some(USER_SECRET.into()));
    set_models(
        &runner.user.client,
        UserModelSettingsUpdate::default().credential(endpoint.id, own),
    )
    .await?;
    // the group credential is rotated, read back, and read with its secret
    let rotate = EntityMetadataUpdate::model_credential().credential_secret(ROTATED_SECRET);
    runner
        .admin
        .entities
        .update(credential.id, EntityUpdate::default().metadata(rotate))
        .await?;
    runner.admin.entities.get(credential.id).await?;
    runner
        .admin
        .entities
        .model_credential_secret(credential.id)
        .await?;
    // a run binds the models, and the admin reads its token and its models with their secrets
    let env = HarnessEnv {
        export: [HarnessEnvVar::OpenAiApiKey, HarnessEnvVar::ModelId]
            .into_iter()
            .collect(),
        names: BTreeMap::new(),
    };
    let flow = runs::model_workflow(
        "Triage",
        &groups,
        vec![runs::ModelAgent::new(&["gpt-5"])],
        None,
        (&runs::gen_model_harness_spec(), Some(&env)),
        &runner.admin,
    )
    .await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let run = run_instance(&runner, flow.id(), &RunInputs::default().sample(&sha256)).await?;
    let token = run_token_of(&runner.admin, &run).await?;
    runner
        .admin
        .reactions
        .run_models(&run.group, run.id)
        .await?;
    // the runner reads their own user record
    runner.user.client.users.info().await?;
    // report the secrets that must not have been logged
    let secrets = [GROUP_SECRET, USER_SECRET, ROTATED_SECRET, &token.token];
    std::fs::write(
        std::path::Path::new(&dir).join("secrets.txt"),
        secrets.join("\n"),
    )?;
    Ok(())
}

/// Neither the API nor its client logs a credential's secret, a rotated secret, or a run token
/// while credentials are created and read, user settings are written, and an admin reads a
/// run's token and its models
#[tokio::test]
async fn secrets_stay_out_of_logs() -> Result<(), Error> {
    // a directory for the child's logs and the secrets it reports
    let dir = std::env::temp_dir().join(format!("thorium-trace-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir)?;
    // run the scenario in a child with its own namespace and port, logging at the debug level
    let port = std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port();
    let mut child = KillOnDrop(
        std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "trace_scenario_child",
                "--ignored",
                "--nocapture",
            ])
            .env(TRACE_CHILD_DIR, &dir)
            .env("THORIUM__THORIUM__NAMESPACE", format!("{}_trace", ns()))
            .env("THORIUM__THORIUM__PORT", port.to_string())
            .env("THORIUM__THORIUM__TRACING__LOCAL__LEVEL", "Debug")
            .stdout(std::fs::File::create(dir.join("stdout.log"))?)
            .stderr(std::fs::File::create(dir.join("stderr.log"))?)
            .spawn()?,
    );
    // wait for the scenario to finish
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.0.try_wait()? {
            break status;
        }
        assert!(
            started.elapsed() < Duration::from_secs(TRACE_CHILD_SECONDS),
            "the logging scenario timed out"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    // read everything the child logged and the secrets it used
    let mut logs = String::new();
    for name in ["stdout.log", "stderr.log"] {
        logs.push_str(&String::from_utf8_lossy(&std::fs::read(dir.join(name))?));
    }
    let secrets = std::fs::read_to_string(dir.join("secrets.txt")).unwrap_or_default();
    let secrets = secrets.lines().collect::<Vec<&str>>();
    // the scenario passed, without echoing any line that holds a secret
    let tail = logs
        .lines()
        .filter(|line| !secrets.iter().any(|secret| line.contains(secret)))
        .rev()
        .take(30)
        .collect::<Vec<&str>>();
    assert!(
        status.success(),
        "the logging scenario failed; the last lines of its log were {tail:#?}"
    );
    // the log shows the requests that carry secrets, so the check below looks at real output
    for seen in [
        "Starting Request",
        "/run-models",
        "/run-token",
        "/api/entities/models/credentials/",
        "/api/users/",
    ] {
        assert!(logs.contains(seen), "the log never mentions {seen}");
    }
    // the log holds none of the four secrets
    is!(secrets.len(), 4);
    for (index, secret) in secrets.iter().enumerate() {
        assert!(
            secret.len() >= 16,
            "secret #{index} is too short to search for"
        );
        assert!(!logs.contains(secret), "the log holds secret #{index}");
    }
    // nothing is left behind
    std::fs::remove_dir_all(&dir)?;
    Ok(())
}

/// The blocking client starts tool runs and workflow runs too
#[cfg(feature = "sync")]
#[test]
fn run_client_blocking() -> Result<(), Error> {
    // build the fixtures with the async client on the blocking client's runtime
    let (runner, triage, image, sha256) = thorium::RUNTIME.block_on(async {
        let runner = runner("carpo", "pomona").await?;
        let triage = workflow(&runner, "Triage").await?;
        let groups = [runner.group.as_str()];
        let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.admin).await?;
        let sha256 = sample(&runner.group, &runner.user.client).await?;
        Ok::<_, Error>((runner, triage, image, sha256))
    })?;
    // a blocking client for the runner
    let client = thorium::ThoriumBlocking::build(&runner.admin.host)
        .token(&runner.user.token)
        .build_blocking()?;
    let inputs = RunInputs::default().sample(&sha256);
    // each run method goes through the blocking client
    let tool_run = client
        .reactions
        .run_tool(image.id, &ToolRunRequest::new(inputs.clone()))?;
    let workflow_run = client
        .reactions
        .run_workflow(triage.id(), &WorkflowRunRequest::new(inputs))?;
    for id in [tool_run.id, workflow_run.id] {
        is!(client.reactions.get(&runner.group, id)?.id, id);
    }
    Ok(())
}

/// A retry of a failed workflow run whose stored reaction can't be read answers the read's
/// error rather than that the run doesn't exist
#[tokio::test]
async fn retry_of_unreadable_run_is_not_missing() -> Result<(), Error> {
    // a runner's failed workflow run
    let runner = runner("thyone", "fides").await?;
    let triage = workflow(&runner, "Triage").await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let client = &runner.user.client.reactions;
    let created = client
        .run_workflow(
            triage.id(),
            &WorkflowRunRequest::new(RunInputs::default().sample(&sha256)),
        )
        .await?;
    let auth = runner.user.auth_header();
    runs::handle_reaction(
        &runner.user.client,
        &auth,
        &runner.group,
        created.id,
        "fail",
    )
    .await?;
    // the stored reaction's status no longer parses
    let key = format!("{}:reaction_data:{}:{}", ns(), runner.group, created.id);
    let _: i64 = redis::cmd("hset")
        .arg(&key)
        .arg("status")
        .arg("{not json")
        .query_async(&mut redis_conn().await)
        .await
        .expect("the corrupted status");
    // the retry reports the failed read instead of a missing run
    let Err(err) = client.retry_workflow_run(triage.id(), created.id).await else {
        panic!("a retry of an unreadable run was accepted");
    };
    let status = err.status().map(|status| status.as_u16());
    assert_ne!(status, Some(404), "{err:?}");
    Ok(())
}

/// Make a group the runner is only a plain user of, owned by the admin
///
/// # Arguments
///
/// * `runner` - The runner
async fn user_only_group(runner: &Runner) -> Result<String, Error> {
    // the admin owns the group and the runner is a user of it
    generators::god_group(
        &runner.admin,
        "juno",
        &runner.suffix,
        &GroupRoles::default().user(&runner.user.username),
    )
    .await
}

/// Make a tool in the runner's group and another group and return its first instance
///
/// # Arguments
///
/// * `runner` - The runner, who submits the tool
/// * `other` - The other group to put the tool and its instance in
async fn two_group_instance(runner: &Runner, other: &str) -> Result<Entity, Error> {
    // the runner submits a tool and its instance into both groups
    let groups = [runner.group.as_str(), other];
    let (_, instance) = generators::tool(
        &groups,
        "1.0.0",
        &generators::gen_image_spec(),
        &runner.user.client,
    )
    .await?;
    Ok(instance)
}

/// A scoped token can't delete what it submitted from groups outside its scope
#[tokio::test]
async fn scoped_token_delete_stays_within_its_groups() -> Result<(), Error> {
    // a runner who submitted an instance into two groups
    let runner = runner("callisto", "ceres").await?;
    let other = user_only_group(&runner).await?;
    let instance = two_group_instance(&runner, &other).await?;
    // a scoped token limited to the first group is refused and nothing is deleted
    let name = format!("scoped-{}", runner.suffix);
    let req = ScopedTokenRequest::new(&name).group(&runner.group);
    let limited = runner.user.client.users.create_scoped_token(&req).await?;
    let limited = token_client(&runner.admin, &limited.token).await?;
    assert_status(
        limited.entities.delete(instance.id).await,
        401,
        "a token limited to one group deleted a two group instance",
    )?;
    is!(
        runner.admin.entities.get(instance.id).await?.groups.len(),
        2
    );
    // a scoped token covering both groups can delete it
    let name = format!("both-{}", runner.suffix);
    let req = ScopedTokenRequest::new(&name)
        .group(&runner.group)
        .group(&other);
    let both = runner.user.client.users.create_scoped_token(&req).await?;
    let both = token_client(&runner.admin, &both.token).await?;
    both.entities.delete(instance.id).await?;
    assert_status(
        runner.admin.entities.get(instance.id).await,
        404,
        "the instance survived a delete by a token covering both groups",
    )
}

/// A run token can't delete what the runner submitted from groups outside its scope
#[tokio::test]
async fn run_token_delete_stays_within_its_groups() -> Result<(), Error> {
    // a runner who submitted an instance into two groups
    let runner = runner("thebe", "ceres").await?;
    let other = user_only_group(&runner).await?;
    let instance = two_group_instance(&runner, &other).await?;
    // start a run in the first group and take its token
    let triage = Box::pin(workflow(&runner, "Triage")).await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let (_, scoped) = start_run(&runner, &triage, &RunInputs::default().sample(&sha256)).await?;
    // the run token is refused and the instance stays in both groups
    assert_status(
        scoped.entities.delete(instance.id).await,
        401,
        "a run token deleted a two group instance",
    )?;
    is!(
        runner.admin.entities.get(instance.id).await?.groups.len(),
        2
    );
    // the runner's full token can still delete it
    runner.user.client.entities.delete(instance.id).await?;
    assert_status(
        runner.admin.entities.get(instance.id).await,
        404,
        "the runner couldn't delete its own instance",
    )
}

/// A run token can't log out, which would rotate its value and lock out the run's other workers
#[tokio::test]
async fn run_token_logout_is_refused() -> Result<(), Error> {
    // start a run and read its token
    let runner = runner("pasiphae", "ceres").await?;
    let triage = Box::pin(workflow(&runner, "Triage")).await?;
    let sha256 = sample(&runner.group, &runner.user.client).await?;
    let (run, scoped) = start_run(&runner, &triage, &RunInputs::default().sample(&sha256)).await?;
    let before = run_token_of(&runner.admin, &run).await?;
    // logging out with the run token is refused
    let refused = scoped
        .users
        .logout()
        .await
        .err()
        .and_then(|error| error.status());
    is!(refused, Some(reqwest::StatusCode::UNAUTHORIZED));
    // the token's value is unchanged and still authenticates
    let after = run_token_of(&runner.admin, &run).await?;
    is!(after.token, before.token);
    token_client(&runner.admin, &before.token).await?;
    Ok(())
}

/// A workflow run's runner, and clients holding the run's token
struct EdgeFixture {
    /// The runner and the group they own
    runner: Runner,
    /// A client with the run token
    scoped: Thorium,
    /// The run token's authorization header value
    run_auth: String,
}

/// Start a workflow run in a group the runner owns and keep its run token
///
/// # Arguments
///
/// * `moon` - The moon the runner is named after
/// * `god` - The god the group is named after
async fn edge_fixture(moon: &str, god: &str) -> Result<EdgeFixture, Error> {
    // a K8s developer who owns G with an Image tool and a ready workflow
    let runner = runner(moon, god).await?;
    let g = runner.group.clone();
    let groups = [g.as_str()];
    let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &runner.user.client).await?;
    let (_, cli) = runs::container_tool(&groups, image.id, &runner.admin).await?;
    let triage = runs::ready_workflow(
        "Triage",
        &groups,
        &[(&cli, ToolMechanism::Cli)],
        &runner.admin,
    )
    .await?;
    // a workflow run on a file in G
    let content = generators::gen_string(2048);
    let input = sample_in(&content, &[&g], &runner.admin).await?;
    let created = runner
        .user
        .client
        .reactions
        .run_workflow(
            triage.id(),
            &WorkflowRunRequest::new(RunInputs::default().sample(&input)),
        )
        .await?;
    // read the run's token and build a client that holds it
    let run = runner.user.client.reactions.get(&g, created.id).await?;
    let token = run_token_of(&runner.admin, &run).await?;
    let scoped = token_client(&runner.admin, &token.token).await?;
    let run_auth = test_utilities::auth_header(&token.token);
    Ok(EdgeFixture {
        runner,
        scoped,
        run_auth,
    })
}

/// The status a request ended with: 200 for any success, the error's status otherwise
///
/// # Arguments
///
/// * `result` - The result of the request
fn code<T>(result: Result<T, Error>) -> u16 {
    // successes are all reported as 200
    match result {
        Ok(_) => 200,
        Err(error) => error.status().map_or(0, |status| status.as_u16()),
    }
}

/// Whether an entity has an edge of a kind to another entity, read with the admin route
///
/// # Arguments
///
/// * `admin` - An admin client to read the associations with
/// * `id` - The entity the edge starts at
/// * `kind` - The kind of edge to look for
/// * `other` - The entity the edge points to
async fn has_edge(
    admin: &Thorium,
    id: Uuid,
    kind: AssociationKind,
    other: Uuid,
) -> Result<bool, Error> {
    // list every association of the entity and look for the edge
    let found: Vec<Association> = admin.entities.associations(id).await?;
    Ok(found.iter().any(|assoc| {
        assoc.kind == kind
            && assoc.direction == Directionality::To
            && matches!(&assoc.other, AssociationTarget::Entity { id, .. } if *id == other)
    }))
}

/// Run tokens can't create or delete the edges that make up the agentic graph, and the runner's
/// full token still can
///
/// The edges decide which agents, tools, and toolsets a workflow is built from, so a run that could
/// write them would change the workflows other runs follow.
#[tokio::test]
async fn run_token_cannot_write_graph_edges() -> Result<(), Error> {
    let f = Box::pin(edge_fixture("amalthea", "ceres")).await?;
    let g = f.runner.group.clone();
    let groups = [g.as_str()];
    let full = &f.runner.user.client;
    let admin = &f.runner.admin;
    let suffix = &f.runner.suffix;
    // two agents, an agentic tool, and a toolset in G
    let lead_name = format!("lead-{suffix}");
    let helper_name = format!("helper-{suffix}");
    let kit_name = format!("kit-{suffix}");
    let lead = full
        .entities
        .create(generators::gen_agent(
            &lead_name,
            &groups,
            AgentEntityRequest::new(generators::gen_agent_config("lead")),
        ))
        .await?
        .id;
    let helper = full
        .entities
        .create(generators::gen_agent(
            &helper_name,
            &groups,
            AgentEntityRequest::new(generators::gen_agent_config("helper")),
        ))
        .await?
        .id;
    let (tool, _) =
        generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), full).await?;
    let kit = full
        .entities
        .create(generators::gen_toolset(
            &kit_name,
            &groups,
            ToolSetEntityRequest::default(),
        ))
        .await?
        .id;
    // build the target of an edge
    let entity = |id: Uuid, name: &str| AssociationTarget::Entity {
        id,
        name: name.to_owned(),
    };
    // the graph edges: the lead calls the tool, the toolset includes it, the lead delegates
    let edges = [
        (
            AssociationKind::Calls,
            lead,
            lead_name.clone(),
            tool.id,
            tool.name.clone(),
        ),
        (
            AssociationKind::Includes,
            kit,
            kit_name.clone(),
            tool.id,
            tool.name.clone(),
        ),
        (
            AssociationKind::DelegatesTo,
            lead,
            lead_name.clone(),
            helper,
            helper_name.clone(),
        ),
    ];
    for (kind, src, src_name, dst, dst_name) in &edges {
        // the run token's create is refused and no edge appears
        let create =
            AssociationRequest::new(*kind, entity(*src, src_name)).target(entity(*dst, dst_name));
        let status = code(f.scoped.associations.create(&create).await);
        assert_eq!(status, 401, "run token {kind:?} create");
        assert!(
            !has_edge(admin, *src, *kind, *dst).await?,
            "run token created a {kind:?} edge"
        );
        // the runner's full token creates it
        let status = code(full.associations.create(&create).await);
        assert_eq!(status, 200, "full token {kind:?} create");
        assert!(
            has_edge(admin, *src, *kind, *dst).await?,
            "full token {kind:?} edge is missing"
        );
        // the run token's delete is refused and the edge stays
        let delete = AssociationDeleteRequest::new(*kind, entity(*src, src_name))
            .target(entity(*dst, dst_name));
        let status = code(f.scoped.associations.delete(&delete).await);
        assert_eq!(status, 401, "run token {kind:?} delete");
        assert!(
            has_edge(admin, *src, *kind, *dst).await?,
            "run token removed a {kind:?} edge"
        );
        // the runner's full token deletes it
        let status = code(full.associations.delete(&delete).await);
        assert_eq!(status, 200, "full token {kind:?} delete");
        assert!(
            !has_edge(admin, *src, *kind, *dst).await?,
            "full token {kind:?} edge is still there"
        );
    }
    Ok(())
}

/// Send an entity tag request with an authorization header
///
/// # Arguments
///
/// * `host` - The API to send the request to
/// * `auth` - The authorization header value to send
/// * `id` - The entity to tag
/// * `delete` - Whether to delete the tags instead of adding them
/// * `body` - The tag request body
async fn raw_entity_tags(
    host: &str,
    auth: &str,
    id: Uuid,
    delete: bool,
    body: &serde_json::Value,
) -> Result<u16, Error> {
    // build the tag request for this entity
    let url = format!("{host}/api/entities/tags/{id}");
    let client = reqwest::Client::new();
    let req = if delete {
        client.delete(url)
    } else {
        client.post(url)
    };
    // send it with the given authorization
    let resp = req.header("authorization", auth).json(body).send().await?;
    Ok(resp.status().as_u16())
}

/// Run tokens can't tag or untag a definition, which could change the tools a toolset selects,
/// but still tag the data entities a harness writes about
#[tokio::test]
async fn run_token_cannot_tag_definitions() -> Result<(), Error> {
    let f = Box::pin(edge_fixture("callirrhoe", "vesta")).await?;
    let g = f.runner.group.clone();
    let groups = [g.as_str()];
    let full = &f.runner.user.client;
    let admin = &f.runner.admin;
    let host = admin.host.clone();
    // a toolset in G that selects tools tagged kit=<value>, and an agentic tool in G
    let value = format!("red-{}", f.runner.suffix);
    full.entities
        .create(generators::gen_toolset(
            &format!("kit-{}", f.runner.suffix),
            &groups,
            ToolSetEntityRequest::default().selector(ToolSetSelector::new("kit", &value)),
        ))
        .await?;
    let (tool, _) =
        generators::tool(&groups, "1.0.0", &generators::gen_agentic_spec(), full).await?;
    let body = serde_json::json!({"groups": [g.clone()], "tags": {"kit": [value.clone()]}});
    // count the toolset rebuilds queued for the tool
    let queued = || async {
        Ok::<_, Error>(
            test_utilities::events::graph_changes(admin, tool.id)
                .await?
                .len(),
        )
    };
    let before = queued().await?;
    // the run token's tag is refused and queues no rebuild
    let status = raw_entity_tags(&host, &f.run_auth, tool.id, false, &body).await?;
    assert_eq!(status, 401, "run token tool tag add");
    assert_eq!(queued().await?, before, "run token tag queued a rebuild");
    // the runner's full token tags the tool, which queues a rebuild
    let status =
        raw_entity_tags(&host, &f.runner.user.auth_header(), tool.id, false, &body).await?;
    assert_eq!(status, 204, "full token tool tag add");
    let tagged = queued().await?;
    assert!(tagged > before, "full token tag queued no rebuild");
    // the run token's untag is refused and queues no rebuild
    let status = raw_entity_tags(&host, &f.run_auth, tool.id, true, &body).await?;
    assert_eq!(status, 401, "run token tool tag delete");
    assert_eq!(queued().await?, tagged, "run token untag queued a rebuild");
    // the runner's full token untags the tool
    let status = raw_entity_tags(&host, &f.runner.user.auth_header(), tool.id, true, &body).await?;
    assert_eq!(status, 204, "full token tool tag delete");
    // a data entity is not a definition, so the run token still tags it
    let data = full
        .entities
        .create(generators::gen_entity(&g, generators::gen_other_meta()))
        .await?
        .id;
    let status = raw_entity_tags(&host, &f.run_auth, data, false, &body).await?;
    assert_eq!(status, 204, "run token data entity tag add");
    Ok(())
}

/// Run tokens can't ask for a model endpoint refresh, and the runner's full token still can
#[tokio::test]
async fn run_token_cannot_refresh_endpoints() -> Result<(), Error> {
    let f = Box::pin(edge_fixture("elara", "juno")).await?;
    // an allowed endpoint in G, whose group the runner owns
    let created = model_endpoint(&f.runner, "refresh", gateway_at_random(), None, &["m"]).await?;
    // the run token's refresh is refused
    let status = code(f.scoped.entities.refresh_model_endpoint(created.id).await);
    assert_eq!(status, 401, "run token endpoint refresh");
    // the runner's full token refreshes it
    let full = &f.runner.user.client;
    let status = code(full.entities.refresh_model_endpoint(created.id).await);
    assert_eq!(status, 200, "full token endpoint refresh");
    Ok(())
}
