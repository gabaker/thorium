//! Tests how workers claim and run the jobs of tool instances

use thorium::client::ResultsClient;
use thorium::models::{
    Actions, BareMetalEnvironment, ImageArgs, ImageScaler, PipelineRequest, Pools,
    ReactionListParams, ReactionRequest, ReactionStatus, Resources, ResultGetParams, StageLogsAdd,
    ToolSpec, ToolVersion, WorkerRegistrationList, WorkloadEnvironment, WorkloadSpec,
};
use thorium::test_utilities::{self, generators};
use thorium::{Error, Thorium, fail, is};
use uuid::Uuid;

/// Register a shared worker as an admin for a user
///
/// # Arguments
///
/// * `name` - The name of the worker
/// * `user` - The user the worker runs as
/// * `group` - The group the worker serves
/// * `tool_instance` - The tool instance the worker runs
/// * `scaler` - The scaler the worker is under
async fn shared_worker(
    name: &str,
    user: &str,
    group: &str,
    tool_instance: Uuid,
    scaler: ImageScaler,
) -> Result<(), Error> {
    // a worker with no run serves its user's shared queue
    let list = WorkerRegistrationList::default().add(
        "cluster0",
        "node0",
        name,
        user,
        group,
        tool_instance,
        None,
        Resources::default(),
        Pools::Deadline,
    );
    // workers are registered by admins, like the scaler
    let admin = test_utilities::admin_client().await?;
    admin.system.register_workers(scaler, &list).await?;
    Ok(())
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

/// Claim one job of a tool instance for a worker
///
/// # Arguments
///
/// * `client` - The client to claim with
/// * `group` - The group to claim in
/// * `instance` - The tool instance to claim jobs of
/// * `worker` - The worker to claim for
async fn claim_one(
    client: &Thorium,
    group: &str,
    instance: Uuid,
    worker: &str,
) -> Result<Vec<thorium::models::GenericJob>, Error> {
    // claim at most one job
    client
        .jobs
        .claim_workload(group, instance, "cluster0", "node0", worker, 1)
        .await
}

/// The claim by pipeline and stage is gone; workers claim only by tool instance
#[tokio::test]
async fn stage_claim_route_gone() -> Result<(), Error> {
    // a pipeline with a queued job in a group
    let admin = test_utilities::admin_client().await?;
    generators::node("cluster0", "node0", Resources::default(), &admin).await?;
    let suffix = generators::name_suffix();
    let roles = generators::GroupRoles::default();
    let group = generators::god_group(&admin, "ceres", &suffix, &roles).await?;
    let (_, instance) =
        generators::stage_tool(&group, &generators::gen_image_spec(), &admin).await?;
    let pipe = pipe_for(&group, instance, &admin).await?;
    admin
        .reactions
        .create(&ReactionRequest::new(group.as_str(), pipe.as_str()))
        .await?;
    let worker = generators::gen_string(24);
    shared_worker(&worker, "thorium", &group, instance, ImageScaler::K8s).await?;
    // the stage claim path is not a route, even for an admin
    let token = admin.users.info().await?.token;
    let url = format!(
        "{}/api/jobs/claim/{group}/{pipe}/harvest/cluster0/node0/{worker}/1",
        admin.host
    );
    let resp = reqwest::Client::new()
        .patch(url)
        .header("authorization", test_utilities::auth_header(&token))
        .send()
        .await?;
    is!(resp.status().as_u16(), 404);
    // the same job is claimed through the workload route
    is!(claim_one(&admin, &group, instance, &worker).await?.len(), 1);
    generators::delete_worker(&worker, &admin).await?;
    Ok(())
}

/// A claim's status update names the claimed job and its stage beside the worker, so the
/// reaction's status log tells which of its parallel stages runs, whatever their tools are named
#[tokio::test]
async fn claim_update_names_the_job_and_stage() -> Result<(), Error> {
    // a reaction of a pipeline whose two parallel stages run two tools named otherwise
    let admin = test_utilities::admin_client().await?;
    generators::node("cluster0", "node0", Resources::default(), &admin).await?;
    let suffix = generators::name_suffix();
    let roles = generators::GroupRoles::default();
    let group = generators::god_group(&admin, "pallas", &suffix, &roles).await?;
    let (_, harvest) =
        generators::stage_tool(&group, &generators::gen_image_spec(), &admin).await?;
    let (_, sow) = generators::stage_tool(&group, &generators::gen_image_spec(), &admin).await?;
    let pipe = generators::gen_string(20).to_lowercase();
    let req = PipelineRequest::new(&group, &pipe, serde_json::json!([["harvest", "sow"]]))
        .stage_tool("harvest", harvest)
        .stage_tool("sow", sow);
    admin.pipelines.create(&req).await?;
    let reaction = admin
        .reactions
        .create(&ReactionRequest::new(group.as_str(), pipe.as_str()))
        .await?
        .id;
    // a worker of the second stage's tool claims that stage's job
    let worker = generators::gen_string(24);
    shared_worker(&worker, "thorium", &group, sow, ImageScaler::K8s).await?;
    let jobs = claim_one(&admin, &group, sow, &worker).await?;
    is!(jobs.len(), 1);
    is!(jobs[0].stage, "sow");
    // the reaction's one claim update names the job, its stage, and the worker
    let logs = admin.reactions.status_logs(&group, &reaction).await?;
    let claims = logs
        .iter()
        .filter(|log| matches!(log.action, Actions::JobRunning))
        .collect::<Vec<_>>();
    is!(claims.len(), 1);
    is!(claims[0].update.get("job"), Some(&jobs[0].id.to_string()));
    is!(
        claims[0].update.get("stage").map(String::as_str),
        Some("sow")
    );
    is!(claims[0].update.get("worker"), Some(&worker));
    generators::delete_worker(&worker, &admin).await?;
    Ok(())
}

/// A shared worker serves every pipeline job its user queues for its tool instance in its group,
/// and never another user's; jobs in another group wait for a worker of that group
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn shared_worker_serves_pipeline_jobs() -> Result<(), Error> {
    // two users in two groups that share one tool
    let admin = test_utilities::admin_client().await?;
    generators::node("cluster0", "node0", Resources::default(), &admin).await?;
    let suffix = generators::name_suffix();
    let callisto = generators::moon_user(&admin, "callisto", &suffix).await?;
    let leda = generators::moon_user(&admin, "leda", &suffix).await?;
    let roles = generators::GroupRoles::default()
        .user(&callisto.username)
        .user(&leda.username);
    let group = generators::god_group(&admin, "vesta", &suffix, &roles).await?;
    let other = generators::god_group(&admin, "janus", &suffix, &roles).await?;
    let (_, instance) = generators::tool(
        &[group.as_str(), other.as_str()],
        "1.0.0",
        &generators::gen_image_spec(),
        &admin,
    )
    .await?;
    let instance = instance.id;
    let pipe = pipe_for(&group, instance, &admin).await?;
    let other_pipe = pipe_for(&other, instance, &admin).await?;
    // callisto's shared worker in the first group
    let worker = generators::gen_string(24);
    shared_worker(
        &worker,
        &callisto.username,
        &group,
        instance,
        ImageScaler::K8s,
    )
    .await?;
    // callisto's first reaction is served by the worker
    let first = callisto
        .client
        .reactions
        .create(&ReactionRequest::new(group.as_str(), pipe.as_str()))
        .await?
        .id;
    let jobs = claim_one(&callisto.client, &group, instance, &worker).await?;
    is!(jobs.len(), 1);
    is!(jobs[0].reaction, first);
    is!(jobs[0].tool_instance, instance);
    assert!(jobs[0].run.is_none());
    // the job completes and a second reaction is served by the same worker
    callisto
        .client
        .jobs
        .proceed(&jobs[0], &StageLogsAdd::default(), 1, &worker)
        .await?;
    let second = callisto
        .client
        .reactions
        .create(&ReactionRequest::new(group.as_str(), pipe.as_str()))
        .await?
        .id;
    let jobs = claim_one(&callisto.client, &group, instance, &worker).await?;
    is!(jobs.len(), 1);
    is!(jobs[0].reaction, second);
    // leda's job and callisto's job in the other group aren't served by callisto's worker
    leda.client
        .reactions
        .create(&ReactionRequest::new(group.as_str(), pipe.as_str()))
        .await?;
    callisto
        .client
        .reactions
        .create(&ReactionRequest::new(other.as_str(), other_pipe.as_str()))
        .await?;
    let mine = claim_one(&callisto.client, &group, instance, &worker).await?;
    assert!(mine.is_empty(), "callisto's worker claimed {mine:?}");
    fail!(
        claim_one(&leda.client, &group, instance, &worker).await,
        401
    );
    // leda's own worker and callisto's worker in the other group each claim their user's job
    let leda_worker = generators::gen_string(24);
    shared_worker(
        &leda_worker,
        &leda.username,
        &group,
        instance,
        ImageScaler::K8s,
    )
    .await?;
    let jobs = claim_one(&leda.client, &group, instance, &leda_worker).await?;
    is!(jobs.len(), 1);
    is!(jobs[0].creator, leda.username);
    let other_worker = generators::gen_string(24);
    shared_worker(
        &other_worker,
        &callisto.username,
        &other,
        instance,
        ImageScaler::K8s,
    )
    .await?;
    let jobs = claim_one(&callisto.client, &other, instance, &other_worker).await?;
    is!(jobs.len(), 1);
    is!(jobs[0].group, other);
    for name in [&worker, &leda_worker, &other_worker] {
        generators::delete_worker(name, &admin).await?;
    }
    Ok(())
}

/// The thorium-agent runs a pipeline tool stage end to end: it reads its tool instance, claims
/// the job through the workload route, runs it, and stores its result under the tool's name and
/// version
///
/// Set `THORIUM_AGENT_BIN` to a thorium-agent built from this workspace to run it.
#[tokio::test]
#[ignore = "needs THORIUM_AGENT_BIN, a thorium-agent binary built from this workspace"]
#[allow(clippy::too_many_lines)]
async fn agent_runs_tool_stage_job() -> Result<(), Error> {
    // the agent binary to run
    let agent = std::env::var("THORIUM_AGENT_BIN")
        .map_err(|_| Error::new("THORIUM_AGENT_BIN must point at a thorium-agent binary"))?;
    // a user in a group with a bare metal tool stage that prints a line
    let admin = test_utilities::admin_client().await?;
    generators::node("cluster0", "node0", Resources::default(), &admin).await?;
    let suffix = generators::name_suffix();
    let amalthea = generators::moon_user(&admin, "amalthea", &suffix).await?;
    let roles = generators::GroupRoles::default().user(&amalthea.username);
    let group = generators::god_group(&admin, "minerva", &suffix, &roles).await?;
    let mut spec = WorkloadSpec {
        environment: WorkloadEnvironment::BareMetal(BareMetalEnvironment::default()),
        ..WorkloadSpec::default()
    };
    spec.execution.args = ImageArgs {
        command: Some(vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "echo harvester ran".to_owned(),
        ]),
        ..ImageArgs::default()
    };
    let (tool, instance) =
        generators::tool(&[group.as_str()], "2.3.4", &ToolSpec::Image(spec), &admin).await?;
    let pipe = pipe_for(&group, instance.id, &admin).await?;
    // the user queues a reaction on a sample
    let sha256 = amalthea
        .client
        .files
        .create(generators::gen_sample(&group))
        .await?
        .sha256;
    let reaction = amalthea
        .client
        .reactions
        .create(&ReactionRequest::new(group.as_str(), pipe.as_str()).sample(sha256.as_str()))
        .await?
        .id;
    // the scaler would register the worker, then start the agent with the user's keys
    let worker = generators::gen_string(24);
    shared_worker(
        &worker,
        &amalthea.username,
        &group,
        instance.id,
        ImageScaler::BareMetal,
    )
    .await?;
    let dir = std::env::temp_dir().join(format!("thorium-agent-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&dir).await?;
    let keys = dir.join("keys.yml");
    tokio::fs::write(
        &keys,
        format!("api: {}\ntoken: {}\n", admin.host, amalthea.token),
    )
    .await?;
    let output = tokio::process::Command::new(&agent)
        .args([
            "--cluster",
            "cluster0",
            "--node",
            "node0",
            "--group",
            &group,
        ])
        .args([
            "--tool-instance",
            &instance.id.to_string(),
            "--name",
            &worker,
        ])
        .args(["--keys", &keys.to_string_lossy(), "--limbo", "1"])
        .args([
            "--trace",
            &dir.join("none.yml").to_string_lossy(),
            "bare-metal",
        ])
        .output()
        .await?;
    tokio::fs::remove_dir_all(&dir).await?;
    assert!(
        output.status.success(),
        "agent failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // the reaction completed and its stage logs hold the tool's output
    let done = amalthea.client.reactions.get(&group, reaction).await?;
    is!(done.status, ReactionStatus::Completed);
    let logs = amalthea
        .client
        .reactions
        .logs(&group, &reaction, "harvest", &ReactionListParams::default())
        .await?;
    assert!(
        logs.logs.iter().any(|line| line.contains("harvester ran")),
        "{:?}",
        logs.logs
    );
    // the result is stored under the tool's name with the tool instance's version
    let results = amalthea
        .client
        .files
        .get_results(&sha256, &ResultGetParams::default())
        .await?;
    let outputs = results
        .results
        .get(&tool.name)
        .ok_or_else(|| Error::new(format!("no result under {} in {:?}", tool.name, results)))?;
    is!(outputs.len(), 1);
    is!(outputs[0].tool_version, Some(ToolVersion::from("2.3.4")));
    is!(results.results.len(), 1);
    // the worker removed its record and no job is left queued
    let workers = admin.system.get_worker(&worker).await;
    fail!(workers, 404);
    Ok(())
}
