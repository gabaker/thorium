//! Scrub a backup for bitrot

use async_walkdir::Filtering;
use async_walkdir::WalkDir;
use futures::stream::FuturesUnordered;
use futures::stream::StreamExt;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use kanal::{AsyncReceiver, AsyncSender};
use std::path::PathBuf;
use tokio::task::JoinHandle;

use crate::Error;
use crate::args::{BackupComponents, ScrubBackup};
use crate::backup::failures::{FailureLog, FailurePolicy, FailureRecord, Operation};
use crate::backup::sink::{SinkFactory, TokioFs};
use crate::backup::tables::{
    Association, Comment, Commitish, CommitishList, Entity, NetworkPolicy, Node, Notification,
    Output, OutputStream, RepoData, RepoList, S3Id, SamplesList, Tag,
};
use crate::backup::{Monitor, MonitorUpdate, Scrub, ScrubWorker};
use crate::outcome::RunOutcome;
use crate::shared::progress::bars;
use crate::shared::workers::{WorkerHandle, join_workers, spawn_worker};
use std::sync::Arc;
use tracing::{info, warn};

/// A controller for a single table to scrub
pub struct TableScrub<S: Scrub> {
    /// The component this table is scrubbed as
    component: BackupComponents,
    /// The kanal channel workers should send kanal channel updates over
    updates_tx: AsyncSender<MonitorUpdate>,
    /// The kanal channel to receive archive map updates on
    updates_rx: AsyncReceiver<MonitorUpdate>,
    /// The kanal channel to send orders to workers on
    orders_tx: AsyncSender<PathBuf>,
    /// The kanal channel workers should get orders on
    orders_rx: AsyncReceiver<PathBuf>,
    /// A progress bar for this tables scrub
    progress: MultiProgress,
    /// The number of workers to when scrubbing data
    worker_count: usize,
    /// The currently active workers
    active: FuturesUnordered<WorkerHandle<ScrubWorker<S>, PathBuf>>,
}

impl<S: Scrub> TableScrub<S> {
    /// Create a new table scrub object
    ///
    /// # Arguments
    ///
    /// * `component` - The component this table is scrubbed as
    /// * `worker_count` - The number of workers to use when scrubbing data
    #[must_use]
    pub fn new(component: BackupComponents, worker_count: usize) -> Self {
        // build our kanal channel for monitor updates
        let (updates_tx, updates_rx) = kanal::unbounded_async();
        // build our kanal channel for orders
        let (orders_tx, orders_rx) = kanal::unbounded_async();
        TableScrub {
            component,
            updates_tx,
            updates_rx,
            orders_tx,
            orders_rx,
            progress: bars().clone(),
            worker_count,
            active: FuturesUnordered::default(),
        }
    }

    /// Build the workers for this table scrubber
    ///
    /// # Arguments
    ///
    /// * `log` - The run's failure log
    ///
    /// # Panics
    ///
    /// Panics if the built-in progress bar template is invalid.
    pub fn build_workers(&mut self, log: &Arc<FailureLog>) {
        // build the style for our progress bar
        let bar_style =
            ProgressStyle::with_template("{spinner} Scrubbed {msg} {bytes} {binary_bytes_per_sec}")
                .unwrap()
                .tick_strings(&[
                    "🦀🧼     ",
                    " 🦀🧼    ",
                    "  🦀🧼   ",
                    "   🦀🧼  ",
                    "    🦀🧼 ",
                    "     🦀🧼",
                    "     🧼🦀",
                    "    🧼🦀 ",
                    "   🧼🦀  ",
                    "  🧼🦀   ",
                    " 🧼🦀    ",
                    "🧼🦀     ",
                ]);
        for _ in 0..self.worker_count {
            // create a new progress bar for this worker
            let bar = ProgressBar::new_spinner();
            // set a steady tick rate
            bar.enable_steady_tick(std::time::Duration::from_millis(120));
            // set this bars style
            bar.set_style(bar_style.clone());
            // add this progress bar to our main bar
            let bar = self.progress.add(bar);
            // create a new worker
            let worker = ScrubWorker::<S>::new(
                self.updates_tx.clone(),
                bar,
                self.component.to_string(),
                log,
            );
            // spawn this worker on its share of the orders
            let handle = spawn_worker(&self.orders_rx, |orders| worker.scrub(orders));
            // add this task to our futures set
            self.active.push(handle);
        }
    }

