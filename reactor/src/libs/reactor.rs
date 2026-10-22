//! Handles spawning containers directly for windows nodes
//!
//! This support could likely be extended to linux k8s and baremetal nodes but
//! for k8s nodes would come at the cost of everthing k8s buys us.

use chrono::{DateTime, Utc};
use http::StatusCode;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};
use thorium::Thorium;
use thorium::models::{
    Arch, Component, ImageScaler, NodeGetParams, Os, Version, Worker, WorkerStatus,
};
use tokio::sync::Semaphore;
use tracing::{Level, error, event, instrument};
use uuid::Uuid;

use super::keys;
use super::launchers::{Launcher, WorkloadSource};
use super::tasks::{self, Tasks};
use crate::Error;
use crate::args::Args;

/// Adds a task back into our task queue at the right time
macro_rules! add_task {
    ($reactor:expr, $task:expr) => {{
        // get the datetime to start this task at
        let mut start = crate::from_now!($task.delay());
        // increase by 1 until we have found an open slot to start this job
        loop {
            // determine if a task already exists for this date
            if $reactor.tasks.get(&start).is_none() {
                break;
            }
            // increment start by 1 and try again
            start += Duration::from_secs(1);
        }
        $reactor.tasks.insert(start, $task)
    }};
}

/// Check if an operation failed and if so who we should ban
macro_rules! try_ban {
    ($operation:expr, $user:expr, $bans:expr) => {
        match $operation {
            Ok(user) => user,
            Err(error) => {
                // log this error
                event!(Level::ERROR, error = true, error_msg = error.to_string());
                // add this user to our ban set
                $bans.insert($user.to_owned());
                // skip to the next user
                continue;
            }
        }
    };
}

/// Get the users whose keys our shared workers need
///
/// Per-run workers use their run's keys, so a user whose only workers are per-run gets no keys
/// file holding their full token.
///
/// # Arguments
///
/// * `active` - The currently active workers
/// * `changes` - The workers being launched this loop
fn key_users<'a>(
    active: &'a HashMap<String, Worker>,
    changes: &'a HashMap<String, Worker>,
) -> HashSet<&'a String> {
    // collect the users of every shared worker
    active
        .values()
        .chain(changes.values())
        .filter(|worker| worker.run.is_none())
        .map(|worker| &worker.user)
        .collect()
}

/// Get the runs our workers serve
///
/// # Arguments
///
/// * `active` - The currently active workers
/// * `changes` - The workers being launched this loop
fn live_runs(active: &HashMap<String, Worker>, changes: &HashMap<String, Worker>) -> HashSet<Uuid> {
    // collect the run of every per-run worker
    active
        .values()
        .chain(changes.values())
        .filter_map(|worker| worker.run)
        .collect()
}

/// Write the keys of the runs our new per-run workers serve
///
/// Each run's token is read once per node, and the keys of a run that already has them are kept.
/// A run whose token can't be read drops only its workers from this loop's changes. A run with
/// no token (`404`) has ended, so its workers can never launch and are deleted to free what they
/// hold; the workers of a run whose token read failed otherwise are tried again on the next
/// poll.
///
/// # Arguments
///
/// * `source` - Where to read run tokens from
/// * `scaler` - The scaler this reactor's workers are registered under
/// * `host` - The Thorium API the tokens are for
/// * `base` - The base directory keys are written in
/// * `changes` - The workers being launched this loop
async fn setup_run_keys<S: WorkloadSource + ?Sized>(
    source: &S,
    scaler: ImageScaler,
    host: &str,
    base: &Path,
    changes: &mut HashMap<String, Worker>,
) {
    // get the group of each new run that has no keys yet
    let needed = changes
        .values()
        .filter_map(|worker| worker.run.map(|run| (run, worker.group.clone())))
        .filter(|(run, _)| !keys::run_path_in(base, run).exists())
        .collect::<BTreeMap<Uuid, String>>();
    // read each run's token once and write its keys
    let mut failed = HashSet::new();
    let mut ended = HashSet::new();
    for (run, group) in needed {
        // read this run's token, which an ended run no longer has
        let written = match source.run_token(&group, run).await {
            Ok(token) => keys::write(&keys::run_path_in(base, &run), host, &token).await,
            Err(Error::Thorium(error)) if error.status() == Some(StatusCode::NOT_FOUND) => {
                ended.insert(run);
                Err(Error::Thorium(error))
            }
            Err(error) => Err(error),
        };
        // skip this run's workers this loop if we couldn't write its keys
        if let Err(error) = written {
            event!(
                Level::ERROR,
                msg = "Failed to write run keys",
                run = run.to_string(),
                error = error.to_string()
            );
            failed.insert(run);
        }
    }
    // get the workers of ended runs
    let deletes = changes
        .values()
        .filter(|worker| worker.run.is_some_and(|run| ended.contains(&run)))
        .map(|worker| worker.name.clone())
        .collect::<Vec<String>>();
    // drop the workers of runs whose keys we couldn't write
    changes.retain(|_, worker| worker.run.is_none_or(|run| !failed.contains(&run)));
    // delete the workers of ended runs, which are tried again next poll if this fails
    if !deletes.is_empty()
        && let Err(error) = source.delete_workers(scaler, deletes).await
    {
        event!(
            Level::ERROR,
            msg = "Failed to delete the workers of ended runs",
            error = error.to_string()
        );
    }
}

