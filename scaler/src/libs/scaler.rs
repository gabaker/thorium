//! Schedules resources in Thorium based on the deadline stream

use chrono::prelude::*;
use futures::stream::{self, StreamExt};
use futures::{poll, task::Poll};
use hashbrown::HashMap;
use std::collections::{BTreeMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use thorium::models::system::WorkerRegistrationList;
use thorium::models::{
    Deadline, ImageScaler, ReactionStatus, StageLogsAdd, ToolKind, WorkerDeleteMap,
    WorkerRegistration,
};
use thorium::{Conf, Error, Thorium};
use tokio::task::JoinHandle;
use tracing::{Level, event, instrument, span};
use uuid::Uuid;

use super::Cache;
use super::schedulers::{self, Allocatable, ReqMap, Scheduler, WorkerDeletion};
use super::tasks::{self, TaskResult, Tasks, ZombieChecker};
use crate::args::Args;
use crate::from_now;
use crate::libs::Spawned;
use crate::libs::schedulers::RunPodEnv;

/// returns false if this is false
macro_rules! filter {
    ($test:expr) => {
        if !$test {
            return false;
        }
    };
}

/// The shortest time between two early cache reloads in seconds
const EARLY_RELOAD_INTERVAL: i64 = 30;

/// The most per-run env fetches in flight at once
const RUN_ENV_FETCHES: usize = 10;

/// The stage log line a run whose token was removed mid-run is failed with
pub const RUN_TOKEN_REVOKED: &str = "run token revoked";

/// spawn an async tokio task
macro_rules! spawn {
    ($vec:expr, $future:expr) => {
        $vec.push(tokio::spawn(async move { $future.await }))
    };
}

macro_rules! add_task {
    ($scaler:expr, $task:expr) => {{
        // get the datetime to start this task at
        let mut start = crate::from_now!(i64::from($task.delay(&$scaler.conf)));
        // increase by 1 until we have found an open slot to start this job
        loop {
            // determine if a task already exists for this date
            if $scaler.tasks.get(&start).is_none() {
                break;
            }
            // increment start by 1 and try again
            start += chrono::Duration::seconds(1);
        }
        $scaler.tasks.insert(start, $task)
    }};
}

/// The different reasons a worker may have failed out
///
/// Hash/Eq/PartialEq is implemented on solely the worker field for this. Meaning that when
/// placed in a hashed collection two different kinds with the same worker will
/// conflict/overwrite each other. This is done because having two different error out
/// reasons for the same worker is nonsensical and could result in trying to reset the
/// same worker twice.
#[derive(Debug, Clone, Eq)]
pub enum ErrorOutKinds {
    /// This worker exceeded its memory allocations/limits
    Oom(String),
    /// This worker cannot be created due to a missing prereq
    #[allow(dead_code)]
    StuckCreating(String),
    /// This worker cannot be created as its image cannot be pulled
    ImagePullBackoff(String),
}

impl ErrorOutKinds {
    /// create a report that a worker has OOM'd
    ///
    /// # Arguments
    ///
    /// * `worker` - The worker that has been oomed
    pub fn oom<W: Into<String>>(worker: W) -> Self {
        ErrorOutKinds::Oom(worker.into())
    }

    /// create a report that a worker is stuck creating
    ///
    /// # Arguments
    ///
    /// * `worker` - The worker that is stuck in the creating state
    #[allow(dead_code)]
    pub fn stuck_creating<W: Into<String>>(worker: W) -> Self {
        ErrorOutKinds::StuckCreating(worker.into())
    }

    /// create a report that a worker's image cannot be pulled
    ///
    /// # Arguments
    ///
    /// * `worker` - The worker whose image cannot be pulled
    pub fn image_pull_backoff<W: Into<String>>(worker: W) -> Self {
        ErrorOutKinds::ImagePullBackoff(worker.into())
    }

    /// Get the name of the worker regardless of reason
    pub fn worker(&self) -> &String {
        match self {
            Self::Oom(worker) => worker,
            Self::StuckCreating(worker) => worker,
            Self::ImagePullBackoff(worker) => worker,
        }
    }

    /// Get our reason as a string
    pub fn reason_as_str(&self) -> &'static str {
        match self {
            Self::Oom(_) => "OOM",
            Self::StuckCreating(_) => "StuckCreating",
            Self::ImagePullBackoff(_) => "ImagePullBackoff",
        }
    }
}

// Only implement Hash on worker to prevent multiple
// error out reasons for the same worker being stored at once.
impl Hash for ErrorOutKinds {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // get our worker
        let worker = self.worker();
        // hash our worker
        state.write(worker.as_bytes());
    }
}

// Only implement Partial EQ on worker to prevent multiple
// error out reasons for the same worker being stored at once.
impl PartialEq for ErrorOutKinds {
    fn eq(&self, other: &Self) -> bool {
        // check if we have the same worker
        self.worker() == other.worker()
    }
}

/// Sync our ban sets against a newly pulled cache
///
/// This is the cache arm of `Scaler::check_tasks` extracted so it can be unit tested. The stale
/// bans from the old cache are cleared first and then every scheduler repopulates the ban sets
/// from the new cache, so the bans derived from the new cache are kept.
///
/// # Arguments
///
/// * `bans` - The ban sets to sync
/// * `sync` - Syncs every scheduler against the new cache, adding any bans it finds
async fn resync_bans<F>(bans: &mut BanSets, sync: F) -> Result<(), Error>
where
    F: AsyncFnOnce(&mut BanSets) -> Result<(), Error>,
{
    // clear the stale bans from our old cache before syncing
    bans.clear();
    // sync our schedulers against the new cache, adding any bans they find
    sync(bans).await?;
    Ok(())
}

/// Build the batches of worker registrations `Scaler::register_workers` submits
///
/// This is the batching in `Scaler::register_workers` extracted so it can be unit tested. A batch
/// is submitted once it reaches 25 workers and a fresh batch is started afterwards, so each worker
/// is registered exactly once.
///
/// # Arguments
///
/// * `reqs` - The requisitions whose spawns should be registered
fn registration_batches(reqs: &ReqMap) -> Vec<WorkerRegistrationList> {
    // track the batches to submit in order
    let mut batches = Vec::default();
    // build a list to track the new workers we are spawning
    let mut registrations = WorkerRegistrationList::default();
    // get the worker registration info for each cluster
    for (cluster, spawn_map) in &reqs.spawns {
        // crawl over the groups for this cluster
        for spawn_group in spawn_map.values() {
            // build a worker registration object for each spawned worker
            for spawn in spawn_group {
                // build this workers registration object
                let worker = WorkerRegistration::new(
                    cluster,
                    &spawn.node,
                    &spawn.name,
                    &spawn.req.user,
                    &spawn.req.group,
                    spawn.req.tool_instance,
                    spawn.req.run,
                    spawn.resources,
                    spawn.pool,
                );
                // add this new spawn to our list of workers to register
                registrations.add_mut(worker);
            }
            // if we have at least 25 workers then submit them to Thorium
            if registrations.workers.len() >= 25 {
                // submit this batch and start a new one so no worker is sent twice
                batches.push(std::mem::take(&mut registrations));
            }
        }
    }
    // skip registering new workers if we have none
    if !registrations.workers.is_empty() {
        batches.push(registrations);
    }
    batches
}

/// Decide whether the cache should be reloaded early to inspect a new workload's image
///
/// # Arguments
///
/// * `requested` - Whether the cache requested a reload
/// * `last` - When the cache was last reloaded early
/// * `now` - The current timestamp
fn early_reload_due(requested: bool, last: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    // reload only when requested and the last early reload was long enough ago
    requested
        && last.is_none_or(|last| now - last >= chrono::Duration::seconds(EARLY_RELOAD_INTERVAL))
}

/// Get the clusters `Scaler::schedule` asks to delete workers and the scale downs for each
///
/// This is the cluster selection in `Scaler::schedule` extracted so it can be unit tested. Every
/// cluster with new scale down orders is returned with them, and every cluster whose scheduler
/// still has pending deletes from an earlier loop is returned with no new scale downs, so it is
/// still asked to validate them instead of leaving those workers in the being deleted state
/// until that cluster happens to get another scale down.
///
/// # Arguments
///
/// * `scale_down` - The new scale down orders by cluster
/// * `pending` - The clusters whose schedulers have pending deletes to validate
fn delete_targets(
    mut scale_down: HashMap<String, Vec<Spawned>>,
    pending: &HashSet<String>,
) -> HashMap<String, Vec<Spawned>> {
    // crawl over the clusters with pending deletes
    for cluster in pending {
        // make sure quiet clusters are still called to validate their pending deletes
        if !scale_down.contains_key(cluster) {
            scale_down.insert(cluster.clone(), Vec::default());
        }
    }
    scale_down
}

/// The run env one or more per-run spawns of a tick share
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RunEnvKey {
    /// The run group the run's reaction is in
    group: String,
    /// The run
    run: Uuid,
    /// Whether the run's models are needed (only a harness's worker gets them)
    models: bool,
}

/// A per-run spawn whose env couldn't be fetched
#[derive(Debug)]
pub struct RunEnvFailure {
    /// The spawn, taken out of the tick's spawns
    pub spawn: Spawned,
    /// Why its env couldn't be fetched
    pub error: Error,
    /// Whether its run's token is gone (the run token read returned `404`)
    pub revoked: bool,
}