    /// Start our global progress tracker
    async fn start_global_tracker(&self) -> JoinHandle<()> {
        // get a handle to our update recieve channel
        let update_rx = self.updates_rx.clone();
        // build the style for our progress bar
        let bar_style = ProgressStyle::with_template(
            "{spinner:.green} {elapsed_precise} Total Scrubbed {bytes} {binary_bytes_per_sec}",
        )
        .unwrap()
        .tick_strings(&[
            "🦀📋         ",
            " 🦀📋        ",
            "  🦀📋       ",
            "   🦀📋      ",
            "    🦀📋     ",
            "     🦀📋    ",
            "      🦀📋   ",
            "       🦀📋  ",
            "        🦀📋 ",
            "         🦀📋",
            "        🦀📋 ",
            "       🦀📋  ",
            "      🦀📋   ",
            "     🦀📋    ",
            "    🦀📋     ",
            "   🦀📋      ",
            "  🦀📋       ",
            " 🦀📋        ",
            "🦀📋         ",
        ]);
        // create a new progress bar for this worker
        let bar = ProgressBar::new_spinner();
        // set a steady tick rate
        bar.enable_steady_tick(std::time::Duration::from_millis(120));
        // set this bars style
        bar.set_style(bar_style.clone());
        // add this progress bar to our main bar
        let bar = self.progress.add(bar);
        // create our monitor
        let monitor = Monitor::new(bar, update_rx);
        // start our monitor
        tokio::spawn(async move { monitor.start().await })
    }

    /// Start scrubbing data
    ///
    /// # Arguments
    ///
    /// * `path` - The path to our this tables backup directory
    /// * `log` - The run's failure log
    pub async fn spawn(&mut self, mut path: PathBuf, log: &FailureLog) -> Result<(), Error> {
        // build the path to this tables backup map
        path.push(S::name());
        path.push("maps");
        // crawl all of our maps for this table
        let mut walker = WalkDir::new(path.clone()).filter(|entry| async move {
            if !entry.file_name().to_string_lossy().ends_with(".thoriummap") {
                Filtering::Ignore
            } else {
                Filtering::Continue
            }
        });
        while let Some(entry) = walker.next().await {
            // send no more orders once --abort-on-failure stopped the run
            if log.stopped() {
                break;
            }
            // record any map that couldn't be listed, since its partitions won't be checked
            match entry {
                Ok(entry) => self.orders_tx.send(entry.path()).await?,
                Err(error) => {
                    let record = FailureRecord::failed(
                        self.component.to_string(),
                        Operation::ReadArchive,
                        format!("failed to list the maps of {}: {error}", S::name()),
                    )
                    .table(S::name())
                    .key(path.to_string_lossy());
                    log.record(&record).await?;
                }
            }
        }
        Ok(())
    }

    /// Wait for all of our workers to finish
    ///
    /// # Arguments
    ///
    /// * `log` - The run's failure log, where a worker that ended in an error is recorded
    async fn wait_for_workers(&mut self, log: &FailureLog) -> Result<(), Error> {
        // stop sending orders and join every worker directly
        let mut joined = join_workers(&mut self.orders_tx, &self.orders_rx, &mut self.active).await;
        // judge the step before the finished workers are shut down
        let failed = joined
            .step_failed()
            .then(|| joined.step_error(&format!("the {} scrub", self.component)));
        // shut down the workers that finished
        for worker in std::mem::take(&mut joined.finished) {
            worker.shutdown();
        }
        // record each worker that ended early once, with the map it was checking
        for failure in &joined.errors {
            let record = FailureRecord::worker_ended(
                &self.component.to_string(),
                Operation::Verify,
                S::name(),
                failure,
            );
            // a step that can't record its workers' errors still reports them
            if let Err(record_error) = log.record(&record).await {
                return Err(failed.unwrap_or(record_error));
            }
        }
        // a step whose workers all ended early can't be trusted to have handled its orders
        if let Some(error) = failed {
            return Err(error);
        }
        Ok(())
    }

