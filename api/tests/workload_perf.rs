//! Measures what the workload claim and the per-workload stats cost in Redis round trips and time
//!
//! Redis counts commands per server, not per client, so the counts only hold while nothing else
//! uses the test Redis server. This is its own test binary so no other test file's tests send
//! commands while it counts, and the tests in it run one at a time. Don't share the test Redis
//! server with another run while this binary runs.

use std::collections::BTreeMap;
use std::time::Instant;
use thorium::models::{
    GroupRequest, GroupUpdate, GroupUsersRequest, ImageScaler, PipelineRequest, Pools,
    ReactionRequest, Resources, RunInputs, ScopedTokenRequest, ToolMechanism, ToolRunRequest,
    UserRole, WorkerRegistrationList, WorkflowRunRequest,
};
use thorium::test_utilities::{self, generators, runs};
use thorium::{Error, Thorium, is};
use tokio::sync::Mutex;
use uuid::Uuid;

/// Runs the tests in this binary one at a time, since every one sends Redis commands that would
/// land in the counted ones
static SERIAL: Mutex<()> = Mutex::const_new(());

/// The number of calls Redis has served for each command
type CommandCounts = BTreeMap<String, i64>;

/// The commands that write to Redis
const WRITES: [&str; 12] = [
    "zadd", "zrem", "hset", "hsetnx", "hdel", "del", "sadd", "srem", "expire", "lpush", "rpush",
    "ltrim",
];

/// A connection to the test Redis used to read its command stats
struct Stats {
    /// The open Redis connection, reused so reading stats doesn't add connection commands
    conn: redis::aio::MultiplexedConnection,
}

impl Stats {
    /// Connect to the test Redis
    async fn connect() -> Result<Self, Error> {
        // reuse the harness's pool so the connection selects the configured database
        let pool = test_utilities::get_redis_client(&test_utilities::CONF).await;
        let pooled = pool
            .get()
            .await
            .map_err(|err| Error::new(format!("Failed to connect to Redis: {err}")))?;
        Ok(Stats {
            conn: pooled.clone(),
        })
    }

    /// Read how many calls Redis has served for each command, leaving out `info` itself
    async fn counts(&mut self) -> Result<CommandCounts, Error> {
        // read the command stats
        let info: String = redis::cmd("info")
            .arg("commandstats")
            .query_async(&mut self.conn)
            .await
            .map_err(|err| Error::new(format!("Failed to read command stats: {err}")))?;
        // parse each `cmdstat_<name>:calls=<n>,...` line
        let counts = info
            .lines()
            .filter_map(|line| line.strip_prefix("cmdstat_"))
            .filter_map(|line| {
                let (name, stats) = line.split_once(':')?;
                let calls = stats.split(',').find_map(|kv| kv.strip_prefix("calls="))?;
                Some((name.to_owned(), calls.trim().parse::<i64>().ok()?))
            })
            .filter(|(name, _)| name != "info")
            .collect();
        Ok(counts)
    }
}

/// Get the commands served between two command stat snapshots
///
/// # Arguments
///
/// * `before` - The counts before an action
/// * `after` - The counts after an action
fn delta(before: &CommandCounts, after: &CommandCounts) -> CommandCounts {
    // keep only the commands whose count changed
    after
        .iter()
        .map(|(name, calls)| (name.clone(), calls - before.get(name).unwrap_or(&0)))
        .filter(|(_, calls)| *calls != 0)
        .collect()
}

/// Get a percentile of a sorted list of latencies in microseconds
///
/// # Arguments
///
/// * `sorted` - The sorted latencies
/// * `pct` - The percentile to get
fn percentile(sorted: &[u128], pct: usize) -> u128 {
    // pick the entry at this percentile
    sorted[(sorted.len() * pct / 100).min(sorted.len() - 1)]
}

/// A group with a one stage pipeline and a worker for the admin
struct ClaimSetup {
    /// The admin client
    admin: Thorium,
    /// The group the pipeline is in
    group: String,
    /// The pipeline to create reactions for
    pipeline: String,
    /// The tool instance the pipeline's stage runs
    tool_instance: Uuid,
    /// The admin's worker
    worker: String,
}

