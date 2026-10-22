//! Tests that job runtimes stay within the bounds the rest of Thorium can schedule with

use chrono::{DateTime, Duration, Utc};
use thorium::models::{
    BareMetalEnvironment, EntityMetadata, GenericJob, ImageArgs, ImageScaler, PipelineRequest,
    Pools, ReactionRequest, ReactionStatus, Resources, ToolSpec, WorkerRegistrationList,
    WorkloadEnvironment, WorkloadSpec,
};
use thorium::test_utilities::{self, generators};
use thorium::{Error, Thorium, fail, is};
use uuid::Uuid;

/// A runtime far too large to add to any timestamp
const HUGE_RUNTIME: u64 = 10_000_000_000_000;

/// One week in seconds, the longest a job of a tool instance without a timeout may run
const WEEK: u64 = 7 * 24 * 60 * 60;

/// The timeout of the tool instances these tests run, in seconds
const TIMEOUT: u64 = 300;

/// The name of the one stage of each test pipeline
const STAGE: &str = "harvest";

/// Run a redis command against the test redis
///
/// # Arguments
///
/// * `cmd` - The command to run
async fn redis_exec(cmd: &bb8_redis::redis::Cmd) -> Result<(), Error> {
    // get a connection to the test redis
    let redis = test_utilities::redis_pool().await;
    let mut conn = redis
        .get()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    // run our command
    let (): () = cmd
        .query_async(&mut *conn)
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    Ok(())
}

/// Read every runtime in a tool instance's runtime history in a group, newest first
///
/// # Arguments
///
/// * `group` - The group the tool instance ran in
/// * `instance` - The tool instance
async fn runtime_history(group: &str, instance: Uuid) -> Result<Vec<u64>, Error> {
    // get a connection to the test redis
    let redis = test_utilities::redis_pool().await;
    let mut conn = redis
        .get()
        .await
        .map_err(|err| Error::new(err.to_string()))?;
    // read this tool instance's runtime history in the group
    bb8_redis::redis::cmd("LRANGE")
        .arg(history_key(group, instance))
        .arg(0)
        .arg(-1)
        .query_async(&mut *conn)
        .await
        .map_err(|err| Error::new(err.to_string()))
}

/// Get the newest runtime in a tool instance's runtime history in a group
///
/// # Arguments
///
/// * `group` - The group the tool instance ran in
/// * `instance` - The tool instance
async fn newest_runtime(group: &str, instance: Uuid) -> Result<u64, Error> {
    // the history is newest first
    runtime_history(group, instance)
        .await?
        .first()
        .copied()
        .ok_or_else(|| Error::new(format!("{} is empty", history_key(group, instance))))
}

/// Build the key to a tool instance's runtime history in a group
///
/// # Arguments
///
/// * `group` - The group the tool instance ran in
/// * `instance` - The tool instance
fn history_key(group: &str, instance: Uuid) -> String {
    // histories are kept per group and tool instance
    let ns = &test_utilities::CONF.thorium.namespace;
    format!("{ns}:workload_ttc:{group}:{instance}")
}

/// Build an Image tool spec whose jobs may run for at most some seconds
///
/// # Arguments
///
/// * `timeout` - The longest a job may run, or `None` for no timeout
fn timed_spec(timeout: Option<u64>) -> ToolSpec {
    // a K8s workload with the timeout set
    let ToolSpec::Image(mut workload) = generators::gen_image_spec() else {
        unreachable!("an image spec is always an Image tool spec")
    };
    workload.execution.timeout = timeout;
    ToolSpec::Image(workload)
}

/// A one stage pipeline whose stage runs a tool's first instance
struct Setup {
    /// The group the pipeline and tool are in
    group: String,
    /// The name of the pipeline
    pipe: String,
    /// The id of the tool
    tool: Uuid,
    /// The tool instance the stage runs
    instance: Uuid,
    /// The worker that claims the stage's jobs
    worker: String,
}

