//! Tests the Jobs routes in Thorium

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::prelude::*;
use thorium::models::{
    EntityMetadata, EventType, GenericJob, GroupAllowed, ImageScaler, JobResets, JobStatus,
    Pipeline, PipelineRequest, Pools, ReactionListParams, ReactionRequest, ReactionStatus,
    Resources, ScopedTokenRequest, StageLogsAdd, UserRole, WorkerDeleteMap, WorkerRegistrationList,
};
use thorium::test_utilities::faults::ReactionRun;
use thorium::test_utilities::{self, events, generators};
use thorium::{Error, Thorium, fail, is};
use uuid::Uuid;

/// unwraps the status counts of the thorium user for a pipeline stage's tool instance
macro_rules! get_stats {
    ($stats:expr, $group:expr, $pipe:expr, $stage:expr) => {
        if let Some(group_stats) = $stats.groups.get(&$group) {
            let tool_instance = generators::stage_instance(&$pipe, $stage);
            if let Some(users) = group_stats.workloads.get(&tool_instance) {
                if let Some(statuses) = users.get(&"thorium".to_owned()) {
                    statuses
                } else {
                    return Err(Error::new(format!(
                        "Failed == check because thorium not in {:#?}",
                        users
                    )));
                }
            } else {
                return Err(Error::new(format!(
                    "Failed == check because {} not in {:#?}",
                    tool_instance, group_stats.workloads
                )));
            }
        } else {
            return Err(Error::new(format!(
                "Failed == check because {} not in {:#?}",
                $group, $stats.groups
            )));
        }
    };
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

/// Build the key to a shared workload queue
///
/// # Arguments
///
/// * `group` - The group of the jobs
/// * `tool_instance` - The tool instance of the jobs
/// * `user` - The user who created the jobs
/// * `status` - The status of the jobs in the queue
fn workload_queue(group: &str, tool_instance: Uuid, user: &str, status: &JobStatus) -> String {
    // shared queues are per group, tool instance, and user
    format!(
        "{}:workload_queue:{group}:{tool_instance}:{user}:{status}",
        ns()
    )
}

/// Build the key to a run queue
///
/// # Arguments
///
/// * `group` - The group of the jobs
/// * `run` - The run the jobs belong to
/// * `tool_instance` - The tool instance of the jobs
/// * `status` - The status of the jobs in the queue
fn run_queue(group: &str, run: Uuid, tool_instance: Uuid, status: &JobStatus) -> String {
    // run queues are per group, run, and tool instance
    format!("{}:run_queue:{group}:{run}:{tool_instance}:{status}", ns())
}

/// Count the members of a sorted set
///
/// # Arguments
///
/// * `key` - The key of the sorted set
async fn zcard(key: &str) -> u64 {
    // count the set's members
    redis::cmd("zcard")
        .arg(key)
        .query_async(&mut redis_conn().await)
        .await
        .expect("a count")
}

/// List the members of a sorted set
///
/// # Arguments
///
/// * `key` - The key of the sorted set
async fn zmembers(key: &str) -> Vec<String> {
    // read every member
    redis::cmd("zrange")
        .arg(key)
        .arg(0)
        .arg(-1)
        .query_async(&mut redis_conn().await)
        .await
        .expect("members")
}

/// Create a one stage pipeline running a new Image tool in an environment
///
/// # Arguments
///
/// * `group` - The group of the pipeline
/// * `scaler` - The scaler whose environment the tool runs in
/// * `client` - The client to create the pipeline with
async fn one_stage_pipe(
    group: &str,
    scaler: ImageScaler,
    client: &Thorium,
) -> Result<(Pipeline, Uuid), Error> {
    // create the stage's tool
    let (stage, instance) =
        generators::stage_tool(group, &generators::gen_image_spec_for(scaler), client).await?;
    // create the pipeline around it
    let name = generators::gen_string(20).to_lowercase();
    let req = PipelineRequest::new(group, &name, serde_json::json!([[&stage]]))
        .stage_tool(&stage, instance);
    client.pipelines.create(&req).await?;
    Ok((client.pipelines.get(group, &name).await?, instance))
}

/// Create a one stage pipeline running an existing tool instance
///
/// # Arguments
///
/// * `group` - The group of the pipeline
/// * `instance` - The tool instance the stage runs
/// * `client` - The client to create the pipeline with
async fn pipe_for(group: &str, instance: Uuid, client: &Thorium) -> Result<Pipeline, Error> {
    // a single stage pinned to the instance
    let name = generators::gen_string(20).to_lowercase();
    let req = PipelineRequest::new(group, &name, serde_json::json!([["stage"]]))
        .stage_tool("stage", instance);
    client.pipelines.create(&req).await?;
    client.pipelines.get(group, &name).await
}

/// Register a worker as an admin for the client's user, optionally serving one run
///
/// # Arguments
///
/// * `name` - The name of the worker
/// * `group` - The group the worker serves
/// * `scaler` - The scaler the worker is under
/// * `run` - The run the worker serves, if any
/// * `client` - The client of the user the worker runs as
async fn workload_worker(
    name: &str,
    group: &str,
    scaler: ImageScaler,
    run: Option<Uuid>,
    client: &Thorium,
) -> Result<(), Error> {
    // the worker runs as the client's user
    let user = client.users.info().await?.username;
    let mut list = WorkerRegistrationList::default().add(
        "cluster0",
        "node0",
        name,
        user,
        group,
        "pipe",
        "stage",
        Resources::default(),
        Pools::Deadline,
    );
    if let Some(run) = run {
        list.workers[0].run = Some(run);
    }
    // workers are registered by admins, like the scaler
    let admin = test_utilities::admin_client().await?;
    admin.system.register_workers(scaler, &list).await?;
    Ok(())
}

/// Jobs of a run go on that run's queue, and per-run workers claim only their run's jobs
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn run_jobs_use_run_queues() -> Result<(), Error> {
    // a group, a harness stage, and a tool stage
    let admin = test_utilities::admin_client().await?;
    generators::node("cluster0", "node0", Resources::default(), &admin).await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let (owner_pipe, harness) = one_stage_pipe(&group, ImageScaler::K8s, &admin).await?;
    let (call_pipe, tool) = one_stage_pipe(&group, ImageScaler::K8s, &admin).await?;
    // a reaction that is its own run, like a workflow run
    test_utilities::faults::run_reactions(&owner_pipe.name, ReactionRun::Own);
    let run = admin
        .reactions
        .create(&ReactionRequest::new(&group, &owner_pipe.name))
        .await?
        .id;
    let owner = admin.reactions.get(&group, run).await?;
    is!(owner.run, Some(run));
    let owner_job = admin.jobs.get(owner.jobs[0]).await?;
    is!(owner_job.run, Some(run));
    is!(
        zcard(&run_queue(&group, run, harness, &JobStatus::Created)).await,
        1
    );
    // its deadline member ends with the run
    let deadlines = zmembers(&format!("{}:streams:system:K8s:deadlines", ns())).await;
    let member = deadlines
        .iter()
        .find(|member| member.contains(&owner_job.id.to_string()))
        .expect("the run job's deadline");
    is!(member.ends_with(&format!(",\"run\":\"{run}\"}}")), true);
    // two calls of one tool instance in that run share one run queue
    test_utilities::faults::run_reactions(&call_pipe.name, ReactionRun::Of(run));
    let mut calls = Vec::new();
    for _ in 0..2 {
        let call = admin
            .reactions
            .create(&ReactionRequest::new(&group, &call_pipe.name).parent(run))
            .await?
            .id;
        let call = admin.reactions.get(&group, call).await?;
        is!(call.run, Some(run));
        let job = admin.jobs.get(call.jobs[0]).await?;
        is!(job.run, Some(run));
        calls.push(job);
    }
    is!(
        zcard(&run_queue(&group, run, tool, &JobStatus::Created)).await,
        2
    );
    // a pipeline reaction on the same tool has no run and uses the shared queue
    let shared_pipe = pipe_for(&group, tool, &admin).await?;
    let shared = admin
        .reactions
        .create(&ReactionRequest::new(&group, &shared_pipe.name))
        .await?;
    let shared = admin.reactions.get(&group, shared.id).await?;
    is!(shared.run, None::<Uuid>);
    let shared_job = admin.jobs.get(shared.jobs[0]).await?;
    is!(shared_job.run, None::<Uuid>);
    is!(
        zcard(&workload_queue(
            &group,
            tool,
            "thorium",
            &JobStatus::Created
        ))
        .await,
        1
    );
    // a per-run worker claims only its run's jobs of the tool instance
    let run_worker = generators::gen_string(24);
    workload_worker(&run_worker, &group, ImageScaler::K8s, Some(run), &admin).await?;
    let claimed = admin
        .jobs
        .claim_workload(&group, tool, "cluster0", "node0", &run_worker, 5)
        .await?;
    let mut ids = claimed.iter().map(|job| job.id).collect::<Vec<Uuid>>();
    ids.sort();
    let mut expected = calls.iter().map(|job| job.id).collect::<Vec<Uuid>>();
    expected.sort();
    is!(ids, expected);
    is!(claimed.iter().all(|job| job.run == Some(run)), true);
    is!(
        zcard(&workload_queue(
            &group,
            tool,
            "thorium",
            &JobStatus::Created
        ))
        .await,
        1
    );
    // finishing a call expires none of the run's queues
    admin
        .jobs
        .proceed(&claimed[0], &generators::stage_logs(), 1, &run_worker)
        .await?;
    let ttl: i64 = redis::cmd("ttl")
        .arg(run_queue(&group, run, tool, &JobStatus::Completed))
        .query_async(&mut redis_conn().await)
        .await
        .expect("a ttl");
    is!(ttl, -1);
    // finishing the run expires its run queues
    let owner_claim = admin
        .jobs
        .claim_workload(&group, harness, "cluster0", "node0", &run_worker, 1)
        .await?;
    is!(owner_claim.len(), 1);
    admin
        .jobs
        .proceed(&owner_claim[0], &generators::stage_logs(), 1, &run_worker)
        .await?;
    admin
        .jobs
        .proceed(&claimed[1], &generators::stage_logs(), 1, &run_worker)
        .await?;
    let ttl: i64 = redis::cmd("ttl")
        .arg(run_queue(&group, run, harness, &JobStatus::Completed))
        .query_async(&mut redis_conn().await)
        .await
        .expect("a ttl");
    assert!(ttl > 0);
    generators::delete_worker(&run_worker, &admin).await?;
    Ok(())
}

/// A shared worker never claims run jobs
#[tokio::test]
async fn shared_worker_skips_run_jobs() -> Result<(), Error> {
    // a reaction that is its own run
    let admin = test_utilities::admin_client().await?;
    generators::node("cluster0", "node0", Resources::default(), &admin).await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let (pipe, tool) = one_stage_pipe(&group, ImageScaler::K8s, &admin).await?;
    test_utilities::faults::run_reactions(&pipe.name, ReactionRun::Own);
    let run = admin
        .reactions
        .create(&ReactionRequest::new(&group, &pipe.name))
        .await?
        .id;
    // a shared worker of the same tool instance claims nothing
    let worker = generators::gen_string(24);
    workload_worker(&worker, &group, ImageScaler::K8s, None, &admin).await?;
    let claimed = admin
        .jobs
        .claim_workload(&group, tool, "cluster0", "node0", &worker, 5)
        .await?;
    is!(claimed.len(), 0);
    is!(
        zcard(&run_queue(&group, run, tool, &JobStatus::Created)).await,
        1
    );
    generators::delete_worker(&worker, &admin).await?;
    Ok(())
}

/// A workload job whose data is gone is pruned with its exact deadline member
#[tokio::test]
async fn dangling_workload_job_pruned() -> Result<(), Error> {
    // a pipeline reaction with one queued job
    let admin = test_utilities::admin_client().await?;
    generators::node("cluster0", "node0", Resources::default(), &admin).await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let (pipe, tool) = one_stage_pipe(&group, ImageScaler::K8s, &admin).await?;
    let created = admin
        .reactions
        .create(&ReactionRequest::new(&group, &pipe.name))
        .await?;
    let reaction = admin.reactions.get(&group, created.id).await?;
    let job = reaction.jobs[0];
    // the status entry carries the job's deadline member
    let created_queue = workload_queue(&group, tool, "thorium", &JobStatus::Created);
    let entries = zmembers(&created_queue).await;
    let entry: serde_json::Value = serde_json::from_str(&entries[0]).expect("an entry");
    let member = entry["deadline"]
        .as_str()
        .expect("a deadline member")
        .to_owned();
    let deadlines = format!("{}:streams:system:K8s:deadlines", ns());
    let score: Option<f64> = redis::cmd("zscore")
        .arg(&deadlines)
        .arg(&member)
        .query_async(&mut redis_conn().await)
        .await
        .expect("a score");
    is!(score.is_some(), true);
    // the job's data disappears
    let _: () = redis::cmd("del")
        .arg(format!("{}:job_data:{job}", ns()))
        .query_async(&mut redis_conn().await)
        .await
        .expect("delete the job");
    // a claim prunes its status entry and deadline member
    let worker = generators::gen_string(24);
    workload_worker(&worker, &group, ImageScaler::K8s, None, &admin).await?;
    let claimed = admin
        .jobs
        .claim_workload(&group, tool, "cluster0", "node0", &worker, 1)
        .await?;
    is!(claimed.len(), 0);
    is!(zcard(&created_queue).await, 0);
    is!(
        zcard(&workload_queue(
            &group,
            tool,
            "thorium",
            &JobStatus::Running
        ))
        .await,
        0
    );
    let score: Option<f64> = redis::cmd("zscore")
        .arg(&deadlines)
        .arg(&member)
        .query_async(&mut redis_conn().await)
        .await
        .expect("a score");
    is!(score, None::<f64>);
    // an entry without a deadline member leaves the old job's deadline member in place
    let (old_job, old_reaction) = (Uuid::new_v4(), Uuid::new_v4());
    let old_entry = serde_json::json!({"job": old_job, "reaction": old_reaction}).to_string();
    let old_member = format!(
        "{{\"group\":\"{group}\",\"pipeline\":\"{}\",\"stage\":\"stage\",\"creator\":\"thorium\",\"job_id\":\"{old_job}\",\"reaction\":\"{old_reaction}\"}}",
        pipe.name
    );
    let mut conn = redis_conn().await;
    let _: () = redis::cmd("zadd")
        .arg(&created_queue)
        .arg(1)
        .arg(&old_entry)
        .query_async(&mut conn)
        .await
        .expect("queue an old entry");
    let _: () = redis::cmd("zadd")
        .arg(&deadlines)
        .arg(1)
        .arg(&old_member)
        .query_async(&mut conn)
        .await
        .expect("add the old job's deadline");
    let claimed = admin
        .jobs
        .claim_workload(&group, tool, "cluster0", "node0", &worker, 1)
        .await?;
    is!(claimed.len(), 0);
    is!(zcard(&created_queue).await, 0);
    let score: Option<i64> = redis::cmd("zscore")
        .arg(&deadlines)
        .arg(&old_member)
        .query_async(&mut conn)
        .await
        .expect("a score");
    is!(score, Some(1));
    let _: () = redis::cmd("zrem")
        .arg(&deadlines)
        .arg(&old_member)
        .query_async(&mut conn)
        .await
        .expect("remove the old job's deadline");
    generators::delete_worker(&worker, &admin).await?;
    Ok(())
}

/// A stage label holding quotes and backslashes is escaped in its deadline member, which is
/// removed by exact match when the job proceeds
#[tokio::test]
async fn escaped_stage_label_deadline() -> Result<(), Error> {
    // a pipeline whose one stage name holds a quote and a backslash
    let admin = test_utilities::admin_client().await?;
    generators::node("cluster0", "node0", Resources::default(), &admin).await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let tool = generators::tool_instance(&[&group], &admin).await?;
    let stage = "a\"b\\c";
    let name = generators::gen_string(20).to_lowercase();
    let req =
        PipelineRequest::new(&group, &name, serde_json::json!([[stage]])).stage_tool(stage, tool);
    admin.pipelines.create(&req).await?;
    let created = admin
        .reactions
        .create(&ReactionRequest::new(&group, &name))
        .await?;
    let job = admin.reactions.get(&group, created.id).await?.jobs[0];
    // the job's deadline member is valid JSON holding the label exactly
    let deadlines = format!("{}:streams:system:K8s:deadlines", ns());
    let member = zmembers(&deadlines)
        .await
        .into_iter()
        .find(|member| member.contains(&job.to_string()))
        .expect("the job's deadline member");
    let parsed: serde_json::Value = serde_json::from_str(&member).expect("valid json");
    is!(parsed["stage"].as_str(), Some(stage));
    // proceeding the job removes the member by exact match
    let worker = generators::gen_string(24);
    workload_worker(&worker, &group, ImageScaler::K8s, None, &admin).await?;
    let claimed = admin
        .jobs
        .claim_workload(&group, tool, "cluster0", "node0", &worker, 1)
        .await?;
    is!(&claimed[0].stage, stage);
    admin
        .jobs
        .proceed(&claimed[0], &generators::stage_logs(), 1, &worker)
        .await?;
    let score: Option<f64> = redis::cmd("zscore")
        .arg(&deadlines)
        .arg(&member)
        .query_async(&mut redis_conn().await)
        .await
        .expect("a score");
    is!(score, None::<f64>);
    generators::delete_worker(&worker, &admin).await?;
    Ok(())
}

/// Tool instance runtimes are recorded per job, averaged over the last ten thousand runs in every
/// group, and seeded when a version is published
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn workload_runtimes() -> Result<(), Error> {
    // a pipeline reaction on a new tool
    let admin = test_utilities::admin_client().await?;
    generators::node("cluster0", "node0", Resources::default(), &admin).await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let other = generators::groups(1, &admin).await?.remove(0).name;
    let (tool, first) = generators::tool(
        &[&group, &other],
        "1.0.0",
        &generators::gen_image_spec(),
        &admin,
    )
    .await?;
    // the first version is seeded with the default runtime
    let runtimes = admin.jobs.workload_runtimes().await?;
    is!(runtimes.runtimes.get(&first.id), Some(&600.0));
    // proceeding a job records its runtime for the tool instance and not for an image
    let pipe = pipe_for(&group, first.id, &admin).await?;
    admin
        .reactions
        .create(&ReactionRequest::new(&group, &pipe.name))
        .await?;
    let worker = generators::gen_string(24);
    workload_worker(&worker, &group, ImageScaler::K8s, None, &admin).await?;
    let claimed = admin
        .jobs
        .claim_workload(&group, first.id, "cluster0", "node0", &worker, 1)
        .await?;
    admin
        .jobs
        .proceed(&claimed[0], &generators::stage_logs(), 42, &worker)
        .await?;
    let ttc = format!("{}:workload_ttc:{group}:{}", ns(), first.id);
    let times: Vec<u64> = redis::cmd("lrange")
        .arg(&ttc)
        .arg(0)
        .arg(-1)
        .query_async(&mut redis_conn().await)
        .await
        .expect("runtimes");
    is!(times, vec![42]);
    let image_ttc: u64 = redis::cmd("llen")
        .arg(format!("{}:image_ttc:{group}:stage", ns()))
        .query_async(&mut redis_conn().await)
        .await
        .expect("a length");
    is!(image_ttc, 0);
    // only the newest ten thousand of twelve thousand runtimes are averaged
    let mut conn = redis_conn().await;
    let _: () = redis::cmd("del")
        .arg(&ttc)
        .query_async(&mut conn)
        .await
        .expect("clear");
    for (value, count) in [(1000, 2000), (10, 10_000)] {
        let _: () = redis::cmd("lpush")
            .arg(&ttc)
            .arg(vec![value; count])
            .query_async(&mut conn)
            .await
            .expect("push runtimes");
    }
    // the same tool instance ran in a second group
    let other_ttc = format!("{}:workload_ttc:{other}:{}", ns(), first.id);
    let _: () = redis::cmd("lpush")
        .arg(&other_ttc)
        .arg(vec![30; 10_000])
        .query_async(&mut conn)
        .await
        .expect("push runtimes");
    let _: () = redis::cmd("zadd")
        .arg(format!("{}:workloads:{other}", ns()))
        .arg(chrono::Utc::now().timestamp())
        .arg(first.id.to_string())
        .query_async(&mut conn)
        .await
        .expect("mark the tool instance active");
    // publishing a new version seeds it from its previous version
    admin.jobs.update_workload_runtimes().await?;
    let runtimes = admin.jobs.workload_runtimes().await?;
    is!(runtimes.runtimes.get(&first.id), Some(&20.0));
    admin
        .entities
        .update(
            tool.id,
            generators::gen_tool_publish("2.0.0", &generators::gen_image_spec()),
        )
        .await?;
    let second = admin.entities.get(tool.id).await?;
    let EntityMetadata::Tool(meta) = &second.metadata else {
        return Err(Error::new("not a tool".to_owned()));
    };
    let second = meta.latest.expect("a latest instance");
    let runtimes = admin.jobs.workload_runtimes().await?;
    is!(runtimes.runtimes.get(&second), Some(&20.0));
    // a version with no runs keeps its seeded runtime
    admin.jobs.update_workload_runtimes().await?;
    let runtimes = admin.jobs.workload_runtimes().await?;
    is!(runtimes.runtimes.get(&second), Some(&20.0));
    // a runtime that already exists isn't overwritten by a publish
    let third = Uuid::new_v4();
    test_utilities::faults::create_with_id(
        &format!("{}@3.0.0", second_name(&admin, tool.id).await?),
        third,
    );
    let _: () = redis::cmd("hset")
        .arg(format!("{}:workload_runtimes", ns()))
        .arg(third.to_string())
        .arg(7)
        .query_async(&mut conn)
        .await
        .expect("set a runtime");
    admin
        .entities
        .update(
            tool.id,
            generators::gen_tool_publish("3.0.0", &generators::gen_image_spec()),
        )
        .await?;
    let runtimes = admin.jobs.workload_runtimes().await?;
    is!(runtimes.runtimes.get(&third), Some(&7.0));
    // non-admins can't update or read runtimes
    let user = generators::user_with_role(&admin, UserRole::User).await?;
    fail!(user.client.jobs.update_workload_runtimes().await, 401);
    fail!(user.client.jobs.workload_runtimes().await, 401);
    generators::delete_worker(&worker, &admin).await?;
    Ok(())
}

/// Get the name of a tool
///
/// # Arguments
///
/// * `client` - The client to read the tool with
/// * `tool` - The id of the tool
async fn second_name(client: &Thorium, tool: Uuid) -> Result<String, Error> {
    // read the tool's name
    Ok(client.entities.get(tool).await?.name)
}

/// The workload claim takes the job with the earliest deadline first, from shared and run queues
#[tokio::test]
async fn workload_claim_earliest_deadline_first() -> Result<(), Error> {
    // a shared job with a late deadline queued before one with an early deadline
    let admin = test_utilities::admin_client().await?;
    generators::node("cluster0", "node0", Resources::default(), &admin).await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let (pipe, tool) = one_stage_pipe(&group, ImageScaler::K8s, &admin).await?;
    let mut jobs = Vec::with_capacity(2);
    for sla in [86_400 * 7, 60] {
        let created = admin
            .reactions
            .create(&ReactionRequest::new(&group, &pipe.name).sla(sla))
            .await?;
        let reaction = admin.reactions.get(&group, created.id).await?;
        jobs.push(admin.jobs.get(reaction.jobs[0]).await?);
    }
    assert!(jobs[1].deadline < jobs[0].deadline);
    // a limit-one claim takes the early job, then the late one
    let worker = generators::gen_string(24);
    workload_worker(&worker, &group, ImageScaler::K8s, None, &admin).await?;
    for expected in [&jobs[1], &jobs[0]] {
        let claimed = admin
            .jobs
            .claim_workload(&group, tool, "cluster0", "node0", &worker, 1)
            .await?;
        is!(claimed.len(), 1);
        is!(claimed[0].id, expected.id);
    }
    // two calls in one run with a late and an early deadline, queued in that order
    let (owner_pipe, _) = one_stage_pipe(&group, ImageScaler::K8s, &admin).await?;
    let (call_pipe, call_tool) = one_stage_pipe(&group, ImageScaler::K8s, &admin).await?;
    test_utilities::faults::run_reactions(&owner_pipe.name, ReactionRun::Own);
    let run = admin
        .reactions
        .create(&ReactionRequest::new(&group, &owner_pipe.name))
        .await?
        .id;
    test_utilities::faults::run_reactions(&call_pipe.name, ReactionRun::Of(run));
    let mut calls = Vec::with_capacity(2);
    for sla in [86_400 * 7, 60] {
        let created = admin
            .reactions
            .create(
                &ReactionRequest::new(&group, &call_pipe.name)
                    .parent(run)
                    .sla(sla),
            )
            .await?;
        let reaction = admin.reactions.get(&group, created.id).await?;
        calls.push(admin.jobs.get(reaction.jobs[0]).await?);
    }
    assert!(calls[1].deadline < calls[0].deadline);
    // the run's worker takes the early call, then the late one
    let run_worker = generators::gen_string(24);
    workload_worker(&run_worker, &group, ImageScaler::K8s, Some(run), &admin).await?;
    for expected in [&calls[1], &calls[0]] {
        let claimed = admin
            .jobs
            .claim_workload(&group, call_tool, "cluster0", "node0", &run_worker, 1)
            .await?;
        is!(claimed.len(), 1);
        is!(claimed[0].id, expected.id);
    }
    generators::delete_worker(&worker, &admin).await?;
    generators::delete_worker(&run_worker, &admin).await?;
    Ok(())
}

/// The workload claim checks the worker's user and claims each job exactly once
#[tokio::test]
async fn workload_claim_checks() -> Result<(), Error> {
    // a group with two users who can each claim for their own workers
    let admin = test_utilities::admin_client().await?;
    generators::node("cluster0", "node0", Resources::default(), &admin).await?;
    let suffix = generators::name_suffix();
    let io = generators::moon_user(&admin, "io", &suffix).await?;
    let carme = generators::moon_user(&admin, "carme", &suffix).await?;
    let roles = generators::GroupRoles::default()
        .user(&io.username)
        .user(&carme.username);
    let group = generators::god_group(&admin, "mercury", &suffix, &roles).await?;
    let (pipe, tool) = one_stage_pipe(&group, ImageScaler::K8s, &admin).await?;
    // io queues five jobs
    let reqs = (0..5)
        .map(|_| ReactionRequest::new(&group, &pipe.name))
        .collect::<Vec<ReactionRequest>>();
    io.client.reactions.create_bulk(&reqs).await?;
    let queue = workload_queue(&group, tool, &io.username, &JobStatus::Created);
    is!(zcard(&queue).await, 5);
    // carme can't claim with io's worker and nothing is popped
    let io_worker = generators::gen_string(24);
    workload_worker(&io_worker, &group, ImageScaler::K8s, None, &io.client).await?;
    let resp = carme
        .client
        .jobs
        .claim_workload(&group, tool, "cluster0", "node0", &io_worker, 5)
        .await;
    fail!(resp, 401);
    is!(zcard(&queue).await, 5);
    // io and an admin claim with io's worker
    let mine = io
        .client
        .jobs
        .claim_workload(&group, tool, "cluster0", "node0", &io_worker, 1)
        .await?;
    is!(mine.len(), 1);
    let admins = admin
        .jobs
        .claim_workload(&group, tool, "cluster0", "node0", &io_worker, 1)
        .await?;
    is!(admins.len(), 1);
    // two workers claiming three each at once claim the last three jobs exactly once
    let second_worker = generators::gen_string(24);
    workload_worker(&second_worker, &group, ImageScaler::K8s, None, &io.client).await?;
    let (left, right) = tokio::join!(
        io.client
            .jobs
            .claim_workload(&group, tool, "cluster0", "node0", &io_worker, 3),
        io.client
            .jobs
            .claim_workload(&group, tool, "cluster0", "node0", &second_worker, 3)
    );
    let (left, right) = (left?, right?);
    assert!(left.len() <= 3 && right.len() <= 3);
    let mut all = left
        .iter()
        .chain(right.iter())
        .chain(mine.iter())
        .chain(admins.iter())
        .map(|job| job.id)
        .collect::<Vec<Uuid>>();
    all.sort();
    all.dedup();
    is!(all.len(), 5);
    is!(left.len() + right.len(), 3);
    generators::delete_worker(&io_worker, &admin).await?;
    generators::delete_worker(&second_worker, &admin).await?;
    Ok(())
}

/// The workload claim keeps the worker user check for full tokens
#[tokio::test]
async fn workload_claim_user_check() -> Result<(), Error> {
    // a worker of user A with one queued job
    let admin = test_utilities::admin_client().await?;
    generators::node("cluster0", "node0", Resources::default(), &admin).await?;
    let suffix = generators::name_suffix();
    let a = generators::moon_user(&admin, "adrastea", &suffix).await?;
    let b = generators::moon_user(&admin, "metis", &suffix).await?;
    let roles = generators::GroupRoles::default()
        .user(&a.username)
        .user(&b.username);
    let group = generators::god_group(&admin, "bacchus", &suffix, &roles).await?;
    let (pipe, tool) = one_stage_pipe(&group, ImageScaler::K8s, &admin).await?;
    let reqs = (0..2)
        .map(|_| ReactionRequest::new(&group, &pipe.name))
        .collect::<Vec<ReactionRequest>>();
    a.client.reactions.create_bulk(&reqs).await?;
    let worker = generators::gen_string(24);
    workload_worker(&worker, &group, ImageScaler::K8s, None, &a.client).await?;
    let queue = workload_queue(&group, tool, &a.username, &JobStatus::Created);
    // B's full token is refused and nothing is popped
    let resp = b
        .client
        .jobs
        .claim_workload(&group, tool, "cluster0", "node0", &worker, 1)
        .await;
    fail!(resp, 401);
    is!(zcard(&queue).await, 2);
    // A's token and an admin's token are served
    is!(
        a.client
            .jobs
            .claim_workload(&group, tool, "cluster0", "node0", &worker, 1)
            .await?
            .len(),
        1
    );
    is!(
        admin
            .jobs
            .claim_workload(&group, tool, "cluster0", "node0", &worker, 1)
            .await?
            .len(),
        1
    );
    generators::delete_worker(&worker, &admin).await?;
    Ok(())
}

/// Group stats count shared workload jobs per tool instance and user, and the runtimes update
/// prunes tool instances idle for over an hour
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn workload_stats_and_prune() -> Result<(), Error> {
    // a group whose owner queues jobs of four tool instances
    let admin = test_utilities::admin_client().await?;
    generators::node("cluster0", "node0", Resources::default(), &admin).await?;
    let suffix = generators::name_suffix();
    let owner = generators::moon_user(&admin, "himalia", &suffix).await?;
    let roles = generators::GroupRoles::default().owner(&owner.username);
    let group = generators::god_group(&admin, "apollo", &suffix, &roles).await?;
    let mut pipes = Vec::new();
    for _ in 0..4 {
        pipes.push(one_stage_pipe(&group, ImageScaler::K8s, &admin).await?);
    }
    for (pipe, _) in &pipes {
        owner
            .client
            .reactions
            .create(&ReactionRequest::new(&group, &pipe.name))
            .await?;
    }
    let ids = pipes.iter().map(|(_, id)| *id).collect::<Vec<Uuid>>();
    // the stats report each tool instance's queued job for the owner
    let stats = admin.system.stats().await?;
    let group_stats = stats.groups.get(&group).expect("the group's stats");
    for id in &ids {
        let user_stats = group_stats.workloads[id]
            .get(&owner.username)
            .expect("the owner's stats");
        is!(user_stats.created, 1);
        is!(user_stats.total, 1);
    }
    // each tool instance is scored with the time it was last queued
    let set = format!("{}:workloads:{group}", ns());
    let now = chrono::Utc::now().timestamp();
    for id in &ids {
        let score: Option<i64> = redis::cmd("zscore")
            .arg(&set)
            .arg(id.to_string())
            .query_async(&mut redis_conn().await)
            .await
            .expect("a score");
        assert!((now - score.expect("a score")).abs() < 600);
    }
    // reading stats removes nothing
    is!(zcard(&set).await, 4);
    // the first tool instance only finished and the second is running
    let worker = generators::gen_string(24);
    workload_worker(&worker, &group, ImageScaler::K8s, None, &owner.client).await?;
    let done = owner
        .client
        .jobs
        .claim_workload(&group, ids[0], "cluster0", "node0", &worker, 1)
        .await?;
    owner
        .client
        .jobs
        .proceed(&done[0], &generators::stage_logs(), 5, &worker)
        .await?;
    owner
        .client
        .jobs
        .claim_workload(&group, ids[1], "cluster0", "node0", &worker, 1)
        .await?;
    // the fourth tool instance only finished too
    let late = owner
        .client
        .jobs
        .claim_workload(&group, ids[3], "cluster0", "node0", &worker, 1)
        .await?;
    owner
        .client
        .jobs
        .proceed(&late[0], &generators::stage_logs(), 5, &worker)
        .await?;
    // an hour later the first, second, and fourth are idle by score and the third was queued
    // half an hour ago
    let mut conn = redis_conn().await;
    for (id, age) in [(ids[0], 61), (ids[1], 61), (ids[2], 30), (ids[3], 61)] {
        let _: () = redis::cmd("zadd")
            .arg(&set)
            .arg(now - age * 60)
            .arg(id.to_string())
            .query_async(&mut conn)
            .await
            .expect("age the tool instance");
    }
    // a job of the fourth tool instance is queued while the prune is paused before removing it
    test_utilities::faults::pause_workload_prune(&group);
    let updater = admin.clone();
    let update = tokio::spawn(async move { updater.jobs.update_workload_runtimes().await });
    while !test_utilities::faults::workload_prune_paused(&group) {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    owner
        .client
        .reactions
        .create(&ReactionRequest::new(&group, &pipes[3].0.name))
        .await?;
    test_utilities::faults::release_workload_prune(&group);
    update.await.expect("the update task")?;
    // the finished tool instance was averaged and then removed
    let runtimes = admin.jobs.workload_runtimes().await?;
    is!(runtimes.runtimes.get(&ids[0]), Some(&5.0));
    let members = zmembers(&set).await;
    is!(members.contains(&ids[0].to_string()), false);
    // the running, recently queued, and requeued tool instances are kept
    is!(members.contains(&ids[1].to_string()), true);
    is!(members.contains(&ids[2].to_string()), true);
    is!(members.contains(&ids[3].to_string()), true);
    // the removed tool instance's counts are no longer reported
    let stats = admin.system.stats().await?;
    let group_stats = stats.groups.get(&group).expect("the group's stats");
    is!(group_stats.workloads.contains_key(&ids[0]), false);
    generators::delete_worker(&worker, &admin).await?;
    Ok(())
}

/// External tool stages are claimed by registered external workers and spawned by nothing
#[tokio::test]
async fn external_end_to_end() -> Result<(), Error> {
    // a reaction on an External tool
    let admin = test_utilities::admin_client().await?;
    generators::node("cluster0", "node0", Resources::default(), &admin).await?;
    let suffix = generators::name_suffix();
    let creator = generators::named_user_with_role(
        &admin,
        &generators::readable_name("sinope", &suffix),
        UserRole::Developer {
            k8s: true,
            bare_metal: true,
            windows: true,
            kvm: true,
            external: true,
        },
    )
    .await?;
    let stranger = generators::moon_user(&admin, "pasiphae", &suffix).await?;
    let roles = generators::GroupRoles::default()
        .owner(&creator.username)
        .user(&stranger.username);
    let group = generators::god_group(&admin, "diana", &suffix, &roles).await?;
    let (pipe, tool) = one_stage_pipe(&group, ImageScaler::External, &admin).await?;
    let created = creator
        .client
        .reactions
        .create(&ReactionRequest::new(&group, &pipe.name))
        .await?;
    let reaction = admin.reactions.get(&group, created.id).await?;
    let job = admin.jobs.get(reaction.jobs[0]).await?;
    is!(job.scaler, ImageScaler::External);
    is!(job.generator, false);
    // a non-admin can't register an External worker
    let worker = generators::gen_string(24);
    let list = WorkerRegistrationList::default().add(
        "cluster0",
        "node0",
        &worker,
        &creator.username,
        &group,
        "pipe",
        "stage",
        Resources::default(),
        Pools::Deadline,
    );
    let resp = creator
        .client
        .system
        .register_workers(ImageScaler::External, &list)
        .await;
    fail!(resp, 401);
    // after an admin registers one for the creator, the creator claims the job
    admin
        .system
        .register_workers(ImageScaler::External, &list)
        .await?;
    let stranger_claim = stranger
        .client
        .jobs
        .claim_workload(&group, tool, "cluster0", "node0", &worker, 1)
        .await;
    let stranger_empty = stranger_claim.map_or(true, |jobs| jobs.is_empty());
    is!(stranger_empty, true);
    let claimed = creator
        .client
        .jobs
        .claim_workload(&group, tool, "cluster0", "node0", &worker, 1)
        .await?;
    is!(claimed.len(), 1);
    is!(claimed[0].id, job.id);
    // proceeding with the worker's name completes it
    creator
        .client
        .jobs
        .proceed(&claimed[0], &generators::stage_logs(), 1, &worker)
        .await?;
    let reaction = admin.reactions.get(&group, created.id).await?;
    is!(reaction.status, ReactionStatus::Completed);
    // the worker is deleted
    admin
        .system
        .delete_workers(
            ImageScaler::External,
            &WorkerDeleteMap::default().add(&worker),
        )
        .await?;
    Ok(())
}

/// The deadlines response omits the run of pipeline jobs and includes the run of run jobs
#[tokio::test]
async fn deadlines_response_run_key() -> Result<(), Error> {
    // a pipeline reaction and a reaction that is its own run
    let admin = test_utilities::admin_client().await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let (pipe, _) = one_stage_pipe(&group, ImageScaler::K8s, &admin).await?;
    let (run_pipe, _) = one_stage_pipe(&group, ImageScaler::K8s, &admin).await?;
    test_utilities::faults::run_reactions(&run_pipe.name, ReactionRun::Own);
    let start = chrono::Utc::now() - chrono::Duration::days(1);
    let plain = admin
        .reactions
        .create(&ReactionRequest::new(&group, &pipe.name))
        .await?;
    let run = admin
        .reactions
        .create(&ReactionRequest::new(&group, &run_pipe.name))
        .await?;
    let plain_job = admin.reactions.get(&group, plain.id).await?.jobs[0];
    let run_job = admin.reactions.get(&group, run.id).await?.jobs[0];
    // read the raw deadlines response
    let end = start + chrono::Duration::days(365 * 100);
    let url = format!(
        "{}/api/jobs/deadlines/K8s/{}/{}?limit=100000",
        admin.host,
        start.timestamp(),
        end.timestamp()
    );
    let raw: Vec<serde_json::Value> = reqwest::Client::new()
        .get(&url)
        .header(
            "authorization",
            test_utilities::auth_header(&token_value(&admin).await?),
        )
        .send()
        .await?
        .json()
        .await?;
    let find = |id: Uuid| {
        raw.iter()
            .find(|deadline| deadline["job_id"] == serde_json::json!(id))
            .cloned()
            .expect("a deadline")
    };
    is!(find(plain_job).get("run").is_none(), true);
    is!(find(run_job)["run"], serde_json::json!(run.id));
    Ok(())
}

/// Get the token a client authenticates with
///
/// # Arguments
///
/// * `client` - The client whose token to get
async fn token_value(client: &Thorium) -> Result<String, Error> {
    // read the token through the user's own info
    Ok(client.users.info().await?.token)
}

/// The runtimes routes are admin only and return each tool instance's averaged runtime
#[tokio::test]
async fn workload_runtime_routes() -> Result<(), Error> {
    // a tool whose jobs complete
    let admin = test_utilities::admin_client().await?;
    generators::node("cluster0", "node0", Resources::default(), &admin).await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let (pipe, tool) = one_stage_pipe(&group, ImageScaler::K8s, &admin).await?;
    let reqs = (0..2)
        .map(|_| ReactionRequest::new(&group, &pipe.name))
        .collect::<Vec<ReactionRequest>>();
    admin.reactions.create_bulk(&reqs).await?;
    let worker = generators::gen_string(24);
    workload_worker(&worker, &group, ImageScaler::K8s, None, &admin).await?;
    let claimed = admin
        .jobs
        .claim_workload(&group, tool, "cluster0", "node0", &worker, 2)
        .await?;
    for (job, runtime) in claimed.iter().zip([10, 30]) {
        admin
            .jobs
            .proceed(job, &generators::stage_logs(), runtime, &worker)
            .await?;
    }
    // an update writes the averaged runtime keyed by tool instance
    admin.jobs.update_workload_runtimes().await?;
    let runtimes = admin.jobs.workload_runtimes().await?;
    is!(runtimes.runtimes.get(&tool), Some(&20.0));
    // a non-admin can't use either route
    let user = generators::user_with_role(&admin, UserRole::User).await?;
    fail!(user.client.jobs.update_workload_runtimes().await, 401);
    fail!(user.client.jobs.workload_runtimes().await, 401);
    generators::delete_worker(&worker, &admin).await?;
    Ok(())
}

#[tokio::test]
async fn proceed() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // register our test node
    generators::node("cluster0", "node0", Resources::default(), &client).await?;
    // Create a group to test reactions creation in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a random pipeline
    let pipe_req = generators::pipelines(&group, 1, false, &client)
        .await?
        .remove(0);
    // get the pipeline for this pipeline order
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // Create a random reaction based on our pipeline request
    let req = generators::gen_reaction(&group, &pipe, None);
    // get a timestamp before we create this reaction so we ensure its fits in the deadline stream
    let start = Utc::now();
    // make sure that we were able to create a reaction and our jobs
    let id = client.reactions.create(&req).await?;
    // make sure this external job is not in the deadline stream
    let end = start + chrono::Duration::hours(100_000_000);
    // attempt to claim a job for every stage of our reaction
    for stage in pipe.order.iter().flatten() {
        // register our test worker
        generators::worker(
            "cluster0", "node0", "proceed", &group, &pipe.name, stage, &client,
        )
        .await?;
        // make sure this stage updated the stage status counters correctly
        let stats = client.system.stats().await?;
        is!(get_stats!(stats, group, pipe, stage).created, 1);
        is!(get_stats!(stats, group, pipe, stage).running, 0);
        is!(get_stats!(stats, group, pipe, stage).completed, 0);
        is!(get_stats!(stats, group, pipe, stage).failed, 0);
        is!(get_stats!(stats, group, pipe, stage).sleeping, 0);
        is!(get_stats!(stats, group, pipe, stage).total, 1);
        // get our reactions data
        let react = client.reactions.get(&req.group, id.id).await?;
        // try to claim a job
        let job = client
            .jobs
            .claim_workload(
                &req.group,
                generators::stage_instance(&pipe, stage),
                "cluster0",
                "node0",
                "proceed",
                1,
            )
            .await?;
        // check if any of our deadlines match our external job
        let deadlines = client
            .jobs
            .deadlines(ImageScaler::K8s, &start, &end, 10_000)
            .await?;
        let found = deadlines.iter().any(|item| item.job_id == job[0].id);
        is!(found, true);
        // make sure this job matches our reaction
        is!(&job, react);
        // build random stage logs
        let logs = generators::stage_logs();
        // make sure this stage updated the stage status counters correctly
        let stats = client.system.stats().await?;
        is!(get_stats!(stats, group, pipe, stage).created, 0);
        is!(get_stats!(stats, group, pipe, stage).running, 1);
        is!(get_stats!(stats, group, pipe, stage).completed, 0);
        is!(get_stats!(stats, group, pipe, stage).failed, 0);
        is!(get_stats!(stats, group, pipe, stage).sleeping, 0);
        is!(get_stats!(stats, group, pipe, stage).total, 1);
        // proceed with this stage
        client.jobs.proceed(&job[0], &logs, 121, "proceed").await?;
        // check our stage logs were correct
        client
            .reactions
            .logs(&group, &id.id, stage, &ReactionListParams::default())
            .await?;
        // make sure this stage updated the stage status counters correctly
        let stats = client.system.stats().await?;
        is!(get_stats!(stats, group, pipe, stage).created, 0);
        is!(get_stats!(stats, group, pipe, stage).running, 0);
        is!(get_stats!(stats, group, pipe, stage).completed, 1);
        is!(get_stats!(stats, group, pipe, stage).failed, 0);
        is!(get_stats!(stats, group, pipe, stage).sleeping, 0);
        is!(get_stats!(stats, group, pipe, stage).total, 1);
        // delete our worker
        generators::delete_worker("proceed", &client).await?;
    }
    Ok(())
}

#[tokio::test]
async fn error() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group to test reactions creation in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a random pipeline
    let pipe_req = generators::pipelines(&group, 1, false, &client)
        .await?
        .remove(0);
    // get the pipeline for this pipeline order
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // Create a random reaction based on our pipeline request
    let req = generators::gen_reaction(&group, &pipe, None);
    // make sure that we were able to create a reaction and our jobs
    let id = client.reactions.create(&req).await?;
    // get our reactions data
    let react = client.reactions.get(&req.group, id.id).await?;
    // get the name of the first stage of this pipeline
    let stage = &pipe.order[0][0];
    // register our test node
    generators::node("cluster0", "node0", Resources::default(), &client).await?;
    // register our test worker
    generators::worker(
        "cluster0", "node0", "error", &group, &pipe.name, stage, &client,
    )
    .await?;
    // make sure this stage updated the stage status counters correctly
    let stats = client.system.stats().await?;
    is!(get_stats!(stats, group, pipe, stage).created, 1);
    is!(get_stats!(stats, group, pipe, stage).running, 0);
    is!(get_stats!(stats, group, pipe, stage).completed, 0);
    is!(get_stats!(stats, group, pipe, stage).failed, 0);
    is!(get_stats!(stats, group, pipe, stage).sleeping, 0);
    is!(get_stats!(stats, group, pipe, stage).total, 1);
    // try to claim a job for the first stage
    let job = client
        .jobs
        .claim_workload(
            &req.group,
            generators::stage_instance(&pipe, stage),
            "cluster0",
            "node0",
            "error",
            1,
        )
        .await?;
    // make sure this stage updated the stage status counters correctly
    let stats = client.system.stats().await?;
    is!(get_stats!(stats, group, pipe, stage).created, 0);
    is!(get_stats!(stats, group, pipe, stage).running, 1);
    is!(get_stats!(stats, group, pipe, stage).completed, 0);
    is!(get_stats!(stats, group, pipe, stage).failed, 0);
    is!(get_stats!(stats, group, pipe, stage).sleeping, 0);
    is!(get_stats!(stats, group, pipe, stage).total, 1);
    // make sure this job matches our reaction
    is!(&job, react);
    // build random stage logs
    let logs = generators::stage_logs().code(137);
    // error out this stage
    client.jobs.error(&job[0].id, &logs, "error").await?;
    // make sure this stage updated the stage status counters correctly
    let stats = client.system.stats().await?;
    is!(get_stats!(stats, group, pipe, stage).created, 0);
    is!(get_stats!(stats, group, pipe, stage).running, 0);
    is!(get_stats!(stats, group, pipe, stage).completed, 0);
    is!(get_stats!(stats, group, pipe, stage).failed, 1);
    is!(get_stats!(stats, group, pipe, stage).sleeping, 0);
    is!(get_stats!(stats, group, pipe, stage).total, 1);
    // delete our worker
    generators::delete_worker("error", &client).await?;
    Ok(())
}