/// Fetch one run env: the run's token, and its models when they're needed
///
/// Returns why a read failed and whether the run's token is gone when one does.
///
/// # Arguments
///
/// * `thorium` - The scaler's admin client
/// * `key` - The run env to fetch
async fn fetch_run_env(thorium: &Thorium, key: &RunEnvKey) -> Result<RunPodEnv, (String, bool)> {
    // read the run's token, which is gone once the run has ended or it was deleted
    let token = match thorium.reactions.run_token(&key.group, key.run).await {
        Ok(token) => token.token,
        Err(error) => {
            let revoked = error.status() == Some(reqwest::StatusCode::NOT_FOUND);
            return Err((error.msg().unwrap_or_else(|| error.to_string()), revoked));
        }
    };
    // a harness's worker also gets the run's models
    let models = if key.models {
        match thorium.reactions.run_models(&key.group, key.run).await {
            Ok(models) => Some(models),
            Err(error) => return Err((error.msg().unwrap_or_else(|| error.to_string()), false)),
        }
    } else {
        None
    };
    Ok(RunPodEnv { token, models })
}

/// Fetch the env values of every per-run spawn of a tick, at most 10 at once
///
/// Each spawn gets its env values, and the spawns whose fetch failed are taken out and returned
/// so they can be recorded as failed spawns. Spawns of one run and kind of worker share one
/// fetch, and nothing is kept for the next tick.
///
/// # Arguments
///
/// * `thorium` - The scaler's admin client
/// * `cache` - The scaler's cache, to tell a harness's workers apart
/// * `spawns` - The tick's spawns by cluster and deadline
pub(crate) async fn fill_run_envs(
    thorium: &Thorium,
    cache: &Cache,
    spawns: &mut HashMap<String, BTreeMap<DateTime<Utc>, Vec<Spawned>>>,
) -> Vec<RunEnvFailure> {
    // find the run env each per-run spawn needs
    let key_of = |spawn: &Spawned| {
        spawn.req.run.map(|run| RunEnvKey {
            group: spawn.req.group.clone(),
            run,
            models: cache
                .get_workload(&spawn.req.tool_instance)
                .is_some_and(|workload| workload.kind == ToolKind::Harness),
        })
    };
    let keys = spawns
        .values()
        .flat_map(BTreeMap::values)
        .flatten()
        .filter_map(key_of)
        .collect::<HashSet<RunEnvKey>>();
    // fetch them all at once, a few at a time
    let fetched = stream::iter(keys)
        .map(|key| async move {
            let result = fetch_run_env(thorium, &key).await;
            (key, result)
        })
        .buffer_unordered(RUN_ENV_FETCHES)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<HashMap<RunEnvKey, Result<RunPodEnv, (String, bool)>>>();
    // give each spawn its env, taking out the ones whose fetch failed
    let mut failures = Vec::new();
    for by_deadline in spawns.values_mut() {
        for group in by_deadline.values_mut() {
            for spawn in std::mem::take(group) {
                match key_of(&spawn).map(|key| fetched.get(&key)) {
                    // a shared spawn needs no env
                    None => group.push(spawn),
                    Some(Some(Ok(env))) => {
                        let mut spawn = spawn;
                        spawn.run_env = Some(env.clone());
                        group.push(spawn);
                    }
                    Some(Some(Err((error, revoked)))) => failures.push(RunEnvFailure {
                        spawn,
                        error: Error::new(format!(
                            "Failed to get the run env of a per-run worker: {error}"
                        )),
                        revoked: *revoked,
                    }),
                    // every key was fetched, so this never happens
                    Some(None) => failures.push(RunEnvFailure {
                        spawn,
                        error: Error::new("No run env was fetched for a per-run worker"),
                        revoked: false,
                    }),
                }
            }
        }
    }
    failures
}

/// Fail a still running run whose token was removed, with a stage log saying why
///
/// A run that already ended, or is gone, is left alone.
///
/// # Arguments
///
/// * `thorium` - The scaler's admin client
/// * `group` - The run group
/// * `run` - The run whose token is gone
#[instrument(name = "scaler::fail_revoked_run", skip(thorium), err(Debug))]
pub(crate) async fn fail_revoked_run(
    thorium: &Thorium,
    group: &str,
    run: Uuid,
) -> Result<bool, Error> {
    // a run that's gone has nothing to fail
    let reaction = match thorium.reactions.get(group, run).await {
        Ok(reaction) => reaction,
        Err(error) if error.status() == Some(reqwest::StatusCode::NOT_FOUND) => return Ok(false),
        Err(error) => return Err(error),
    };
    // only a run that's still running is failed
    if !matches!(
        reaction.status,
        ReactionStatus::Created | ReactionStatus::Started
    ) {
        return Ok(false);
    }
    // say why in its first stage's logs
    if let Some(stage) = reaction.plan.stages.first().and_then(|step| step.first()) {
        let mut logs = StageLogsAdd::default();
        logs.add(RUN_TOKEN_REVOKED);
        thorium
            .reactions
            .add_stage_logs(group, &run, &stage.name, &logs)
            .await?;
    }
    // fail it so it doesn't wait on workers that can never be spawned
    thorium.reactions.fail(group, &run).await?;
    Ok(true)
}

/// Banned groups and users due to setup errors
///
/// This will expire and reset every 10 minutes.
#[derive(Debug)]
pub struct BanSets {
    /// when this banlist should be refreshed
    pub expire: DateTime<Utc>,
    /// The groups to ban spawning resources for
    pub groups: HashSet<String>,
    /// The users to ban spawning resources for
    pub users: HashSet<String>,
    /// The tool instances to ban from spawning by group
    pub reqs: HashMap<String, HashSet<Uuid>>,
}

impl Default for BanSets {
    /// Create a new ban set with nothing banned
    fn default() -> Self {
        Self::new()
    }
}

impl BanSets {
    /// Create a new ban set
    #[must_use]
    pub fn new() -> Self {
        Self {
            expire: from_now!(600),
            groups: HashSet::default(),
            users: HashSet::default(),
            reqs: HashMap::default(),
        }
    }

    /// Filter deadlines that have been banned
    ///
    /// This does not do any checking of if we have resources to spawn them yet.
    ///
    /// # Arguments
    ///
    /// * `cache` - A cache of info from Thorium we use for scaling
    /// * `deadlines` - The deadlines to filter
    pub fn filter_deadlines(&self, cache: &Cache, deadline: &Deadline) -> bool {
        // if we don't know about this deadlines group then filter it
        filter!(cache.groups.contains(&deadline.group));
        // if we don't know about this deadlines users then filter it
        filter!(cache.users.contains_key(&deadline.creator));
        // filter any deadline whose workload isn't cached, isn't ours, or (on k8s) whose image
        // hasn't been inspected yet
        filter!(cache.workload_ready(&deadline.tool_instance, &deadline.group));
        // filter any banned groups
        filter!(!self.groups.contains(&deadline.group));
        // filter any banned users
        filter!(!self.users.contains(&deadline.creator));
        // if this group has any banned reqs then filter those out
        if let Some(banned_reqs) = self.reqs.get(&deadline.group) {
            // filter any banned tool instances
            filter!(!banned_reqs.contains(&deadline.tool_instance));
        }
        true
    }

    /// check if this ban rule set is expired and clear it if so
    pub fn is_expired(&mut self) {
        // only clear our bans once their expiration has passed
        if self.expire <= Utc::now() {
            self.clear();
        }
    }

    /// Clear all banned items
    pub fn clear(&mut self) {
        self.expire = from_now!(600);
        self.groups.clear();
        self.users.clear();
        self.reqs.clear();
    }
}

/// Scales pods in k8s up and down based on the deadline stream
pub struct Scaler {
    /// The type of scheduler in use
    scaler_type: ImageScaler,
    /// The Thorium config
    pub(crate) conf: Conf,
    /// A client for Thorium
    pub(crate) thorium: Arc<Thorium>,
    /// A cache of info from Thorium we use for scaling
    pub cache: Arc<Cache>,
    /// The zombie checker
    pub zombies: ZombieChecker,
    /// The scheduler to use to scheduler resources
    schedulers: HashMap<String, Box<dyn Scheduler + Send>>,
    /// The clusters that need to be setup before we can schedule on them again
    unready: HashSet<String>,
    /// The resources currently available in this cluster
    pub allocatable: Allocatable,
    /// A queue of tasks to complete sorted by the time to start executing them
    tasks: BTreeMap<DateTime<Utc>, Tasks>,
    /// The currently active background tasks that have been spawned
    active: Vec<JoinHandle<Result<TaskResult, Error>>>,
    /// When the cache was last reloaded early to inspect a new workload's image
    last_early_reload: Option<DateTime<Utc>>,
    /// The runs failed for a removed token since the last cache reload, so each is failed once
    revoked_runs: HashSet<Uuid>,
}

impl Scaler {
    /// Creates a new Scaler
    ///
    /// # Arguments
    ///
    /// * `args` - The command line args passed to the scaler
    pub async fn new(args: Args) -> Result<Self, Error> {
        // try to load a config file
        let conf = Conf::new(&args.config)?;
        // build a Thorium client
        let thorium = Arc::new(Thorium::from_key_file(&args.auth).await?);
        // build our scaler
        Self::build(
            conf,
            args.auth,
            thorium,
            args.scaler,
            args.dry_run,
            &args.context_name,
        )
        .await
    }

    /// Creates a new Scaler
    ///
    /// # Arguments
    ///
    /// * `conf` - The Thorium config to use
    /// * `auth` - The path to the keys to use with Thorium
    /// * `thorium` - The Thorium client to use
    /// * `scaler` - The scaler type to set
    /// * `dry_run` - Whether we should use the dry run scheduler
    pub async fn build(
        conf: Conf,
        auth: String,
        thorium: Arc<Thorium>,
        scaler_type: ImageScaler,
        dry_run: bool,
        context_name: &String,
    ) -> Result<Self, Error> {
        // start our scaler creation span
        let span = span!(Level::INFO, "Scaler Initialization");
        // build the scheduler instances we are going to be scheduling jobs too
        let mut schedulers = HashMap::default();
        schedulers::new(&mut schedulers, &scaler_type, dry_run, context_name, &conf).await?;
        // build and load our cache
        let cache =
            Arc::new(Cache::new(thorium.clone(), conf.clone(), auth, scaler_type, &span).await?);
        // build a zombie checker
        let zombies = ZombieChecker::new(scaler_type, &thorium);
        // get our cluster settings
        let settings = thorium.system.get_settings().await?;
        // start with an empty allocatable object
        let allocatable = Allocatable::new(scaler_type, &conf, &settings, &cache);
        // build our task queue
        let tasks = tasks::Tasks::setup_queue(&conf);
        // all clusters need to be setup before we can schedule on them
        let unready = schedulers.keys().cloned().collect();
        // instance the scaler
        let scaler = Scaler {
            scaler_type,
            conf,
            thorium,
            cache,
            zombies,
            schedulers,
            unready,
            allocatable,
            tasks,
            active: Vec::default(),
            last_early_reload: None,
            revoked_runs: HashSet::default(),
        };
        Ok(scaler)
    }