impl ClaimSetup {
    /// Create a group, a one stage pipeline, and a worker
    async fn new() -> Result<Self, Error> {
        // get admin client and register our test node
        let admin = test_utilities::admin_client().await?;
        generators::node("cluster0", "node0", Resources::default(), &admin).await?;
        // create a group with a one stage pipeline
        let group = generators::groups(1, &admin).await?.remove(0).name;
        let (stage, tool_instance) = generators::stage_tool(
            &group,
            &generators::gen_image_spec_for(ImageScaler::K8s),
            &admin,
        )
        .await?;
        let pipeline = generators::gen_string(20).to_lowercase();
        let req = PipelineRequest::new(&group, &pipeline, serde_json::json!([[&stage]]))
            .stage_tool(&stage, tool_instance);
        admin.pipelines.create(&req).await?;
        // register a worker for the admin
        let worker = generators::gen_string(24);
        let user = admin.users.info().await?.username;
        let workers = WorkerRegistrationList::default().add(
            "cluster0",
            "node0",
            &worker,
            user,
            &group,
            tool_instance,
            None,
            Resources::default(),
            Pools::Deadline,
        );
        admin
            .system
            .register_workers(ImageScaler::K8s, &workers)
            .await?;
        Ok(ClaimSetup {
            admin,
            group,
            pipeline,
            tool_instance,
            worker,
        })
    }

    /// Queue jobs by creating reactions in bulk
    ///
    /// # Arguments
    ///
    /// * `count` - The number of jobs to queue
    async fn queue(&self, count: usize) -> Result<(), Error> {
        // create the reactions 100 at a time
        let mut left = count;
        while left > 0 {
            let batch = left.min(100);
            let reqs = (0..batch)
                .map(|_| ReactionRequest::new(&self.group, &self.pipeline))
                .collect::<Vec<ReactionRequest>>();
            self.admin.reactions.create_bulk(&reqs).await?;
            left -= batch;
        }
        Ok(())
    }

    /// Claim one job and return how long the claim took in microseconds
    async fn claim(&self) -> Result<u128, Error> {
        // time one claim of one job
        let start = Instant::now();
        let jobs = self
            .admin
            .jobs
            .claim_workload(
                &self.group,
                self.tool_instance,
                "cluster0",
                "node0",
                &self.worker,
                1,
            )
            .await?;
        let elapsed = start.elapsed().as_micros();
        is!(jobs.len(), 1);
        Ok(elapsed)
    }
}

/// A workflow run with queued tool calls on its run queue and a per-run worker for them
struct RunClaimSetup {
    /// A client authenticated with the run's token
    scoped: Thorium,
    /// The group the run is in
    group: String,
    /// The Image tool instance the tool calls run
    image: Uuid,
    /// The container tool the harness calls
    tool: Uuid,
    /// The run's worker for the image
    worker: String,
}