impl Setup {
    /// Create a tool in a new group and a pipeline whose one stage runs its first instance
    ///
    /// # Arguments
    ///
    /// * `timeout` - The timeout of the tool's first instance
    /// * `client` - The client to create the tool and pipeline with
    async fn new(timeout: Option<u64>, client: &Thorium) -> Result<Self, Error> {
        // create a tool in a new group
        let group = generators::groups(1, client).await?.remove(0).name;
        let (tool, instance) =
            generators::tool(&[group.as_str()], "1.0.0", &timed_spec(timeout), client).await?;
        // pin a one stage pipeline to the tool's first instance
        let pipe = generators::gen_string(20).to_lowercase();
        let req = PipelineRequest::new(&group, &pipe, serde_json::json!([[STAGE]]))
            .stage_tool(STAGE, instance.id);
        client.pipelines.create(&req).await?;
        let worker = format!("runtimes-{}", &pipe[..8]);
        Ok(Setup {
            group,
            pipe,
            tool: tool.id,
            instance: instance.id,
            worker,
        })
    }

    /// Create reactions for the pipeline and claim their jobs with the setup's worker
    ///
    /// # Arguments
    ///
    /// * `count` - The number of reactions to create and jobs to claim
    /// * `client` - The client to create and claim with
    async fn claim(&self, count: u64, client: &Thorium) -> Result<Vec<GenericJob>, Error> {
        // create reactions so jobs exist to claim
        for _ in 0..count {
            let req = ReactionRequest::new(&self.group, &self.pipe);
            client.reactions.create(&req).await?;
        }
        // register a node and a worker for the stage's tool instance
        generators::node("cluster0", "node0", Resources::default(), client).await?;
        let user = client.users.info().await?.username;
        let list = WorkerRegistrationList::default().add(
            "cluster0",
            "node0",
            &self.worker,
            user,
            &self.group,
            self.instance,
            None,
            Resources::default(),
            Pools::Deadline,
        );
        client
            .system
            .register_workers(ImageScaler::K8s, &list)
            .await?;
        // claim our jobs from the tool instance the stage runs
        let jobs = client
            .jobs
            .claim_workload(
                &self.group,
                self.instance,
                "cluster0",
                "node0",
                &self.worker,
                count,
            )
            .await?;
        assert_eq!(jobs.len() as u64, count, "claimed the wrong number of jobs");
        Ok(jobs)
    }

    /// Mark the tool instance as recently queued in its group, so its runtimes are averaged
    async fn mark_active(&self) -> Result<(), Error> {
        // the averaging task reads each group's set of recently queued tool instances
        let ns = &test_utilities::CONF.thorium.namespace;
        redis_exec(
            bb8_redis::redis::cmd("ZADD")
                .arg(format!("{ns}:workloads:{}", self.group))
                .arg(Utc::now().timestamp())
                .arg(self.instance.to_string()),
        )
        .await
    }
}

/// Get the stored average runtime of a tool instance
///
/// # Arguments
///
/// * `instance` - The tool instance
/// * `client` - The client to read runtimes with
async fn average_runtime(instance: Uuid, client: &Thorium) -> Result<f64, Error> {
    // read every tool instance's runtime and keep this one's
    let runtimes = client.jobs.workload_runtimes().await?;
    runtimes
        .runtimes
        .get(&instance)
        .copied()
        .ok_or_else(|| Error::new(format!("no runtime for tool instance {instance}")))
}

/// Check that a job is no longer running and that its tool instance counts the expected
/// completions
///
/// # Arguments
///
/// * `job` - The job that proceeded
/// * `completed` - The number of completed jobs its tool instance should count
/// * `client` - The client to check with
async fn finished(job: &GenericJob, completed: u64, client: &Thorium) -> Result<(), Error> {
    // the job is no longer in the running list
    let start = DateTime::from_timestamp(0, 0).unwrap();
    let end = Utc::now() + Duration::weeks(1);
    let running = client
        .jobs
        .running(ImageScaler::K8s, &start, &end, 10_000)
        .await?;
    assert!(
        running.iter().all(|run| run.job_id != job.id),
        "job {} is still running",
        job.id
    );
    // its tool instance counts it as completed
    let stats = client.system.stats().await?;
    let counts = stats
        .groups
        .get(&job.group)
        .and_then(|group| group.workloads.get(&job.tool_instance))
        .and_then(|users| users.get("thorium"))
        .ok_or_else(|| Error::new(format!("no stats for stage {}", job.stage)))?;
    assert_eq!(
        counts.completed, completed,
        "stage {} completions",
        job.stage
    );
    Ok(())
}