    /// Setup any unready clusters and update the resources for all clusters this scaler can see
    ///
    /// Clusters that fail to setup or report their resources are removed from scheduling
    /// until they recover. This only errors if no clusters are available to schedule on.
    #[instrument(name = "Scaler::update_resources", skip_all, err(Debug))]
    async fn update_resources(&mut self) -> Result<(), Error> {
        // track how many clusters we can schedule on
        let mut ready = 0;
        // crawl over each cluster and get its available resources
        for (name, scheduler) in &mut self.schedulers {
            // setup this cluster if it hasn't been setup or has failed since
            if self.unready.contains(name) {
                if let Err(err) = scheduler
                    .setup(name, &self.cache, &mut self.allocatable.bans)
                    .await
                {
                    // log that we failed to setup this cluster
                    event!(
                        Level::ERROR,
                        msg = "Failed to setup cluster",
                        cluster = name,
                        error = err.msg()
                    );
                    // make sure we don't schedule anything on this cluster
                    self.allocatable.mark_unreachable(name);
                    continue;
                }
                // this cluster is setup and can be scheduled on again
                self.unready.remove(name);
                event!(Level::INFO, msg = "Cluster is setup", cluster = name);
            }
            // get the burstable resources config for this cluster
            let burstable_conf = self.conf.thorium.scaler.burstable(self.scaler_type, name);
            // get the available resources for this cluster
            match scheduler
                .resources_available(&self.thorium, &self.cache.settings, &burstable_conf)
                .await
            {
                Ok(update) => {
                    // update this clusters resources
                    self.allocatable.update(name, update);
                    ready += 1;
                }
                Err(err) => {
                    // log that we failed to get this clusters resources
                    event!(
                        Level::ERROR,
                        msg = "Failed to get cluster resources",
                        cluster = name,
                        error = err.msg()
                    );
                    // stop scheduling on this cluster and set it up again once it recovers
                    self.allocatable.mark_unreachable(name);
                    self.unready.insert(name.clone());
                }
            }
        }
        // make sure we have at least one cluster to schedule on
        if ready == 0 {
            return Err(Error::new("No clusters are available to schedule on"));
        }
        // resize our deadline pool
        self.allocatable.resize_deadline_pool();
        Ok(())
    }

    /// check if any of our spawned tasks have returned and handle the response
    ///
    /// # Arguments
    ///
    /// * `span` - The span to log traces under
    #[instrument(name = "Scaler::check_tasks", skip_all, err(Debug))]
    async fn check_tasks(&mut self) -> Result<(), Error> {
        // track any futures that haven't completed yet
        let mut uncompleted = Vec::default();
        // crawl over our active tasks and handle any that have completed
        for mut handle in self.active.drain(..) {
            // check if this future has completed
            if let Poll::Ready(join_result) = poll!(&mut handle) {
                // get our compelted task
                let completed = join_result??;
                // log that a task was completed
                event!(Level::INFO, task = completed.as_str());
                // check if a join error occured
                match completed {
                    // An updated cache has been pulled
                    TaskResult::Cache(mut cache) => {
                        // keep any workloads cached while the new cache was being loaded
                        cache.adopt_workloads(&self.cache);
                        // get our schedulers so they can sync against the new cache
                        let schedulers = &mut self.schedulers;
                        let unready = &self.unready;
                        // track the clusters that failed to sync
                        let mut failed = Vec::default();
                        // sync our ban sets against the new cache
                        resync_bans(&mut self.allocatable.bans, async |bans: &mut BanSets| {
                            // crawl over each cluster and sync any cache updates
                            for (name, scheduler) in schedulers.iter_mut() {
                                // unready clusters will be fully setup with this cache once they recover
                                if unready.contains(name) {
                                    continue;
                                }
                                if let Err(err) =
                                    scheduler.sync_to_new_cache(name, &cache, bans).await
                                {
                                    // log that we failed to sync this cluster
                                    event!(
                                        Level::ERROR,
                                        msg = "Failed to sync cluster to new cache",
                                        cluster = name,
                                        error = err.msg()
                                    );
                                    failed.push(name.clone());
                                }
                            }
                            Ok(())
                        })
                        .await?;
                        // stop scheduling on any failed clusters and set them up again once they recover
                        for name in failed {
                            self.allocatable.mark_unreachable(&name);
                            self.unready.insert(name);
                        }
                        // save the cache in the scaler for next update
                        self.cache = Arc::new(cache);
                        // runs failed for a removed token are checked again after a reload
                        self.revoked_runs.clear();
                        // add a new task for refreshing our cache
                        add_task!(self, Tasks::CacheReload)
                    }
                    // reschedule any other completed task
                    completed => add_task!(self, completed.next_task()),
                };
            } else {
                // this task hasn't completed so keep tracking it
                uncompleted.push(handle);
            }
        }
        // reinsert our uncompleted tasks
        self.active.append(&mut uncompleted);
        Ok(())
    }

    /// check if any tasks need to be executed and spawn them
    #[instrument(name = "Scaler::spawn_tasks", skip_all, err(Debug))]
    async fn spawn_tasks(&mut self) -> Result<(), Error> {
        // get the current timestamp to compare against
        let now = Utc::now();
        // get updated info on Thorium
        let info = self.thorium.system.get_info(Some(self.scaler_type)).await?;
        // check if we should reload our cache early
        if info.expired_cache(self.scaler_type) {
            // set our cache refresh to happen now
            self.tasks.retain(|_, task| *task != Tasks::CacheReload);
            self.tasks.insert(Utc::now(), Tasks::CacheReload);
        }
        // reload our cache early to inspect a new workload's image, at most once per interval
        if early_reload_due(self.cache.reload_requested(), self.last_early_reload, now) {
            // clear the request now that it is being served
            self.cache.clear_reload_request();
            self.last_early_reload = Some(now);
            // set our cache refresh to happen now
            self.tasks.retain(|_, task| *task != Tasks::CacheReload);
            self.tasks.insert(now, Tasks::CacheReload);
        }
        // get any tasks we want to spawn and build a list of completed blocking tasks to rerun again
        let mut completed = Vec::default();
        // get the tasks that are ready to run
        let ready = self
            .tasks
            .extract_if(.., |time, _| time < &now)
            .collect::<Vec<_>>();
        for (_, task) in ready {
            // log that we are spawning a task
            event!(Level::INFO, task = task.as_str());
            // spawn or execute this task
            match task {
                // zombie checks require a mutable reference to the scheduler so we cannot run that
                // as a background task
                Tasks::ZombieJobs => {
                    // check for any zombie jobs and reset them
                    self.zombies.check(&self.allocatable).await?;
                    completed.push(Tasks::ZombieJobs);
                }
                // tell Thorium to sync ldap groups
                Tasks::LdapSync => {
                    // clone client so we can send it to another thread
                    let client = self.thorium.clone();
                    spawn!(self.active, tasks::sync_ldap(client));
                }
                // build a new cache object
                Tasks::CacheReload => {
                    // clone data so we can send it to another thread
                    let stale_cache = self.cache.clone();
                    let client = self.thorium.clone();
                    let scaler = self.scaler_type;
                    spawn!(self.active, Cache::refresh(stale_cache, client, scaler));
                }
                // get the total amount of resources available in the cluster
                Tasks::Resources => {
                    // update the resources for all clusters
                    self.update_resources().await?;
                    completed.push(Tasks::Resources);
                }
                // tell Thorium to update image runtimes
                Tasks::UpdateRuntimes => {
                    // clone client so we can send it to another thread
                    let client = self.thorium.clone();
                    spawn!(self.active, tasks::update_runtimes(client));
                }
                // tell Thorium to cleanup any expired data
                Tasks::Cleanup => {
                    // clone client so we can send it to another thread
                    let client = self.thorium.clone();
                    spawn!(self.active, tasks::cleanup(client));
                }
                // tell Thorium to decrease our users fair share ranks
                Tasks::DecreaseFairShare => {
                    // decrease our users fair share ranks
                    self.allocatable.decrease_fair_share_ranks(&self.conf);
                }
            }
        }
        // add any blocking completed tasks back to our task list
        for task in completed {
            add_task!(self, task);
        }
        Ok(())
    }

    /// Set all the workers we wish to spawn as being spawned
    ///
    /// # Arguments
    ///
    /// * `reqs` - The reqs we spawned
    #[instrument(name = "Scaler::register_workers", skip_all, err(Debug))]
    async fn register_workers(&self, reqs: &ReqMap) -> Result<(), Error> {
        // submit each batch of new workers to Thorium in order
        for registrations in registration_batches(reqs) {
            // try to register our new workers
            self.thorium
                .system
                .register_workers(self.scaler_type, &registrations)
                .await?;
        }
        Ok(())
    }