impl RunClaimSetup {
    /// Start a workflow run and register its worker for the image its tool calls run
    async fn new() -> Result<Self, Error> {
        // a developer who owns a group with a workflow calling a container tool
        let admin = test_utilities::admin_client().await?;
        generators::node("cluster0", "node0", Resources::default(), &admin).await?;
        let runner = generators::user_with_role(
            &admin,
            UserRole::Developer {
                k8s: true,
                bare_metal: false,
                windows: false,
                external: false,
                kvm: false,
            },
        )
        .await?;
        let group = generators::groups(1, &admin).await?.remove(0).name;
        let update = GroupUpdate::default()
            .owners(thorium::models::GroupUsersUpdate::default().direct_add(&runner.username));
        admin.groups.update(&group, &update).await?;
        let groups = [group.as_str()];
        let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &admin).await?;
        let (_, tool) = runs::container_tool(&groups, image.id, &admin).await?;
        let workflow =
            runs::ready_workflow("Perf", &groups, &[(&tool, ToolMechanism::Cli)], &admin).await?;
        let sha256 = runner
            .client
            .files
            .create(generators::gen_sample(&group))
            .await?
            .sha256;
        // start the run and get its token
        let created = runner
            .client
            .reactions
            .run_workflow(
                workflow.id(),
                &WorkflowRunRequest::new(RunInputs::default().sample(&sha256)),
            )
            .await?;
        let token = admin.reactions.run_token(&group, created.id).await?;
        let scoped = Thorium::build(&admin.host)
            .token(&token.token)
            .build()
            .await?;
        // register the run's worker for the image
        let worker = generators::gen_string(24);
        let workers = WorkerRegistrationList::default().add(
            "cluster0",
            "node0",
            &worker,
            &runner.username,
            &group,
            image.id,
            Some(created.id),
            Resources::default(),
            Pools::Deadline,
        );
        admin
            .system
            .register_workers(ImageScaler::K8s, &workers)
            .await?;
        Ok(RunClaimSetup {
            scoped,
            group,
            image: image.id,
            tool: tool.id,
            worker,
        })
    }

    /// Queue tool calls on the run's queue with the run's token
    ///
    /// # Arguments
    ///
    /// * `count` - The number of tool calls to queue
    async fn queue(&self, count: usize) -> Result<(), Error> {
        // each call runs on the run's own inputs
        let req = ToolRunRequest::new(RunInputs::default());
        for _ in 0..count {
            self.scoped.reactions.run_tool(self.tool, &req).await?;
        }
        Ok(())
    }

    /// Claim one tool call job with the run's token and return how long it took in microseconds
    async fn claim(&self) -> Result<u128, Error> {
        // time one claim of one job
        let start = Instant::now();
        let jobs = self
            .scoped
            .jobs
            .claim_workload(
                &self.group,
                self.image,
                "cluster0",
                "node0",
                &self.worker,
                1,
            )
            .await?;
        let elapsed = start.elapsed().as_micros();
        is!(jobs.len(), 1);
        Ok(elapsed)
    }
}

/// The Redis commands 1,000 pipeline reactions created in one bulk request send, measured
/// before reactions recorded their provenance
const PIPELINE_COMMANDS_PER_1000: i64 = 47_027;

/// The Redis commands 100 workflow runs of one instance send, measured before reactions recorded
/// their provenance
const RUN_COMMANDS_PER_100: i64 = 11_300;

/// A group with a one stage pipeline and a ready workflow calling a container tool, both owned by
/// a developer
struct ProvenanceSetup {
    /// The developer who creates the reactions and runs
    runner: Thorium,
    /// The group everything is in
    group: String,
    /// The pipeline to create reactions for
    pipeline: String,
    /// The tool instance the pipeline's stage runs
    stage: Uuid,
    /// The workflow instance to run
    instance: Uuid,
    /// The harness the workflow's entry agent runs in
    harness: Uuid,
    /// The container tool the workflow's agent calls
    tool: Uuid,
    /// The Image tool instance the container tool pins
    image: Uuid,
    /// A file to run the workflow on
    sha256: String,
}

impl ProvenanceSetup {
    /// Create the group, the pipeline, the workflow, and a file
    async fn new() -> Result<Self, Error> {
        // a developer who owns a group
        let admin = test_utilities::admin_client().await?;
        let runner = generators::user_with_role(
            &admin,
            UserRole::Developer {
                k8s: true,
                bare_metal: false,
                windows: false,
                external: false,
                kvm: false,
            },
        )
        .await?;
        let group = generators::groups(1, &admin).await?.remove(0).name;
        let update = GroupUpdate::default()
            .owners(thorium::models::GroupUsersUpdate::default().direct_add(&runner.username));
        admin.groups.update(&group, &update).await?;
        let groups = [group.as_str()];
        // a one stage pipeline running an Image tool
        let (_, stage) = runs::image_tool(&groups, ImageScaler::K8s, &admin).await?;
        let pipeline = generators::gen_string(20).to_lowercase();
        let req = PipelineRequest::new(&group, &pipeline, serde_json::json!([["stage"]]))
            .stage_tool("stage", stage.id);
        admin.pipelines.create(&req).await?;
        // a ready workflow whose agent calls a container tool pinning another Image tool
        let (_, image) = runs::image_tool(&groups, ImageScaler::K8s, &admin).await?;
        let (_, tool) = runs::container_tool(&groups, image.id, &admin).await?;
        let workflow = Box::pin(runs::ready_workflow(
            "Perf",
            &groups,
            &[(&tool, ToolMechanism::Cli)],
            &admin,
        ))
        .await?;
        let sha256 = runner
            .client
            .files
            .create(generators::gen_sample(&group))
            .await?
            .sha256;
        Ok(ProvenanceSetup {
            runner: runner.client,
            group,
            pipeline,
            stage: stage.id,
            instance: workflow.id(),
            harness: workflow.harness.id,
            tool: tool.id,
            image: image.id,
            sha256,
        })
    }