/// A runtime a little above the tool instance's timeout proceeds and is stored as the timeout,
/// so a job whose finish was noticed late still completes
#[tokio::test]
async fn proceed_clamps_runtimes_slightly_above_the_tool_instance_timeout() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // claim jobs whose tool instance has a timeout
    let setup = Setup::new(Some(TIMEOUT), &client).await?;
    let slack = (TIMEOUT / 10).max(300);
    let runtimes = [TIMEOUT + 1, TIMEOUT + TIMEOUT / 10, TIMEOUT + slack];
    let jobs = setup.claim(runtimes.len() as u64, &client).await?;
    let logs = generators::stage_logs();
    for (completed, (job, runtime)) in (1..).zip(jobs.iter().zip(runtimes)) {
        // a runtime within the slack above the timeout proceeds
        client
            .jobs
            .proceed(job, &logs, runtime, &setup.worker)
            .await?;
        finished(job, completed, &client).await?;
        // the tool instance's runtime history holds the timeout instead
        let newest = newest_runtime(&setup.group, setup.instance).await?;
        assert_eq!(
            newest, TIMEOUT,
            "a runtime of {runtime} was stored as {newest}"
        );
    }
    Ok(())
}

/// A runtime far above the tool instance's timeout is refused and not recorded, and one within
/// it still proceeds
#[tokio::test]
async fn proceed_refuses_runtimes_far_above_the_tool_instance_timeout() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // claim a job whose tool instance has a timeout
    let setup = Setup::new(Some(TIMEOUT), &client).await?;
    let slack = (TIMEOUT / 10).max(300);
    let job = setup.claim(1, &client).await?.remove(0);
    let logs = generators::stage_logs();
    // runtimes above the timeout and its slack are refused
    for runtime in [HUGE_RUNTIME, TIMEOUT + slack + 1] {
        let resp = client
            .jobs
            .proceed(&job, &logs, runtime, &setup.worker)
            .await;
        fail!(resp, 400);
    }
    // a runtime within the timeout still proceeds and is the only one stored
    client
        .jobs
        .proceed(&job, &logs, TIMEOUT - 1, &setup.worker)
        .await?;
    finished(&job, 1, &client).await?;
    is!(
        runtime_history(&setup.group, setup.instance).await?,
        vec![TIMEOUT - 1]
    );
    Ok(())
}

/// A job keeps the timeout of the tool instance it claimed: publishing a version with a lower
/// timeout mid-run doesn't refuse or shorten the running job's runtime
#[tokio::test]
async fn proceed_keeps_the_claimed_tool_instance_timeout() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // claim a job, then publish a version whose timeout is below the runtime it will report
    let claimed = 3600;
    let setup = Setup::new(Some(claimed), &client).await?;
    let job = setup.claim(1, &client).await?.remove(0);
    let lowered = 100;
    client
        .entities
        .update(
            setup.tool,
            generators::gen_tool_publish("2.0.0", &timed_spec(Some(lowered))),
        )
        .await?;
    let tool = client.entities.get(setup.tool).await?;
    let EntityMetadata::Tool(meta) = &tool.metadata else {
        return Err(Error::new("not a tool".to_owned()));
    };
    let latest = meta.latest.expect("a latest instance");
    assert_ne!(latest, setup.instance, "no new version was published");
    // a runtime far above the new version's bound but within the claimed one proceeds
    let runtime = claimed - 1;
    assert!(runtime > lowered + (lowered / 10).max(300));
    let logs = generators::stage_logs();
    client
        .jobs
        .proceed(&job, &logs, runtime, &setup.worker)
        .await?;
    finished(&job, 1, &client).await?;
    // the runtime is stored as is for the claimed tool instance only
    is!(newest_runtime(&setup.group, setup.instance).await?, runtime);
    is!(
        runtime_history(&setup.group, latest).await?,
        Vec::<u64>::new()
    );
    Ok(())
}

/// A huge runtime already in a tool instance's history doesn't push its average past the bound
#[tokio::test]
async fn averaged_tool_instance_runtimes_are_clamped() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // put a huge runtime in a recently queued tool instance's runtime history
    let setup = Setup::new(Some(TIMEOUT), &client).await?;
    redis_exec(
        bb8_redis::redis::cmd("LPUSH")
            .arg(history_key(&setup.group, setup.instance))
            .arg(HUGE_RUNTIME),
    )
    .await?;
    setup.mark_active().await?;
    // average every tool instance's runtime history
    client.jobs.update_workload_runtimes().await?;
    // the average is the tool instance's timeout
    let average = average_runtime(setup.instance, &client).await?;
    assert!(
        (average - f64::from(u32::try_from(TIMEOUT).unwrap())).abs() < f64::EPSILON,
        "an average runtime of {average} isn't the timeout of {TIMEOUT}"
    );
    Ok(())
}