#[tokio::test]
async fn empty() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group to test reactions creation in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a random pipeline
    let pipe_req = generators::pipelines(&group, 1, false, &client)
        .await?
        .remove(0);
    // get the pipeline for this pipeline order
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // register our test node
    generators::node("cluster0", "node0", Resources::default(), &client).await?;
    // attempt to claim a job for every stage of our reaction
    for stage in pipe.order.iter().flatten() {
        // register our test worker
        generators::worker(
            "cluster0", "node0", "empty", &group, &pipe.name, stage, &client,
        )
        .await?;
        // try to claim a job
        let job = client
            .jobs
            .claim_workload(
                &group,
                generators::stage_instance(&pipe, stage),
                "cluster0",
                "node0",
                "empty",
                1,
            )
            .await?;
        // make sure this returned an empty list
        is!(job.is_empty(), true);
        // delete our worker
        generators::delete_worker("empty", &client).await?;
    }
    Ok(())
}

#[tokio::test]
async fn external_reset() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group to test reactions creation in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a random pipeline
    let pipe_req = generators::pipelines(&group, 1, true, &client)
        .await?
        .remove(0);
    // get the pipeline for this pipeline order
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // register our test node
    generators::node("cluster0", "node0", Resources::default(), &client).await?;
    // Create a random reaction based on our pipeline request
    let req = generators::gen_reaction(&group, &pipe, None);
    // get a timestamp before we create this reaction so we ensure its fits in the deadline stream
    let start = Utc::now();
    // make sure this external job is not in the deadline stream
    let end = start + chrono::Duration::hours(100_000_000);
    // make sure that we were able to create a reaction and our jobs
    let id = client.reactions.create(&req).await?;
    // check if any of our deadlines match our external job
    let deadlines = client
        .jobs
        .deadlines(ImageScaler::K8s, &start, &end, 10_000)
        .await?;
    let found = deadlines.iter().any(|item| item.job_id == id.id);
    is!(found, false);
    // get our reactions data
    let react = client.reactions.get(&req.group, id.id).await?;
    // get our stage name
    let stage = &pipe.order[0][0];
    // register our test worker
    generators::worker_ext(
        "cluster0",
        "node0",
        "external_reset",
        &group,
        &pipe.name,
        stage,
        &client,
    )
    .await?;
    // try to claim a job for the first stage
    let job = client
        .jobs
        .claim_workload(
            &req.group,
            generators::stage_instance(&pipe, stage),
            "cluster0",
            "node0",
            "external_reset",
            1,
        )
        .await?;
    // make sure this job matches our reaction
    is!(&job, react);
    // build this list of jobs to reset
    let reset_req = JobResets::with_capacity(ImageScaler::K8s, "Test", 1).add(job[0].id);
    // reset this job
    client.jobs.bulk_reset(&reset_req).await?;
    // check if any of our deadlines match our external job
    let deadlines = client
        .jobs
        .deadlines(ImageScaler::K8s, &start, &end, 10_000)
        .await?;
    let found = deadlines.iter().any(|item| item.job_id == id.id);
    is!(found, false);
    // delete this reaction so we don't break any remaining tests
    client.reactions.delete(&req.group, &id.id).await?;
    // delete our worker
    generators::delete_worker_ext("external_reset", &client).await?;
    Ok(())
}