    /// Create pipeline reactions in one bulk request
    ///
    /// # Arguments
    ///
    /// * `count` - The number of reactions to create
    async fn bulk(&self, count: usize) -> Result<(), Error> {
        // one request for every reaction
        let reqs = (0..count)
            .map(|_| ReactionRequest::new(&self.group, &self.pipeline))
            .collect::<Vec<ReactionRequest>>();
        let resp = self.runner.reactions.create_bulk(&reqs).await?;
        is!(resp.created.len(), count);
        Ok(())
    }

    /// Start one workflow run on the file
    async fn run(&self) -> Result<(), Error> {
        // run the workflow instance on the file
        let req = WorkflowRunRequest::new(RunInputs::default().sample(&self.sha256));
        self.runner
            .reactions
            .run_workflow(self.instance, &req)
            .await?;
        Ok(())
    }

    /// Get how many times each tool instance and the workflow instance were read from Scylla
    fn reads(&self) -> [u64; 5] {
        // read the counts in a fixed order
        [
            self.stage,
            self.instance,
            self.harness,
            self.tool,
            self.image,
        ]
        .map(test_utilities::faults::entity_reads)
    }
}

/// Recording run provenance costs no Redis round trips: 1,000 pipeline reactions created in bulk
/// and 100 workflow runs of one instance send the same commands per reaction as before it was
/// recorded, pipeline reactions read each stage's tool instance once per request, and each
/// workflow run reads its container tool's pinned image at most once
#[tokio::test]
async fn run_provenance_cost() -> Result<(), Error> {
    // keep the other tests from sending commands while we count
    let _serial = SERIAL.lock().await;
    let setup = Box::pin(ProvenanceSetup::new()).await?;
    let mut stats = Stats::connect().await?;
    // warm the API's pooled connections and caches with uncounted creates
    setup.bulk(10).await?;
    setup.run().await?;
    // count 1,000 pipeline reactions created in one bulk request
    let reads = setup.reads();
    let before = stats.counts().await?;
    setup.bulk(1_000).await?;
    let pipeline_counts = delta(&before, &stats.counts().await?);
    let pipeline_reads = setup.reads();
    // count 100 workflow runs of one instance
    let before = stats.counts().await?;
    for _ in 0..100 {
        setup.run().await?;
    }
    let run_counts = delta(&before, &stats.counts().await?);
    let run_reads = setup.reads();
    // report the counts for the comparison with the base
    let pipeline_total: i64 = pipeline_counts.values().sum();
    let run_total: i64 = run_counts.values().sum();
    let reads_of = |after: [u64; 5], before: [u64; 5]| {
        after
            .iter()
            .zip(before)
            .map(|(after, before)| after - before)
            .collect::<Vec<u64>>()
    };
    let pipeline_read_deltas = reads_of(pipeline_reads, reads);
    let run_read_deltas = reads_of(run_reads, pipeline_reads);
    println!(
        "PERFJSON {}",
        serde_json::json!({
            "pipeline_reactions": 1_000,
            "pipeline_commands": pipeline_total,
            "pipeline_by_command": pipeline_counts,
            "pipeline_reads": {"stage": pipeline_read_deltas[0]},
            "workflow_runs": 100,
            "run_commands": run_total,
            "run_by_command": run_counts,
            "run_reads": {
                "instance": run_read_deltas[1],
                "harness": run_read_deltas[2],
                "tool": run_read_deltas[3],
                "image": run_read_deltas[4],
            },
        })
    );
    // the bulk request reads its stage's tool instance once and no workflow entity
    is!(pipeline_read_deltas, vec![1, 0, 0, 0, 0]);
    // each workflow run reads its pinned image at most once and no stage tool instance
    is!(run_read_deltas[0], 0);
    assert!(run_read_deltas[4] <= 100, "{run_read_deltas:?}");
    // the Redis commands per reaction are the ones sent before provenance was recorded
    assert!(
        pipeline_total <= PIPELINE_COMMANDS_PER_1000,
        "1,000 pipeline reactions sent {pipeline_total} commands"
    );
    assert!(
        run_total <= RUN_COMMANDS_PER_100,
        "100 workflow runs sent {run_total} commands"
    );
    Ok(())
}