    /// Scrub this tables backed up data
    ///
    /// # Arguments
    ///
    /// * `path` - The backup's root directory
    /// * `log` - The run's failure log
    pub async fn scrub(&mut self, path: PathBuf, log: &Arc<FailureLog>) -> Result<(), Error> {
        // get the name of the table to scrub without any '_'
        let pretty_name = S::pretty_name();
        // get the path to this specific scrub
        let table_path = path.join(S::name());
        // check if the subdir for this table exists
        if !tokio::fs::try_exists(&table_path).await.map_err(|err| {
            Error::new(format!(
                "Failed to check if path '{}' exists: {}",
                table_path.to_string_lossy(),
                err
            ))
        })? {
            // the path is missing, so skip this restore
            warn!(
                "Path '{}' missing. Skipping {} scrub...",
                table_path.to_string_lossy(),
                pretty_name
            );
            return Ok(());
        }
        // log the table we are backing up
        info!("Scrubbing {pretty_name}");
        // start our global tracker
        let handle = self.start_global_tracker().await;
        // build our workers
        self.build_workers(log);
        // spawn our scrub workers
        self.spawn(path, log).await?;
        // wait for all of our workers to finish
        self.wait_for_workers(log).await?;
        // tell our global tracker to finish
        self.updates_tx.send(MonitorUpdate::Finished).await?;
        // wait for our global tracker to finish
        handle.await?;
        // log what this component checked and failed
        log.summarize(&self.component.to_string());
        Ok(())
    }
}

/// The scrub controller for a single Thorium backup
pub struct ScrubController {
    /// The samples list table
    samples_list: TableScrub<SamplesList>,
    /// The s3 ids table
    s3_ids: TableScrub<S3Id>,
    /// The comments table
    comments: TableScrub<Comment>,
    /// The results table
    results: TableScrub<Output>,
    /// The results stream table
    results_stream: TableScrub<OutputStream>,
    /// The tags table
    tags: TableScrub<Tag>,
    /// The repo data table
    repo_data: TableScrub<RepoData>,
    /// The repo list table
    repos_list: TableScrub<RepoList>,
    /// The commit table
    commitish: TableScrub<Commitish>,
    /// The commit list table
    commitish_list: TableScrub<CommitishList>,
    /// The nodes tables
    nodes: TableScrub<Node>,
    /// The network policies table
    network_policies: TableScrub<NetworkPolicy>,
    /// The entities table
    entities: TableScrub<Entity>,
    /// The associations table
    associations: TableScrub<Association>,
    /// The notifications table
    notifications: TableScrub<Notification>,
}