#[tokio::test]
async fn external_proceed() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group to test reactions creation in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a random pipeline
    let pipe_req = generators::pipelines(&group, 1, true, &client)
        .await?
        .remove(0);
    // get the pipeline for this pipeline order
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // register our test node
    generators::node("cluster0", "node0", Resources::default(), &client).await?;
    // Create a random reaction based on our pipeline request
    let req = generators::gen_reaction(&group, &pipe, None);
    // get a timestamp before we create this reaction so we ensure its fits in the deadline stream
    let start = Utc::now();
    // make sure that we were able to create a reaction and our jobs
    let id = client.reactions.create(&req).await?;
    // make sure this external job is not in the deadline stream
    let end = start + chrono::Duration::hours(100_000_000);
    let deadlines = client
        .jobs
        .deadlines(ImageScaler::K8s, &start, &end, 10_000)
        .await?;
    // check if any of our deadlines match our external job
    let found = deadlines.iter().any(|item| item.job_id == id.id);
    is!(found, false);
    // attempt to claim a job for every stage of our reaction
    for stage in pipe.order.iter().flatten() {
        // get our reactions data
        let react = client.reactions.get(&req.group, id.id).await?;
        // register our test worker
        generators::worker_ext(
            "cluster0",
            "node0",
            "external_proceed",
            &group,
            &pipe.name,
            stage,
            &client,
        )
        .await?;
        // try to claim a job
        let job = client
            .jobs
            .claim_workload(
                &req.group,
                generators::stage_instance(&pipe, stage),
                "cluster0",
                "node0",
                "external_proceed",
                1,
            )
            .await?;
        // make sure this job matches our reaction
        is!(&job, react);
        // build random stage logs
        let logs = generators::stage_logs();
        // proceed with this stage
        client
            .jobs
            .proceed(&job[0], &logs, 121, "external_proceed")
            .await?;
        // delete our worker
        generators::delete_worker_ext("external_proceed", &client).await?;
    }
    Ok(())
}