/// Creating a reaction works even when a tool instance's stored runtime is huge
#[tokio::test]
async fn reactions_survive_huge_tool_instance_runtimes() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // store a huge average runtime for the pipeline's tool instance
    let setup = Setup::new(Some(TIMEOUT), &client).await?;
    let ns = &test_utilities::CONF.thorium.namespace;
    redis_exec(
        bb8_redis::redis::cmd("HSET")
            .arg(format!("{ns}:workload_runtimes"))
            .arg(setup.instance.to_string())
            .arg(HUGE_RUNTIME),
    )
    .await?;
    // creating a reaction for the pipeline still succeeds
    let req = ReactionRequest::new(&setup.group, &setup.pipe);
    client.reactions.create(&req).await?;
    Ok(())
}

/// A job whose tool instance has no timeout can't proceed with a runtime far above a week, and
/// the tool instance's recorded and average runtimes are clamped to a week
#[tokio::test]
async fn untimed_tool_instances_are_capped_at_a_week() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // claim a job whose tool instance has no timeout
    let setup = Setup::new(None, &client).await?;
    let job = setup.claim(1, &client).await?.remove(0);
    // a runtime above a week and its slack is refused, and one just above a week is
    // recorded as a week
    let logs = generators::stage_logs();
    let resp = client
        .jobs
        .proceed(&job, &logs, WEEK + WEEK / 10 + 1, &setup.worker)
        .await;
    fail!(resp, 400);
    client
        .jobs
        .proceed(&job, &logs, WEEK + 1, &setup.worker)
        .await?;
    is!(newest_runtime(&setup.group, setup.instance).await?, WEEK);
    // a huge runtime in the tool instance's history averages to at most a week
    redis_exec(
        bb8_redis::redis::cmd("LPUSH")
            .arg(history_key(&setup.group, setup.instance))
            .arg(HUGE_RUNTIME),
    )
    .await?;
    setup.mark_active().await?;
    client.jobs.update_workload_runtimes().await?;
    let average = average_runtime(setup.instance, &client).await?;
    assert!(
        average <= f64::from(u32::try_from(WEEK).unwrap()),
        "an untimed tool instance averaged a runtime of {average}"
    );
    Ok(())
}

/// A recently queued tool instance that can no longer be read, like one deleted while its jobs
/// ran, is still averaged, and its average is clamped to a week
#[tokio::test]
async fn unreadable_tool_instances_are_capped_at_a_week() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // a tool instance id that no entity has, queued in a group with a huge runtime history
    let group = generators::groups(1, &client).await?.remove(0).name;
    let missing = Uuid::new_v4();
    let ns = &test_utilities::CONF.thorium.namespace;
    redis_exec(
        bb8_redis::redis::cmd("LPUSH")
            .arg(history_key(&group, missing))
            .arg(HUGE_RUNTIME),
    )
    .await?;
    redis_exec(
        bb8_redis::redis::cmd("ZADD")
            .arg(format!("{ns}:workloads:{group}"))
            .arg(Utc::now().timestamp())
            .arg(missing.to_string()),
    )
    .await?;
    // averaging every tool instance's runtime history still succeeds
    client.jobs.update_workload_runtimes().await?;
    // the missing tool instance's average is a week
    let average = average_runtime(missing, &client).await?;
    assert!(
        (average - f64::from(u32::try_from(WEEK).unwrap())).abs() < f64::EPSILON,
        "an unreadable tool instance averaged a runtime of {average}"
    );
    Ok(())
}

