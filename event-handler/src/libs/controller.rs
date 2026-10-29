//! The controller for handling events in Thorium
use kanal::AsyncSender;
use linearize::StaticMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use strum::IntoEnumIterator;
use thorium::models::{EventCacheStatus, EventCacheStatusOpts, ScrubbedUser};
use thorium::{Conf, Error, Thorium};
use tokio::signal::unix::{SignalKind, signal};
use tokio::task::JoinHandle;
use tracing::{Level, event};

use super::cache::TriggerCache;
use super::heartbeat::Heartbeat;
use super::identity;
use crate::args::Args;
use crate::libs::workers::{WorkerCacheUpdates, WorkerMsg};

use super::stats::EventWorkerStatChannels;
use super::workers::{
    EventWorkerKinds, GroupAllowances, SigmaRuleContext, WorkerCacheUpdateKinds, YaraRuleContext,
};

/// Get info on all users in Thorium
///
/// # Arguments
///
/// * `thorium` - a Thorium client
async fn get_users(thorium: &Thorium) -> Result<HashMap<String, ScrubbedUser>, Error> {
    // get all users in Thorium
    let list = thorium.users.list_details().await?;
    // create a user map preallocated for the number of users we found
    let mut users = HashMap::with_capacity(list.len());
    // convert this user list into a map of users by name
    for user in list {
        // add this user to our map
        users.insert(user.username.clone(), user);
    }
    // get all pipelines in Thorium so we can
    Ok(users)
}

/// Get the per group allowability snapshots for every group in Thorium
///
/// Group kill switches bind even admins so the yara hit writer pre-filters its
/// write groups against these snapshots instead of letting one disallowing
/// group 401 a whole write.
///
/// # Arguments
///
/// * `thorium` - a Thorium client
async fn get_allowances(thorium: &Thorium) -> Result<HashMap<String, GroupAllowances>, Error> {
    // track the allowances for every group
    let mut allowances = HashMap::default();
    // get a cursor over all groups we can see
    let mut cursor = thorium.groups.list().page_size(100).exec().await?;
    // crawl over the groups in this cursor
    loop {
        // snapshot each groups allowances
        for name in cursor.names.drain(..) {
            // get this groups details
            let group = thorium.groups.get(&name).await?;
            // snapshot the hit relevant allowances for this group
            allowances.insert(name, GroupAllowances::from(&group));
        }
        // check if this cursor has been exhausted
        if cursor.exhausted {
            break;
        }
        // get the next page of data
        cursor.next().await?;
    }
    Ok(allowances)
}

/// The shared cache/context for different workers
pub struct EventWorkerCache {
    /// A shared trigger cache
    pub triggers: TriggerCache,
    /// A shared sigma context
    pub sigma: SigmaRuleContext,
    /// The users we know about
    pub users: HashMap<String, ScrubbedUser>,
    /// The config for Thorium
    pub conf: Conf,
    /// A shared yara rule context
    pub yara: YaraRuleContext,
    /// The per group allowability snapshots for partitioning hit writes
    pub allowances: HashMap<String, GroupAllowances>,
    /// The cache generation counters as of our last rebuild check
    ///
    /// Generations are monotonic and never consumed on read so any number of
    /// replicas can poll them without stealing each others staleness info.
    last_gens: EventCacheStatus,
    /// When we last forced a full cache rebuild
    last_full: Instant,
    /// Whether a yara context rebuild is pending its debounce
    yara_pending: bool,
    /// When we last rebuilt the yara context and how long the compile took
    last_yara_build: Option<(Instant, Duration)>,
}

impl EventWorkerCache {
    /// Create a new cache
    ///
    /// # Arguments
    ///
    /// * `conf` - The Thorium config
    /// * `thorium` - A Thorium api client
    pub async fn new(conf: Conf, thorium: &Thorium) -> Result<Self, Error> {
        // snapshot the cache generations before the first build so any update
        // that lands mid build moves a generation our first poll will see
        let last_gens = thorium
            .events
            .get_cache_status(&EventCacheStatusOpts::default())
            .await?;
        // get our max depth
        let max_depth = conf.thorium.events.max_depth;
        // get a new trigger cache object
        let triggers = TriggerCache::new(thorium, max_depth).await?;
        // build a new sigma rule context
        let sigma = SigmaRuleContext::new(thorium).await?;
        // build a new yara rule context
        let yara = YaraRuleContext::new(thorium).await?;
        // get all of the users currently in Thorium
        let users = get_users(thorium).await?;
        // get the per group allowability snapshots
        let allowances = get_allowances(thorium).await?;
        // build our cache
        let cache = EventWorkerCache {
            triggers,
            sigma,
            users,
            conf,
            yara,
            allowances,
            last_gens,
            // the cache was just fully built so start the backstop timer now
            last_full: Instant::now(),
            yara_pending: false,
            last_yara_build: None,
        };
        Ok(cache)
    }