#[tokio::test]
async fn checkpoint() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group to test reactions creation in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a random pipeline
    let pipe_req = generators::gen_generator_pipe(&group, &client).await?;
    // get the pipeline for this pipeline order
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // register our test node
    generators::node("cluster0", "node0", Resources::default(), &client).await?;
    // Create a random reaction based on our pipeline request
    let req = generators::gen_reaction(&group, &pipe, None);
    // make sure that we were able to create a reaction and our jobs
    let id = client.reactions.create(&req).await?;
    // attempt to claim a job for every stage of our reaction
    for stage in pipe.order.iter().flatten().take(1) {
        for i in 0..3 {
            // get our reactions data
            let react = client.reactions.get(&req.group, id.id).await?;
            // register our test worker
            generators::worker(
                "cluster0",
                "node0",
                "checkpoint",
                &group,
                &pipe.name,
                stage,
                &client,
            )
            .await?;
            // try to claim a job
            let jobs = client
                .jobs
                .claim_workload(
                    &req.group,
                    generators::stage_instance(&pipe, stage),
                    "cluster0",
                    "node0",
                    "checkpoint",
                    1,
                )
                .await?;
            // if this isn't the first loop then make sure our checkpoint was set
            if i != 0 {
                let check = vec![format!("checkpoint-{}", i - 1)];
                is!(jobs[0].args.kwargs.get("--checkpoint"), Some(&check));
            }
            // make sure this job matches our reaction
            is!(&jobs, react);
            // checkpoint this job
            let checkpoint = format!("checkpoint-{i}");
            client.jobs.checkpoint(&jobs[0], checkpoint).await?;
            // if this isn't the last loop then sleep the generator
            if i < 2 {
                // checkpoint this job
                let checkpoint = format!("checkpoint-{i}");
                // sleep this generator
                client.jobs.sleep(&jobs[0].id, checkpoint).await?;
            }
            // build random stage logs
            let logs = generators::stage_logs();
            // proceed with this stage
            client
                .jobs
                .proceed(&jobs[0], &logs, 121, "checkpoint")
                .await?;
            // delete our worker
            generators::delete_worker_ext("checkpoint", &client).await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn sleep() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // Create a group to test reactions creation in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a random pipeline
    let pipe_req = generators::gen_generator_pipe(&group, &client).await?;
    // get the pipeline for this pipeline order
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // register our test node
    generators::node("cluster0", "node0", Resources::default(), &client).await?;
    // Create a random reaction based on our pipeline request
    let req = generators::gen_reaction(&group, &pipe, None);
    // make sure that we were able to create a reaction and our jobs
    let id = client.reactions.create(&req).await?;
    // attempt to claim a job for every stage of our reaction
    for stage in pipe.order.iter().flatten().take(1) {
        for i in 0..3 {
            // get our reactions data
            let react = client.reactions.get(&req.group, id.id).await?;
            // register our test worker
            generators::worker(
                "cluster0", "node0", "sleep", &group, &pipe.name, stage, &client,
            )
            .await?;
            // try to claim a job
            let jobs = client
                .jobs
                .claim_workload(
                    &req.group,
                    generators::stage_instance(&pipe, stage),
                    "cluster0",
                    "node0",
                    "sleep",
                    1,
                )
                .await?;
            // if this isn't the first loop then make sure our checkpoint was set
            if i != 0 {
                let check = vec![format!("checkpoint-{}", i - 1)];
                is!(jobs[0].args.kwargs.get("--checkpoint"), Some(&check));
            }
            // make sure this job matches our reaction
            is!(&jobs, react);
            // if this isn't the last loop then sleep the generator
            if i < 2 {
                // checkpoint this job
                let checkpoint = format!("checkpoint-{i}");
                // sleep this generator
                client.jobs.sleep(&jobs[0].id, checkpoint).await?;
            }
            // build random stage logs
            let logs = generators::stage_logs();
            // proceed with this stage
            client.jobs.proceed(&jobs[0], &logs, 121, "sleep").await?;
            // delete our worker
            generators::delete_worker("sleep", &client).await?;
        }
    }
    Ok(())
}

/// Duplicate concurrent proceed requests for one job only count that job once
///
/// `proceed` only executes the Redis pipeline that increments the reaction's
/// `current_stage_progress` if the job still has the status the route handler checked. Two
/// overlapping proceed requests for the SAME job (an agent retry racing the original) therefore
/// can't both count the job, so a stage of three jobs is not treated as complete after only two
/// of them finished.
///
/// The race is probabilistic, so this races many reactions and checks that none of them
/// completes while one of its three jobs is still running.
#[tokio::test]
#[ignore = "probabilistic race between concurrent duplicate proceed requests"]
async fn duplicate_concurrent_proceed_counts_job_once() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // register our test node
    generators::node("cluster0", "node0", Resources::default(), &client).await?;
    // create a group to test proceeding in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a random pipeline with a single stage of three images
    let pipe_req = generators::pipelines(&group, 1, false, &client)
        .await?
        .remove(0);
    // get the pipeline for this pipeline order
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // get the three images in our only stage
    let images = pipe.order[0].clone();
    // register a worker for each image in our stage
    for (i, image) in images.iter().enumerate() {
        generators::worker(
            "cluster0",
            "node0",
            &format!("c4_04_{i}"),
            &group,
            &pipe.name,
            image,
            &client,
        )
        .await?;
    }
    // race duplicate proceeds against many reactions to widen the race window
    for _ in 0..25 {
        // build a random reaction based on our pipeline request
        let req = generators::gen_reaction(&group, &pipe, None);
        // create the reaction and its three jobs
        let id = client.reactions.create(&req).await?;
        // claim the job for each image in our stage
        let mut jobs = Vec::with_capacity(images.len());
        for (i, image) in images.iter().enumerate() {
            let mut claimed = client
                .jobs
                .claim_workload(
                    &group,
                    generators::stage_instance(&pipe, image),
                    "cluster0",
                    "node0",
                    &format!("c4_04_{i}"),
                    1,
                )
                .await?;
            jobs.push(claimed.remove(0));
        }
        // build random stage logs
        let logs = generators::stage_logs();
        // send two concurrent proceeds for the first job, at most one may succeed
        let _ = tokio::join!(
            client.jobs.proceed(&jobs[0], &logs, 1, "c4_04_0"),
            client.jobs.proceed(&jobs[0], &logs, 1, "c4_04_0")
        );
        // complete the second job once
        client.jobs.proceed(&jobs[1], &logs, 1, "c4_04_1").await?;
        // the third job is still running so the reaction must not be completed
        let react = client.reactions.get(&group, id.id).await?;
        assert_ne!(
            react.status,
            ReactionStatus::Completed,
            "a duplicate proceed must not count a job twice and complete a stage early"
        );
        // complete the third job so this reaction finishes
        client.jobs.proceed(&jobs[2], &logs, 1, "c4_04_2").await?;
    }
    // delete our workers
    for i in 0..images.len() {
        generators::delete_worker(&format!("c4_04_{i}"), &client).await?;
    }
    Ok(())
}