    /// Schedules resources based on our requisition map
    ///
    /// # Arguments
    ///
    /// * `reqs` - The requisition map to schedule off of
    /// * `span` - The span to log traces too
    #[allow(clippy::too_many_lines)]
    #[instrument(name = "Scaler::schedule", skip_all, err(Debug))]
    async fn schedule(&mut self) -> Result<HashSet<String>, Error> {
        // clone our current resource counts so we can track changes
        let past = self.allocatable.counts.clone();
        // track our succesful scale changes and the the correctly deleted ones
        let mut changes: HashMap<DateTime<Utc>, Vec<Spawned>> =
            HashMap::with_capacity(self.allocatable.changes.spawns.len());
        let mut deleted = HashSet::with_capacity(self.allocatable.changes.scale_down.len());
        // get the clusters whose schedulers still have pending deletes to validate
        let pending = self
            .schedulers
            .iter()
            .filter(|(_, scheduler)| scheduler.has_pending_deletes())
            .map(|(name, _)| name.clone())
            .collect::<HashSet<String>>();
        // get the clusters to scale down workers on or validate pending deletes for
        let scale_down = std::mem::take(&mut self.allocatable.changes.scale_down);
        let targets = delete_targets(scale_down, &pending);
        // start our delete workers span if we have workers to delete or pending deletes to validate
        if !targets.is_empty() {
            // start our delete workers span
            let del_span = span!(Level::INFO, "Deleting Workers");
            // crawl over the clusters we have scale down orders or pending deletes for
            for (cluster_name, deletes) in targets {
                // get a mutable reference to the target cluster
                if let Some(cluster) = self.schedulers.get_mut(&cluster_name) {
                    // delete the requested workers
                    let results = cluster.delete(&self.thorium, &self.cache, deletes).await;
                    // check the status of this clusters deletes
                    for result in results {
                        // check the status of this deletion
                        match result {
                            // this worker was successfully deleted
                            WorkerDeletion::Deleted(delete) => {
                                deleted.insert(delete.name);
                            }
                            // We ran into an error when deleting this worker
                            WorkerDeletion::Error { delete, error } => {
                                // log that we failed to delete a worker
                                event!(
                                    parent: &del_span,
                                    Level::ERROR,
                                    msg = "Failed to delete worker",
                                    cluster = cluster_name,
                                    req = delete.req.to_string(),
                                    name = delete.name,
                                    error = error.msg()
                                );
                            }
                        }
                    }
                }
            }
            // close our delete worker span
            drop(del_span);
        }
        // track the workers we fail to spawn
        let mut failed = WorkerDeleteMap::default();
        // start our spawn workers span if we have workers to spawn
        if !self.allocatable.changes.spawns.is_empty() {
            let spawn_span = span!(Level::INFO, "Spawning Workers");
            // Register our new workers before spawning them to avoid race conditions
            self.register_workers(&self.allocatable.changes).await?;
            // take this tick's spawns so their run env can be filled in
            let mut spawns = std::mem::take(&mut self.allocatable.changes.spawns);
            // a K8s per-run worker gets its run's token and models as env values, fetched now
            if self.scaler_type == ImageScaler::K8s {
                for failure in fill_run_envs(&self.thorium, &self.cache, &mut spawns).await {
                    // log that this worker couldn't get its env
                    event!(
                        parent: &spawn_span,
                        Level::ERROR,
                        msg = "Failed to get a per-run worker's env",
                        req = failure.spawn.req.to_string(),
                        name = failure.spawn.name,
                        error = failure.error.msg()
                    );
                    // a run whose token was removed mid-run is failed once
                    if failure.revoked
                        && let Some(run) = failure.spawn.req.run
                        && self.revoked_runs.insert(run)
                        && let Err(error) =
                            fail_revoked_run(&self.thorium, &failure.spawn.req.group, run).await
                    {
                        event!(
                            parent: &spawn_span,
                            Level::ERROR,
                            msg = "Failed to fail a run whose token was removed",
                            run = run.to_string(),
                            error = error.msg()
                        );
                    }
                    // the worker is cleared and its deadline tried again next tick
                    schedulers::record_failed_spawn(
                        &mut self.allocatable.fairshare_pool,
                        &mut self.allocatable.workload_counts,
                        failure.spawn,
                        &mut deleted,
                        &mut failed,
                    );
                }
            }
            // crawl over the clusters we have spawns for
            for (cluster_name, spawn_map) in spawns.drain() {
                // get a mutable reference to the target cluster
                if let Some(cluster) = self.schedulers.get_mut(&cluster_name) {
                    // spawn the new resources for this cluster
                    let errors = cluster.spawn(&self.cache, &spawn_map).await;
                    // check if any of our spawns failed
                    for (deadline, spawns) in spawn_map {
                        // crawl over the spawns in this spawn group
                        for spawn in spawns {
                            // if this resources name is in our error map then log the failed spawn
                            match errors.get(&spawn.name) {
                                // there was an error when spawning this resource
                                Some(error) => {
                                    // log that we failed to spawn a worker
                                    event!(
                                        parent: &spawn_span,
                                        Level::ERROR,
                                        msg = "Failed to span worker",
                                        cluster = cluster_name,
                                        req = spawn.req.to_string(),
                                        name = spawn.name,
                                        error = error.msg()
                                    );
                                    // track this failed worker
                                    schedulers::record_failed_spawn(
                                        &mut self.allocatable.fairshare_pool,
                                        &mut self.allocatable.workload_counts,
                                        spawn,
                                        &mut deleted,
                                        &mut failed,
                                    );
                                }
                                None => {
                                    // a spawned worker's pod holds its env, so the scaler keeps none
                                    let mut spawn = spawn;
                                    spawn.run_env = None;
                                    // get an entry to this resources current count
                                    let entry = self
                                        .allocatable
                                        .counts
                                        .entry(spawn.req.clone())
                                        .or_default();
                                    // increase this resources scale by 1
                                    *entry += 1;
                                    // log a that new worker was spawned
                                    event!(
                                        parent: &spawn_span,
                                        Level::INFO,
                                        msg = "Spawned worker",
                                        cluster = cluster_name,
                                        req = spawn.req.to_string(),
                                        name = spawn.name,
                                    );
                                    // get an entry to this spawns deadline group
                                    let dl_entry = changes.entry(deadline).or_default();
                                    // add this spawned worker to this deadline group
                                    dl_entry.push(spawn);
                                }
                            }
                        }
                    }
                }
            }
            // close our delete worker span
            drop(spawn_span);
        }
        // commit our finalized changes
        let req_changes = self.allocatable.commit(changes);
        // start our nested log scalers span
        let log_span = span!(Level::INFO, "Log Scale Changes");
        // log any successful changes to our resources
        for req in req_changes {
            // get the current and prior count for this resource
            let new = self.allocatable.counts.get(&req).unwrap_or(&0);
            let prior = past.get(&req).unwrap_or(&0);
            // calculate the change
            let change = new - prior;
            // log the change in scale
            event!(
                parent: &log_span,
                Level::INFO,
                msg = "Scaling workers",
                req = req.to_string(),
                new = new,
                change = change,
            );
        }
        // close our log scale changes span
        drop(log_span);
        // remove any workers we failed to spawn
        self.thorium
            .system
            .delete_workers(self.scaler_type, &failed)
            .await?;
        Ok(deleted)
    }

    /// Error out any workers jobs that have permenantly failed
    #[instrument(name = "Scaler::error_out", skip_all)]
    async fn error_out(&mut self, error_out: HashSet<ErrorOutKinds>, failed: &mut HashSet<String>) {
        // build a stream of our failed workers info
        let mut worker_stream = stream::iter(error_out)
            .map(|reason| async {
                // get this workers info
                let worker = self.thorium.system.get_worker(reason.worker()).await;
                // return our worker and kind tuple
                (reason, worker)
            })
            .buffer_unordered(10);
        // build an OOM ERROR LOG
        let add = StageLogsAdd::default();
        // fail out any workers we find
        while let Some((reason, worker_res)) = worker_stream.next().await {
            // log an error if we failed to get a worker
            match worker_res {
                Ok(worker) => {
                    // get this workers job if it has one
                    if let Some(active) = &worker.active {
                        // try to fail out this job
                        if let Err(error) = self
                            .thorium
                            .jobs
                            .error(&active.job, &add, &worker.name)
                            .await
                        {
                            // we failed to error out this job
                            event!(
                                Level::ERROR,
                                worker = worker.name,
                                error = error.msg(),
                                active = format!("{active:?}")
                            );
                        }
                        // log that we are erroring out this job
                        event!(
                            Level::INFO,
                            worker = worker.name,
                            job = active.job.to_string(),
                            reason = reason.reason_as_str(),
                        );
                    } else {
                        // log that we are erroring out this job
                        event!(
                            Level::INFO,
                            worker = worker.name,
                            active = "None",
                            reason = reason.reason_as_str(),
                        );
                    }
                    // add this worker to our failed set
                    failed.insert(worker.name);
                }
                Err(error) => {
                    // we faled to get this jobs worker
                    event!(Level::ERROR, error = error.msg());
                }
            }
        }
    }

    /// Clear any failed jobs across all clusters
    ///
    /// # Arguments
    ///
    /// * `span` - The span to log traces under
    #[instrument(name = "Scaler::clear_terminal", skip_all)]
    async fn clear_terminal(&mut self, mut terminal: HashSet<String>) {
        // build a list of all of our failed and terminal workers
        let mut failed = HashSet::default();
        let mut error_out = HashSet::default();
        // crawl over each cluster and get it setup for executing jobs
        for (name, scheduler) in &mut self.schedulers {
            // skip any clusters that are currently unreachable
            if self.unready.contains(name) {
                continue;
            }
            // if this fails just continue since failed resources may just be clutter
            if let Err(err) = scheduler
                .clear_terminal(
                    &self.thorium,
                    &self.allocatable,
                    &self.cache.groups,
                    &mut failed,
                    &mut terminal,
                    &mut error_out,
                )
                .await
            {
                event!(Level::ERROR, error = err.msg());
            }
        }
        // error out any errored out workers
        self.error_out(error_out, &mut failed).await;
        // free any terminal resources
        if let Err(error) = self
            .allocatable
            .free_deleted(&self.thorium, &failed, &terminal, &self.conf)
            .await
        {
            event!(Level::ERROR, error = error.msg());
        }
    }