    /// How long between forced full cache rebuilds
    ///
    /// Generation bumps in redis can be lost (e.g. a crash between a scylla
    /// write and the bump) and an old API never moves them at all, so the
    /// controller periodically rebuilds every cache component as a backstop.
    const FULL_REBUILD_BACKSTOP: Duration = Duration::from_secs(600);

    /// Rebuild any stale parts of our cache
    ///
    /// Staleness is detected by comparing each components monotonic generation
    /// counter against the last one we saw; the counters are never consumed on
    /// read so one replicas poll can never leave its peers running stale. A
    /// change to one component (e.g. a new sigma rule) does not rebuild the
    /// others, and updates are only broadcast for components that were
    /// actually rebuilt. An old API without generation counters always reports
    /// zeros which never differ, so it degrades to the full rebuild backstop
    /// instead of busy rebuilding on every poll.
    ///
    /// # Arguments
    ///
    /// * `thorium` - A Thorium api client
    pub async fn rebuild_stale(
        &mut self,
        thorium: &Arc<Thorium>,
    ) -> Result<Vec<WorkerCacheUpdateKinds>, Error> {
        // track cache data if any was updated
        let mut updated = Vec::default();
        // read the cache status without consuming it so our peers see it too
        let opts = EventCacheStatusOpts::default();
        // check which cache components need to be updated
        let status = thorium.events.get_cache_status(&opts).await?;
        // periodically force a full rebuild in case a generation bump was lost
        let force = self.last_full.elapsed() >= Self::FULL_REBUILD_BACKSTOP;
        // check which generations moved since our last rebuild check
        let users_stale = status.users_gen != self.last_gens.users_gen;
        let triggers_stale = status.triggers_gen != self.last_gens.triggers_gen;
        let sigma_stale = status.sigma_rules_gen != self.last_gens.sigma_rules_gen;
        let yara_stale = status.yara_rules_gen != self.last_gens.yara_rules_gen;
        // rebuild our users map if its stale
        if force || users_stale {
            // get the users currently in Thorium
            self.users = get_users(thorium).await?;
            // tell subscribed workers their user map changed
            updated.push(WorkerCacheUpdateKinds::Users);
        }
        // rebuild our trigger cache if its stale; user changes also rebuild
        // this cache since it embeds the user map
        if force || triggers_stale || users_stale {
            // get a new trigger cache object
            self.triggers = TriggerCache::new(thorium, self.conf.thorium.events.max_depth).await?;
            // tell subscribed workers their trigger cache changed
            updated.push(WorkerCacheUpdateKinds::ReactionTriggers);
            // group mutations ride the triggers generation so refresh the
            // allowability snapshots alongside the trigger cache
            self.allowances = get_allowances(thorium).await?;
            // tell subscribed workers their allowances changed
            updated.push(WorkerCacheUpdateKinds::GroupAllowances);
        }
        // rebuild our sigma rule context if its stale
        if force || sigma_stale {
            // compile a new sigma rule context
            self.sigma = SigmaRuleContext::new(thorium).await?;
            // tell subscribed workers their sigma rules changed
            updated.push(WorkerCacheUpdateKinds::SigmaRulesContext);
        }
        // a moved yara generation marks a rebuild as pending; the rebuild
        // itself is debounced by the last compiles duration so rule churn
        // cannot stack overlapping multi second compiles
        if yara_stale {
            self.yara_pending = true;
        }
        // rebuild the yara context once any debounce window has passed
        let debounced = match self.last_yara_build {
            // wait at least the longer of 30s or the last compiles duration
            Some((at, took)) => at.elapsed() >= took.max(Duration::from_secs(30)),
            // the first rebuild never waits
            None => true,
        };
        if force || (self.yara_pending && debounced) {
            // time this compile so the next debounce can be sized by it
            let started = Instant::now();
            // compile a new yara rule context
            self.yara = YaraRuleContext::new(thorium).await?;
            // track when and how long this rebuild was
            self.last_yara_build = Some((Instant::now(), started.elapsed()));
            // this rebuild satisfied any pending flag
            self.yara_pending = false;
            // tell subscribed workers their yara rules changed
            updated.push(WorkerCacheUpdateKinds::YaraRulesContext);
        }
        // remember the generations this pass acted on; a bump that lands mid
        // rebuild yields a higher generation our next poll will catch
        self.last_gens = status;
        // track when we last forced a full rebuild
        if force {
            self.last_full = Instant::now();
        }
        Ok(updated)
    }
}