/// An error and a proceed racing for the same job are not both applied
///
/// `error` (like `proceed`) only executes the Redis pipeline that marks the job Failed if the job
/// is still Running. An error request racing a proceed (or a duplicate error) for the same job
/// therefore can't both be applied, so a job is never both counted toward its stage and failed,
/// and its reaction is never failed by a job that also completed.
///
/// The race is probabilistic, so this races many reactions and checks that the error and the
/// proceed never both succeed.
#[tokio::test]
#[ignore = "probabilistic race between concurrent error and proceed"]
async fn concurrent_error_and_proceed_apply_once() -> Result<(), Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // register our test node
    generators::node("cluster0", "node0", Resources::default(), &client).await?;
    // create a group to test erroring in
    let group = generators::groups(1, &client).await?.remove(0).name;
    // create a random pipeline with a single stage of three images
    let pipe_req = generators::pipelines(&group, 1, false, &client)
        .await?
        .remove(0);
    // get the pipeline for this pipeline order
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    // get the first image in our only stage
    let image = pipe.order[0][0].clone();
    // register a worker for this image
    generators::worker(
        "cluster0",
        "node0",
        "adj_error",
        &group,
        &pipe.name,
        &image,
        &client,
    )
    .await?;
    // race an error against a proceed for many jobs to widen the race window
    for _ in 0..25 {
        // build a random reaction based on our pipeline request
        let req = generators::gen_reaction(&group, &pipe, None);
        // create the reaction and its jobs
        client.reactions.create(&req).await?;
        // claim the job for our image
        let mut claimed = client
            .jobs
            .claim_workload(
                &group,
                generators::stage_instance(&pipe, &image),
                "cluster0",
                "node0",
                "adj_error",
                1,
            )
            .await?;
        let job = claimed.remove(0);
        // build random stage logs
        let logs = generators::stage_logs();
        // send an error and a proceed for the same job at once, at most one may succeed
        let (errored, proceeded) = tokio::join!(
            client.jobs.error(&job.id, &logs, "adj_error"),
            client.jobs.proceed(&job, &logs, 1, "adj_error")
        );
        assert!(
            !(errored.is_ok() && proceeded.is_ok()),
            "an error and a proceed for the same job must not both be applied"
        );
    }
    // delete our worker
    generators::delete_worker("adj_error", &client).await?;
    Ok(())
}