    /// Initialize this scaler to begin scheduling jobs
    ///
    /// This does not need to be called before start and is used for step purposes.
    #[instrument(name = "Scaler::init", skip_all, err(Debug))]
    pub async fn init(&mut self) -> Result<(), Error> {
        // setup all clusters and get an initial count of their resources
        self.update_resources().await
    }

    /// Perform a single scale loop
    ///
    /// In most cases you want to just call `start`.
    #[instrument(name = "Scaler::single_scale_loop", skip_all, err(Debug))]
    pub async fn single_scale_loop(&mut self) -> Result<(), Error> {
        // check if any new tasks need to be spawned
        self.spawn_tasks().await?;
        // check if any tasks completed
        self.check_tasks().await?;
        // check to see if our ban rule sets have expired
        self.allocatable.bans.is_expired();
        // try to allocate resources to workers
        self.allocatable
            .allocate(&self.thorium, &self.cache)
            .await?;
        // schedule resources based on our requisition map and ban any failures
        let deleted = self.schedule().await?;
        // sleep for 1s to make sure worker registration has time to happen
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        // clear out any terminal resources
        self.clear_terminal(deleted).await;
        Ok(())
    }

    /// Start scaling pods up and down based on the deadline stream
    pub async fn start(&mut self) -> Result<(), Error> {
        // setup this scaler
        self.init().await?;
        // loop forever scaling pods up and down based on the deadline stream
        loop {
            // perform a single scale loop
            self.single_scale_loop().await?;
            // sleep for the configured dwell between scale attempts
            let dwell =
                std::time::Duration::from_secs(self.conf.thorium.scaler.dwell(self.scaler_type));
            tokio::time::sleep(dwell).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::test_api::{
        self, GROUP, MockApi, MockCrane, World, bare_metal_spec, deadline, k8s_spec, tool_instance,
    };
    use std::time::Instant;
    use thorium::models::{SpawnLimits, WorkerRegistrationList, WorkloadSpec};
    use uuid::Uuid;

    /// Build a world of tool instances with deadlines round robin over their requisitions
    ///
    /// # Arguments
    ///
    /// * `specs` - Each tool instance's workload
    /// * `users` - The users whose jobs are queued
    /// * `count` - The number of deadlines
    /// * `per_run` - Whether every deadline is its own run
    fn build_world(
        specs: &[WorkloadSpec],
        users: &[&str],
        count: usize,
        per_run: bool,
    ) -> (World, Vec<Uuid>) {
        // serve each tool instance
        let mut world = World::default();
        let ids = (0..specs.len())
            .map(|i| Uuid::from_u128(5000 + i as u128))
            .collect::<Vec<Uuid>>();
        for (i, (id, spec)) in ids.iter().zip(specs).enumerate() {
            let entity = tool_instance(*id, &format!("tool-{i}"), "1.0.0", &[GROUP], spec.clone());
            world.entities.insert(*id, entity);
        }
        world.users = users
            .iter()
            .map(|name| test_api::user(name, &[GROUP]))
            .collect();
        // queue deadlines round robin over every user and tool instance, oldest first
        let reqs = ids.len() * users.len();
        world.deadlines = (0..count)
            .map(|i| {
                let user = users[i % users.len()];
                let id = ids[(i / users.len()) % ids.len()];
                let run = per_run.then(Uuid::new_v4);
                deadline(user, GROUP, id, run, i64::try_from(count - i).unwrap())
            })
            .collect();
        // every run's token can be read
        for run in world.deadlines.iter().filter_map(|deadline| deadline.run) {
            world.run_tokens.insert(run, Ok(format!("token-{run}")));
        }
        // the stats count the shared jobs of each requisition
        let created = (count / reqs.max(1)) as u64;
        let jobs = ids
            .iter()
            .flat_map(|id| users.iter().map(move |user| (*id, *user, created, 0)))
            .collect::<Vec<_>>();
        if !per_run {
            world.stats = Some(test_api::workload_stats(&jobs));
        }
        (world, ids)
    }

    /// Run one tick of allocation and scheduling
    ///
    /// # Arguments
    ///
    /// * `scaler` - The scaler to tick
    async fn tick(scaler: &mut Scaler) -> usize {
        // allocate and schedule as the scale loop does
        scaler
            .allocatable
            .allocate(&scaler.thorium, &scaler.cache)
            .await
            .unwrap();
        let spawned = scaler
            .allocatable
            .changes
            .spawns
            .values()
            .flat_map(|by_deadline| by_deadline.values())
            .map(Vec::len)
            .sum();
        scaler.schedule().await.unwrap();
        spawned
    }

    /// Reload a scaler's cache as `check_tasks` does when a reload completes
    ///
    /// # Arguments
    ///
    /// * `scaler` - The scaler whose cache to reload
    async fn reload(scaler: &mut Scaler) {
        // build the new cache and swap it in, keeping workloads added during the reload
        let result = Cache::refresh(
            scaler.cache.clone(),
            scaler.thorium.clone(),
            scaler.scaler_type,
        )
        .await
        .unwrap();
        let TaskResult::Cache(mut cache) = result else {
            panic!("a refresh returns a cache");
        };
        cache.adopt_workloads(&scaler.cache);
        scaler.cache = Arc::new(cache);
    }

    /// Build models a harness worker gets, with one credential env value
    ///
    /// # Arguments
    ///
    /// * `secret` - The value of the credential env value
    fn run_models(secret: &str) -> thorium::models::RunModels {
        thorium::models::RunModels {
            models: thorium::models::HarnessModels::default(),
            env: [(
                "ANTHROPIC_API_KEY".to_owned(),
                thorium::models::ModelSecret::from(secret),
            )]
            .into_iter()
            .collect(),
        }
    }

    /// Get the runs of the run token reads a mock API answered
    ///
    /// # Arguments
    ///
    /// * `api` - The mock API
    /// * `suffix` - The route's last segment
    fn run_reads(api: &MockApi, suffix: &str) -> Vec<String> {
        api.matching("GET", "/api/reactions/")
            .into_iter()
            .filter(|req| req.path.ends_with(suffix))
            .map(|req| req.path)
            .collect()
    }

    /// Build a reaction of a run in a status, with one planned stage
    ///
    /// # Arguments
    ///
    /// * `id` - The run's id
    /// * `status` - The run's status
    fn run_reaction(id: Uuid, status: &ReactionStatus) -> thorium::models::Reaction {
        // a workflow run with one harness stage
        let value = serde_json::json!({
            "id": id,
            "group": GROUP,
            "creator": "io",
            "pipeline": "workflow:triage",
            "status": status,
            "current_stage": 0,
            "current_stage_progress": 0,
            "current_stage_length": 1,
            "args": {},
            "sla": Utc::now(),
            "jobs": [],
            "tags": [],
            "parent": null,
            "sub_reactions": 0,
            "completed_sub_reactions": 0,
            "generators": [],
            "samples": [],
            "ephemeral": [],
            "parent_ephemeral": {},
            "repos": [],
            "trigger_depth": null,
            "has_cache": false,
            "source": {"Tool": {"tool_instance": id}},
            "plan": {"stages": [[{"name": "harness", "tool_instance": id}]]},
            "run_token": format!("run-{id}"),
            "run": id,
        });
        serde_json::from_value(value).unwrap()
    }

    /// A K8s scaler fetches each per-run spawn's token, and for a harness its models, right
    /// before spawning it, 10 at a time; an Image worker of the same run fetches no models, a
    /// shared spawn fetches nothing, and a spawned worker's env isn't kept
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[allow(clippy::too_many_lines)]
    async fn per_run_spawns_fetch_their_env() {
        // a harness and an Image tool instance, both K8s
        let harness = Uuid::from_u128(7001);
        let image = Uuid::from_u128(7002);
        let mut world = World::default();
        world.entities.insert(
            harness,
            test_api::harness_instance(
                harness,
                "pal-harness",
                &[GROUP],
                k8s_spec("registry.example/pal-harness:1", SpawnLimits::Unlimited),
            ),
        );
        world.entities.insert(
            image,
            tool_instance(
                image,
                "strings",
                "1.0.0",
                &[GROUP],
                k8s_spec("registry.example/strings:1", SpawnLimits::Unlimited),
            ),
        );
        world.users = vec![test_api::user("io", &[GROUP])];
        // one workflow run calling the Image tool, plus a pipeline job of the Image tool
        let run = Uuid::new_v4();
        world.deadlines = vec![
            deadline("io", GROUP, harness, Some(run), 30),
            deadline("io", GROUP, image, Some(run), 20),
            deadline("io", GROUP, image, None, 10),
        ];
        world.stats = Some(test_api::workload_stats(&[(image, "io", 1, 0)]));
        world
            .run_tokens
            .insert(run, Ok("run-token-value".to_owned()));
        world
            .run_models
            .insert(run, Ok(run_models("sk-run-model-secret")));
        let api = MockApi::start(world).await;
        let crane = MockCrane::new();
        let mut scaler = Box::pin(test_api::scaler(
            &api,
            test_api::conf(&crane),
            ImageScaler::K8s,
        ))
        .await;
        tick(&mut scaler).await;
        reload(&mut scaler).await;
        api.clear();
        // one tick spawns all three, reading the run's token for both kinds of worker and its
        // models only for the harness
        assert_eq!(tick(&mut scaler).await, 3);
        assert_eq!(run_reads(&api, "/run-token").len(), 2);
        assert_eq!(run_reads(&api, "/run-models").len(), 1);
        let mut runs = Vec::new();
        for req in api.matching("POST", "/api/system/worker/K8s") {
            let list: WorkerRegistrationList = serde_json::from_slice(&req.body).unwrap();
            runs.extend(
                list.workers
                    .iter()
                    .map(|worker| (worker.tool_instance, worker.run)),
            );
        }
        runs.sort();
        assert_eq!(
            runs,
            vec![(harness, Some(run)), (image, None), (image, Some(run))]
        );
        // the spawned workers keep no env
        assert!(
            scaler
                .allocatable
                .counts
                .keys()
                .all(|req| req.tool_instance == harness || req.tool_instance == image)
        );
        // filling the env of a tick's spawns gives each per-run spawn its values
        let workload = scaler.cache.get_workload(&harness).unwrap();
        let projected = workload.projection(GROUP).unwrap().clone();
        let image_workload = scaler.cache.get_workload(&image).unwrap();
        let image_projected = image_workload.projection(GROUP).unwrap().clone();
        let spawn = |req: thorium::models::Requisition,
                     workload: &crate::libs::cache::CachedWorkload,
                     projected: &thorium::models::Image| {
            Spawned::new(
                "cluster0",
                "node0",
                req,
                workload,
                projected,
                thorium::models::Pools::Deadline,
            )
        };
        let mut spawns: HashMap<String, BTreeMap<DateTime<Utc>, Vec<Spawned>>> = HashMap::new();
        spawns.entry("cluster0".to_owned()).or_default().insert(
            Utc::now(),
            vec![
                spawn(
                    thorium::models::Requisition::new("io", GROUP, harness, Some(run)),
                    &workload,
                    &projected,
                ),
                spawn(
                    thorium::models::Requisition::new("io", GROUP, image, Some(run)),
                    &image_workload,
                    &image_projected,
                ),
                spawn(
                    thorium::models::Requisition::new("io", GROUP, image, None),
                    &image_workload,
                    &image_projected,
                ),
            ],
        );
        let failures = fill_run_envs(&scaler.thorium, &scaler.cache, &mut spawns).await;
        assert!(failures.is_empty());
        let filled = spawns["cluster0"].values().next().unwrap();
        let harness_env = filled[0].run_env.as_ref().unwrap();
        assert_eq!(harness_env.token, "run-token-value");
        assert!(harness_env.models.is_some());
        let image_env = filled[1].run_env.as_ref().unwrap();
        assert_eq!(image_env.token, "run-token-value");
        assert!(image_env.models.is_none());
        assert!(filled[2].run_env.is_none());
        // a spawn's debug output names its env vars but holds no value
        let debug = format!("{:?}", filled[0]);
        assert!(debug.contains("THORIUM_TOKEN") && debug.contains("ANTHROPIC_API_KEY"));
        assert!(!debug.contains("run-token-value") && !debug.contains("sk-run-model-secret"));
    }

    /// A per-run spawn whose token read returns `404`, whose models read returns `409`, or whose
    /// read fails is recorded as a failed spawn and fetched again next tick, without stopping the
    /// tick's other spawns; at most 10 reads are in flight, and a run whose token was removed
    /// while it's running is failed once with a stage log
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[allow(clippy::too_many_lines)]
    async fn failed_run_env_fetches() {
        // a harness tool instance
        let harness = Uuid::from_u128(7101);
        let mut world = World::default();
        world.entities.insert(
            harness,
            test_api::harness_instance(
                harness,
                "pal-harness",
                &[GROUP],
                k8s_spec("registry.example/pal-harness:2", SpawnLimits::Unlimited),
            ),
        );
        world.users = vec![test_api::user("io", &[GROUP])];
        // 50 runs: 20 without a token, 20 with a banned credential, and 10 fine
        let runs = (0..50).map(|_| Uuid::new_v4()).collect::<Vec<Uuid>>();
        for (i, run) in runs.iter().enumerate() {
            if i < 20 {
                world.run_tokens.insert(*run, Err(404));
            } else {
                world.run_tokens.insert(*run, Ok(format!("token-{run}")));
                let models = if i < 40 {
                    Err(409)
                } else {
                    Ok(run_models("sk"))
                };
                world.run_models.insert(*run, models);
            }
        }
        world.run_env_delay = std::time::Duration::from_millis(20);
        let api = MockApi::start(world).await;
        let crane = MockCrane::new();
        let scaler = Box::pin(test_api::scaler(
            &api,
            test_api::conf(&crane),
            ImageScaler::K8s,
        ))
        .await;
        let thorium = api.client().await;
        scaler
            .cache
            .prefetch(&thorium, [harness].into_iter().collect())
            .await;
        // filling the env of 50 spawns at once keeps at most 10 reads in flight
        let workload = scaler.cache.get_workload(&harness).unwrap();
        let projected = workload.projection(GROUP).unwrap().clone();
        let mut spawns: HashMap<String, BTreeMap<DateTime<Utc>, Vec<Spawned>>> = HashMap::new();
        spawns.entry("cluster0".to_owned()).or_default().insert(
            Utc::now(),
            runs.iter()
                .map(|run| {
                    Spawned::new(
                        "cluster0",
                        "node0",
                        thorium::models::Requisition::new("io", GROUP, harness, Some(*run)),
                        &workload,
                        &projected,
                        thorium::models::Pools::Deadline,
                    )
                })
                .collect(),
        );
        api.clear();
        let start = Instant::now();
        let failures = fill_run_envs(&thorium, &scaler.cache, &mut spawns).await;
        println!(
            "filling 50 per-run spawns, 40 failing their reads, took {:?}",
            start.elapsed()
        );
        assert!(
            api.max_run_env_reads
                .load(std::sync::atomic::Ordering::SeqCst)
                <= 10
        );
        assert!(
            api.max_run_env_reads
                .load(std::sync::atomic::Ordering::SeqCst)
                > 1
        );
        assert_eq!(run_reads(&api, "/run-token").len(), 50);
        assert_eq!(run_reads(&api, "/run-models").len(), 30);
        // the 40 failed spawns are taken out, 20 of them for a removed token
        assert_eq!(failures.len(), 40);
        assert_eq!(
            failures.iter().filter(|failure| failure.revoked).count(),
            20
        );
        let kept = spawns["cluster0"].values().next().unwrap();
        assert_eq!(kept.len(), 10);
        assert!(kept.iter().all(|spawn| spawn.run_env.is_some()));
        // in a tick, failed spawns are cleared and a running run without a token is failed once
        let mut world = World::default();
        world.entities.insert(
            harness,
            test_api::harness_instance(
                harness,
                "pal-harness",
                &[GROUP],
                k8s_spec("registry.example/pal-harness:3", SpawnLimits::Unlimited),
            ),
        );
        world.users = vec![test_api::user("io", &[GROUP])];
        let (started, completed, banned, flaky, fine) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        world.deadlines = [started, completed, banned, flaky, fine]
            .iter()
            .enumerate()
            .map(|(i, run)| {
                deadline(
                    "io",
                    GROUP,
                    harness,
                    Some(*run),
                    100 - i64::try_from(i).unwrap(),
                )
            })
            .collect();
        world.run_tokens.insert(started, Err(404));
        world.run_tokens.insert(completed, Err(404));
        world
            .run_tokens
            .insert(banned, Ok("banned-token".to_owned()));
        world.run_models.insert(banned, Err(409));
        world.run_tokens.insert(flaky, Err(500));
        world.run_models.insert(flaky, Ok(run_models("sk")));
        world.run_tokens.insert(fine, Ok("fine-token".to_owned()));
        world.run_models.insert(fine, Ok(run_models("sk")));
        world
            .reactions
            .insert(started, run_reaction(started, &ReactionStatus::Started));
        world.reactions.insert(
            completed,
            run_reaction(completed, &ReactionStatus::Completed),
        );
        let api = MockApi::start(world).await;
        let mut scaler = Box::pin(test_api::scaler(
            &api,
            test_api::conf(&crane),
            ImageScaler::K8s,
        ))
        .await;
        tick(&mut scaler).await;
        reload(&mut scaler).await;
        api.clear();
        let start = Instant::now();
        assert_eq!(tick(&mut scaler).await, 5);
        println!(
            "a tick with 5 per-run spawns, 4 failing, took {:?}",
            start.elapsed()
        );
        // the four failed workers are deleted and only the fine one is counted
        let mut deleted = 0;
        for req in api.matching("DELETE", "/api/system/worker/K8s") {
            let map: WorkerDeleteMap = serde_json::from_slice(&req.body).unwrap();
            deleted += map.workers.len();
        }
        assert_eq!(deleted, 4);
        let live = scaler
            .allocatable
            .counts
            .iter()
            .filter(|(_, count)| **count > 0)
            .map(|(req, _)| req.run)
            .collect::<Vec<_>>();
        assert_eq!(live, vec![Some(fine)]);
        // only the running run without a token is failed, with its reason logged
        assert_eq!(api.count("POST", "/api/reactions/handle/"), 1);
        assert_eq!(
            api.matching("POST", "/api/reactions/handle/")[0].path,
            format!("/api/reactions/handle/{GROUP}/{started}/fail")
        );
        let logs = api.matching("POST", &format!("/api/reactions/logs/{GROUP}/{started}"));
        assert_eq!(logs.len(), 1);
        assert!(String::from_utf8_lossy(&logs[0].body).contains(RUN_TOKEN_REVOKED));
        // the failed runs are fetched again next tick, and the flaky one now spawns
        api.world
            .lock()
            .unwrap()
            .run_tokens
            .insert(flaky, Ok("flaky-token".to_owned()));
        api.clear();
        tick(&mut scaler).await;
        let reads = run_reads(&api, "/run-token");
        for run in [started, banned, flaky] {
            assert!(reads.iter().any(|path| path.contains(&run.to_string())));
        }
        assert!(
            scaler
                .allocatable
                .counts
                .iter()
                .any(|(req, count)| req.run == Some(flaky) && *count > 0)
        );
        // the run without a token isn't failed again
        assert_eq!(api.count("POST", "/api/reactions/handle/"), 0);
    }

    /// A Direct scaler registers a per-run worker for a per-run job, with its run set, and reads
    /// no run token or models, because the reactor reads the token
    #[tokio::test]
    async fn direct_registers_per_run_workers() {
        // a bare metal tool instance with one tool run job
        let (mut world, ids) = build_world(&[bare_metal_spec()], &["io"], 1, true);
        let run = world.deadlines[0].run.unwrap();
        world.run_tokens.clear();
        let api = MockApi::start(world).await;
        let crane = MockCrane::new();
        let mut scaler = Box::pin(test_api::scaler(
            &api,
            test_api::conf(&crane),
            ImageScaler::BareMetal,
        ))
        .await;
        api.clear();
        // one tick registers one per-run worker
        assert_eq!(tick(&mut scaler).await, 1);
        let registered = api.matching("POST", "/api/system/worker/BareMetal");
        assert_eq!(registered.len(), 1);
        let list: WorkerRegistrationList = serde_json::from_slice(&registered[0].body).unwrap();
        assert_eq!(list.workers.len(), 1);
        assert_eq!(list.workers[0].tool_instance, ids[0]);
        assert_eq!(list.workers[0].run, Some(run));
        // nothing about the run was read and no worker was deleted
        assert_eq!(api.count("GET", "/api/reactions/"), 0);
        for req in api.matching("DELETE", "/api/system/worker/") {
            let map: WorkerDeleteMap = serde_json::from_slice(&req.body).unwrap();
            assert_eq!(map.workers.len(), 0);
        }
    }

    /// Per-run requisitions are one per run and tool instance, separate from the shared one of the
    /// same user and tool instance; a tool instance's spawn limit caps one run's workers, and a
    /// container tool call queued with its parent run's deadline is served before a pipeline job
    /// queued after it
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[allow(clippy::too_many_lines)]
    async fn per_run_requisitions_and_limits() {
        // two Image tool instances, the first limited to two workers
        let first = Uuid::from_u128(7201);
        let second = Uuid::from_u128(7202);
        let mut world = World::default();
        world.entities.insert(
            first,
            tool_instance(
                first,
                "strings",
                "1.0.0",
                &[GROUP],
                k8s_spec("registry.example/strings:2", SpawnLimits::Basic(2)),
            ),
        );
        world.entities.insert(
            second,
            tool_instance(
                second,
                "capa",
                "1.0.0",
                &[GROUP],
                k8s_spec("registry.example/capa:2", SpawnLimits::Unlimited),
            ),
        );
        world.users = vec![test_api::user("io", &[GROUP])];
        // a workflow run with three calls of the first tool and one of the second
        let run = Uuid::new_v4();
        world.deadlines = vec![
            deadline("io", GROUP, first, Some(run), 50),
            deadline("io", GROUP, first, Some(run), 40),
            deadline("io", GROUP, first, Some(run), 30),
            deadline("io", GROUP, second, Some(run), 20),
        ];
        world.run_tokens.insert(run, Ok("token".to_owned()));
        let api = MockApi::start(world).await;
        let crane = MockCrane::new();
        let mut scaler = Box::pin(test_api::scaler(
            &api,
            test_api::conf(&crane),
            ImageScaler::K8s,
        ))
        .await;
        tick(&mut scaler).await;
        reload(&mut scaler).await;
        api.clear();
        // the first tool gets two workers for the run and the second one, all with the run set
        tick(&mut scaler).await;
        let mut workers = Vec::new();
        for req in api.matching("POST", "/api/system/worker/K8s") {
            let list: WorkerRegistrationList = serde_json::from_slice(&req.body).unwrap();
            workers.extend(
                list.workers
                    .iter()
                    .map(|worker| (worker.tool_instance, worker.run)),
            );
        }
        workers.sort();
        assert_eq!(
            workers,
            vec![(first, Some(run)), (first, Some(run)), (second, Some(run))]
        );
        // with one slot left, the call queued with its parent's deadline beats a later pipeline job
        let mut world = World::default();
        world.entities.insert(
            first,
            tool_instance(
                first,
                "strings",
                "1.0.0",
                &[GROUP],
                k8s_spec("registry.example/strings:3", SpawnLimits::Basic(1)),
            ),
        );
        world.users = vec![test_api::user("io", &[GROUP])];
        let parent = Uuid::new_v4();
        world.deadlines = vec![
            deadline("io", GROUP, first, Some(parent), 120),
            deadline("io", GROUP, first, None, 10),
        ];
        world.stats = Some(test_api::workload_stats(&[(first, "io", 1, 0)]));
        world.run_tokens.insert(parent, Ok("token".to_owned()));
        let api = MockApi::start(world).await;
        let mut scaler = Box::pin(test_api::scaler(
            &api,
            test_api::conf(&crane),
            ImageScaler::K8s,
        ))
        .await;
        tick(&mut scaler).await;
        reload(&mut scaler).await;
        api.clear();
        tick(&mut scaler).await;
        let mut workers = Vec::new();
        for req in api.matching("POST", "/api/system/worker/K8s") {
            let list: WorkerRegistrationList = serde_json::from_slice(&req.body).unwrap();
            workers.extend(list.workers.iter().map(|worker| worker.run));
        }
        assert_eq!(workers, vec![Some(parent)]);
    }

    /// Shared workload jobs over 10 requisitions make 4 API requests per tick after the first,
    /// which also fetches each tool instance's spec once, while per-run jobs are filtered before
    /// allocation and never registered
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn scale_loop_requests_per_tick() {
        // 2 users and 5 K8s tool instances with shared jobs, inspected by a reload
        let specs = (0..5)
            .map(|i| {
                k8s_spec(
                    &format!("registry.example/tool-{i}:1"),
                    SpawnLimits::Unlimited,
                )
            })
            .collect::<Vec<_>>();
        let (world, _) = build_world(&specs, &["io", "europa"], 100, false);
        let api = MockApi::start(world).await;
        let crane = MockCrane::new();
        let mut scaler = Box::pin(test_api::scaler(
            &api,
            test_api::conf(&crane),
            ImageScaler::K8s,
        ))
        .await;
        api.clear();
        let start = Instant::now();
        let first = tick(&mut scaler).await;
        // the first tick reads each spec once and filters the uninspected images
        assert_eq!(api.count("GET", "/api/entities/"), 5);
        assert_eq!(first, 0);
        reload(&mut scaler).await;
        api.clear();
        // later ticks make the base's 4 requests and spawn workers
        let mut spawned = 0;
        for _ in 0..9 {
            spawned += tick(&mut scaler).await;
        }
        let elapsed = start.elapsed();
        assert!(spawned > 0);
        assert_eq!(api.count("GET", "/api/system/stats"), 9);
        assert_eq!(api.count("GET", "/api/jobs/deadlines/"), 9);
        assert!(api.count("POST", "/api/system/worker/K8s") <= 9);
        assert!(api.count("DELETE", "/api/system/worker/K8s") <= 9);
        assert_eq!(api.count("GET", "/api/entities/"), 0);
        assert_eq!(api.count("GET", "/api/images"), 0);
        assert_eq!(api.count("POST", "/api/images"), 0);
        // the registered workers name their tool instance and no run
        for req in api.matching("POST", "/api/system/worker/K8s") {
            let list: WorkerRegistrationList = serde_json::from_slice(&req.body).unwrap();
            assert!(list.workers.iter().all(|worker| worker.run.is_none()));
        }
        println!("shared ticks took {elapsed:?}");
        // per-run jobs of the same tool instances get workers registered with their runs, each
        // fetching its run's token once and no models
        let (world, _) = build_world(&specs, &["io", "europa"], 100, true);
        let api = MockApi::start(world).await;
        let mut scaler = Box::pin(test_api::scaler(
            &api,
            test_api::conf(&crane),
            ImageScaler::K8s,
        ))
        .await;
        tick(&mut scaler).await;
        reload(&mut scaler).await;
        api.clear();
        let mut spawned = 0;
        for _ in 0..3 {
            spawned += tick(&mut scaler).await;
        }
        assert!(spawned > 0);
        let mut registered = 0;
        for req in api.matching("POST", "/api/system/worker/K8s") {
            let list: WorkerRegistrationList = serde_json::from_slice(&req.body).unwrap();
            assert!(list.workers.iter().all(|worker| worker.run.is_some()));
            registered += list.workers.len();
        }
        assert_eq!(registered, spawned);
        assert_eq!(api.count("GET", "/api/reactions/"), spawned);
        assert_eq!(
            api.matching("GET", "/api/reactions/")
                .iter()
                .filter(|req| req.path.ends_with("/run-models"))
                .count(),
            0
        );
    }

    /// A bare metal tool instance's pipeline job gets a shared worker registered with its tool
    /// instance on a node, and External tool instances are scheduled by no scaler while the scale
    /// loop keeps running
    #[tokio::test]
    async fn direct_and_external_workloads() {
        // a bare metal tool instance and an External one, each with a queued job
        let external = WorkloadSpec::default();
        let (world, ids) = build_world(&[bare_metal_spec(), external], &["io"], 2, false);
        let api = MockApi::start(world).await;
        let crane = MockCrane::new();
        let mut scaler = Box::pin(test_api::scaler(
            &api,
            test_api::conf(&crane),
            ImageScaler::BareMetal,
        ))
        .await;
        api.clear();
        // one tick registers a single bare metal worker for the bare metal tool instance
        let spawned = tick(&mut scaler).await;
        assert_eq!(spawned, 1);
        let registered = api.matching("POST", "/api/system/worker/BareMetal");
        assert_eq!(registered.len(), 1);
        let list: WorkerRegistrationList = serde_json::from_slice(&registered[0].body).unwrap();
        assert_eq!(list.workers.len(), 1);
        assert_eq!(list.workers[0].tool_instance, ids[0]);
        assert_eq!(list.workers[0].run, None);
        let raw = String::from_utf8_lossy(&registered[0].body);
        assert!(!raw.contains("pipeline") && !raw.contains("stage"), "{raw}");
        // a K8s scaler with the External job queued allocates nothing and keeps running
        let mut k8s = Box::pin(test_api::scaler(
            &api,
            test_api::conf(&crane),
            ImageScaler::K8s,
        ))
        .await;
        api.clear();
        for _ in 0..2 {
            assert_eq!(tick(&mut k8s).await, 0);
        }
        assert_eq!(api.count("POST", "/api/system/worker/"), 0);
    }

    /// The workload cache in steady state: over 100 ticks with 50 K8s tool instances in demand
    /// and two reloads, each spec is fetched once, images are only inspected inside reloads, and
    /// each tick reads the stats and the deadline window once
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn workload_cache_steady_state() {
        // 50 K8s tool instances with queued jobs
        let specs = (0..50)
            .map(|i| {
                k8s_spec(
                    &format!("registry.example/steady-{i}:1"),
                    SpawnLimits::Unlimited,
                )
            })
            .collect::<Vec<_>>();
        let (world, _) = build_world(&specs, &["io", "europa"], 500, false);
        let api = MockApi::start(world).await;
        let crane = MockCrane::new();
        let mut scaler = Box::pin(test_api::scaler(
            &api,
            test_api::conf(&crane),
            ImageScaler::K8s,
        ))
        .await;
        api.clear();
        let mut times = Vec::with_capacity(100);
        for i in 0..100 {
            // reload twice during the run
            if i == 30 || i == 70 {
                let before = crane.inspected().len();
                reload(&mut scaler).await;
                assert_eq!(crane.inspected().len() - before, 50);
            }
            // ticks never inspect images
            let before = crane.inspected().len();
            let start = Instant::now();
            tick(&mut scaler).await;
            times.push(start.elapsed());
            assert_eq!(crane.inspected().len(), before);
        }
        assert_eq!(api.count("GET", "/api/entities/"), 50);
        assert_eq!(crane.inspected().len(), 100);
        assert_eq!(api.count("GET", "/api/system/stats"), 100);
        assert_eq!(api.count("GET", "/api/jobs/deadlines/"), 100);
        let mean = times[1..].iter().sum::<std::time::Duration>() / 99;
        println!("mean tick after the first: {mean:?}");
    }

    /// An early cache reload is due only when requested and at most once per 30 seconds
    #[test]
    fn early_reloads_are_rate_limited() {
        let now = Utc::now();
        // a request with no earlier early reload is due
        assert!(early_reload_due(true, None, now));
        // nothing is due without a request
        assert!(!early_reload_due(false, None, now));
        // a request within 30 seconds of the last early reload waits
        assert!(!early_reload_due(
            true,
            Some(now - chrono::Duration::seconds(10)),
            now
        ));
        // a request 30 seconds after it is due
        assert!(early_reload_due(
            true,
            Some(now - chrono::Duration::seconds(30)),
            now
        ));
    }

    /// Workers are registered with their requisition's tool instance and run
    #[test]
    fn registrations_carry_tool_instance_and_run() {
        // a spawn for a per-run requisition
        let mut reqs = spawn_map(&[1]);
        let tool_instance = Uuid::new_v4();
        let run = Uuid::new_v4();
        for spawns in reqs.spawns.values_mut().flat_map(|map| map.values_mut()) {
            for spawn in spawns {
                spawn.req =
                    thorium::models::Requisition::new("io", GROUP, tool_instance, Some(run));
            }
        }
        // its registration names both
        let batches = registration_batches(&reqs);
        assert_eq!(batches[0].workers[0].tool_instance, tool_instance);
        assert_eq!(batches[0].workers[0].run, Some(run));
    }

    /// `BanSets::is_expired` clears a ban set only once its `expire` timestamp has passed: an
    /// expired ban set (expire in the past) is cleared, while a still-valid one (expire in the
    /// future) is retained until it expires.
    #[test]
    fn is_expired_clears_only_after_expiry() {
        // an expired ban set: expire timestamp is in the past
        let mut expired = BanSets::new();
        expired.expire = Utc::now() - chrono::Duration::seconds(60);
        // populate it with a banned group so we can observe whether it gets cleared
        expired.groups.insert("corn".to_string());
        // an expired ban set must be cleared
        expired.is_expired();
        assert!(
            expired.groups.is_empty(),
            "an expired ban set was not cleared"
        );

        // a still-valid ban set: expire timestamp is in the future
        let mut valid = BanSets::new();
        valid.expire = Utc::now() + chrono::Duration::seconds(600);
        // populate it with a banned group
        valid.groups.insert("corn".to_string());
        // a still-valid ban set must be retained (not cleared early)
        valid.is_expired();
        assert!(
            valid.groups.contains("corn"),
            "a still-valid ban set was cleared before it expired"
        );
    }

    /// When a new cache is pulled the stale bans are dropped before every scheduler repopulates
    /// the ban sets from it, so a ban added while syncing the new cache is kept.
    #[tokio::test]
    async fn resync_keeps_bans_from_new_cache() {
        // a ban set holding a ban derived from the previous cache
        let mut bans = BanSets::new();
        bans.groups.insert("stale".to_owned());
        // the sync against the new cache bans a group
        resync_bans(&mut bans, async |bans: &mut BanSets| {
            bans.groups.insert("new".to_owned());
            Ok(())
        })
        .await
        .unwrap();
        // the ban found in the new cache must be kept
        assert!(
            bans.groups.contains("new"),
            "bans derived from the new cache must not be cleared"
        );
    }

    /// Build a spawned deadline worker with a unique name
    ///
    /// # Arguments
    ///
    /// * `name` - The unique name of this worker
    fn spawned(name: String) -> Spawned {
        Spawned {
            req: thorium::models::Requisition::new("user", "group", Uuid::nil(), None),
            cluster: "cluster".to_owned(),
            node: "node".to_owned(),
            name,
            resources: thorium::models::Resources::default(),
            pool: thorium::models::Pools::Deadline,
            spawn: true,
            scaled_down: false,
            down_scalable: Utc::now(),
            run_env: None,
        }
    }

    /// Build a requisition map with groups of spawned workers in a single cluster
    ///
    /// # Arguments
    ///
    /// * `groups` - The number of workers in each deadline group
    fn spawn_map(groups: &[usize]) -> ReqMap {
        let mut reqs = ReqMap::default();
        let mut deadlines = BTreeMap::default();
        for (group, count) in groups.iter().enumerate() {
            // give each group its own deadline
            let deadline = Utc::now() + chrono::Duration::seconds(group as i64);
            let workers = (0..*count)
                .map(|i| spawned(format!("worker-{group}-{i}")))
                .collect();
            deadlines.insert(deadline, workers);
        }
        reqs.spawns.insert("cluster".to_owned(), deadlines);
        reqs
    }

    /// Each new worker is registered exactly once: 60 workers in three groups of 20 are split
    /// across batches without any worker being re-sent in a later batch, which keeps
    /// registration requests linear in the number of spawns.
    #[test]
    fn each_worker_registered_once() {
        // three deadline groups of 20 workers each
        let reqs = spawn_map(&[20, 20, 20]);
        // build the batches that would be submitted
        let batches = registration_batches(&reqs);
        // gather every registered worker name across all batches
        let names: Vec<&String> = batches
            .iter()
            .flat_map(|batch| batch.workers.iter().map(|worker| &worker.name))
            .collect();
        let unique: HashSet<&String> = names.iter().copied().collect();
        // every worker is registered and none of them more than once
        assert_eq!(unique.len(), 60);
        assert_eq!(
            names.len(),
            60,
            "each spawned worker must be registered exactly once"
        );
    }

    /// A small spawn wave is registered in a single batch.
    #[test]
    fn small_spawn_wave_is_registered_in_one_batch() {
        // a single group of 10 workers
        let reqs = spawn_map(&[10]);
        // build the batches that would be submitted
        let batches = registration_batches(&reqs);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].workers.len(), 10);
    }

