//! The restore controller for Thorium
use async_walkdir::Filtering;
use async_walkdir::WalkDir;
use chrono::{DateTime, Utc};
use futures::stream::FuturesUnordered;
use futures::stream::StreamExt;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use kanal::{AsyncReceiver, AsyncSender};
use scylla::client::session::Session;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use thorium::Conf;
use thorium::CtlConf;
use thorium::Thorium;
use thorium::models::RestoreParams;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use super::preflight::{self, Verdict};
use super::utils;
use crate::Error;
use crate::args::Args;
use crate::args::{BackupComponents, CensusKinds, NewCensus, RestoreBackup};
use crate::backup::failures::{
    ComponentCounts, FailureLog, FailurePolicy, FailureRecord, Operation,
};
use crate::backup::graphics::Graphic;
use crate::backup::kept::{self, KeptUsers};
use crate::backup::manifest::{self, FORMAT_VERSION};
use crate::backup::s3::S3RestoreShared;
use crate::backup::sink::{SinkFactory, TokioFs, create_private_dir};
use crate::backup::tables::{
    Association, Comment, Commitish, CommitishList, Entity, NetworkPolicy, Node, Notification,
    Output, OutputStream, RepoData, RepoList, S3Id, SamplesList, Tag,
};
use crate::backup::utils::{first_link, is_link};
use crate::backup::verify::TABLES;
use crate::backup::version;
use crate::backup::views::{self, ViewStatements};
use crate::backup::wipe::{ScyllaSnapshots, WIPE_FILE, WipeReport, Wiper};
use crate::backup::{
    Monitor, MonitorUpdate, Restore, RestoreWorker, S3Monitor, S3MonitorUpdate, S3Restore,
    S3RestoreWorker,
};
use crate::outcome::RunOutcome;
use crate::shared::progress::bars;
use crate::shared::retry::RetryPolicy;
use crate::shared::workers::{WorkerHandle, join_workers, spawn_worker};

/// Walks a table's `maps/` directory in a backup and sends each archive map in it to the
/// restore workers, recording each map or listing it can't use
///
/// # Arguments
///
/// * `root` - The backup root
/// * `component` - The component the table is restored as
/// * `table` - The table
/// * `log` - The run's failure log
/// * `orders` - Where each map to restore is sent
pub async fn walk_maps(
    root: &Path,
    component: BackupComponents,
    table: &str,
    log: &FailureLog,
    orders: &AsyncSender<PathBuf>,
) -> Result<(), Error> {
    // the table's maps directory
    let path = root.join(table).join("maps");
    // refuse a linked table, maps, or data directory, since its maps or the data they index
    // would come from outside the backup
    let data = root.join(table).join("data");
    let refused = match first_link(&[&root.join(table), &path, &data]).await {
        Ok(None) => None,
        Ok(Some(link)) => Some((link, "is a symbolic link".to_owned())),
        Err(error) => Some((path.clone(), format!("can't be read: {error}"))),
    };
    if let Some((refused, why)) = refused {
        let record = FailureRecord::failed(
            component.to_string(),
            Operation::ReadArchive,
            format!(
                "{} {why}, so the maps of {table} aren't restored",
                refused.display()
            ),
        )
        .table(table)
        .key(refused.to_string_lossy());
        return log.record(&record).await;
    }
    // crawl all of our maps for this table
    let mut walker = WalkDir::new(path.clone()).filter(|entry| async move {
        if entry.file_name().to_string_lossy().ends_with(".thoriummap") {
            Filtering::Continue
        } else {
            Filtering::Ignore
        }
    });
    while let Some(entry) = walker.next().await {
        // send no more orders once --abort-on-failure stopped the run
        if log.stopped() {
            break;
        }
        // record any map that couldn't be listed or is a link, since its rows won't be restored
        match entry {
            Ok(entry) => {
                let why = match is_link(&entry.path()).await {
                    Ok(false) => None,
                    Ok(true) => Some("is a symbolic link".to_owned()),
                    Err(error) => Some(format!("can't be read: {error}")),
                };
                if let Some(why) = why {
                    let record = FailureRecord::failed(
                        component.to_string(),
                        Operation::ReadArchive,
                        format!(
                            "{} {why}, so its rows aren't restored",
                            entry.path().display()
                        ),
                    )
                    .table(table)
                    .key(entry.path().to_string_lossy());
                    log.record(&record).await?;
                } else {
                    orders.send(entry.path()).await?;
                }
            }
            Err(error) => {
                let record = FailureRecord::failed(
                    component.to_string(),
                    Operation::ReadArchive,
                    format!("failed to list the maps of {table}: {error}"),
                )
                .table(table)
                .key(path.to_string_lossy());
                log.record(&record).await?;
            }
        }
    }
    Ok(())
}

/// Walks an S3 component's `objects/` directory in a backup and sends each object file in it to
/// the upload workers, recording each object or listing it can't use
///
/// # Arguments
///
/// * `dir` - The component's directory in the backup
/// * `component` - The component the objects are restored as
/// * `table` - The name of the component's directory, which records name as its table
/// * `log` - The run's failure log
/// * `orders` - Where each object to upload is sent
pub async fn walk_objects(
    dir: &Path,
    component: BackupComponents,
    table: &str,
    log: &FailureLog,
    orders: &AsyncSender<PathBuf>,
) -> Result<(), Error> {
    // refuse a linked component or objects directory, since its objects would come from outside
    // the backup
    let objects = dir.join("objects");
    let refused = match first_link(&[dir, &objects]).await {
        Ok(None) => None,
        Ok(Some(link)) => Some((link, "is a symbolic link".to_owned())),
        Err(error) => Some((objects.clone(), format!("can't be read: {error}"))),
    };
    if let Some((refused, why)) = refused {
        let record = FailureRecord::failed(
            component.to_string(),
            Operation::ReadArchive,
            format!(
                "{} {why}, so the objects of {table} aren't restored",
                refused.display()
            ),
        )
        .table(table)
        .key(refused.to_string_lossy());
        return log.record(&record).await;
    }
    // build our walk dir stream over the component's objects
    let mut walker = WalkDir::new(objects);
    // begin crawling the target dir and restoring its objects
    while let Some(entry) = walker.next().await {
        // send no more orders once --abort-on-failure stopped the run
        if log.stopped() {
            break;
        }
        // record any entry that we fail to open, since its objects won't be restored
        match entry {
            // don't add any directories to our list, and record an entry that can't be read or
            // is a link, whose object isn't uploaded
            Ok(entry) => match entry.metadata().await {
                Ok(metadata) if metadata.is_file() => {
                    orders.send(entry.path()).await?;
                }
                Ok(metadata) if metadata.is_symlink() => {
                    let record = FailureRecord::failed(
                        component.to_string(),
                        Operation::ReadArchive,
                        format!(
                            "{} is a symbolic link, so its object isn't restored",
                            entry.path().display()
                        ),
                    )
                    .table(table)
                    .key(entry.path().to_string_lossy());
                    log.record(&record).await?;
                }
                Ok(_) => (),
                Err(error) => {
                    let record = FailureRecord::failed(
                        component.to_string(),
                        Operation::ReadArchive,
                        format!("failed to read a backed up object of {table}: {error}"),
                    )
                    .table(table)
                    .key(entry.path().to_string_lossy());
                    log.record(&record).await?;
                }
            },
            Err(error) => {
                let record = FailureRecord::failed(
                    component.to_string(),
                    Operation::ReadArchive,
                    format!("failed to list the backed up objects of {table}: {error}"),
                )
                .table(table);
                log.record(&record).await?;
            }
        }
    }
    Ok(())
}