/// An admin can read one job without changing it or any event queue
#[tokio::test]
async fn get_job() -> Result<(), thorium::Error> {
    // get admin client
    let client = test_utilities::admin_client().await?;
    // create a group, pipeline, and reaction whose jobs we can read
    let group = generators::groups(1, &client).await?.remove(0).name;
    let pipe_req = generators::pipelines(&group, 1, false, &client)
        .await?
        .remove(0);
    let pipe = client.pipelines.get(&group, &pipe_req.name).await?;
    let req = generators::gen_reaction(&group, &pipe, None);
    let id = client.reactions.create(&req).await?.id;
    // get the reaction and its first job's id
    let react = client.reactions.get(&group, id).await?;
    let job_id = react.jobs[0];
    // capture the event queues before reading the job
    let triggers = events::queue_state(EventType::ReactionTrigger).await?;
    let sigma = events::queue_state(EventType::SigmaScannableResults).await?;
    // read the job as an admin
    let job = client.jobs.get(job_id).await?;
    // the job matches its reaction and first stage
    is!(job.id, job_id);
    is!(&job.group, &group);
    is!(&job.pipeline, &pipe.name);
    is!(job.reaction, id);
    is!(&job.stage, &pipe.order[0][0]);
    is!(&job.creator, &react.creator);
    is!(job.status, JobStatus::Created);
    // a second read returns the same job, so the read changed nothing
    let again = client.jobs.get(job_id).await?;
    is!(serde_json::to_value(&again)?, serde_json::to_value(&job)?);
    // the reaction is unchanged by the read
    let react_after = client.reactions.get(&group, id).await?;
    is!(react_after.status, react.status);
    is!(react_after.current_stage, react.current_stage);
    // the event queues are unchanged by the read
    is!(
        events::queue_state(EventType::ReactionTrigger).await?,
        triggers
    );
    is!(
        events::queue_state(EventType::SigmaScannableResults).await?,
        sigma
    );
    Ok(())
}