/// The daemon that will monitor this nodes worker and spawn them
pub struct Reactor<L: Launcher> {
    /// The client used to talk to Thorium
    pub thorium: Arc<Thorium>,
    /// The name of the cluster this node is in
    pub cluster: String,
    /// The name of this node
    pub name: String,
    /// A map of currently active workers on this node
    pub active: HashMap<String, Worker>,
    /// A queue of tasks to complete sorted by the time to start executing them
    tasks: BTreeMap<DateTime<Utc>, Tasks>,
    /// Allows the agent to poll the system for info
    system: System,
    /// The launcher to use when launching jobs
    launcher: Arc<L>,
    /// The args used to start this reactor
    args: Args,
    /// Stop spawning new agents as an update is needed
    halt_spawning: bool,
    /// shutdown this reactor and exit
    shutdown: bool,
}

impl<L: Launcher> Reactor<L> {
    /// Create a new reactor
    ///
    /// # Arguments
    ///
    /// * `args` - The args passed to this reactor
    #[instrument(name = "Reactor::new", skip_all)]
    pub async fn new(args: Args, launcher: L) -> Result<Self, Error> {
        // create a new Thorium client
        let thorium = Thorium::from_key_file(&args.keys).await?;
        // get this nodes name
        let name = args.node()?;
        // setup our task queue
        let tasks = Tasks::setup_queue(&args);
        // configure our system poller to listen to specific info
        let refresh = RefreshKind::default()
            .with_cpu(CpuRefreshKind::everything())
            .with_memory(MemoryRefreshKind::everything());
        // setup a system poller
        let system = System::new_with_specifics(refresh);
        // build our reactor
        let reactor = Reactor {
            thorium: Arc::new(thorium),
            cluster: args.cluster.clone(),
            name,
            active: HashMap::default(),
            tasks,
            system,
            launcher: Arc::new(launcher),
            args,
            halt_spawning: false,
            shutdown: false,
        };
        Ok(reactor)
    }

    /// Check if we need to spawn and execute any tasks
    #[instrument(name = "Reactor::spawn_tasks", skip_all)]
    async fn spawn_tasks(&mut self) -> Result<(), Error> {
        // get the current timestamp
        let now = Utc::now();
        // track the tasks we completed
        let mut completed = Vec::default();
        // get any tasks we want to spawn and build a list of completed blocking tasks to rerun again
        let tasks_due: Vec<_> = self.tasks.extract_if(.., |time, _| time < &now).collect();
        for (_, task) in tasks_due {
            // log that we are spawning a task
            event!(Level::INFO, task = task.as_str());
            // spawn or execute this task
            match task {
                Tasks::Update => self.check_update().await?,
                Tasks::Resources => {
                    tasks::update_resources(
                        &self.cluster,
                        &self.name,
                        &self.thorium,
                        &mut self.system,
                    )
                    .await?;
                }
            }
            completed.push(task);
        }
        // add any blocking completed tasks back to our task list
        for task in completed {
            add_task!(self, task);
        }
        Ok(())
    }