/// The run-queue claim with a run token checks the worker's run on the worker hash it already
/// reads: no more Redis commands than the development base's stage claim (55, with 11 pool
/// checkouts) and no tool instance read
#[tokio::test]
async fn run_claim_cost() -> Result<(), Error> {
    // keep the other tests from sending commands while we count
    let _serial = SERIAL.lock().await;
    // set up a run with queued tool calls and its worker
    let setup = Box::pin(RunClaimSetup::new()).await?;
    setup.queue(3).await?;
    let mut stats = Stats::connect().await?;
    // warm the API's pooled connections with an untimed claim
    setup.claim().await?;
    // count one claim's commands and tool instance reads
    let reads = test_utilities::faults::entity_reads(setup.image);
    let before = stats.counts().await?;
    setup.claim().await?;
    let counts = delta(&before, &stats.counts().await?);
    let total: i64 = counts.values().sum();
    println!("run claim commands ({total}): {counts:?}");
    // the claim reads no tool instance and stays within the base's stage claim
    is!(test_utilities::faults::entity_reads(setup.image), reads);
    assert!(total <= 55, "the run claim sent {total} commands");
    assert!(counts.get("ping").copied().unwrap_or_default() <= 11);
    Ok(())
}

/// The p50 and p99 of 1,000 run-queue claims with a run token
#[tokio::test]
#[ignore = "benchmark: 1,100 tool calls, timing sensitive"]
async fn run_claim_latency() -> Result<(), Error> {
    // keep the other tests from sending commands while we time
    let _serial = SERIAL.lock().await;
    // set up a run with 1,100 queued tool calls
    let setup = Box::pin(RunClaimSetup::new()).await?;
    setup.queue(1_100).await?;
    // warm up with 100 untimed claims
    for _ in 0..100 {
        setup.claim().await?;
    }
    // time 1,000 claims
    let mut times = Vec::with_capacity(1_000);
    for _ in 0..1_000 {
        times.push(setup.claim().await?);
    }
    times.sort_unstable();
    println!(
        "run claim: p50 {}us p99 {}us",
        percentile(&times, 50),
        percentile(&times, 99)
    );
    Ok(())
}

/// Read the event handler's trigger cache flag and the scaler's cache flag
///
/// # Arguments
///
/// * `conn` - The Redis connection to read with
async fn cache_flags(
    conn: &mut redis::aio::MultiplexedConnection,
) -> (Option<String>, Option<String>) {
    // read both flags in one round trip
    let ns = &test_utilities::CONF.thorium.namespace;
    redis::pipe()
        .cmd("hget")
        .arg(format!("{ns}:event-handler:cache"))
        .arg("status")
        .cmd("hget")
        .arg(format!("{ns}:system_info"))
        .arg("scaler_cache")
        .query_async(conn)
        .await
        .expect("the cache flags")
}