/// A singular table restore
struct TableRestore<R: Restore> {
    /// The component this table is restored as
    component: BackupComponents,
    /// A Thorium config
    conf: Conf,
    /// The scylla client for this table
    scylla: Arc<Session>,
    /// The kanal channel workers should send kanal channel updates over
    updates_tx: AsyncSender<MonitorUpdate>,
    /// The kanal channel to receive archive map updates on
    updates_rx: AsyncReceiver<MonitorUpdate>,
    /// The kanal channel to send orders to workers on
    orders_tx: AsyncSender<PathBuf>,
    /// The kanal channel workers should get orders on
    orders_rx: AsyncReceiver<PathBuf>,
    /// A progress bar for this tables restore
    progress: MultiProgress,
    /// The number of workers to use when restoring data
    worker_count: usize,
    /// The currently active workers
    active: FuturesUnordered<WorkerHandle<RestoreWorker<R>, PathBuf>>,
}

impl<R: Restore> TableRestore<R> {
    /// Create a new table restore object
    ///
    /// # Arguments
    ///
    /// * `component` - The component this table is restored as
    /// * `conf` - A thorium config
    /// * `scylla` - The scylla client to use
    /// * `worker_count` - The number of workers to use
    pub fn new(
        component: BackupComponents,
        conf: &Conf,
        scylla: &Arc<Session>,
        worker_count: usize,
    ) -> Self {
        // build our kanal channel for monitor updates
        let (updates_tx, updates_rx) = kanal::unbounded_async();
        // build our kanal channel for orders
        let (orders_tx, orders_rx) = kanal::unbounded_async();
        // build our table restore object
        TableRestore {
            component,
            conf: conf.clone(),
            scylla: scylla.clone(),
            updates_tx,
            updates_rx,
            orders_tx,
            orders_rx,
            progress: bars().clone(),
            worker_count,
            active: FuturesUnordered::default(),
        }
    }