    /// Check if we need to spawn any new workers
    ///
    /// Returns a map of the new workers we need to spawn
    #[instrument(name = "Reactor::poll", skip_all)]
    async fn poll(&mut self) -> Result<HashMap<String, Worker>, Error> {
        // use default params for this node
        let params = NodeGetParams::default().scaler(self.args.scaler);
        // get the current desired state for this node
        let mut info = self
            .thorium
            .system
            .get_node(&self.cluster, &self.name, &params)
            .await?;
        // get a mutable reference to the launcher from its Arc;
        // the only thing using this Arc was the launches themselves which are complete,
        // so this should never error
        let launcher_mut = Arc::get_mut(&mut self.launcher)
            .expect("Launcher Arc cannot be mutated because it is in use elsewhere");
        // reconcile the current worker state in the reactor and the API
        launcher_mut
            .reconcile(&self.thorium, &mut info, &mut self.active)
            .await?;
        // extract any workers that the API has registered for shutdown
        let (mut workers, shutdowns): (HashMap<String, Worker>, HashMap<String, Worker>) = info
            .workers
            .into_iter()
            .partition(|(_, worker)| worker.status != WorkerStatus::Shutdown);
        // shutdown those workers
        if !shutdowns.is_empty() {
            // downselect to just our workers names
            let names = shutdowns.into_keys().collect();
            launcher_mut.shutdown(&self.thorium, names).await?;
        }
        // compare to our currently active workers and determine what needs to be spawned still
        workers.retain(|name, _| !self.active.contains_key(name));
        // log how many changes are needed if any are
        if !workers.is_empty() {
            event!(Level::INFO, changes = workers.len());
        }
        Ok(workers)
    }

    /// Make sure that the the keys for our target workers are loaded
    ///
    /// Shared workers use their user's keys and per-run workers their run's keys. The keys of
    /// runs no worker on this node serves anymore are deleted.
    ///
    /// # Arguments
    ///
    /// * `changes` - The changes to the current workers to apply this loop
    #[instrument(name = "Reactor::setup_keys", skip_all)]
    async fn setup_keys(&mut self, changes: &mut HashMap<String, Worker>) {
        // get the base directory our keys are written in
        let base = keys::base_path();
        // get the users of our shared workers to write keys for
        let users = key_users(&self.active, changes)
            .into_iter()
            .cloned()
            .collect::<Vec<String>>();
        // track the users we should ban
        let mut bans: HashSet<String> = HashSet::default();
        // try to setup all of our users tokens
        for name in users {
            // get this users info
            let user = try_ban!(self.thorium.users.get(&name).await, name, bans);
            // write this users keys if they aren't current
            let write = keys::ensure_user(&base, &self.thorium.host, &user.username, &user.token);
            try_ban!(write.await, name, bans);
        }
        // drop any shared workers that we failed to setup keys for
        changes.retain(|_, worker| worker.run.is_some() || !bans.contains(&worker.user));
        // write the keys of any new runs
        setup_run_keys(
            self.thorium.as_ref(),
            self.args.scaler,
            &self.thorium.host,
            &base,
            changes,
        )
        .await;
        // delete the keys of runs nothing on this node serves anymore
        if let Err(error) = keys::sweep_runs(&base, &live_runs(&self.active, changes)).await {
            event!(
                Level::ERROR,
                msg = "Failed to delete run keys",
                error = error.to_string()
            );
        }
    }