/// A channel and task join handle
type ChannelAndHandle = (AsyncSender<WorkerMsg>, JoinHandle<Result<(), Error>>);

/// A controller for handling different event workers in Thorium
pub struct EventWorkerController {
    /// A Thorium client
    thorium: Arc<Thorium>,
    /// The identity of this event handler process
    worker: String,
    /// The shared cache across different workers
    shared_cache: EventWorkerCache,
    /// The stats channels for different worker kinds
    stats_channels: EventWorkerStatChannels,
    /// The different stats reporter handles
    stats_handles: StaticMap<EventWorkerKinds, Option<JoinHandle<Result<(), Error>>>>,
    /// The different worker handles
    handles: StaticMap<EventWorkerKinds, Vec<ChannelAndHandle>>,
    /// The handle for this processes heartbeat task
    heartbeat: Option<JoinHandle<()>>,
}

impl EventWorkerController {
    /// Create a new controller
    ///
    /// # Arguments
    ///
    /// * `args` - The command line args passed to the event handler
    /// * `conf` - The Thorium Config
    pub async fn new(args: Args, conf: Conf) -> Result<Self, Error> {
        // build a Thorium client
        let thorium = Arc::new(Thorium::from_key_file(&args.auth).await?);
        // get this processes worker identity
        let worker = identity::worker_id().to_owned();
        // log the identity this process will own its events under
        event!(Level::INFO, msg = "Event handler identity", worker = worker);
        // build a new shared cache
        let shared_cache = EventWorkerCache::new(conf, &thorium).await?;
        // build a new controller
        let controller = EventWorkerController {
            thorium,
            worker,
            shared_cache,
            stats_channels: EventWorkerStatChannels::default(),
            stats_handles: StaticMap::default(),
            handles: StaticMap::default(),
            heartbeat: None,
        };
        Ok(controller)
    }

    /// Spawn a stat worker for each kind
    fn spawn_stats_worker(&mut self) {
        // spawn the stat worker for each event worker kind
        for kind in EventWorkerKinds::iter() {
            // spawn a starts worker for all valid worker kinds
            let handle = self.stats_channels.spawn_worker(
                kind,
                &self.thorium,
                &self.worker,
                &self.shared_cache.conf,
            );
            // keep track of this stats handle
            self.stats_handles[kind] = Some(handle);
        }
    }

    // check if our stats workers failed
    async fn check_stats(&mut self) -> Result<(), Error> {
        // check the stat worker for each event worker kind
        for kind in EventWorkerKinds::iter() {
            // get this worker kind stats worker handle
            match &self.stats_handles[kind] {
                Some(handle) => {
                    // check if this stats handler has finished
                    if handle.is_finished() {
                        // take this finished handle so it is only consumed once
                        if let Some(handle) = self.stats_handles[kind].take() {
                            // catch fire if our stats handler failed
                            handle.await??;
                        }
                        // a stats worker should never exit cleanly either
                        return Err(Error::new(format!("Stats worker for {kind} exited")));
                    }
                }
                None => return Err(Error::new(format!("Missing stats worker for {kind}"))),
            }
        }
        Ok(())
    }

    /// Respawn any missing workers
    async fn respawn(&mut self) -> Result<(), Error> {
        // for the time being only spawn a single worker of each type
        // TODO we should spawn multiple workers in the future
        for kind in EventWorkerKinds::iter() {
            // count how many workers we need to respawn
            // TODO actually support more then one worker
            let needed = 1 - self.handles[kind].len();
            // spawn the correct number of workers
            for _ in 0..needed {
                // spawn one of this worker kind under our identity
                let (msg_tx, handle) = kind
                    .spawn(
                        &self.shared_cache,
                        &self.stats_channels,
                        &self.thorium,
                        &self.worker,
                    )
                    .await;
                // keep track of this worker
                self.handles[kind].push((msg_tx, handle));
            }
        }
        Ok(())
    }