/// Reading a job needs an admin with a full token and a known job id
#[tokio::test]
async fn get_job_errors() -> Result<(), thorium::Error> {
    // get admin client
    let admin = test_utilities::admin_client().await?;
    // an unknown job id is not found
    fail!(admin.jobs.get(Uuid::new_v4()).await, 404);
    // create a reaction whose job a non-admin tries to read
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let pipe_req = generators::pipelines(&group, 1, false, &admin)
        .await?
        .remove(0);
    let pipe = admin.pipelines.get(&group, &pipe_req.name).await?;
    let req = generators::gen_reaction(&group, &pipe, None);
    let id = admin.reactions.create(&req).await?.id;
    let job_id = admin.reactions.get(&group, id).await?.jobs[0];
    // a non-admin full token is refused
    let user = generators::client(&admin).await?;
    fail!(user.jobs.get(job_id).await, 401);
    // a scoped token of an admin is refused
    let scoped = generators::scoped_client(&admin).await?;
    fail!(scoped.jobs.get(job_id).await, 401);
    // a request without a token is refused
    let url = format!("{}/api/jobs/{job_id}", admin.host);
    let resp = reqwest::Client::new().get(&url).send().await?;
    is!(resp.status().as_u16(), 401);
    Ok(())
}

/// Register a worker for a specific user with the admin client
///
/// # Arguments
///
/// * `admin` - The admin client to register the worker with
/// * `name` - The name of the worker to register
/// * `user` - The user this worker runs jobs for
/// * `group` - The group this worker runs jobs in
/// * `pipeline` - The pipeline this worker runs jobs for
/// * `stage` - The stage this worker runs jobs for
async fn register_worker_for(
    admin: &Thorium,
    name: &str,
    user: &str,
    group: &str,
    pipeline: &str,
    stage: &str,
) -> Result<(), Error> {
    // build a registration for this worker
    let workers = WorkerRegistrationList::default().add(
        "cluster0",
        "node0",
        name,
        user,
        group,
        pipeline,
        stage,
        Resources::default(),
        Pools::Deadline,
    );
    // register this worker
    admin
        .system
        .register_workers(ImageScaler::K8s, &workers)
        .await?;
    Ok(())
}

/// Build the authorization header a token sends
///
/// # Arguments
///
/// * `token` - The raw token to send
fn token_header(token: &str) -> String {
    // base64 encode our token like the client does
    format!("token {}", STANDARD.encode(token.as_bytes()))
}

/// Send a job handle request directly so the `worker` query parameter can be left out
///
/// # Arguments
///
/// * `host` - The Thorium host to send the request to
/// * `auth` - The authorization header to send
/// * `path` - The job handle path after `/api/jobs/handle/`
async fn raw_handle(host: &str, auth: &str, path: &str) -> Result<u16, Error> {
    // send this handle request with empty logs
    let resp = reqwest::Client::new()
        .post(format!("{host}/api/jobs/handle/{path}"))
        .header("authorization", auth)
        .json(&StageLogsAdd::default())
        .send()
        .await?;
    Ok(resp.status().as_u16())
}

/// A pipeline in a group that two non-admin users can edit
struct HandleSetup {
    /// The admin client
    admin: Thorium,
    /// The user whose worker claims jobs
    owner: generators::TestUser,
    /// Another editor of the group
    other: generators::TestUser,
    /// The group the pipeline is in
    group: String,
    /// The pipeline to create reactions for
    pipeline: String,
    /// The first stage of the pipeline
    stage: String,
    /// The tool instance the first stage runs
    tool_instance: Uuid,
}