    /// Launch all of our jobs
    ///
    /// # Arguments
    ///
    /// * `new` - The new workers to launch
    #[instrument(name = "Reactor::launch", skip_all, fields(workers = new.len()))]
    async fn launch(&mut self, new: HashMap<String, Worker>) {
        // only launch new workers if spawning hasn't been halted
        if !self.halt_spawning {
            let mut join_set = tokio::task::JoinSet::new();
            // only launch up to 10 workers at a time
            let semaphore = Arc::new(Semaphore::new(10));
            for (name, worker) in new {
                event!(Level::DEBUG, msg = "Launching worker", worker = name);
                let semaphore = semaphore.clone();
                let launcher = self.launcher.clone();
                let thorium = self.thorium.clone();
                join_set.spawn(async move {
                    // clone the worker's name in case we error out
                    let worker_name = worker.name.clone();
                    // get a permit from the semaphore
                    match semaphore.acquire().await {
                        Ok(_permit) => launcher
                            .launch(&thorium, worker)
                            .await
                            .map_err(|err| (worker_name, err)),
                        Err(err) => Err((
                            worker_name,
                            Error::with_context("Error acquiring semaphore permit", err),
                        )),
                    }
                });
            }
            // log errors and unzip workers and launches into separate collections
            let (workers, launches): (Vec<_>, Vec<_>) = join_set
                .join_all()
                .await
                .into_iter()
                // log all errors and only get the successes
                .filter_map(|res| match res {
                    Ok(success) => Some(success),
                    Err((worker_name, err)) => {
                        error!(
                            msg = "Error launching worker",
                            worker = worker_name,
                            err = ?err
                        );
                        None
                    }
                })
                .unzip();
            // zip worker *references* and launches back up for the to launcher resolve
            let launches = workers.iter().zip(launches.into_iter()).collect();
            // get a mutable reference to the launcher from its Arc;
            // the only thing using this Arc was the launches themselves which are complete,
            // so this should never error
            let launcher_mut = Arc::get_mut(&mut self.launcher)
                .expect("Launcher Arc cannot be mutated because it is in use elsewhere");
            launcher_mut.resolve_launches(launches);
            // add the active workers to our active map
            self.active.extend(
                workers
                    .into_iter()
                    .map(|worker| (worker.name.clone(), worker)),
            );
        }
    }

    /// Check if we need an update or not and apply it if possible
    ///
    /// If an update is needed, the reactor will first update the Linux and Windows agents,
    /// then update itself and mark itself for shutdown. The reactor must be deployed to
    /// auto-restart (systemd service, K8s, etc.) in order to come back after update.
    #[instrument(name = "Reactor::check_update", skip_all, err(Debug))]
    async fn check_update(&mut self) -> Result<(), Error> {
        // Get the current Thorium version
        let version = self.thorium.updates.get_version().await?;
        // check if the agents need updating
        self.check_update_agents(&version).await?;
        if !self.args.skip_self_update {
            // get the current version
            let current = env!("CARGO_PKG_VERSION");
            // compare to our version and see if its different
            if version.thorium != semver::Version::parse(current)? {
                // set the halt spawning flag so we stop spawning new agents
                self.halt_spawning = true;
                // we need to update the reactor log the version difference
                event!(
                    Level::WARN,
                    reactor = current,
                    api = version.thorium.to_string(),
                    update_needed = true,
                );
                // update ourselves
                self.thorium.updates.update(Component::Reactor).await?;
                // shutdown this reactor; the reactor must deployed with some mechanism to auto-restart
                // in order to continue after update
                self.shutdown = true;
            }
        }
        Ok(())
    }

    /// Check if the agents need an update and apply if no jobs are running
    ///
    /// # Arguments
    ///
    /// * `api_version` - The version returned from the API
    #[instrument(skip_all, fields(api_version = api_version.thorium.to_string()))]
    async fn check_update_agents(&mut self, api_version: &Version) -> Result<(), Error> {
        let agent_path = PathBuf::from(&self.args.agents_dir).join("thorium-agent");
        // check the version of the Linux thorium agent
        let output = tokio::process::Command::new(&agent_path)
            .arg("--version")
            .output()
            .await
            .map_err(|err| {
                Error::with_context(
                    format!(
                        "Error checking version of Thorium agent at '{}'",
                        agent_path.display()
                    ),
                    err,
                )
            })?;
        // make sure the command exited cleanly
        if !output.status.success() {
            // form an error message, either stderr, the exit code, or an unknown error
            let err_msg = (!output.stderr.is_empty())
                .then(|| String::from_utf8(output.stderr).ok())
                .flatten()
                .or_else(|| {
                    output
                        .status
                        .code()
                        .map(|exit_code| format!("exit code {exit_code}"))
                })
                .unwrap_or("An unknown error occurred".to_string());
            return Err(Error::new(format!(
                "Checking for Thorium agent version exited in error: {err_msg}"
            )));
        }
        // get a string from the output
        let version_stdout = String::from_utf8(output.stdout).map_err(|err| {
            Error::with_context(
                "Error checking Thorium agent version: agent command output is not valid UTF-8",
                err,
            )
        })?;
        // extract just the version from the output, which should be the last word
        let version_raw = version_stdout.trim().rsplit(' ').next().ok_or(Error::new(
            "Error checking for Thorium agent version: agent command output is empty!",
        ))?;
        let version = semver::Version::parse(version_raw)
            .map_err(|err| {
                Error::with_context(
                    format!("Error checking Thorium agent version: agent output '{version_raw}' is not a valid semver version"),
                    err
                )
            })?;
        if version != api_version.thorium {
            // we need to update the agents; log the version difference
            event!(
                Level::INFO,
                agent = version_raw,
                api = api_version.thorium.to_string(),
                update_needed = true,
            );
            // update our agents
            self.thorium
                .updates
                .update_specific(
                    Os::Linux,
                    Arch::X86_64,
                    Component::Agent,
                    format!("{}/thorium-agent", self.args.agents_dir),
                )
                .await?;
            // assume the Windows agent also needs updating
            self.thorium
                .updates
                .update_specific(
                    Os::Windows,
                    Arch::X86_64,
                    Component::Agent,
                    format!("{}/thorium-agent.exe", self.args.agents_dir),
                )
                .await?;
        }
        Ok(())
    }