impl ScrubController {
    /// Create a new scrub controller
    ///
    /// # Arguments
    ///
    /// * `worker_count` - The number of workers per table
    pub fn new(worker_count: usize) -> Self {
        ScrubController {
            samples_list: TableScrub::new(BackupComponents::SamplesList, worker_count),
            s3_ids: TableScrub::new(BackupComponents::S3Ids, worker_count),
            comments: TableScrub::new(BackupComponents::Comments, worker_count),
            results: TableScrub::new(BackupComponents::Results, worker_count),
            results_stream: TableScrub::new(BackupComponents::ResultsStream, worker_count),
            tags: TableScrub::new(BackupComponents::Tags, worker_count),
            repo_data: TableScrub::new(BackupComponents::RepoData, worker_count),
            repos_list: TableScrub::new(BackupComponents::RepoList, worker_count),
            commitish: TableScrub::new(BackupComponents::Commitish, worker_count),
            commitish_list: TableScrub::new(BackupComponents::CommitishList, worker_count),
            nodes: TableScrub::new(BackupComponents::Nodes, worker_count),
            network_policies: TableScrub::new(BackupComponents::NetworkPolicies, worker_count),
            entities: TableScrub::new(BackupComponents::Entities, worker_count),
            associations: TableScrub::new(BackupComponents::Associations, worker_count),
            notifications: TableScrub::new(BackupComponents::Notifications, worker_count),
        }
    }

    /// Scrub this backup
    ///
    /// Each partition that fails its check is recorded and the scrub carries on, unless
    /// `--abort-on-failure` stopped it.
    ///
    /// # Arguments
    ///
    /// * `backup` - The backup's root directory
    /// * `log` - The run's failure log
    pub async fn scrub(&mut self, backup: PathBuf, log: &Arc<FailureLog>) -> Result<(), Error> {
        // scrub our tables one at a time, stopping between them under --abort-on-failure
        for index in 0..15 {
            if log.stopped() {
                warn!("Stopping the scrub: a failure was recorded under --abort-on-failure");
                break;
            }
            match index {
                0 => self.samples_list.scrub(backup.clone(), log).await?,
                1 => self.s3_ids.scrub(backup.clone(), log).await?,
                2 => self.comments.scrub(backup.clone(), log).await?,
                3 => self.results.scrub(backup.clone(), log).await?,
                4 => self.results_stream.scrub(backup.clone(), log).await?,
                5 => self.tags.scrub(backup.clone(), log).await?,
                6 => self.repo_data.scrub(backup.clone(), log).await?,
                7 => self.repos_list.scrub(backup.clone(), log).await?,
                8 => self.commitish.scrub(backup.clone(), log).await?,
                9 => self.commitish_list.scrub(backup.clone(), log).await?,
                10 => self.nodes.scrub(backup.clone(), log).await?,
                11 => self.network_policies.scrub(backup.clone(), log).await?,
                12 => self.entities.scrub(backup.clone(), log).await?,
                13 => self.associations.scrub(backup.clone(), log).await?,
                _ => self.notifications.scrub(backup.clone(), log).await?,
            }
        }
        Ok(())
    }
}

/// Scrubs a backup and returns how the run ended
///
/// # Arguments
///
/// * `scrub_args` - The `backup scrub` arguments
/// * `workers` - The number of workers per table
/// * `sinks` - The file-system seam the scrub's report is written through
pub async fn run(
    scrub_args: &ScrubBackup,
    workers: usize,
    sinks: Arc<dyn SinkFactory>,
) -> Result<RunOutcome, Error> {
    // record failures in a report directory so the backup's own files never change
    let reports = super::restore::report_dir(&scrub_args.backup, "scrub").await?;
    let policy = FailurePolicy::from_flag(scrub_args.abort_on_failure);
    let log = Arc::new(FailureLog::open(&reports, policy, sinks.as_ref()).await?);
    // create a new table scrub controller
    let mut controller = ScrubController::new(workers);
    // start scrubbing data
    controller.scrub(scrub_args.backup.clone(), &log).await?;
    // the outcome follows the records the run made
    let outcome = RunOutcome::finished(&log);
    super::backup::log_outcome("scrub", outcome, &log);
    Ok(outcome)
}

/// Handle the backup scrub command
///
/// # Arguments
///
/// * `scrub_args` - The `backup scrub` arguments
/// * `workers` - The number of workers per table
pub async fn handle(scrub_args: &ScrubBackup, workers: usize) -> Result<RunOutcome, Error> {
    // scrub through the real file system
    run(scrub_args, workers, Arc::new(TokioFs)).await
}