    /// Check and rebuld any stale cache
    ///
    /// This will also inform all workers of their new cache items
    async fn check_cache(&mut self) -> Result<(), Error> {
        // rebuild any parts of our cache that are stale
        let update_kinds = self.shared_cache.rebuild_stale(&self.thorium).await?;
        // step over the updates that occured
        for update_kind in update_kinds {
            // step over our worker kinds and see if they are subscribed to this update
            for (worker_kind, handles) in &self.handles {
                // determine if this worker is subscribed to this kind of update
                if worker_kind.is_subscribed(update_kind) {
                    // build the update message to send
                    let cache_update = WorkerCacheUpdates::new(&self.shared_cache, update_kind);
                    // wrap our update
                    let wrapped = WorkerMsg::Update(cache_update);
                    // send this update to all of this kind of workers
                    for (msg_tx, _) in handles {
                        // a dead worker has dropped its receiver and is handled
                        // by check_tasks right after this, so a failed send must
                        // only warn: a panic here would skip the shutdown drain
                        if let Err(error) = msg_tx.send(wrapped.clone()).await {
                            // warn that this worker could not receive its update
                            event!(
                                Level::WARN,
                                msg = "Failed to send a cache update to a worker",
                                error = error.to_string(),
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Check if any of our tasks have failed
    async fn check_tasks(&mut self) -> Result<(), Error> {
        // check all of the different worker kinds
        for kind in EventWorkerKinds::iter() {
            // track the handles that have finished
            let mut finished = Vec::default();
            // check all spawnwed tasks
            for (index, (_, handle)) in self.handles[kind].iter().enumerate() {
                // check if this handle has finished
                if handle.is_finished() {
                    // keep track of a handle that has finished
                    finished.push(index);
                }
            }
            // consume any finished handles; remove them before awaiting so a
            // fatal error can never leave a consumed handle behind for the
            // shutdown drain to poll again
            // traverse them in reverse to ensure our indexes remain valid
            for index in finished.into_iter().rev() {
                // remove this finished handle
                let (_, handle) = self.handles[kind].swap_remove(index);
                // TODO we can't just catch fire here since we have multiple worker kinds now
                handle.await??;
            }
        }
        Ok(())
    }

    /// Make sure our heartbeat task is still beating
    ///
    /// A process without heartbeats would eventually be presumed dead and have
    /// its in flight events reaped out from under it, so a dead heartbeat task
    /// is a fatal error: the process exits and k8s restarts the pod.
    fn check_heartbeat(&self) -> Result<(), Error> {
        // get our heartbeat tasks handle
        match &self.heartbeat {
            Some(handle) => {
                // a finished heartbeat task means this process is beatless
                if handle.is_finished() {
                    // log that our heartbeat task died
                    event!(
                        Level::ERROR,
                        msg = "Heartbeat task exited; restarting this process",
                        worker = self.worker,
                    );
                    // die so k8s restarts this pod instead of letting it be reaped
                    return Err(Error::new("Heartbeat task exited"));
                }
                Ok(())
            }
            // a controller without a heartbeat task is a bug
            None => Err(Error::new("Missing heartbeat task")),
        }
    }

    /// Spawn the heartbeat task that keeps this worker registered
    fn spawn_heartbeat(&mut self) {
        // build the heartbeat task for this workers identity
        let heartbeat = Heartbeat::new(&self.thorium, &self.worker, &self.shared_cache.conf);
        // spawn and track this heartbeat task
        self.heartbeat = Some(tokio::spawn(heartbeat.start()));
    }

    /// How long workers get to finish and clear their current page on shutdown
    ///
    /// Kubernetes grants 30 seconds of termination grace by default, so this
    /// quiesce budget leaves time for the drain call and process teardown.
    const QUIESCE_GRACE: Duration = Duration::from_secs(20);

    /// Stop every spawned task so nothing pops new events mid drain
    ///
    /// Workers are asked to stop and given a grace period to finish and clear
    /// their current page first: aborting a worker between processing a page
    /// and clearing it would leave already handled events in flight for the
    /// drain to replay. Only workers still mid page at the deadline are
    /// aborted, so a rolling update replays at most those in progress pages.
    async fn stop_tasks(&mut self) {
        // abort our heartbeat task first so it cannot re-register this worker
        if let Some(handle) = self.heartbeat.take() {
            // stop this heartbeat task
            handle.abort();
            // wait for this task to fully wind down
            let _ = handle.await;
        }
        // ask every event worker to stop cleanly after its current page so in
        // progress batches finish their clears instead of replaying post drain
        for kind in EventWorkerKinds::iter() {
            // ask each worker of this kind to stop
            for (msg_tx, _) in &self.handles[kind] {
                // a failed send just means this worker already exited
                let _ = msg_tx.send(WorkerMsg::Stop).await;
            }
        }
        // give every worker a shared deadline to finish its current page
        let deadline = Instant::now() + Self::QUIESCE_GRACE;
        // stop every event worker before draining so none of them can pop
        // drained events right back into our in flight keys
        for kind in EventWorkerKinds::iter() {
            // quiesce each worker of this kind
            for (_, mut handle) in self.handles[kind].drain(..) {
                // compute how much of the quiesce budget remains
                let remaining = deadline.saturating_duration_since(Instant::now());
                // wait for this worker to stop cleanly between pages
                if tokio::time::timeout(remaining, &mut handle).await.is_err() {
                    // warn that this worker is being aborted mid page
                    event!(
                        Level::WARN,
                        msg = "Aborting a worker still mid page at shutdown",
                        worker = self.worker,
                    );
                    // this worker is still mid page at the deadline so stop it
                    handle.abort();
                    // wait for this task to fully wind down
                    let _ = handle.await;
                }
            }
            // abort this kinds stats worker too
            if let Some(handle) = self.stats_handles[kind].take() {
                // stop this stats task
                handle.abort();
                // wait for this task to fully wind down
                let _ = handle.await;
            }
        }
    }

    /// Drain this workers in flight events back to the pending queue
    ///
    /// This is best effort: on failure (e.g. an old API without the route) the
    /// events are recovered later by the reaper instead.
    async fn drain(&self) {
        // try to drain all of our in flight events back to the pending queue
        match self.thorium.events.drain_worker(&self.worker).await {
            Ok(drained) => {
                // log how many in flight events were drained
                event!(
                    Level::INFO,
                    msg = "Drained in flight events",
                    worker = self.worker,
                    drained = drained,
                );
            }
            Err(error) => {
                // a failed drain just means the reaper recovers our events later
                event!(
                    Level::WARN,
                    msg = "Failed to drain in flight events",
                    worker = self.worker,
                    error = error.to_string(),
                );
            }
        }
    }

    /// Shut this controller down by stopping every task and draining
    async fn shutdown(&mut self) {
        // stop every task so nothing pops new events mid drain
        self.stop_tasks().await;
        // drain our in flight events back to the pending queue best effort
        self.drain().await;
    }

    /// Run a single control loop pass
    async fn single_pass(&mut self) -> Result<(), Error> {
        // check our cache status and rebuild anything that is stale; a failed
        // poll or rebuild must not kill the daemon since transient API errors
        // (rolling upgrades, auth blips) heal on a later pass or the backstop
        if let Err(error) = self.check_cache().await {
            // warn that this cache check failed and retry on the next pass
            event!(
                Level::WARN,
                msg = "Failed to check or rebuild the event cache",
                worker = self.worker,
                error = error.to_string(),
            );
        }
        // check if any of our tasks have failed
        self.check_tasks().await?;
        // check if our stats workers have failed
        self.check_stats().await?;
        // make sure our heartbeat task is still beating
        self.check_heartbeat()?;
        // sleep for 5 seconds
        tokio::time::sleep(Duration::from_secs(5)).await;
        Ok(())
    }

    /// Start handling events of all event types
    pub async fn start(mut self) -> Result<(), Error> {
        // spawn all of our stats workers
        self.spawn_stats_worker();
        // spawn all of our workers
        self.respawn().await?;
        // spawn the heartbeat task that keeps this worker registered
        self.spawn_heartbeat();
        // listen for sigterm so k8s pod shutdowns drain instead of strand events
        let mut sigterm = signal(SignalKind::terminate())
            .map_err(|error| Error::new(format!("Failed to listen for SIGTERM: {error}")))?;
        // loop forever checking for task failures or if we should update our trigger cache
        loop {
            // run a control pass unless this process is being terminated
            let pass = tokio::select! {
                // k8s is terminating this pod so stop looping and drain
                _ = sigterm.recv() => None,
                // run a single control loop pass
                result = self.single_pass() => Some(result),
            };
            // act on this pass outside the select so the unfinished branches
            // future no longer borrows this controller
            match pass {
                // a sigterm shuts down cleanly and exits 0 for k8s
                None => {
                    // log that this process was asked to terminate
                    event!(
                        Level::INFO,
                        msg = "SIGTERM received; draining in flight events",
                        worker = self.worker,
                    );
                    // stop every task and drain our in flight events
                    self.shutdown().await;
                    return Ok(());
                }
                // a fatal error still drains best effort before dying
                Some(Err(error)) => {
                    // stop every task and drain our in flight events
                    self.shutdown().await;
                    return Err(error);
                }
                // this pass was clean so keep looping
                Some(Ok(())) => (),
            }
        }
    }
}