impl HandleSetup {
    /// Create two users, a group they both edit, and a pipeline in it
    async fn new() -> Result<Self, Error> {
        // get admin client
        let admin = test_utilities::admin_client().await?;
        // register our test node
        generators::node("cluster0", "node0", Resources::default(), &admin).await?;
        // create two users who can both edit one group
        let owner = generators::user_with_role(&admin, UserRole::User).await?;
        let other = generators::user_with_role(&admin, UserRole::User).await?;
        let roles = generators::GroupRoles::default()
            .user(&owner.username)
            .user(&other.username);
        let group = generators::group_with_roles(&roles, GroupAllowed::default(), &admin).await?;
        // create a pipeline in this group with the admin
        let pipe_req = generators::pipelines(&group, 1, false, &admin)
            .await?
            .remove(0);
        let pipe = admin.pipelines.get(&group, &pipe_req.name).await?;
        let stage = pipe.order[0][0].clone();
        let tool_instance = generators::stage_instance(&pipe, &stage);
        Ok(HandleSetup {
            admin,
            owner,
            other,
            group,
            pipeline: pipe.name,
            stage,
            tool_instance,
        })
    }

    /// Create a reaction as the owner so its jobs are the owner's
    async fn reaction(&self) -> Result<(), Error> {
        // get our pipeline
        let pipe = self
            .admin
            .pipelines
            .get(&self.group, &self.pipeline)
            .await?;
        // create a reaction as the owner
        let req = generators::gen_reaction(&self.group, &pipe, None);
        self.owner.client.reactions.create(&req).await?;
        Ok(())
    }

    /// Claim one job for a worker with a client
    ///
    /// # Arguments
    ///
    /// * `client` - The client to claim with
    /// * `worker` - The worker to claim for
    async fn claim(&self, client: &Thorium, worker: &str) -> Result<Vec<GenericJob>, Error> {
        client
            .jobs
            .claim_workload(
                &self.group,
                self.tool_instance,
                "cluster0",
                "node0",
                worker,
                1,
            )
            .await
    }
}

/// Job handles are bound to the claiming worker for every non-admin caller
#[tokio::test]
async fn job_handles_bound_to_claiming_worker() -> Result<(), Error> {
    // set up two editors of a group with a pipeline
    let setup = HandleSetup::new().await?;
    let host = setup.admin.host.clone();
    let owner = &setup.owner;
    let other = &setup.other;
    // register two workers for the owner
    let worker = generators::gen_string(24);
    let other_worker = generators::gen_string(24);
    for name in [&worker, &other_worker] {
        register_worker_for(
            &setup.admin,
            name,
            &owner.username,
            &setup.group,
            &setup.pipeline,
            &setup.stage,
        )
        .await?;
    }
    // create four reactions as the owner
    for _ in 0..4 {
        setup.reaction().await?;
    }
    // a claim for the owner's worker with another user's token is rejected
    fail!(setup.claim(&other.client, &worker).await, 401);
    // the owner's worker claims a job
    let job = setup.claim(&owner.client, &worker).await?.remove(0);
    let logs = generators::stage_logs();
    // proceed and error naming another worker are rejected
    fail!(
        owner
            .client
            .jobs
            .proceed(&job, &logs, 1, &other_worker)
            .await,
        401
    );
    fail!(
        owner.client.jobs.error(&job.id, &logs, &other_worker).await,
        401
    );
    // proceed and error without a worker are a bad request
    let proceed_path = format!("{}/proceed/1", job.id);
    let error_path = format!("{}/error", job.id);
    is!(
        raw_handle(&host, &owner.auth_header(), &proceed_path).await?,
        400
    );
    is!(
        raw_handle(&host, &owner.auth_header(), &error_path).await?,
        400
    );
    // another editor of the group can't proceed, error, sleep, or checkpoint this job
    fail!(
        other.client.jobs.proceed(&job, &logs, 1, &worker).await,
        401
    );
    fail!(other.client.jobs.error(&job.id, &logs, &worker).await, 401);
    fail!(other.client.jobs.sleep(&job.id, "check").await, 401);
    fail!(other.client.jobs.checkpoint(&job, "check").await, 401);
    // the owner can checkpoint and sleep the job by id alone like generator code does, and
    // sleep only accepts a running job, so the rejected calls above changed nothing
    owner.client.jobs.checkpoint(&job, "check-0").await?;
    owner.client.jobs.sleep(&job.id, "check-1").await?;
    // the owner's worker proceeds with the job
    owner.client.jobs.proceed(&job, &logs, 1, &worker).await?;
    // the owner's worker errors out its next job
    let job = setup.claim(&owner.client, &worker).await?.remove(0);
    owner.client.jobs.error(&job.id, &logs, &worker).await?;
    // a job reset to created can't be handled by its old worker
    let job = setup.claim(&owner.client, &worker).await?.remove(0);
    let resets = JobResets::with_capacity(ImageScaler::K8s, "Test", 1).add(job.id);
    setup.admin.jobs.bulk_reset(&resets).await?;
    fail!(
        owner.client.jobs.proceed(&job, &logs, 1, &worker).await,
        401
    );
    fail!(owner.client.jobs.error(&job.id, &logs, &worker).await, 401);
    // the admin errors out a claimed job naming the failed worker like the scaler does
    let job = setup.claim(&owner.client, &worker).await?.remove(0);
    setup.admin.jobs.error(&job.id, &logs, &worker).await?;
    // the admin errors out a claimed job without naming a worker
    let job = setup.claim(&owner.client, &worker).await?.remove(0);
    let admin_token = setup.admin.users.info().await?.token;
    is!(
        raw_handle(
            &host,
            &token_header(&admin_token),
            &format!("{}/error", job.id)
        )
        .await?,
        202
    );
    // clean up our workers
    for name in [&worker, &other_worker] {
        generators::delete_worker(name, &setup.admin).await?;
    }
    Ok(())
}

/// A worker delete needs every worker to be in the caller's groups, and an Analyst's
/// groups are the ones it is a member of
#[tokio::test]
async fn worker_delete_checks_groups() -> Result<(), Error> {
    // get admin client
    let admin = test_utilities::admin_client().await?;
    // register our test node
    generators::node("cluster0", "node0", Resources::default(), &admin).await?;
    // create an owner of two groups
    let owner = generators::user_with_role(&admin, UserRole::User).await?;
    let roles = generators::GroupRoles::default().owner(&owner.username);
    let in_scope = generators::group_with_roles(&roles, GroupAllowed::default(), &admin).await?;
    let out_of_scope =
        generators::group_with_roles(&roles, GroupAllowed::default(), &admin).await?;
    // register the owner's workers in both groups
    let in_worker = generators::gen_string(24);
    let out_worker = generators::gen_string(24);
    let admin_worker = generators::gen_string(24);
    for (name, group) in [
        (&in_worker, &in_scope),
        (&out_worker, &out_of_scope),
        (&admin_worker, &out_of_scope),
    ] {
        register_worker_for(&admin, name, &owner.username, group, "pipe", "stage").await?;
    }
    // build a scoped token for the owner limited to one group
    let req = ScopedTokenRequest::new(Uuid::new_v4().simple().to_string()).group(&in_scope);
    let token = owner.client.users.create_scoped_token(&req).await?.token;
    let scoped = Thorium::build(admin.host.clone())
        .token(&token)
        .build()
        .await?;
    // the scoped token can't delete the owner's worker outside its groups
    fail!(generators::delete_worker(&out_worker, &scoped).await, 401);
    // the worker was kept
    is!(admin.system.get_worker(&out_worker).await?.name, out_worker);
    // a delete naming workers in and out of its groups is rejected and deletes neither
    let both = WorkerDeleteMap::default().add(&in_worker).add(&out_worker);
    fail!(
        scoped.system.delete_workers(ImageScaler::K8s, &both).await,
        401
    );
    is!(admin.system.get_worker(&in_worker).await?.name, in_worker);
    // the scoped token can delete the owner's worker in its groups
    generators::delete_worker(&in_worker, &scoped).await?;
    fail!(admin.system.get_worker(&in_worker).await, 404);
    // the owner's full token deletes its worker like the agent does at exit
    generators::delete_worker(&out_worker, &owner.client).await?;
    fail!(admin.system.get_worker(&out_worker).await, 404);
    // an admin deletes workers in any group like the scaler does
    generators::delete_worker(&admin_worker, &admin).await?;
    fail!(admin.system.get_worker(&admin_worker).await, 404);
    // an Analyst who is a member of only one group
    let analyst = generators::user_with_role(&admin, UserRole::Analyst).await?;
    let member_roles = generators::GroupRoles::default().user(&analyst.username);
    let member =
        generators::group_with_roles(&member_roles, GroupAllowed::default(), &admin).await?;
    let other = generators::group_with_roles(
        &generators::GroupRoles::default(),
        GroupAllowed::default(),
        &admin,
    )
    .await?;
    // register the analyst's workers in both groups
    let member_worker = generators::gen_string(24);
    let other_worker = generators::gen_string(24);
    for (name, group) in [(&member_worker, &member), (&other_worker, &other)] {
        register_worker_for(&admin, name, &analyst.username, group, "pipe", "stage").await?;
    }
    // the analyst's full token can't delete its worker in a group it isn't a member of
    fail!(
        generators::delete_worker(&other_worker, &analyst.client).await,
        401
    );
    is!(
        admin.system.get_worker(&other_worker).await?.name,
        other_worker
    );
    // the analyst's full token deletes its worker in its own group
    generators::delete_worker(&member_worker, &analyst.client).await?;
    fail!(admin.system.get_worker(&member_worker).await, 404);
    Ok(())
}