/// The thorium-agent runs a tool stage whose tool instance has a timeout, and its measured
/// runtime is accepted and recorded; a runtime past the bound reported for the next job of the
/// same tool instance is refused, leaving that job running until a runtime within the bound is
/// reported
///
/// Set `THORIUM_AGENT_BIN` to a thorium-agent built from this workspace to run it.
#[tokio::test]
#[ignore = "needs THORIUM_AGENT_BIN, a thorium-agent binary built from this workspace"]
#[allow(clippy::too_many_lines)]
async fn agent_runtimes_are_bounded_by_the_tool_instance() -> Result<(), Error> {
    // the agent binary to run
    let agent = std::env::var("THORIUM_AGENT_BIN")
        .map_err(|_| Error::new("THORIUM_AGENT_BIN must point at a thorium-agent binary"))?;
    // a user in a group with a bare metal tool stage that sleeps briefly under a timeout
    let admin = test_utilities::admin_client().await?;
    generators::node("cluster0", "node0", Resources::default(), &admin).await?;
    let suffix = generators::name_suffix();
    let adrastea = generators::moon_user(&admin, "adrastea", &suffix).await?;
    let roles = generators::GroupRoles::default().user(&adrastea.username);
    let group = generators::god_group(&admin, "ceres", &suffix, &roles).await?;
    let mut spec = WorkloadSpec {
        environment: WorkloadEnvironment::BareMetal(BareMetalEnvironment::default()),
        ..WorkloadSpec::default()
    };
    spec.execution.timeout = Some(TIMEOUT);
    spec.execution.args = ImageArgs {
        command: Some(vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "sleep 1; echo harvester ran".to_owned(),
        ]),
        ..ImageArgs::default()
    };
    let (_, instance) =
        generators::tool(&[group.as_str()], "1.0.0", &ToolSpec::Image(spec), &admin).await?;
    let pipe = generators::gen_string(20).to_lowercase();
    let req = PipelineRequest::new(&group, &pipe, serde_json::json!([[STAGE]]))
        .stage_tool(STAGE, instance.id);
    admin.pipelines.create(&req).await?;
    // the user queues a reaction on a sample
    let sha256 = adrastea
        .client
        .files
        .create(generators::gen_sample(&group))
        .await?
        .sha256;
    let first = adrastea
        .client
        .reactions
        .create(&ReactionRequest::new(group.as_str(), pipe.as_str()).sample(sha256.as_str()))
        .await?
        .id;
    // the scaler would register the worker, then start the agent with the user's keys
    let worker = generators::gen_string(24);
    let list = WorkerRegistrationList::default().add(
        "cluster0",
        "node0",
        &worker,
        &adrastea.username,
        &group,
        instance.id,
        None,
        Resources::default(),
        Pools::Deadline,
    );
    admin
        .system
        .register_workers(ImageScaler::BareMetal, &list)
        .await?;
    let dir = std::env::temp_dir().join(format!("thorium-agent-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&dir).await?;
    let keys = dir.join("keys.yml");
    tokio::fs::write(
        &keys,
        format!("api: {}\ntoken: {}\n", admin.host, adrastea.token),
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
    // the agent's job completed and its measured runtime is recorded within the timeout
    let done = adrastea.client.reactions.get(&group, first).await?;
    is!(done.status, ReactionStatus::Completed);
    let history = runtime_history(&group, instance.id).await?;
    println!("runtimes the agent recorded: {history:?}");
    is!(history.len(), 1);
    assert!(history[0] <= TIMEOUT, "the agent recorded {history:?}");
    // a worker of the same tool instance claims the next job and reports a runtime past the bound
    let second = adrastea
        .client
        .reactions
        .create(&ReactionRequest::new(group.as_str(), pipe.as_str()).sample(sha256.as_str()))
        .await?
        .id;
    let next = generators::gen_string(24);
    let list = WorkerRegistrationList::default().add(
        "cluster0",
        "node0",
        &next,
        &adrastea.username,
        &group,
        instance.id,
        None,
        Resources::default(),
        Pools::Deadline,
    );
    admin
        .system
        .register_workers(ImageScaler::BareMetal, &list)
        .await?;
    let job = adrastea
        .client
        .jobs
        .claim_workload(&group, instance.id, "cluster0", "node0", &next, 1)
        .await?
        .remove(0);
    let logs = generators::stage_logs();
    let over = TIMEOUT + (TIMEOUT / 10).max(300) + 1;
    let resp = adrastea.client.jobs.proceed(&job, &logs, over, &next).await;
    if let Err(err) = &resp {
        println!("over-bound proceed refused: {err:?}");
    }
    fail!(
        resp,
        400,
        &format!("above the {} seconds tool instance", TIMEOUT + 300)
    );
    // nothing was recorded and the reaction still runs
    is!(runtime_history(&group, instance.id).await?, history);
    let running = adrastea.client.reactions.get(&group, second).await?;
    is!(running.status, ReactionStatus::Started);
    // a runtime within the bound finishes it
    adrastea.client.jobs.proceed(&job, &logs, 2, &next).await?;
    let done = adrastea.client.reactions.get(&group, second).await?;
    is!(done.status, ReactionStatus::Completed);
    is!(runtime_history(&group, instance.id).await?[0], 2);
    generators::delete_worker(&next, &admin).await?;
    Ok(())
}
