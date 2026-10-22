//! Measures what the workload claim and the per-workload stats cost in Redis round trips and time
//!
//! Redis counts commands per server, not per client, so the counts only hold while nothing else
//! uses the test Redis server. This is its own test binary so no other test file's tests send
//! commands while it counts, and the tests in it run one at a time. Don't share the test Redis
//! server with another run while this binary runs.

use std::collections::BTreeMap;
use std::time::Instant;
use thorium::models::{
    GroupRequest, GroupUsersRequest, ImageScaler, PipelineRequest, Pools, ReactionRequest,
    Resources, UserRole, WorkerRegistrationList,
};
use thorium::test_utilities::{self, generators};
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
            "pipe",
            "stage",
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