    /// Start polling Thorium for changes to apply to this node
    pub async fn start(mut self) -> Result<(), Error> {
        if !self.args.skip_update {
            // apply any needed updates
            self.check_update().await?;
        }
        // update this nodes resource info
        tasks::update_resources(&self.cluster, &self.name, &self.thorium, &mut self.system).await?;
        let dwell = std::time::Duration::from_secs(self.args.dwell_time.into());
        // loop forever getting the desired state and applying it
        loop {
            // check if we have any tasks that need to be spawned
            self.spawn_tasks().await?;
            // check for changes in this node
            let mut changes = self.poll().await?;
            // make sure our users keys are setup
            self.setup_keys(&mut changes).await;
            // spawn our changes
            self.launch(changes).await;
            // shutdown this reactor if needed
            if self.shutdown {
                break;
            }
            // sleep for the configured dwell between scale attempts
            tokio::time::sleep(dwell).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libs::launchers::fake::FakeWorkloadSource;
    use clap::Parser;
    use thorium::models::{Node, Pools, Resources};
    use uuid::Uuid;

    /// A launcher that reads the image of every `bare-` worker and launches every other worker
    struct FakeLauncher;

    #[async_trait::async_trait]
    impl Launcher for FakeLauncher {
        type LaunchedWorkerData = ();

        /// Read a bare metal worker's image, or launch any other worker
        ///
        /// # Arguments
        ///
        /// * `thorium` - A Thorium client
        /// * `worker` - The worker to launch
        async fn launch(&self, thorium: &Thorium, worker: Worker) -> Result<(Worker, ()), Error> {
            // bare metal workers read their image like the real launcher
            if worker.name.starts_with("bare-") {
                thorium
                    .workload_image(&worker, ImageScaler::BareMetal)
                    .await?;
            }
            Ok((worker, ()))
        }

        /// Nothing to resolve
        ///
        /// # Arguments
        ///
        /// * `_launches` - The launched workers
        fn resolve_launches(&mut self, _launches: Vec<(&Worker, ())>) {}

        /// Nothing to reconcile
        ///
        /// # Arguments
        ///
        /// * `_thorium` - A Thorium client
        /// * `_info` - The node's info
        /// * `_active` - The active workers
        async fn reconcile(
            &mut self,
            _thorium: &Thorium,
            _info: &mut Node,
            _active: &mut HashMap<String, Worker>,
        ) -> Result<(), Error> {
            Ok(())
        }

        /// Nothing to shut down
        ///
        /// # Arguments
        ///
        /// * `_thorium` - A Thorium client
        /// * `_workers` - The workers to shut down
        async fn shutdown(
            &mut self,
            _thorium: &Thorium,
            _workers: HashSet<String>,
        ) -> Result<(), Error> {
            Ok(())
        }
    }

    /// Build a unique temporary keys directory
    fn keys_dir() -> PathBuf {
        // build a directory no other test uses
        let dir = std::env::temp_dir().join(format!("reactor-keys-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Get the mode bits of a file
    ///
    /// # Arguments
    ///
    /// * `path` - The file to get the mode of
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// Write a value as evidence if an evidence directory is set
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the evidence file
    /// * `value` - The value to write
    fn evidence<T: serde::Serialize>(name: &str, value: &T) {
        // only write evidence when a directory is set
        if let Ok(dir) = std::env::var("THORIUM_EVIDENCE_DIR") {
            let dir = Path::new(&dir).join("reactor-run-keys");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(name), serde_json::to_string_pretty(value).unwrap()).unwrap();
        }
    }

    /// List every file under a directory with its mode
    ///
    /// # Arguments
    ///
    /// * `base` - The directory to list
    fn listing(base: &Path) -> Vec<String> {
        // walk the directory
        let mut files = Vec::new();
        let mut stack = vec![base.to_path_buf()];
        while let Some(path) = stack.pop() {
            if path.is_dir() {
                stack.extend(
                    std::fs::read_dir(&path)
                        .unwrap()
                        .map(|entry| entry.unwrap().path()),
                );
            } else {
                let relative = path.strip_prefix(base).unwrap().display().to_string();
                files.push(format!("{:o} {relative}", mode(&path)));
            }
        }
        files.sort();
        files
    }

    /// A new per-run worker gets its run's keys read once per run and written in the keys file
    /// format only its owner can read, users with only per-run workers get no keys, and user keys
    /// are only readable by their owner too
    #[tokio::test]
    async fn run_keys_written_once_per_run() {
        // a source with a token for one run
        let base = keys_dir();
        let run = Uuid::new_v4();
        let mut source = FakeWorkloadSource::default();
        source.run_tokens.insert(run, "run-token-1".to_owned());
        // two new workers of the run, a per-run worker of another user, and a shared worker
        let mut first = worker("run-first");
        first.run = Some(run);
        let mut second = worker("run-second");
        second.run = Some(run);
        let mut europa = worker("run-europa");
        europa.user = "europa".to_owned();
        europa.run = Some(run);
        let shared = worker("shared-io");
        let mut changes = HashMap::new();
        for worker in [first, second, europa, shared] {
            changes.insert(worker.name.clone(), worker);
        }
        // only the shared worker's user gets user keys
        let active = HashMap::new();
        let users = key_users(&active, &changes);
        assert_eq!(
            users.into_iter().cloned().collect::<Vec<_>>(),
            vec!["io".to_owned()]
        );
        // the run's token is read once and written in the keys file format
        setup_run_keys(
            &source,
            ImageScaler::BareMetal,
            "http://thorium.test",
            &base,
            &mut changes,
        )
        .await;
        assert_eq!(source.token_reads(), vec![("ceres".to_owned(), run)]);
        assert_eq!(changes.len(), 4);
        let path = keys::run_path_in(&base, &run);
        let expected = serde_norway::to_string(&thorium::Keys::new_token(
            "http://thorium.test",
            "run-token-1",
        ))
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
        assert_eq!(mode(&path), 0o600);
        // a later worker of the run reads no token again
        let mut later = worker("run-later");
        later.run = Some(run);
        let mut changes = HashMap::new();
        changes.insert(later.name.clone(), later);
        setup_run_keys(
            &source,
            ImageScaler::BareMetal,
            "http://thorium.test",
            &base,
            &mut changes,
        )
        .await;
        assert_eq!(source.token_reads().len(), 1);
        // a user's keys are only readable by their owner, even if an older reactor wrote them
        keys::ensure_user(&base, "http://thorium.test", "io", "user-token")
            .await
            .unwrap();
        let user_path = keys::path_in(&base, "io");
        assert_eq!(mode(&user_path), 0o600);
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&user_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        keys::ensure_user(&base, "http://thorium.test", "io", "user-token")
            .await
            .unwrap();
        assert_eq!(mode(&user_path), 0o600);
        evidence("token-reads.json", &source.token_reads());
        evidence("written.json", &listing(&base));
        // remove our keys directory
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A run whose token can't be read drops only its workers from this poll, writes no keys,
    /// and bans no user
    #[tokio::test]
    async fn missing_run_token_drops_only_its_workers() {
        // a source with no token for an ended run
        let base = keys_dir();
        let ended = Uuid::new_v4();
        let source = FakeWorkloadSource::default();
        let mut gone = worker("run-gone");
        gone.run = Some(ended);
        let shared = worker("shared-io");
        let mut changes = HashMap::new();
        changes.insert(gone.name.clone(), gone);
        changes.insert(shared.name.clone(), shared);
        // only the ended run's worker is dropped and nothing is written
        setup_run_keys(
            &source,
            ImageScaler::BareMetal,
            "http://thorium.test",
            &base,
            &mut changes,
        )
        .await;
        assert_eq!(source.token_reads(), vec![("ceres".to_owned(), ended)]);
        assert!(changes.contains_key("shared-io"));
        assert!(!changes.contains_key("run-gone"));
        assert!(!keys::run_path_in(&base, &ended).exists());
        evidence("missing-token-reads.json", &source.token_reads());
        // remove our keys directory
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A per-run worker whose run ended before it launched is deleted on the first poll so it
    /// frees its resources, and its run's token isn't read again; a run whose token read fails
    /// for another reason keeps its workers registered and is retried
    #[tokio::test]
    async fn ended_run_workers_are_deleted() {
        // a source with no token for an ended run and a failing token read for another run
        let base = keys_dir();
        let (ended, flaky) = (Uuid::new_v4(), Uuid::new_v4());
        let mut source = FakeWorkloadSource::default();
        source.failing_runs.insert(flaky);
        let mut gone = worker("run-gone");
        gone.run = Some(ended);
        let mut retried = worker("run-retried");
        retried.run = Some(flaky);
        let mut registered: HashMap<String, Worker> = HashMap::new();
        registered.insert(gone.name.clone(), gone);
        registered.insert(retried.name.clone(), retried);
        // poll three times with the workers the API still lists
        for _ in 0..3 {
            let deleted = source
                .deleted()
                .into_iter()
                .map(|(_, name)| name)
                .collect::<HashSet<String>>();
            registered.retain(|name, _| !deleted.contains(name));
            let mut changes = registered.clone();
            setup_run_keys(
                &source,
                ImageScaler::BareMetal,
                "http://thorium.test",
                &base,
                &mut changes,
            )
            .await;
            // neither worker launches
            assert!(changes.is_empty());
        }
        // the ended run's worker was deleted once under our scaler and its token read once
        assert_eq!(
            source.deleted(),
            vec![(ImageScaler::BareMetal, "run-gone".to_owned())]
        );
        assert!(!registered.contains_key("run-gone"));
        let reads = source.token_reads();
        assert_eq!(reads.iter().filter(|(_, run)| *run == ended).count(), 1);
        // the other run's worker is still registered and read on every poll
        assert!(registered.contains_key("run-retried"));
        assert_eq!(reads.iter().filter(|(_, run)| *run == flaky).count(), 3);
        // remove our keys directory
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// The keys of a run are deleted with their copies once no worker serves the run, leftover
    /// keys from an earlier reactor are deleted on the first poll, and other runs keep theirs
    #[tokio::test]
    async fn run_keys_deleted_after_last_worker() {
        // keys for a finished run with an agent copy, a live run, and a leftover run
        let base = keys_dir();
        let (finished, live, leftover) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        for run in [finished, live, leftover] {
            keys::write(&keys::run_path_in(&base, &run), "http://thorium.test", "t")
                .await
                .unwrap();
        }
        let copy = keys::uid_copy(&keys::run_path_in(&base, &finished), 65534);
        keys::write(&copy, "http://thorium.test", "t")
            .await
            .unwrap();
        keys::ensure_user(&base, "http://thorium.test", "io", "user-token")
            .await
            .unwrap();
        // only the live run's worker is still active
        let mut active = HashMap::new();
        let mut serving = worker("run-live");
        serving.run = Some(live);
        active.insert(serving.name.clone(), serving);
        let before = listing(&base);
        keys::sweep_runs(&base, &live_runs(&active, &HashMap::new()))
            .await
            .unwrap();
        // the finished and leftover runs' keys are gone and the rest stay
        assert!(!keys::runs_dir_in(&base).join(finished.to_string()).exists());
        assert!(!keys::runs_dir_in(&base).join(leftover.to_string()).exists());
        assert!(keys::run_path_in(&base, &live).exists());
        assert!(keys::path_in(&base, "io").exists());
        evidence(
            "sweep.json",
            &serde_json::json!({"before": before, "after": listing(&base)}),
        );
        // remove our keys directory
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// The keys of a run whose first worker on this node launches in this poll survive the
    /// sweep that runs before the launch
    #[tokio::test]
    async fn run_keys_kept_for_workers_launching_this_poll() {
        // a source with a token for a new run, an active shared worker, and leftover run keys
        let base = keys_dir();
        let (new_run, leftover) = (Uuid::new_v4(), Uuid::new_v4());
        let mut source = FakeWorkloadSource::default();
        source.run_tokens.insert(new_run, "run-token-1".to_owned());
        keys::write(
            &keys::run_path_in(&base, &leftover),
            "http://thorium.test",
            "t",
        )
        .await
        .unwrap();
        let mut active = HashMap::new();
        let shared = worker("shared-io");
        active.insert(shared.name.clone(), shared);
        // the run's first worker is in this poll's changes
        let mut first = worker("run-first");
        first.run = Some(new_run);
        let mut changes = HashMap::new();
        changes.insert(first.name.clone(), first);
        // write the new run's keys then sweep as a poll does
        setup_run_keys(
            &source,
            ImageScaler::BareMetal,
            "http://thorium.test",
            &base,
            &mut changes,
        )
        .await;
        assert!(changes.contains_key("run-first"));
        keys::sweep_runs(&base, &live_runs(&active, &changes))
            .await
            .unwrap();
        // the new run keeps its keys for the launch and only the leftover run's are gone
        assert!(keys::run_path_in(&base, &new_run).exists());
        assert!(!keys::runs_dir_in(&base).join(leftover.to_string()).exists());
        // remove our keys directory
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// Build a worker registered on this node
    ///
    /// # Arguments
    ///
    /// * `name` - The worker's name
    fn worker(name: &str) -> Worker {
        Worker {
            cluster: "cluster0".to_owned(),
            node: "node0".to_owned(),
            scaler: ImageScaler::BareMetal,
            name: name.to_owned(),
            user: "io".to_owned(),
            group: "ceres".to_owned(),
            tool_instance: Uuid::new_v4(),
            run: None,
            status: WorkerStatus::Spawning,
            spawned: Utc::now(),
            heart_beat: None,
            resources: Resources::default(),
            pool: Pools::Deadline,
            active: None,
        }
    }

    /// A worker whose tool instance can't be read fails to launch with an error naming it, while
    /// the reactor keeps launching its other workers
    #[tokio::test]
    async fn unlaunchable_workers_are_skipped() {
        // a reactor with a fake launcher and a client that is never called
        let thorium = Thorium::build("http://127.0.0.1:9")
            .token("t")
            .build()
            .await
            .unwrap();
        let args = Args::parse_from([
            "thorium-reactor",
            "--scaler",
            "bare-metal",
            "--cluster",
            "cluster0",
            "bare-metal",
        ]);
        let mut reactor = Reactor {
            thorium: Arc::new(thorium),
            cluster: "cluster0".to_owned(),
            name: "node0".to_owned(),
            active: HashMap::default(),
            tasks: BTreeMap::default(),
            system: System::new(),
            launcher: Arc::new(FakeLauncher),
            args,
            halt_spawning: false,
            shutdown: false,
        };
        // reading the image from an unreachable API errors with the worker's name and tool
        // instance
        let bare = worker("bare-callisto");
        let err = reactor
            .thorium
            .workload_image(&bare, ImageScaler::BareMetal)
            .await
            .unwrap_err();
        assert!(err.msg().contains("bare-callisto"), "{err}");
        assert!(err.msg().contains(&bare.tool_instance.to_string()), "{err}");
        // launching it and another worker activates only the other one
        let mut new = HashMap::new();
        new.insert(bare.name.clone(), bare);
        new.insert("other-callisto".to_owned(), worker("other-callisto"));
        reactor.launch(new).await;
        assert!(reactor.active.contains_key("other-callisto"));
        assert!(!reactor.active.contains_key("bare-callisto"));
        // a later launch still works
        let mut later = HashMap::new();
        later.insert("later-callisto".to_owned(), worker("later-callisto"));
        reactor.launch(later).await;
        assert!(reactor.active.contains_key("later-callisto"));
    }
}