/// Minting and deleting 1,000 run tokens sets neither cache flag, and a run token's request
/// costs no more Redis commands than an ordinary scoped token's
#[tokio::test]
async fn run_tokens_leave_caches() -> Result<(), Error> {
    // keep the other tests from sending commands while we count
    let _serial = SERIAL.lock().await;
    // a developer who owns a group with an Image tool and a file
    let admin = test_utilities::admin_client().await?;
    let runner = generators::user_with_role(
        &admin,
        UserRole::Developer {
            k8s: true,
            bare_metal: false,
            windows: false,
            external: false,
            kvm: false,
        },
    )
    .await?;
    let group = generators::groups(1, &admin).await?.remove(0).name;
    let update = GroupUpdate::default()
        .owners(thorium::models::GroupUsersUpdate::default().direct_add(&runner.username));
    admin.groups.update(&group, &update).await?;
    let (_, image) = runs::image_tool(&[group.as_str()], ImageScaler::K8s, &admin).await?;
    let sha256 = runner
        .client
        .files
        .create(generators::gen_sample(&group))
        .await?
        .sha256;
    let req = ToolRunRequest::new(RunInputs::default().sample(&sha256));
    // clear both flags after the setup set them
    let pool = test_utilities::get_redis_client(&test_utilities::CONF).await;
    let mut conn = pool.get().await.expect("a Redis connection").clone();
    let ns = test_utilities::CONF.thorium.namespace.clone();
    let _: () = redis::pipe()
        .cmd("hset")
        .arg(format!("{ns}:event-handler:cache"))
        .arg("status")
        .arg("false")
        .cmd("hset")
        .arg(format!("{ns}:system_info"))
        .arg("scaler_cache")
        .arg("false")
        .query_async(&mut conn)
        .await
        .expect("clear the cache flags");
    // mint 1,000 run tokens with tool runs and delete them with their runs
    for _ in 0..1_000 {
        let created = runner.client.reactions.run_tool(image.id, &req).await?;
        runner.client.reactions.delete(&group, &created.id).await?;
    }
    is!(runner.client.users.list_scoped_tokens().await?.len(), 0);
    is!(
        cache_flags(&mut conn).await,
        (Some("false".to_owned()), Some("false".to_owned()))
    );
    // a run token and an ordinary scoped token of the same runner
    let created = runner.client.reactions.run_tool(image.id, &req).await?;
    let token = admin.reactions.run_token(&group, created.id).await?;
    let run = Thorium::build(&admin.host)
        .token(&token.token)
        .build()
        .await?;
    let plain = runner
        .client
        .users
        .create_scoped_token(&ScopedTokenRequest::new("plain-perf").group(&group))
        .await?;
    let plain = Thorium::build(&admin.host)
        .token(&plain.token)
        .build()
        .await?;
    // warm both clients, then count one identical request with each
    let mut stats = Stats::connect().await?;
    run.groups.get(&group).await?;
    plain.groups.get(&group).await?;
    let before = stats.counts().await?;
    run.groups.get(&group).await?;
    let run_counts = delta(&before, &stats.counts().await?);
    let before = stats.counts().await?;
    plain.groups.get(&group).await?;
    let plain_counts = delta(&before, &stats.counts().await?);
    println!("run token request: {run_counts:?}; scoped token request: {plain_counts:?}");
    let run_total: i64 = run_counts.values().sum();
    let plain_total: i64 = plain_counts.values().sum();
    assert!(run_total <= plain_total, "{run_total} > {plain_total}");
    Ok(())
}

