//! Tests that binding job handles to the claiming worker adds no Redis round trips
//!
//! Redis counts commands per server, not per client, so the counts only hold while nothing else
//! uses the test Redis server. This is its own test binary so no other test file's tests send
//! commands while it counts, and the tests in it run one at a time. Don't share the test Redis
//! server with another run while this binary runs.

use std::collections::BTreeMap;
use std::time::Instant;
use thorium::models::{
    GenericJob, GroupAllowed, ImageScaler, Pools, Resources, StageLogsAdd, UserRole,
    WorkerDeleteMap, WorkerRegistrationList,
};
use thorium::test_utilities::{self, generators};
use thorium::{Error, Thorium, is};
use tokio::sync::Mutex;

/// Runs the tests in this binary one at a time, since every one sends Redis commands that would
/// land in the counted ones
static SERIAL: Mutex<()> = Mutex::const_new(());

/// The number of calls Redis has served for each command
type CommandCounts = BTreeMap<String, i64>;

/// A connection to the test Redis used to read its command stats
struct Stats {
    /// The open Redis connection, reused so reading stats doesn't add connection commands
    conn: redis::aio::MultiplexedConnection,
}

impl Stats {
    /// Connect to the test Redis
    async fn connect() -> Result<Self, Error> {
        // build the url to the test Redis using its auth if it is configured
        let redis = &test_utilities::CONF.redis;
        let auth = match (&redis.username, &redis.password) {
            (Some(user), Some(password)) => format!("{user}:{password}@"),
            (None, Some(password)) => format!("default:{password}@"),
            _ => String::new(),
        };
        let url = format!("redis://{auth}{}:{}/{}", redis.host, redis.port, redis.db);
        // connect to the test Redis
        let conn = redis::Client::open(url)
            .map_err(|err| Error::new(format!("Failed to build a Redis client: {err}")))?
            .get_multiplexed_async_connection()
            .await
            .map_err(|err| Error::new(format!("Failed to connect to Redis: {err}")))?;
        Ok(Stats { conn })
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

/// A user, the admin, and a pipeline they can both run jobs for
struct PerfSetup {
    /// The admin client
    admin: Thorium,
    /// The non-admin user whose worker claims jobs
    owner: generators::TestUser,
    /// The group the pipeline is in
    group: String,
    /// The pipeline to create reactions for
    pipeline: String,
    /// The first stage of the pipeline
    stage: String,
    /// The tool instance the first stage runs
    tool_instance: uuid::Uuid,
    /// The owner's worker
    owner_worker: String,
    /// The admin's worker
    admin_worker: String,
}

impl PerfSetup {
    /// Create a user, a group, a pipeline, and a worker for both the user and the admin
    async fn new() -> Result<Self, Error> {
        // get admin client
        let admin = test_utilities::admin_client().await?;
        // register our test node
        generators::node("cluster0", "node0", Resources::default(), &admin).await?;
        // create a user who can edit a group
        let owner = generators::user_with_role(&admin, UserRole::User).await?;
        let roles = generators::GroupRoles::default().user(&owner.username);
        let group = generators::group_with_roles(&roles, GroupAllowed::default(), &admin).await?;
        // create a generator pipeline in this group so its jobs can sleep
        let pipe_req = generators::gen_generator_pipe(&group, &admin).await?;
        let pipe = admin.pipelines.get(&group, &pipe_req.name).await?;
        let stage = pipe.order[0][0].clone();
        let tool_instance = generators::stage_instance(&pipe, &stage);
        // register a worker for the owner and one for the admin
        let owner_worker = generators::gen_string(24);
        let admin_worker = generators::gen_string(24);
        let admin_name = admin.users.info().await?.username;
        for (name, user) in [
            (&owner_worker, &owner.username),
            (&admin_worker, &admin_name),
        ] {
            let workers = WorkerRegistrationList::default().add(
                "cluster0",
                "node0",
                name,
                user,
                &group,
                &pipe.name,
                &stage,
                Resources::default(),
                Pools::Deadline,
            );
            admin
                .system
                .register_workers(ImageScaler::K8s, &workers)
                .await?;
        }
        Ok(PerfSetup {
            admin,
            owner,
            group,
            pipeline: pipe.name,
            stage,
            tool_instance,
            owner_worker,
            admin_worker,
        })
    }

    /// Send many concurrent reads as both users so the API's Redis pool opens its connections
    ///
    /// A new pooled connection sends its own setup commands (`AUTH`, `HELLO`, `CLIENT SETINFO`,
    /// and `SELECT`), which would otherwise land in whichever counted call opened it.
    async fn warm_pool(&self) -> Result<(), Error> {
        // read the group as both users many times at once
        let reads = (0..32).flat_map(|_| {
            [
                self.admin.groups.get(&self.group),
                self.owner.client.groups.get(&self.group),
            ]
        });
        futures::future::try_join_all(reads).await?;
        Ok(())
    }

    /// Create a reaction with a client so its job is that client's user's
    ///
    /// # Arguments
    ///
    /// * `client` - The client to create the reaction with
    async fn reaction(&self, client: &Thorium) -> Result<(), Error> {
        // get our pipeline
        let pipe = self
            .admin
            .pipelines
            .get(&self.group, &self.pipeline)
            .await?;
        // create a reaction with this client
        let req = generators::gen_reaction(&self.group, &pipe, None);
        client.reactions.create(&req).await?;
        Ok(())
    }

    /// Claim one job for a worker with a client
    ///
    /// # Arguments
    ///
    /// * `client` - The client to claim with
    /// * `worker` - The worker to claim for
    async fn claim(&self, client: &Thorium, worker: &str) -> Result<GenericJob, Error> {
        // claim a single job
        let mut jobs = client
            .jobs
            .claim_workload(
                &self.group,
                self.tool_instance,
                "cluster0",
                "node0",
                worker,
                1,
            )
            .await?;
        Ok(jobs.remove(0))
    }

    /// Count the Redis commands each job handle call makes for a client and its worker
    ///
    /// # Arguments
    ///
    /// * `stats` - The connection to read command stats with
    /// * `client` - The client to call the job handle routes with
    /// * `worker` - The worker that claims the jobs
    async fn handle_counts(
        &self,
        stats: &mut Stats,
        client: &Thorium,
        worker: &str,
    ) -> Result<BTreeMap<&'static str, CommandCounts>, Error> {
        // create two jobs for this client's user
        self.reaction(client).await?;
        self.reaction(client).await?;
        // build the logs to send and the call counts to fill in
        let logs = generators::stage_logs();
        let mut counts = BTreeMap::new();
        // count an authenticated group read, which differs between users only in authentication
        let before = stats.counts().await?;
        client.groups.get(&self.group).await?;
        counts.insert("auth", delta(&before, &stats.counts().await?));
        // count the claim
        let before = stats.counts().await?;
        let job = self.claim(client, worker).await?;
        counts.insert("claim", delta(&before, &stats.counts().await?));
        // count the checkpoint
        let before = stats.counts().await?;
        client.jobs.checkpoint(&job, "check-0").await?;
        counts.insert("checkpoint", delta(&before, &stats.counts().await?));
        // count the sleep
        let before = stats.counts().await?;
        client.jobs.sleep(&job.id, "check-1").await?;
        counts.insert("sleep", delta(&before, &stats.counts().await?));
        // count the proceed
        let before = stats.counts().await?;
        client.jobs.proceed(&job, &logs, 1, worker).await?;
        counts.insert("proceed", delta(&before, &stats.counts().await?));
        // count the error of the next job
        let job = self.claim(client, worker).await?;
        let before = stats.counts().await?;
        client.jobs.error(&job.id, &logs, worker).await?;
        counts.insert("error", delta(&before, &stats.counts().await?));
        Ok(counts)
    }
}

/// Subtract one set of command counts from another
///
/// # Arguments
///
/// * `left` - The counts to subtract from
/// * `right` - The counts to subtract
fn minus(left: &CommandCounts, right: &CommandCounts) -> CommandCounts {
    // subtract every command either side used and keep the nonzero differences
    left.keys()
        .chain(right.keys())
        .map(|name| {
            let diff = left.get(name).unwrap_or(&0) - right.get(name).unwrap_or(&0);
            (name.clone(), diff)
        })
        .filter(|(_, diff)| *diff != 0)
        .collect()
}

/// The claiming worker's job handle calls, its claim, and a worker delete make the
/// same Redis calls whether the worker binding checks run (a user) or are skipped (an admin),
/// beyond what authenticating as a different user costs
#[tokio::test]
async fn job_handle_binding_adds_no_redis_calls() -> Result<(), Error> {
    // keep the benchmark from sending commands while we count
    let _serial = SERIAL.lock().await;
    // set up a user and the admin with one worker each
    let setup = PerfSetup::new().await?;
    let mut stats = Stats::connect().await?;
    // open enough pooled connections up front that no counted call pays for opening one
    setup.warm_pool().await?;
    // count the job handle calls for the user, whose calls are checked
    let owner_counts = setup
        .handle_counts(&mut stats, &setup.owner.client, &setup.owner_worker)
        .await?;
    // count the same calls for the admin, whose calls skip the checks
    let admin_counts = setup
        .handle_counts(&mut stats, &setup.admin, &setup.admin_worker)
        .await?;
    // get what authenticating as the user costs over authenticating as the admin
    let auth_cost = minus(&owner_counts["auth"], &admin_counts["auth"]);
    // the checks add no Redis commands on any job handle path or the claim
    for (route, owner) in &owner_counts {
        is!(minus(owner, &admin_counts[route]), auth_cost);
    }
    // register one more worker for the owner and the admin to delete
    let owner_delete = generators::gen_string(24);
    let admin_delete = generators::gen_string(24);
    for name in [&owner_delete, &admin_delete] {
        let workers = WorkerRegistrationList::default().add(
            "cluster0",
            "node0",
            name,
            &setup.owner.username,
            &setup.group,
            &setup.pipeline,
            &setup.stage,
            Resources::default(),
            Pools::Deadline,
        );
        setup
            .admin
            .system
            .register_workers(ImageScaler::K8s, &workers)
            .await?;
    }
    // count the owner's delete, whose ownership and group check reads one hash per worker
    let before = stats.counts().await?;
    setup
        .owner
        .client
        .system
        .delete_workers(
            ImageScaler::K8s,
            &WorkerDeleteMap::default().add(&owner_delete),
        )
        .await?;
    let owner_delete_counts = delta(&before, &stats.counts().await?);
    // count the admin's delete, which skips the check
    let before = stats.counts().await?;
    setup
        .admin
        .system
        .delete_workers(
            ImageScaler::K8s,
            &WorkerDeleteMap::default().add(&admin_delete),
        )
        .await?;
    let admin_delete_counts = delta(&before, &stats.counts().await?);
    // the owner's check adds one pipeline (one pooled connection checkout, which pings)
    // holding one hmget per worker
    let mut expected = auth_cost.clone();
    *expected.entry("hmget".to_owned()).or_insert(0) += 1;
    *expected.entry("ping".to_owned()).or_insert(0) += 1;
    expected.retain(|_, calls| *calls != 0);
    is!(minus(&owner_delete_counts, &admin_delete_counts), expected);
    // clean up our workers
    for name in [&setup.owner_worker, &setup.admin_worker] {
        generators::delete_worker(name, &setup.admin).await?;
    }
    Ok(())
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

/// Create, claim, and time the proceed of one job by a client and its worker
///
/// # Arguments
///
/// * `setup` - The test setup
/// * `client` - The client to proceed with
/// * `worker` - The worker that claims the job
/// * `logs` - The stage logs to send with the proceed
async fn time_proceed(
    setup: &PerfSetup,
    client: &Thorium,
    worker: &str,
    logs: &StageLogsAdd,
) -> Result<u128, Error> {
    // create a reaction and claim its job outside the timed section
    setup.reaction(client).await?;
    let job = setup.claim(client, worker).await?;
    // time only the proceed
    let start = Instant::now();
    client.jobs.proceed(&job, logs, 1, worker).await?;
    Ok(start.elapsed().as_micros())
}

/// Time interleaved proceeds by the admin (unchecked) and the owner (checked)
///
/// Both clients first run untimed warm-up proceeds, then alternate which one goes first in each
/// round, so connection setup, caches, and drift over the run fall on both sides equally.
///
/// # Arguments
///
/// * `setup` - The test setup
/// * `warmup` - The number of untimed proceeds per client
/// * `count` - The number of timed proceeds per client
async fn time_interleaved_proceeds(
    setup: &PerfSetup,
    warmup: usize,
    count: usize,
) -> Result<(Vec<u128>, Vec<u128>), Error> {
    // build the logs every proceed sends
    let logs = generators::stage_logs();
    // warm up both clients without timing them
    for _ in 0..warmup {
        time_proceed(setup, &setup.admin, &setup.admin_worker, &logs).await?;
        time_proceed(setup, &setup.owner.client, &setup.owner_worker, &logs).await?;
    }
    // alternate which client goes first in each timed round
    let mut admin = Vec::with_capacity(count);
    let mut owner = Vec::with_capacity(count);
    for round in 0..count {
        if round % 2 == 0 {
            admin.push(time_proceed(setup, &setup.admin, &setup.admin_worker, &logs).await?);
            owner.push(time_proceed(setup, &setup.owner.client, &setup.owner_worker, &logs).await?);
        } else {
            owner.push(time_proceed(setup, &setup.owner.client, &setup.owner_worker, &logs).await?);
            admin.push(time_proceed(setup, &setup.admin, &setup.admin_worker, &logs).await?);
        }
    }
    // sort both sides so percentiles can be read off directly
    admin.sort_unstable();
    owner.sort_unstable();
    Ok((admin, owner))
}

/// The p50 and p99 of 1,000 checked proceeds stay within 5% of 1,000
/// unchecked (admin) proceeds interleaved with them
#[tokio::test]
#[ignore = "benchmark: 2,200 reactions, timing sensitive"]
async fn job_handle_binding_proceed_latency() -> Result<(), Error> {
    // keep our commands out of the command counting test
    let _serial = SERIAL.lock().await;
    // set up a user and the admin with one worker each
    let setup = PerfSetup::new().await?;
    // time 1,000 interleaved proceeds each for the admin and the owner after 100 warm-ups each
    let (admin, owner) = time_interleaved_proceeds(&setup, 100, 1000).await?;
    // report and compare the percentiles
    for pct in [50, 99] {
        let (base, checked) = (percentile(&admin, pct), percentile(&owner, pct));
        println!("p{pct}: unchecked {base}us checked {checked}us");
        assert!(
            checked * 100 <= base * 105,
            "p{pct} of checked proceeds {checked}us is more than 5% over {base}us"
        );
    }
    // clean up our workers
    for name in [&setup.owner_worker, &setup.admin_worker] {
        generators::delete_worker(name, &setup.admin).await?;
    }
    Ok(())
}