    /// Build our restore workers
    ///
    /// # Arguments
    ///
    /// * `log` - The run's failure log
    /// * `format` - The format version the backup's archives were written in
    async fn build_workers(&mut self, log: &Arc<FailureLog>, format: u32) -> Result<(), Error> {
        // build the style for our progress bar
        let bar_style = ProgressStyle::with_template(
            "{spinner} Restored Rows: {msg} {bytes} {binary_bytes_per_sec}",
        )
        .unwrap()
        .tick_strings(&[
            "📦🦀🌽     🚀",
            "📦 🦀🌽    🚀",
            "📦  🦀🌽   🚀",
            "📦   🦀🌽  🚀",
            "📦    🦀🌽 🚀",
            "📦     🦀🌽🚀",
            "📦       🦀🚀",
            "📦      🦀 🚀",
            "📦     🦀  🚀",
            "📦    🦀   🚀",
            "📦   🦀    🚀",
            "📦  🦀     🚀",
            "📦 🦀      🚀",
            "📦🦀       🚀",
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
            let worker = RestoreWorker::<R>::new(
                &self.scylla,
                &self.conf,
                self.updates_tx.clone(),
                bar,
                &self.component.to_string(),
                log,
                format,
            )
            .await?;
            // spawn this worker on its share of the orders
            let handle = spawn_worker(&self.orders_rx, |orders| worker.restore(orders));
            // add this task to our futures set
            self.active.push(handle);
        }
        Ok(())
    }

    /// Start our monitor
    fn start_monitor(&self) -> JoinHandle<()> {
        // build the style for our progress bar
        let bar_style = ProgressStyle::with_template(
            "{spinner:.green} {elapsed_precise} Total Restored Rows: {msg} {bytes} {binary_bytes_per_sec}",
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
        // clone our update channel
        let update_rx = self.updates_rx.clone();
        // create our monitor
        let monitor = Monitor::new(bar, update_rx);
        // start our monitor
        tokio::spawn(async move { monitor.start().await })
    }

    /// Spawn our workers and have them start restoring data
    ///
    /// # Arguments
    ///
    /// * `path` - The path to our this tables backup directory
    /// * `log` - The run's failure log
    async fn spawn(&mut self, path: PathBuf, log: &FailureLog) -> Result<(), Error> {
        // send every archive map of this table to the workers
        walk_maps(&path, self.component, R::name(), log, &self.orders_tx).await
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
            .then(|| joined.step_error(&format!("the {} restore", self.component)));
        // shut down the workers that finished
        for worker in std::mem::take(&mut joined.finished) {
            worker.shutdown();
        }
        // record each worker that ended early once, with the map it was restoring
        for failure in &joined.errors {
            let record = FailureRecord::worker_ended(
                &self.component.to_string(),
                Operation::Insert,
                R::name(),
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

    /// Start restoring this table
    ///
    /// # Arguments
    ///
    /// * `path` - The backup's root directory
    /// * `log` - The run's failure log
    /// * `format` - The format version the backup's archives were written in
    async fn restore(
        &mut self,
        path: PathBuf,
        log: &Arc<FailureLog>,
        format: u32,
    ) -> Result<(), Error> {
        // get the name of the table to scrub without any '_'
        let pretty_name = R::pretty_name();
        // get the path to this specific scrub
        let table_path = path.join(R::name());
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
                "Path '{}' missing. Skipping {} restore...",
                table_path.to_string_lossy(),
                pretty_name
            );
            return Ok(());
        }
        // log the table we are backing up
        info!("Restoring {pretty_name}");
        // drop the views the registry lists for this table, which restore recreates afterwards
        views::drop_views_of(&self.scylla, &self.conf.thorium.namespace, R::name()).await?;
        // start our global monitor
        let handle = self.start_monitor();
        // build our workers
        self.build_workers(log, format).await?;
        // spawn our restore workers
        self.spawn(path, log).await?;
        // wait for all of our workers to finish
        self.wait_for_workers(log).await?;
        // tell our monitor to finish
        self.updates_tx.send(MonitorUpdate::Finished).await?;
        // wait for our ntracker monitor finish
        handle.await?;
        // log what this component handled and failed
        log.summarize(&self.component.to_string());
        Ok(())
    }
}

/// A singular s3 restore
struct S3RestoreController<R: S3Restore> {
    /// The component these objects are restored as
    component: BackupComponents,
    /// The kanal channel workers should send kanal channel updates over
    updates_tx: AsyncSender<S3MonitorUpdate>,
    /// The kanal channel to receive s3 monitor updates on
    updates_rx: AsyncReceiver<S3MonitorUpdate>,
    /// The kanal channel to send orders to workers on
    orders_tx: AsyncSender<PathBuf>,
    /// The kanal channel workers should get orders on
    orders_rx: AsyncReceiver<PathBuf>,
    /// A progress bar for this tables restore
    progress: MultiProgress,
    /// The number of workers to use when restoring data
    worker_count: usize,
    /// The currently active workers
    active: FuturesUnordered<WorkerHandle<S3RestoreWorker<R>, PathBuf>>,
}

impl<R: S3Restore> S3RestoreController<R> {
    /// Create a new table restore object
    ///
    /// # Arguments
    ///
    /// * `component` - The component these objects are restored as
    /// * `worker_count` - The number of workers to use
    pub fn new(component: BackupComponents, worker_count: usize) -> Self {
        // build our kanal channels
        let (updates_tx, updates_rx) = kanal::unbounded_async();
        let (orders_tx, orders_rx) = kanal::unbounded_async();
        // build our table restore object
        S3RestoreController {
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

    /// Build our restore workers
    ///
    /// # Arguments
    ///
    /// * `shared` - What every upload of the run shares
    /// * `objects` - This component's `objects/` directory in the backup
    /// * `log` - The run's failure log
    fn build_workers(&mut self, shared: &S3RestoreShared, objects: &Path, log: &Arc<FailureLog>) {
        // build the style for our progress bar
        let bar_style = ProgressStyle::with_template(
            "{spinner} Restored S3 Objects: {msg} {bytes} {binary_bytes_per_sec}",
        )
        .unwrap()
        .tick_strings(&[
            "📦🦀🌽     🚀",
            "📦 🦀🌽    🚀",
            "📦  🦀🌽   🚀",
            "📦   🦀🌽  🚀",
            "📦    🦀🌽 🚀",
            "📦     🦀🌽🚀",
            "📦       🦀🚀",
            "📦      🦀 🚀",
            "📦     🦀  🚀",
            "📦    🦀   🚀",
            "📦   🦀    🚀",
            "📦  🦀     🚀",
            "📦 🦀      🚀",
            "📦🦀       🚀",
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
            let worker = S3RestoreWorker::<R>::new(
                shared,
                objects,
                &self.updates_tx,
                bar,
                &self.component.to_string(),
                log,
            );
            // spawn this worker on its share of the orders
            let handle = spawn_worker(&self.orders_rx, |orders| worker.restore(orders));
            // add this task to our futures set
            self.active.push(handle);
        }
    }

    /// Start our global progress tracker
    pub fn start_monitor(&mut self) -> JoinHandle<Result<(), Error>> {
        // get a handle to our update recieve channel
        let update_rx = self.updates_rx.clone();
        // build the style for our progress bar
        let bar_style = ProgressStyle::with_template(
            "{spinner:.green} {elapsed_precise} Total Restored S3 Obejcts: {msg} {bytes} {binary_bytes_per_sec}",
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
        // build our s3 monitor
        let monitor = S3Monitor::new(update_rx, bar);
        // start our s3 monitor
        tokio::spawn(async move { monitor.start().await })
    }

    /// Spawn our workers and have them start restoring data
    ///
    /// # Arguments
    ///
    /// * `path` - The path to the root of the subset of data to restore
    /// * `log` - The run's failure log
    async fn spawn(&mut self, path: PathBuf, log: &FailureLog) -> Result<(), Error> {
        // send every backed up object of this component to the workers
        walk_objects(&path, self.component, R::name(), log, &self.orders_tx).await
    }

    /// Wait for all of our workers to finish
    ///
    /// # Arguments
    ///
    /// * `log` - The run's failure log, where a worker that ended in an error is recorded
    async fn wait_for_workers(&mut self, log: &FailureLog) -> Result<(), Error> {
        // stop sending orders and join every worker directly
        let joined = join_workers(&mut self.orders_tx, &self.orders_rx, &mut self.active).await;
        // judge the step before the finished workers are shut down
        let failed = joined
            .step_failed()
            .then(|| joined.step_error(&format!("the {} restore", self.component)));
        // record each worker that ended early once, with the map it was uploading objects from
        for failure in &joined.errors {
            let record = FailureRecord::worker_ended(
                &self.component.to_string(),
                Operation::Upload,
                R::name(),
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

    /// Start restoring this table
    ///
    /// # Arguments
    ///
    /// * `path` - The path to restore objects too
    /// * `shared` - What every upload of the run shares
    /// * `log` - The run's failure log
    async fn restore(
        &mut self,
        mut path: PathBuf,
        shared: &S3RestoreShared,
        log: &Arc<FailureLog>,
    ) -> Result<(), Error> {
        // get our name without '_'
        let pretty_name = R::pretty_name();
        // nest our path by our table name
        path.push(R::name());
        // check if the subdir for this table exists
        if !tokio::fs::try_exists(&path).await.map_err(|err| {
            Error::new(format!(
                "Failed to check if path '{}' exists: {}",
                path.to_string_lossy(),
                err
            ))
        })? {
            // the path is missing, so skip this restore
            warn!(
                "Path '{}' missing. Skipping {} restore...",
                path.to_string_lossy(),
                pretty_name
            );
            return Ok(());
        }
        // a backup taken without this component holds no objects for it
        if !tokio::fs::try_exists(path.join("objects")).await? {
            warn!(
                "Path '{}' holds no objects. Skipping {pretty_name} object restore...",
                path.to_string_lossy()
            );
            return Ok(());
        }
        // log the table we are backing up
        info!("Restoring {pretty_name}");
        // start our global tracker
        let handle = self.start_monitor();
        // build our workers
        self.build_workers(shared, &path.join("objects"), log);
        // spawn our restore workers
        self.spawn(path, log).await?;
        // wait for all of our workers to finish
        self.wait_for_workers(log).await?;
        // tell our map updater to finish
        self.updates_tx.send(S3MonitorUpdate::Finished).await?;
        // wait for our global tracker to finish
        handle.await??;
        // log what this component handled and failed
        log.summarize(&self.component.to_string());
        Ok(())
    }
}

/// The restore controller for Thorium
pub struct RestoreController {
    /// The Thorctl config to use to restore
    ctl_conf: CtlConf,
    /// The scylla client for the cluster being restored
    scylla: Arc<Session>,
    /// The namespace being restored, which is the keyspace of its tables and views
    namespace: String,
    /// The config of the cluster being restored, which S3 uploads and the census are built from
    s3_conf: Conf,
    /// The number of workers each step uses
    workers: usize,
    /// The statements of the views the restore drops, when the preflight captured them
    views: Option<ViewStatements>,
    /// The format version the backup's archives were written in
    format: u32,
    /// Whether the API flushes its whole Redis database before the Redis restore, instead of
    /// deleting only the namespace's keys
    redis_flush_all: bool,
    /// The operator users the target held before the restore, which the Redis restore keeps
    kept_users: KeptUsers,
    /// What every S3 upload of the restore shares: the client, the retries, and the bound
    s3: S3RestoreShared,
    /// The samples list table
    samples_list: TableRestore<SamplesList>,
    /// The s3 ids table
    s3_ids: TableRestore<S3Id>,
    /// The comments table
    comments: TableRestore<Comment>,
    /// The results table
    results: TableRestore<Output>,
    /// The results table
    results_stream: TableRestore<OutputStream>,
    /// The tags table
    tags: TableRestore<Tag>,
    /// The repo_data table
    repo_data: TableRestore<RepoData>,
    /// The repos_list table
    repos_list: TableRestore<RepoList>,
    /// The commits table
    commitish: TableRestore<Commitish>,
    /// The commits_list table
    commitish_list: TableRestore<CommitishList>,
    /// The nodes table
    nodes: TableRestore<Node>,
    /// The network policies table
    network_policies: TableRestore<NetworkPolicy>,
    /// The entities table
    entities: TableRestore<Entity>,
    /// The associations table, which links entities, files, and repos
    associations: TableRestore<Association>,
    /// The notifications table, which names images and pipelines
    notifications: TableRestore<Notification>,
    /// The samples/repos s3 objects
    s3_ids_objects: S3RestoreController<S3Id>,
    /// The comment attachment objects
    comment_attachments: S3RestoreController<Comment>,
    /// The result files objects
    result_files: S3RestoreController<Output>,
    /// The graphics objects
    graphics: S3RestoreController<Graphic>,
}

impl RestoreController {
    /// Create a new restore controller
    ///
    /// # Arguments
    ///
    /// * `config` - The Thorium config for the cluster to restore
    /// * `ctl_conf` - The Thorctl config to use to restore
    /// * `scylla` - A scylla client for this cluster
    /// * `workers` - The number of workers to use
    #[must_use]
    pub fn new(config: &Conf, ctl_conf: CtlConf, scylla: &Arc<Session>, workers: usize) -> Self {
        // build our table restore objects
        let samples_list =
            TableRestore::new(BackupComponents::SamplesList, config, scylla, workers);
        let s3_ids = TableRestore::new(BackupComponents::S3Ids, config, scylla, workers);
        let comments = TableRestore::new(BackupComponents::Comments, config, scylla, workers);
        let results = TableRestore::new(BackupComponents::Results, config, scylla, workers);
        let results_stream =
            TableRestore::new(BackupComponents::ResultsStream, config, scylla, workers);
        let tags = TableRestore::new(BackupComponents::Tags, config, scylla, workers);
        let repo_data = TableRestore::new(BackupComponents::RepoData, config, scylla, workers);
        let repos_list = TableRestore::new(BackupComponents::RepoList, config, scylla, workers);
        let commits = TableRestore::new(BackupComponents::Commitish, config, scylla, workers);
        let commits_list =
            TableRestore::new(BackupComponents::CommitishList, config, scylla, workers);
        let nodes = TableRestore::new(BackupComponents::Nodes, config, scylla, workers);
        let network_policies =
            TableRestore::new(BackupComponents::NetworkPolicies, config, scylla, workers);
        let entities = TableRestore::new(BackupComponents::Entities, config, scylla, workers);
        let associations =
            TableRestore::new(BackupComponents::Associations, config, scylla, workers);
        let notifications =
            TableRestore::new(BackupComponents::Notifications, config, scylla, workers);
        // build our s3 restore objects
        let s3_ids_objects = S3RestoreController::new(BackupComponents::S3IdsObjects, workers);
        let comment_attachments =
            S3RestoreController::new(BackupComponents::CommentAttachments, workers);
        let result_files = S3RestoreController::new(BackupComponents::ResultFiles, workers);
        let graphics = S3RestoreController::new(BackupComponents::Graphics, workers);
        // build our controller
        RestoreController {
            ctl_conf,
            scylla: scylla.clone(),
            namespace: config.thorium.namespace.clone(),
            s3_conf: config.clone(),
            workers,
            views: None,
            format: FORMAT_VERSION,
            redis_flush_all: false,
            kept_users: KeptUsers::default(),
            s3: S3RestoreShared::new(config, RetryPolicy::new(DEFAULT_ATTEMPTS), DEFAULT_UPLOADS),
            samples_list,
            s3_ids,
            comments,
            results,
            results_stream,
            tags,
            repo_data,
            repos_list,
            commitish: commits,
            commitish_list: commits_list,
            nodes,
            network_policies,
            entities,
            associations,
            notifications,
            s3_ids_objects,
            comment_attachments,
            result_files,
            graphics,
        }
    }

    /// Restore this clusters redis data from disk, returning whether the API restored it and the
    /// operator users it kept of the target
    ///
    /// A Redis state that can't be read or restored is recorded and the restore carries on.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to write this backup too
    /// * `log` - The run's failure log
    async fn restore_redis(
        &self,
        mut path: PathBuf,
        log: &FailureLog,
    ) -> Result<(bool, Vec<String>), Error> {
        // the Redis state is one object under its own component name
        let component = BackupComponents::Redis.to_string();
        // build the path to our backup file
        path.push("redis.json");
        // check if the redis backup exists
        if !tokio::fs::try_exists(&path).await.map_err(|err| {
            Error::new(format!(
                "Failed to check if path '{}' exists: {}",
                path.to_string_lossy(),
                err
            ))
        })? {
            warn!(
                "Redis backup at '{}' not found. Skipping redis restore...",
                path.to_string_lossy()
            );
            return Ok((false, Vec::new()));
        }
        info!("Restoring redis");
        // read, parse, and restore the state, recording any step that fails
        let restored = match self.restore_redis_file(&path).await {
            Ok(kept) => {
                log.handled(&component, 1);
                (true, kept)
            }
            Err(error) => {
                let record = FailureRecord::failed(&component, Operation::Redis, error)
                    .key(path.to_string_lossy());
                log.record(&record).await?;
                (false, Vec::new())
            }
        };
        // log what this component handled and failed
        log.summarize(&component);
        Ok(restored)
    }

    /// Reads a `redis.json` file and restores it through the API, keeping the target's operator
    /// users in place of the backup's, and returns the names of the users kept
    ///
    /// # Arguments
    ///
    /// * `path` - The path of `redis.json`
    async fn restore_redis_file(&self, path: &Path) -> Result<Vec<String>, Error> {
        // a linked redis.json isn't followed, since its state would come from outside the backup
        if is_link(path).await? {
            return Err(Error::new(format!(
                "{} is a symbolic link; a backup holds only real files, so it isn't followed",
                path.display()
            )));
        }
        // build a Thorium client
        let client = Thorium::from_ctl_conf(self.ctl_conf.clone()).await?;
        // load our backup from disk
        let backup_str = tokio::fs::read_to_string(path).await?;
        // deserialize our backup
        let mut backup = serde_json::from_str(&backup_str)?;
        // the target's operator users replace the backup's, so its operator can still log in
        let kept = self.kept_users.keep(&mut backup);
        // restore this backup, flushing the API's whole database first when asked
        let params = RestoreParams {
            flush_db: self.redis_flush_all,
        };
        client.system.restore_with(&backup, &params).await?;
        Ok(kept)
    }

    /// Sets how S3 uploads are retried and how many are in flight at once
    ///
    /// # Arguments
    ///
    /// * `attempts` - Attempts per object, including the first
    /// * `concurrency` - The most uploads in flight at once across the restore
    #[must_use]
    pub fn with_transfers(mut self, attempts: u32, concurrency: usize) -> Self {
        // one client, retry policy, and bound for every S3 component of the run
        self.s3 = S3RestoreShared::new(&self.s3_conf, RetryPolicy::new(attempts), concurrency);
        self
    }

    /// Sets the format version the backup's archives were written in, which is this thoradm's
    /// unless set
    ///
    /// # Arguments
    ///
    /// * `format` - The backup's format version; a legacy backup's is 1
    #[must_use]
    pub fn with_format(mut self, format: u32) -> Self {
        // every table reads its archives as this format
        self.format = format;
        self
    }

    /// Sets whether the API flushes its whole Redis database before the Redis restore
    ///
    /// # Arguments
    ///
    /// * `flush` - Whether to flush the database instead of deleting the namespace's keys
    #[must_use]
    pub fn with_redis_flush_all(mut self, flush: bool) -> Self {
        // the Redis restore asks the API for a flush with this
        self.redis_flush_all = flush;
        self
    }

    /// Takes the operator users the target held, which the Redis restore keeps in place of the
    /// backup's
    ///
    /// # Arguments
    ///
    /// * `kept` - The target's operator users
    #[must_use]
    pub fn with_kept_users(mut self, kept: KeptUsers) -> Self {
        // the Redis restore puts these into the backup it sends
        self.kept_users = kept;
        self
    }

    /// Takes the statements of the views a restore drops, which the preflight captured
    ///
    /// # Arguments
    ///
    /// * `statements` - Each view's statement, by view name
    #[must_use]
    pub fn with_views(mut self, statements: ViewStatements) -> Self {
        // the restore recreates these once the tables are loaded
        self.views = Some(statements);
        self
    }

    /// Returns the statements of the views the restore drops: the preflight's, or for a restore
    /// run without one, those of every registered view the target has, saved to `views.cql`
    ///
    /// # Arguments
    ///
    /// * `path` - The backup being restored
    /// * `log` - The run's failure log, beside which `views.cql` is written
    async fn view_statements(
        &mut self,
        path: &Path,
        log: &FailureLog,
    ) -> Result<ViewStatements, Error> {
        // the preflight already captured and saved them
        if let Some(statements) = self.views.take() {
            return Ok(statements);
        }
        // otherwise capture every registered view's statement, beside the run's failure log
        let report_dir = log.path().parent().unwrap_or(path).to_path_buf();
        let tables: Vec<&str> = TABLES.iter().map(|(table, _)| *table).collect();
        let (statements, missing) =
            ViewStatements::capture(&self.scylla, &self.namespace, &tables, path, &report_dir)
                .await?;
        // a view already missing with no earlier statement can't come back
        for view in missing {
            warn!(
                "{view} is missing and no earlier views.cql holds it, so restore can't recreate it"
            );
        }
        // save them for an admin to replay
        statements.save(&report_dir, &TokioFs).await?;
        Ok(statements)
    }

    /// Recreates the views the table restores dropped, recording a failure in the run's failure
    /// log for each view that can't be recreated; the others are recreated all the same
    ///
    /// # Arguments
    ///
    /// * `statements` - Each view's statement, by view name
    /// * `log` - The run's failure log
    ///
    /// # Returns
    ///
    /// How many views were recreated, or `None` when any couldn't be
    async fn recreate_views(
        &self,
        statements: &ViewStatements,
        log: &FailureLog,
    ) -> Result<Option<usize>, Error> {
        // a view that can't be recreated leaves view-backed routes failing, so it is recorded
        match statements.recreate(&self.scylla, &self.namespace).await {
            Ok(done) if done.failed.is_empty() => Ok(Some(done.views.len())),
            Ok(done) => {
                for (view, error) in &done.failed {
                    let record = FailureRecord::failed(
                        "views",
                        Operation::PostRestore,
                        format!("couldn't recreate view {}.{view}: {error}", self.namespace),
                    )
                    .key(view);
                    log.record(&record).await?;
                }
                Ok(None)
            }
            Err(error) => {
                let record = FailureRecord::failed("views", Operation::PostRestore, &error)
                    .key(views::VIEWS_FILE);
                log.record(&record).await?;
                Ok(None)
            }
        }
    }

    /// Restores some components one at a time
    ///
    /// Each component's failed objects are recorded and the restore carries on, unless
    /// `--abort-on-failure` stopped it. A component that can't run at all is recorded and ends
    /// the restore.
    ///
    /// # Arguments
    ///
    /// * `steps` - The components to restore, in order
    /// * `path` - The path to the data to restore
    /// * `log` - The run's failure log
    #[rustfmt::skip]
    async fn restore_steps(&mut self, steps: &[BackupComponents], path: &Path, log: &Arc<FailureLog>) -> Result<(), Error> {
        let path = path.to_path_buf();
        for component in steps.iter().copied() {
            // run no further components once --abort-on-failure stopped the run
            if log.stopped() {
                warn!("Stopping before {component}: a failure was recorded under --abort-on-failure");
                break;
            }
            // run this component's restore
            let result = match component {
                BackupComponents::SamplesList => self.samples_list.restore(path.clone(), log, self.format).await,
                BackupComponents::S3Ids => self.s3_ids.restore(path.clone(), log, self.format).await,
                BackupComponents::Comments => self.comments.restore(path.clone(), log, self.format).await,
                BackupComponents::Results => self.results.restore(path.clone(), log, self.format).await,
                BackupComponents::ResultsStream => self.results_stream.restore(path.clone(), log, self.format).await,
                BackupComponents::Tags => self.tags.restore(path.clone(), log, self.format).await,
                BackupComponents::RepoData => self.repo_data.restore(path.clone(), log, self.format).await,
                BackupComponents::RepoList => self.repos_list.restore(path.clone(), log, self.format).await,
                BackupComponents::Commitish => self.commitish.restore(path.clone(), log, self.format).await,
                BackupComponents::CommitishList => self.commitish_list.restore(path.clone(), log, self.format).await,
                BackupComponents::Nodes => self.nodes.restore(path.clone(), log, self.format).await,
                BackupComponents::NetworkPolicies => self.network_policies.restore(path.clone(), log, self.format).await,
                BackupComponents::Entities => self.entities.restore(path.clone(), log, self.format).await,
                BackupComponents::Associations => self.associations.restore(path.clone(), log, self.format).await,
                BackupComponents::Notifications => self.notifications.restore(path.clone(), log, self.format).await,
                BackupComponents::S3IdsObjects => self.s3_ids_objects.restore(path.clone(), &self.s3, log).await,
                BackupComponents::CommentAttachments => self.comment_attachments.restore(path.clone(), &self.s3, log).await,
                BackupComponents::ResultFiles => self.result_files.restore(path.clone(), &self.s3, log).await,
                BackupComponents::Graphics => self.graphics.restore(path.clone(), &self.s3, log).await,
                _ => Ok(()),
            };
            // a component that can't run at all is recorded and ends the restore; a step whose
            // workers ended early has recorded each of them already
            if let Err(error) = result {
                let operation = if component.is_in_s3() { Operation::Upload } else { Operation::Insert };
                if let Some(record) = FailureRecord::component_failed(&component.to_string(), operation, &error) {
                    log.record(&record).await?;
                }
                return Err(error);
            }
        }
        Ok(())
    }

    /// Restore a clusters data from disk
    ///
    /// Redis first, then every table, the ones that link or name others after them, then the
    /// views the tables' restores dropped (even when a table step stopped the restore), then every
    /// S3 object. Each component's failed objects are recorded and the restore carries on, unless
    /// `--abort-on-failure` stopped it. A component that can't run at all is recorded and ends the
    /// restore.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to the data to restore
    /// * `log` - The run's failure log
    pub async fn restore(
        &mut self,
        path: &Path,
        log: &Arc<FailureLog>,
    ) -> Result<RestoreDone, Error> {
        // save the statements of the views the table restores drop, before the first drop
        let statements = self.view_statements(path, log).await?;
        // a target that already holds nodes keeps them, so the backup's aren't restored
        let keep_nodes = kept::holds_nodes(&self.scylla, &self.namespace).await?;
        // restore our redis cluster first
        let (redis_restored, kept_users) = self.restore_redis(path.to_path_buf(), log).await?;
        let mut kept = kept_users
            .iter()
            .map(|name| format!("user {name}"))
            .collect::<Vec<_>>();
        // the tables, then the views they dropped, whatever became of the tables
        let steps = TABLE_STEPS
            .iter()
            .copied()
            .filter(|step| !(keep_nodes && *step == BackupComponents::Nodes))
            .collect::<Vec<_>>();
        if keep_nodes && tokio::fs::try_exists(path.join(kept::NODES_TABLE)).await? {
            info!("Keeping the target's nodes; the backup's nodes aren't restored");
            kept.push(format!("table {}", kept::NODES_TABLE));
        }
        let tables = self.restore_steps(&steps, path, log).await;
        let recreated = self.recreate_views(&statements, log).await?;
        tables?;
        // then the objects, and last the census of the restored tables, which the Redis restore
        // emptied and which listings read
        self.restore_steps(&OBJECT_STEPS, path, log).await?;
        let census = self.rebuild_census(path, log).await?;
        // say whether the cluster serves view-backed routes again on its own
        match recreated {
            Some(count) => info!(
                "Restored {}: {count} views recreated, so no API restart is needed",
                path.display()
            ),
            None => warn!(
                "Restored {}, but its views couldn't be recreated: replay {} with cqlsh, or \
                 restart one API pod, whose schema setup creates missing views",
                path.display(),
                views::VIEWS_FILE
            ),
        }
        Ok(RestoreDone {
            views_recreated: recreated,
            census,
            redis_restored,
            kept,
        })
    }

    /// Rebuilds the census of every restored table that has one, and of every table when the
    /// Redis restore ran, recording a census that couldn't count everything
    ///
    /// # Arguments
    ///
    /// * `path` - The backup being restored
    /// * `log` - The run's failure log
    async fn rebuild_census(&self, path: &Path, log: &FailureLog) -> Result<Vec<String>, Error> {
        // the census kinds of the tables the backup holds, or of every table once the Redis
        // restore deleted every census stream
        let redis = tokio::fs::try_exists(path.join("redis.json")).await?;
        let mut kinds = Vec::new();
        for (table, _) in TABLES {
            if redis || tokio::fs::try_exists(path.join(table)).await? {
                for kind in CensusKinds::of_table(table) {
                    if !kinds.contains(kind) {
                        kinds.push(*kind);
                    }
                }
            }
        }
        if kinds.is_empty() {
            return Ok(Vec::new());
        }
        info!("Rebuilding the census of {kinds:?}");
        // count every partition, as `thoradm census new` would
        let cmd = NewCensus {
            census_kinds: kinds.clone(),
            multiplier: 100,
            dry_run: false,
        };
        let outcome = crate::census::new(&cmd, &self.s3_conf, self.workers).await;
        // a census that couldn't count everything leaves listings short, so it is recorded
        let failed = match outcome {
            Ok(RunOutcome::Complete) => None,
            Ok(_) => Some("the census logged errors, so some listings may be short".to_owned()),
            Err(error) => Some(error.to_string()),
        };
        if let Some(error) = failed {
            let record = FailureRecord::failed("census", Operation::PostRestore, error);
            log.record(&record).await?;
        }
        Ok(kinds.iter().map(ToString::to_string).collect())
    }
}

/// What a restore did once its data was written
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreDone {
    /// How many views were recreated, or `None` when they couldn't be
    pub views_recreated: Option<usize>,
    /// The census kinds rebuilt
    pub census: Vec<String>,
    /// Whether the API restored the backup's Redis state, clearing the Redis database first
    pub redis_restored: bool,
    /// What the restore kept of the target in place of the backup's (`user <name>`, `table nodes`)
    pub kept: Vec<String>,
}

/// The attempts per S3 upload when the restore names none, as `--retries` defaults to
const DEFAULT_ATTEMPTS: u32 = 5;

/// The S3 uploads in flight at once when the restore names no bound, as `--s3-concurrency`
/// defaults to
const DEFAULT_UPLOADS: usize = 64;

/// The table components a restore loads, in order: those that link or name others come after them
const TABLE_STEPS: [BackupComponents; 15] = [
    BackupComponents::SamplesList,
    BackupComponents::S3Ids,
    BackupComponents::Comments,
    BackupComponents::Results,
    BackupComponents::ResultsStream,
    BackupComponents::Tags,
    BackupComponents::RepoData,
    BackupComponents::RepoList,
    BackupComponents::Commitish,
    BackupComponents::CommitishList,
    BackupComponents::Nodes,
    BackupComponents::NetworkPolicies,
    BackupComponents::Entities,
    BackupComponents::Associations,
    BackupComponents::Notifications,
];

/// The S3 components a restore uploads, once the tables naming their objects are loaded (the
/// graphics bucket's objects need no table)
const OBJECT_STEPS: [BackupComponents; 4] = [
    BackupComponents::S3IdsObjects,
    BackupComponents::CommentAttachments,
    BackupComponents::ResultFiles,
    BackupComponents::Graphics,
];

/// Builds a fresh report directory for one run under the backup it reads
///
/// # Arguments
///
/// * `backup` - The backup being read
/// * `operation` - The run's operation (`restore`, `scrub`)
pub async fn report_dir(backup: &Path, operation: &str) -> Result<PathBuf, Error> {
    // name the directory for the operation and the UTC time it started
    let started = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
    let dir = backup
        .join("reports")
        .join(format!("{operation}-{started}"));
    // create it, private like the rest of the backup, so the failures file can be opened in it
    create_private_dir(&dir).await?;
    Ok(dir)
}

/// The name of the summary a restore writes in its report directory
pub const SUMMARY_FILE: &str = "summary.json";

/// The step a restore leaves to the admin: the search indexes aren't rebuilt from the restored data
pub const REINDEX_STEP: &str = "Elastic isn't rebuilt: run the search-streamer with --reindex to \
     rebuild the search indexes from the restored data (see the admin guide, How Restore Uses a \
     Backup)";

/// What a restore did, logged when it ends and written as `summary.json` in its report directory
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreSummary {
    /// The backup restored
    pub backup: PathBuf,
    /// The namespace restored into, from thoradm's config
    pub namespace: String,
    /// The backup's format version, or `None` for a legacy backup
    pub format_version: Option<u32>,
    /// Whether the backup was restored as legacy (`--legacy`), without a completeness check
    pub legacy: bool,
    /// The tables and buckets that already held data, which `--merge` restored into
    pub merged_into: Vec<String>,
    /// What `--wipe` truncated before the restore, when the target held data
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wipe: Option<WipeReport>,
    /// What each component restored, skipped, and failed
    pub components: BTreeMap<String, ComponentCounts>,
    /// The restore's own failures file
    pub failures_file: PathBuf,
    /// How many failed or skipped objects the restore itself recorded
    pub failures: u64,
    /// The objects the backup is missing, which `--accept-failures` accepted
    pub missing_from_backup: Vec<FailureRecord>,
    /// The broken links the backup accepted when it was taken, known without `--accept-failures`
    #[serde(default)]
    pub broken_links: Vec<FailureRecord>,
    /// How many views the restore recreated, or `None` when they couldn't be
    pub views_recreated: Option<usize>,
    /// The census kinds the restore rebuilt
    pub census: Vec<String>,
    /// What the restore kept of the target in place of the backup's: its operator users
    /// (`user <name>`) and its nodes (`table nodes`)
    #[serde(default)]
    pub kept: Vec<String>,
    /// What the restore leaves to the admin
    pub not_done: Vec<String>,
    /// What the restore did beyond restoring the backup, or couldn't restore as it was
    #[serde(default)]
    pub warnings: Vec<String>,
    /// When the restore started
    pub started: DateTime<Utc>,
    /// When the restore finished
    pub finished: DateTime<Utc>,
}

impl RestoreSummary {
    /// Logs the summary, a line per fact an admin needs after the restore
    pub fn log(&self) {
        // where it went and what it was
        info!(
            "restore summary: {} restored into namespace {}; {} failed or skipped objects, listed in {}",
            self.backup.display(),
            self.namespace,
            self.failures,
            self.failures_file.display()
        );
        for (component, counts) in &self.components {
            info!(
                "restore summary: {component}: {} restored, {} skipped, {} failed",
                counts.handled, counts.skipped, counts.failed
            );
        }
        // what the flags accepted
        if self.legacy {
            warn!(
                "restore summary: restored as a legacy backup (--legacy); its completeness \
                 couldn't be checked"
            );
        }
        if !self.merged_into.is_empty() {
            warn!(
                "restore summary: merged into data the target held (--merge): {}",
                self.merged_into.join(", ")
            );
        }
        if let Some(wipe) = &self.wipe {
            warn!(
                "restore summary: wiped namespace {} first (--wipe): {} tables truncated, no \
                 object deleted (buckets {} left untouched), kept {}",
                wipe.namespace,
                wipe.tables.iter().filter(|table| table.truncated).count(),
                wipe.buckets_left.join(", "),
                wipe.kept.join(", ")
            );
            // the truncate snapshots it left hold the wiped data and its disk
            if !wipe.snapshots.is_empty() {
                let bytes: u64 = wipe.snapshots.iter().map(|snapshot| snapshot.total).sum();
                warn!(
                    "restore summary: the wipe left {} truncate snapshots, which hold the wiped \
                     data and its disk ({bytes} bytes):",
                    wipe.snapshots.len()
                );
                for snapshot in &wipe.snapshots {
                    warn!("restore summary:   {}", wipe.describe_snapshot(snapshot));
                }
            }
            if wipe.auto_snapshot != Some(false) {
                warn!("restore summary: {}", wipe.clear_snapshots_step());
            }
        }
        match self.views_recreated {
            Some(count) => info!("restore summary: {count} views recreated"),
            None => warn!("restore summary: the views couldn't be recreated"),
        }
        if !self.census.is_empty() {
            info!(
                "restore summary: census rebuilt for {}",
                self.census.join(", ")
            );
        }
        for kept in &self.kept {
            info!("restore summary: kept the target's {kept}");
        }
        for step in &self.not_done {
            warn!("restore summary: not done: {step}");
        }
        for warning in &self.warnings {
            warn!("restore summary: warning: {warning}");
        }
        if !self.missing_from_backup.is_empty() {
            warn!(
                "restore summary: the backup is missing {} objects it recorded as failed \
                 (--accept-failures):",
                self.missing_from_backup.len()
            );
            for record in &self.missing_from_backup {
                warn!(
                    "restore summary: missing {} {} {}",
                    record.component,
                    record
                        .table
                        .as_deref()
                        .or(record.bucket.as_deref())
                        .unwrap_or("-"),
                    record
                        .key
                        .as_deref()
                        .or(record.object.as_deref())
                        .unwrap_or("-")
                );
            }
        }
        // the links the backup accepted are known, so they are listed without a flag
        if !self.broken_links.is_empty() {
            warn!(
                "restore summary: the backup accepted {} broken links, references to objects the \
                 cluster it was taken from lacked, which the restored rows keep:",
                self.broken_links.len()
            );
            for record in &self.broken_links {
                warn!(
                    "restore summary: broken link {} {} {} named by {}",
                    record.component,
                    record.bucket.as_deref().unwrap_or("-"),
                    record.key.as_deref().unwrap_or("-"),
                    record.referrer.as_deref().unwrap_or("-")
                );
            }
        }
    }
}

/// Returns what a restore of a backup did beyond restoring it, or couldn't restore as it was
///
/// # Arguments
///
/// * `config` - The config of the cluster restored
/// * `args` - The restore's arguments
/// * `redis_restored` - Whether the API restored the backup's Redis state
/// * `format` - The backup's format version
async fn restore_warnings(
    config: &Conf,
    args: &RestoreBackup,
    redis_restored: bool,
    format: u32,
) -> Vec<String> {
    // the backup the restore read, whose files say what it held
    let backup = args.backup.as_path();
    let mut warnings = Vec::new();
    // the Redis restore, when it ran with a flush, deleted every key of the database
    if redis_restored && args.redis_flush_all {
        warnings.push(format!(
            "the Redis restore flushed every key in the Redis database at {}:{} with FLUSHDB \
             ASYNC (--redis-flush-all) before loading the backup, namespace {ns}'s job queues and \
             reaction state and other namespaces' keys included",
            config.redis.host,
            config.redis.port,
            ns = config.thorium.namespace
        ));
    } else if redis_restored {
        // the Redis restore deleted the namespace's runtime state with its keys
        warnings.push(format!(
            "the Redis restore deleted every key of namespace {ns} at {}:{} before loading the \
             backup, its job queues and reaction state included",
            config.redis.host,
            config.redis.port,
            ns = config.thorium.namespace
        ));
    } else if tokio::fs::try_exists(backup.join("redis.json"))
        .await
        .unwrap_or(false)
    {
        // a flagged Redis restore that failed after its flush deleted every key of the database
        if args.redis_flush_all {
            warnings.push(format!(
                "the Redis restore was asked to flush every key in the Redis database at {}:{} \
                 with FLUSHDB ASYNC (--redis-flush-all) and failed; when its error names a step \
                 after the flush step, every key in that database was deleted, namespace {ns}'s \
                 and other namespaces' alike, and when it names the flush step, the flush may \
                 still have run (Redis can run FLUSHDB before the connection drops), so check the \
                 database before assuming its keys are intact",
                config.redis.host,
                config.redis.port,
                ns = config.thorium.namespace
            ));
        }
        // a Redis restore that failed at or before its users step leaves no user to log in as
        warnings.push(format!(
            "the Redis restore failed; when its error names the wipe, init, settings, or users \
             step, namespace {} may hold no users, so the same restore can't run again until an \
             admin is bootstrapped with the API's secret key and the Thorctl config uses that \
             admin (see the admin guide, How Restore Uses a Backup)",
            config.thorium.namespace
        ));
    }
    // format 1 keeps no notification TTLs, so its restored notifications take the table default
    if format < 2
        && tokio::fs::try_exists(backup.join("notifications"))
            .await
            .unwrap_or(false)
    {
        warnings.push(format!(
            "restored notifications take the notifications table's default TTL, since this \
             backup's format ({format}) keeps no notification TTLs; notifications that would have \
             expired sooner stay until the default expires them, and permanent ones expire with it"
        ));
    }
    // format 1 holds neither graphics nor entity blobs, nor the columns format 2 added
    if format < 2 {
        warnings.push(format!(
            "this backup's format ({format}) holds no graphics and no result entity blobs, and \
             none of the columns format 2 added (results.tool_version, results.entities, \
             results_stream.tool_version, repos_list.default_checkout, repos_list.earliest, \
             associations.submissions_source, associations.submissions_other), so restored rows \
             leave those columns unset and the target's graphics and entity blobs are not \
             replaced from it"
        ));
    }
    warnings
}

/// Restores a backup to a cluster and returns how the run ended
///
/// Every preflight check runs before the first write; a refusal is recorded and changes nothing.
///
/// # Arguments
///
/// * `config` - The Thorium config of the cluster to restore
/// * `ctl_conf` - The Thorctl config to restore the Redis state with
/// * `scylla` - The Scylla client to use
/// * `workers` - The number of workers per component
/// * `restore_args` - The `backup restore` arguments
/// * `sinks` - The file-system seam the restore's own files are written through
pub async fn run(
    config: &Conf,
    ctl_conf: CtlConf,
    scylla: &Arc<Session>,
    workers: usize,
    restore_args: &RestoreBackup,
    sinks: Arc<dyn SinkFactory>,
) -> Result<RunOutcome, Error> {
    // record failures in a report directory so the backup's own files never change
    let started = Utc::now();
    let reports = preflight::report_dir(restore_args).await?;
    let policy = FailurePolicy::from_flag(restore_args.abort_on_failure);
    let log = Arc::new(FailureLog::open(&reports, policy, sinks.as_ref()).await?);
    // check the backup, the target, and the admin's confirmation before anything is written
    let verdict = Box::pin(preflight::run(
        config,
        &ctl_conf,
        scylla,
        workers,
        restore_args,
        &log,
        &reports,
        sinks.as_ref(),
    ))
    .await?;
    let cleared = match verdict {
        Verdict::Cleared(cleared) => cleared,
        Verdict::Refused => {
            warn!(
                "restore refused before writing anything; the reason is in {}",
                log.path().display()
            );
            return Ok(RunOutcome::Aborted);
        }
    };
    // the target's operator users, which the Redis restore keeps, read before anything is written
    let kept_users = preflight::read_kept_users(&ctl_conf, restore_args).await?;
    // a --wipe restore empties the target's backed-up data first; a wipe that fails stops here,
    // before anything is restored
    let wiped = match &cleared.wipe {
        Some(plan) => {
            let snapshots = ScyllaSnapshots { scylla };
            let wiper = Wiper {
                scylla,
                snapshots: &snapshots,
                target: &cleared.target,
                log: &log,
                reports: &reports,
                sinks: sinks.as_ref(),
                retry: RetryPolicy::new(restore_args.retries),
            };
            match wiper.run(plan.clone()).await? {
                Some(report) => Some(report),
                None => {
                    warn!(
                        "the wipe failed partway, so nothing was restored; what it truncated is in \
                         {}, and a re-run with --wipe {} resumes it",
                        reports.join(WIPE_FILE).display(),
                        plan.namespace
                    );
                    return Ok(RunOutcome::Aborted);
                }
            }
        }
        None => None,
    };
    // build the controller for this cluster, reading a legacy backup as format 1
    let concurrency = usize::try_from(restore_args.s3_concurrency).unwrap_or(usize::MAX);
    let format = cleared
        .manifest
        .as_ref()
        .map_or(1, |manifest| manifest.format_version);
    let mut controller = RestoreController::new(config, ctl_conf, scylla, workers)
        .with_views(cleared.views.clone())
        .with_transfers(restore_args.retries, concurrency)
        .with_format(format)
        .with_redis_flush_all(restore_args.redis_flush_all)
        .with_kept_users(kept_users.clone());
    // retore this cluster from disk
    let done = controller.restore(&restore_args.backup, &log).await?;
    // sum up what was restored and what the flags accepted
    let summary = RestoreSummary {
        backup: restore_args.backup.clone(),
        namespace: config.thorium.namespace.clone(),
        format_version: cleared
            .manifest
            .as_ref()
            .map(|manifest| manifest.format_version),
        legacy: cleared.legacy,
        merged_into: cleared.merged_into.clone(),
        wipe: wiped,
        components: log.all_counts(),
        failures_file: log.path().to_path_buf(),
        failures: log.total(),
        missing_from_backup: cleared.missing.clone(),
        broken_links: cleared.broken_links.clone(),
        views_recreated: done.views_recreated,
        census: done.census,
        kept: done.kept,
        not_done: vec![REINDEX_STEP.to_owned()],
        warnings: restore_warnings(config, restore_args, done.redis_restored, format)
            .await
            .into_iter()
            .chain(cleared.version_mismatch.as_ref().map(version::accepted))
            .chain(kept_users.unread)
            .collect(),
        started,
        finished: Utc::now(),
    };
    manifest::write_json_atomic(&reports, SUMMARY_FILE, &summary, sinks.as_ref()).await?;
    summary.log();
    // the outcome follows the records the run made, not the ones the backup made
    let outcome = RunOutcome::finished(&log);
    super::backup::log_outcome("restore", outcome, &log);
    Ok(outcome)
}

/// Handle a restore sub comamnd
///
/// # Arguments
///
/// * `restore_args` - The args for the restore handler
/// * `args` - The Thoradm args
pub async fn handle(restore_args: &RestoreBackup, args: &Args) -> Result<RunOutcome, Error> {
    // load our config
    let config = Conf::new(&args.cluster_conf)?;
    // load our Thorctl config
    let ctl_conf = CtlConf::from_path(&args.ctl_conf)?;
    // build a new scylla client
    let scylla = Arc::new(utils::get_scylla_client(&config).await?);
    // restore through the real file system
    Box::pin(run(
        &config,
        ctl_conf,
        &scylla,
        args.workers,
        restore_args,
        Arc::new(TokioFs),
    ))
    .await
}

#[cfg(test)]
mod tests {
    use super::restore_warnings;
    use crate::args::RestoreBackup;
    use clap::Parser;
    use std::path::{Path, PathBuf};
    use thorium::Conf;

    /// Loads the test config the integration tests start from
    fn conf() -> Conf {
        // the template, overlaid with any THORIUM__ variables
        Conf::new("../api/tests/thorium-testing.yml").expect("failed to load the test config")
    }

    /// Makes an empty backup directory of its own and returns it
    ///
    /// # Arguments
    ///
    /// * `name` - What the test names its directory after
    fn backup_dir(name: &str) -> PathBuf {
        // a fresh directory under the temp dir
        let dir = std::env::temp_dir().join(format!(
            "thoradm-restore-warnings-{name}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Parses restore arguments for a backup
    ///
    /// # Arguments
    ///
    /// * `backup` - The backup directory
    /// * `extra` - Flags after the backup
    fn args(backup: &Path, extra: &[&str]) -> RestoreBackup {
        // the backup, then the given flags
        let mut argv = vec!["restore".into(), "-b".into(), backup.as_os_str().to_owned()];
        argv.extend(extra.iter().map(Into::into));
        RestoreBackup::parse_from(argv)
    }

    /// A format-1 or legacy restore warns once that the backup holds no graphics, no entity blobs,
    /// and none of the columns format 2 added; a format-2 restore doesn't
    #[tokio::test]
    async fn an_older_format_warns_about_what_it_never_held() {
        // an empty backup restored as format 1, then as format 2
        let dir = backup_dir("format");
        let args = args(&dir, &[]);
        let older = restore_warnings(&conf(), &args, false, 1).await;
        let current = restore_warnings(&conf(), &args, false, 2).await;
        let _ = std::fs::remove_dir_all(&dir);
        let names = |warning: &String| {
            warning.contains("graphics")
                && warning.contains("entity blobs")
                && warning.contains("results.tool_version")
        };
        assert_eq!(older.iter().filter(|w| names(w)).count(), 1, "{older:?}");
        assert!(
            older.iter().all(|warning| !warning.contains("default TTL")),
            "{older:?}"
        );
        assert!(!current.iter().any(names), "{current:?}");
    }

    /// A failed `--redis-flush-all` restore never claims its flush step deleted nothing, since
    /// Redis can run the flush before the connection drops
    #[tokio::test]
    async fn a_failed_flush_never_claims_nothing_was_deleted() {
        // a backup with Redis state whose flagged Redis restore failed
        let dir = backup_dir("flush");
        std::fs::write(dir.join("redis.json"), "{}").unwrap();
        let args = args(&dir, &["--redis-flush-all"]);
        let warnings = restore_warnings(&conf(), &args, false, 2).await;
        let _ = std::fs::remove_dir_all(&dir);
        let flush = warnings
            .iter()
            .find(|warning| warning.contains("--redis-flush-all"))
            .unwrap_or_else(|| panic!("no flush warning in {warnings:?}"));
        assert!(flush.contains("may still have run"), "{flush}");
        assert!(!flush.contains("nothing was deleted"), "{flush}");
    }
}