/// The shared workload claim reads the worker hash for its scaler, reads no image or tool
/// instance, and makes no Scylla read
#[tokio::test]
async fn workload_claim_cost() -> Result<(), Error> {
    // keep the other tests from sending commands while we count
    let _serial = SERIAL.lock().await;
    // set up a pipeline with queued jobs and a worker
    let setup = ClaimSetup::new().await?;
    setup.queue(3).await?;
    let mut stats = Stats::connect().await?;
    // warm the API's pooled connections with an untimed claim
    setup.claim().await?;
    // count what authenticating a request costs
    let before = stats.counts().await?;
    setup.admin.groups.get(&setup.group).await?;
    let auth = delta(&before, &stats.counts().await?);
    // count one claim's commands and tool instance reads
    let reads = test_utilities::faults::entity_reads(setup.tool_instance);
    let before = stats.counts().await?;
    setup.claim().await?;
    let counts = delta(&before, &stats.counts().await?);
    println!("authenticated group read commands: {auth:?}");
    println!("workload claim commands: {counts:?}");
    // the claim reads no tool instance from Scylla
    is!(
        test_utilities::faults::entity_reads(setup.tool_instance),
        reads
    );
    // beyond authenticating, the claim never reads an image's scaler
    let hgets = counts.get("hget").copied().unwrap_or_default();
    assert!(hgets <= auth.get("hget").copied().unwrap_or_default());
    // the claim loads the worker hash
    assert!(counts.get("hgetall").copied().unwrap_or_default() >= 1);
    Ok(())
}

/// The p50 and p99 of 1,000 workload claims
#[tokio::test]
#[ignore = "benchmark: 1,000 reactions, timing sensitive"]
async fn workload_claim_latency() -> Result<(), Error> {
    // keep the other tests from sending commands while we time
    let _serial = SERIAL.lock().await;
    // set up a pipeline with 1,100 queued jobs
    let setup = ClaimSetup::new().await?;
    setup.queue(1_100).await?;
    // warm up with 100 untimed claims
    for _ in 0..100 {
        setup.claim().await?;
    }
    // time 1,000 claims
    let mut times = Vec::with_capacity(1_000);
    for _ in 0..1_000 {
        times.push(setup.claim().await?);
    }
    times.sort_unstable();
    println!(
        "workload claim: p50 {}us p99 {}us",
        percentile(&times, 50),
        percentile(&times, 99)
    );
    Ok(())
}

/// The stats route counts 10 groups of 50 tool instances and 100 members with one pipeline of
/// counts per group and writes nothing
#[tokio::test]
async fn stats_route_cost() -> Result<(), Error> {
    // keep the other tests from sending commands while we count
    let _serial = SERIAL.lock().await;
    let admin = test_utilities::admin_client().await?;
    // create 100 users to be members of every group
    let mut members = GroupUsersRequest::default();
    for _ in 0..100 {
        let user = generators::user_with_role(&admin, UserRole::User).await?;
        members = members.direct(user.username);
    }
    // create 10 groups whose shared workload sets each hold 50 tool instances
    let pool = test_utilities::get_redis_client(&test_utilities::CONF).await;
    let mut conn = pool.get().await.expect("a Redis connection").clone();
    let now = chrono::Utc::now().timestamp();
    let mut groups = Vec::with_capacity(10);
    for _ in 0..10 {
        let req =
            GroupRequest::new(generators::gen_string(16).to_lowercase()).users(members.clone());
        admin.groups.create(&req).await?;
        let key = format!(
            "{}:workloads:{}",
            test_utilities::CONF.thorium.namespace,
            req.name
        );
        for _ in 0..50 {
            let _: () = redis::cmd("zadd")
                .arg(&key)
                .arg(now)
                .arg(Uuid::new_v4().to_string())
                .query_async(&mut conn)
                .await
                .expect("mark a tool instance active");
        }
        groups.push(req.name);
    }
    let mut stats = Stats::connect().await?;
    // warm the API's pooled connections with an uncounted read
    admin.system.stats().await?;
    // count and time one stats read
    let before = stats.counts().await?;
    let start = Instant::now();
    let read = admin.system.stats().await?;
    let elapsed = start.elapsed().as_millis();
    let counts = delta(&before, &stats.counts().await?);
    println!("stats route: {elapsed}ms commands: {counts:?}");
    // every group of ours was read, and each tool instance and member got five counts
    for group in &groups {
        is!(read.groups.contains_key(group), true);
    }
    assert!(counts.get("zcount").copied().unwrap_or_default() >= 10 * 50 * 101 * 5);
    // the stats route writes nothing
    for write in WRITES {
        is!(counts.get(write).copied().unwrap_or_default(), 0);
    }
    Ok(())
}