    /// A cluster with pending deletes from an earlier loop but no new scale downs is still
    /// selected for `Scheduler::delete` (with no new scale downs), while clusters with new scale
    /// downs still get them. `Scheduler::delete` is the only place a scheduler validates its
    /// pending deletes, so this keeps those workers from staying registered in Thorium and in the
    /// allocator in the being deleted state until that cluster happens to get another scale down.
    #[test]
    fn pending_deletes_on_quiet_cluster_are_validated() {
        // cluster a has a new scale down this loop
        let mut scale_down = HashMap::default();
        scale_down.insert("a".to_owned(), vec![spawned("w1".to_owned())]);
        // cluster b has no new scale downs but its scheduler still has pending deletes
        let pending: HashSet<String> = ["b".to_owned()].into_iter().collect();
        // get the clusters we will call delete on
        let targets = delete_targets(scale_down, &pending);
        // the quiet cluster must still be called so it validates its pending deletes
        assert!(
            targets.contains_key("b"),
            "a cluster with pending deletes and no new scale downs must still be validated"
        );
        // it has no new scale downs to hand over
        assert!(targets["b"].is_empty());
        // and the cluster with new scale downs still gets them
        assert_eq!(targets["a"].len(), 1);
    }

    /// A cluster with neither new scale downs nor pending deletes is not called, and a cluster
    /// with only new scale downs keeps them.
    #[test]
    fn pending_deletes_on_quiet_cluster_idle_cluster_is_not_called() {
        // cluster a has a new scale down this loop
        let mut scale_down = HashMap::default();
        scale_down.insert("a".to_owned(), vec![spawned("w1".to_owned())]);
        // no cluster has pending deletes and cluster c is completely idle
        let targets = delete_targets(scale_down, &HashSet::default());
        // only the cluster with new scale downs is called
        assert_eq!(targets.len(), 1);
        assert!(!targets.contains_key("c"));
        assert_eq!(targets["a"][0].name, "w1");
    }
}
