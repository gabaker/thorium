//! The backup controller for Thorium
use async_walkdir::Filtering;
use async_walkdir::WalkDir;
use chrono::Utc;
use data_encoding::HEXLOWER;
use futures::stream::FuturesUnordered;
use futures::stream::StreamExt;
use indicatif::MultiProgress;
use indicatif::ProgressBar;
use indicatif::ProgressStyle;
use kanal::AsyncReceiver;
use kanal::AsyncSender;
use rkyv::Archive;
use rkyv::validation::validators::DefaultValidator;
use scylla::client::session::Session;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use strum::IntoEnumIterator;
use thorium::Conf;
use thorium::CtlConf;
use thorium::Thorium;
use thorium::models::{BackupParams, BackupProblem, BackupProblems, PrunedReference};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use super::utils;
use crate::Error;
use crate::args::Args;
use crate::args::BackupComponents;
use crate::args::NewBackup;
use crate::backup::Utils;
use crate::backup::coverage::{Registry, Store};
use crate::backup::failures::{FAILURES_FILE, FailureLog, FailurePolicy, FailureRecord, Operation};
use crate::backup::graphics::Graphic;
use crate::backup::manifest::{
    self, FORMAT_VERSION, INDEX_FILE, InProgressMarker, LOCK_FILE, Layout, MANIFEST_FILE,
    MARKER_FILE, Manifest, PRUNED_FILE, PrunedEntry, RedisEntry, S3Entry, TableEntry, file_sha256,
    write_atomic,
};
use crate::backup::new_backup::TableTally;
use crate::backup::s3::S3BackupShared;
use crate::backup::s3::{ComponentIndex, STAGING_DIR, clear_staging, remove_staging, sync_dirs};
use crate::backup::sink::{
    PRIVATE_DIR_MODE, PRIVATE_FILE_MODE, SinkFactory, TokioFs, create_private_dir,
};
use crate::backup::tables::Association;
use crate::backup::tables::Entity;
use crate::backup::tables::NetworkPolicy;
use crate::backup::tables::Notification;
use crate::backup::tables::{
    Comment, Commitish, CommitishList, Node, Output, OutputStream, RepoData, RepoList, S3Id,
    SamplesList, Tag,
};
use crate::backup::target::bucket_for;
use crate::backup::version;
use crate::backup::{
    Backup, BackupWorker, Monitor, MonitorUpdate, S3Backup, S3BackupWorker, S3Order,
};
use crate::outcome::RunOutcome;
use crate::shared::progress::bars;
use crate::shared::retry::RetryPolicy;
use crate::shared::scylla::build_token_ranges;
use crate::shared::workers::{WorkerHandle, join_workers, spawn_worker};

/// The file the Redis state is written to; it holds every password hash and API token, so it is
/// created 0600
const REDIS_FILE: &str = "redis.json";

/// A singular table backup
pub struct TableBackup<B: Backup> {
    /// The component this table is backed up as
    component: BackupComponents,
    /// The namespace for this table
    namespace: String,
    /// The scylla client for this table
    scylla: Arc<Session>,
    /// The kanal channel workers should send kanal channel updates over
    updates_tx: AsyncSender<MonitorUpdate>,
    /// The kanal channel to receive archive map updates on
    updates_rx: AsyncReceiver<MonitorUpdate>,
    /// The kanal channel to send orders to workers on
    orders_tx: AsyncSender<(i64, i64)>,
    /// The kanal channel workers should get orders on
    orders_rx: AsyncReceiver<(i64, i64)>,
    /// A progress bar for this tables backup
    progress: MultiProgress,
    /// The number of workers to spawn
    worker_count: usize,
    /// The currently active workers
    active: FuturesUnordered<WorkerHandle<BackupWorker<B>, (i64, i64)>>,
    /// The file-system seam workers write archives through
    sinks: Arc<dyn SinkFactory>,
    /// How a page that fails to arrive is retried
    retry: RetryPolicy,
    /// The archives and range counts the workers listed for the manifest
    tally: Arc<Mutex<TableTally>>,
}

impl<B: Backup> TableBackup<B> {
    /// Create a new table backup
    ///
    /// # Arguments
    ///
    /// * `component` - The component this table is backed up as
    /// * `namespace` - The namespace of the table to back up
    /// * `scylla` - The Scylla client to use
    /// * `workers` - The number of workers to use
    /// * `sinks` - The file-system seam workers write archives through
    /// * `retry` - How a page that fails to arrive is retried
    pub fn new<T: Into<String>>(
        component: BackupComponents,
        namespace: T,
        scylla: &Arc<Session>,
        workers: usize,
        sinks: &Arc<dyn SinkFactory>,
        retry: RetryPolicy,
    ) -> Self {
        // build our kanal channel for monitor updates
        let (updates_tx, updates_rx) = kanal::unbounded_async();
        // build our kanal channel for orders
        let (orders_tx, orders_rx) = kanal::unbounded_async();
        // build our table backup object
        TableBackup {
            component,
            namespace: namespace.into(),
            scylla: scylla.clone(),
            updates_tx,
            updates_rx,
            orders_tx,
            orders_rx,
            progress: bars().clone(),
            worker_count: workers,
            active: FuturesUnordered::default(),
            sinks: sinks.clone(),
            retry,
            tally: Arc::default(),
        }
    }

    /// Returns this table's manifest entry from what its workers listed
    ///
    /// # Arguments
    ///
    /// * `log` - The run's failure log, which counts the table's skipped rows
    fn entry(&self, log: &FailureLog) -> TableEntry {
        // take what the workers listed, in a stable order
        let tally = self
            .tally
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut archives = tally.archives.clone();
        archives.sort_by_key(|archive| archive.uuid);
        let mut ranges = tally.ranges.clone();
        ranges.sort_by_key(|range| range.start);
        drop(tally);
        // the columns the registry says this table's archives carry
        let columns = Registry::classify(Store::ScyllaTable, B::name())
            .and_then(|entry| entry.columns)
            .map_or_else(Vec::new, |columns| {
                columns
                    .archived
                    .iter()
                    .map(|column| (*column).to_owned())
                    .collect()
            });
        TableEntry {
            rows: archives.iter().map(|archive| archive.rows).sum(),
            archives,
            ranges,
            skipped: log.counts(&self.component.to_string()).skipped,
            columns,
        }
    }

    /// Build a single backup worker
    ///
    /// # Arguments
    ///
    /// * `path` - The path for this worker to store archives at
    /// * `log` - The run's failure log
    async fn spawn_workers(&self, path: &Path, log: &Arc<FailureLog>) -> Result<(), Error> {
        let path = path.to_path_buf();
        // put the archives in a data folder and put the maps in a map folder
        let data_path = path.join("data");
        let map_path = path.join("maps");
        // create our data dir
        create_private_dir(&data_path).await?;
        create_private_dir(&map_path).await?;
        // build the style for our progress bar
        let bar_style = ProgressStyle::with_template(
            "{spinner:.green} Backed Up Rows: {msg} {bytes} {binary_bytes_per_sec}",
        )
        .unwrap()
        .tick_strings(&[
            "🦀🌽     📦",
            " 🦀🌽    📦",
            "  🦀🌽   📦",
            "   🦀🌽  📦",
            "    🦀🌽 📦",
            "     🦀🌽📦",
            "       🦀📦",
            "      🦀 📦",
            "     🦀  📦",
            "    🦀   📦",
            "   🦀    📦",
            "  🦀     📦",
            " 🦀      📦",
            "🦀       📦",
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
            let worker = BackupWorker::<B>::new(
                &self.scylla,
                &self.namespace,
                self.updates_tx.clone(),
                &data_path,
                &map_path,
                bar,
                &self.sinks,
                self.component.to_string(),
                log,
                self.retry,
                &self.tally,
            )
            .await?;
            // spawn this worker on its share of the orders
            let handle = spawn_worker(&self.orders_rx, |orders| worker.start(orders));
            // add this task to our futures set
            self.active.push(handle);
        }
        Ok(())
    }

    /// Start our monitor
    pub fn start_monitor(&mut self) -> JoinHandle<()> {
        // build the style for our progress bar
        let bar_style = ProgressStyle::with_template(
            "{spinner:.green} {elapsed_precise} Total Backed Up Rows: {msg} {bytes} {binary_bytes_per_sec}",
        )
        .unwrap()
        .tick_strings(&[
            "🦀📋       ",
            " 🦀📋      ",
            "  🦀📋     ",
            "   🦀📋    ",
            "    🦀📋   ",
            "     🦀📋  ",
            "       🦀📋",
            "      🦀📋 ",
            "     🦀📋  ",
            "    🦀📋   ",
            "   🦀📋    ",
            "  🦀📋     ",
            " 🦀📋      ",
            "🦀📋       ",
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

    /// Start sending token segments to our workers
    ///
    /// # Arguments
    ///
    /// * `chunk_count` - The number of chunks to break our token ring into
    /// * `log` - The run's failure log, whose stop signal ends the orders early
    async fn start(&mut self, chunk_count: u64, log: &FailureLog) -> Result<(), Error> {
        // split the ring into contiguous ranges and send them in ring order
        for segment in build_token_ranges(chunk_count) {
            // send no more orders once --abort-on-failure stopped the run
            if log.stopped() {
                break;
            }
            // add this segment to our channel
            self.orders_tx.send(segment).await?;
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
            .then(|| joined.step_error(&format!("the {} backup", self.component)));
        // shut down the workers that finished
        for worker in std::mem::take(&mut joined.finished) {
            worker.shutdown();
        }
        // record each worker that ended early once, with the range it was reading
        for failure in &joined.errors {
            let record = FailureRecord::worker_ended(
                &self.component.to_string(),
                Operation::ReadRange,
                B::name(),
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

    /// Start backing up this table
    ///
    /// # Arguments
    ///
    /// * `path` - The path to store all of this Thorium clusters backups in
    /// * `chunk_count` - The number of chunks to break our token range into
    /// * `resume` - Whether this run resumes an interrupted backup, whose archives of this table
    ///   are deleted and scanned again
    /// * `log` - The run's failure log
    pub async fn backup(
        &mut self,
        mut path: PathBuf,
        chunk_count: u64,
        resume: bool,
        log: &Arc<FailureLog>,
    ) -> Result<(), Error> {
        // log the table we are backing up
        info!("Backing up {}", B::pretty_name());
        // nest our path by our table name
        path.push(B::name());
        // an interrupted run's archives of this table may be partial, so scan the table again
        if resume {
            for dir in [path.join("data"), path.join("maps")] {
                match tokio::fs::remove_dir_all(&dir).await {
                    Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                        return Err(Error::from(error));
                    }
                    _ => (),
                }
            }
        }
        // start our archive map updater
        let handle = self.start_monitor();
        // build our workers
        self.spawn_workers(&path, log).await?;
        // start backing up data
        self.start(chunk_count, log).await?;
        // wait for all of our workers to finish
        self.wait_for_workers(log).await?;
        // make the table's directories durable now that its archives are synced
        self.sync_dirs(&path, log).await?;
        // tell our monitor to finish and exit
        self.updates_tx.send(MonitorUpdate::Finished).await?;
        // wait for our map updater to finish
        handle.await?;
        // log what this component handled, skipped, and failed
        log.summarize(&self.component.to_string());
        Ok(())
    }

    /// Syncs a table's directory and the backup root, so the directories its archives live in
    /// survive a crash; a directory that can't be synced is recorded
    ///
    /// # Arguments
    ///
    /// * `table_dir` - The table's directory in the backup
    /// * `log` - The run's failure log
    async fn sync_dirs(&self, table_dir: &Path, log: &FailureLog) -> Result<(), Error> {
        // the table's directory holds data/ and maps/; the root holds the table's directory
        for dir in [Some(table_dir), table_dir.parent()].into_iter().flatten() {
            if let Err(error) = self.sinks.sync_dir(dir).await {
                let record = FailureRecord::failed(
                    self.component.to_string(),
                    Operation::WriteArchive,
                    format!("{} couldn't be synced to disk: {error}", dir.display()),
                )
                .table(B::name())
                .key(dir.to_string_lossy());
                log.record(&record).await?;
            }
        }
        Ok(())
    }
}

impl<B: Backup> std::fmt::Debug for TableBackup<B> {
    /// Allow TableBackup to be debug printed
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TableBackup")
            .field("namespace", &self.namespace)
            .field("table", &B::name())
            .finish()
    }
}

/// A singular table backup
pub struct S3BackupController<S: S3Backup> {
    /// The component these objects are backed up as
    component: BackupComponents,
    /// The namespace for this table
    namespace: String,
    /// The Thorium config for the cluster we are backing up
    conf: Conf,
    /// The kanal channel workers should send kanal channel updates over
    updates_tx: AsyncSender<MonitorUpdate>,
    /// The kanal channel to receive archive map updates on
    updates_rx: AsyncReceiver<MonitorUpdate>,
    /// The kanal channel to send orders to workers on
    orders_tx: AsyncSender<S3Order>,
    /// The kanal channel workers should get orders on
    orders_rx: AsyncReceiver<S3Order>,
    /// A progress bar for this tables backup
    progress: MultiProgress,
    /// The number of workers to spawn
    worker_count: usize,
    /// The currently active workers
    active: FuturesUnordered<WorkerHandle<S3BackupWorker<S>, S3Order>>,
    /// What every worker shares: the client, the file-system seam, retries, and the bound on
    /// downloads in flight
    shared: S3BackupShared,
    /// This component's manifest entry, once it has run
    entry: Option<S3Entry>,
}

impl<S: S3Backup> S3BackupController<S> {
    /// Create a new table backup
    ///
    /// # Arguments
    ///
    /// * `component` - The component these objects are backed up as
    /// * `namespace` - The namespace of the table to back up
    /// * `conf` - The Thorium config for the cluster we are backing up
    /// * `workers` - The number of workers to use
    /// * `shared` - What every worker shares, including the run-wide bound on downloads
    pub fn new<T: Into<String>>(
        component: BackupComponents,
        namespace: T,
        conf: &Conf,
        workers: usize,
        shared: &S3BackupShared,
    ) -> Self {
        // build our kanal channel for monitor updates
        let (updates_tx, updates_rx) = kanal::unbounded_async();
        // build our kanal channel for orders
        let (orders_tx, orders_rx) = kanal::unbounded_async();
        // build our table backup object
        S3BackupController {
            component,
            namespace: namespace.into(),
            conf: conf.clone(),
            updates_tx,
            updates_rx,
            orders_tx,
            orders_rx,
            progress: bars().clone(),
            worker_count: workers,
            active: FuturesUnordered::default(),
            shared: shared.clone(),
            entry: None,
        }
    }

    /// Spawn our s3 backup workers
    ///
    /// # Arguments
    ///
    /// * `path` - The component's backup directory
    /// * `index` - The component's object index
    /// * `log` - The run's failure log
    async fn spawn_workers(
        &self,
        path: &mut PathBuf,
        index: &Arc<ComponentIndex>,
        log: &Arc<FailureLog>,
    ) -> Result<(), Error>
    where
        <S as Archive>::Archived:
            for<'a> bytecheck::CheckBytes<DefaultValidator<'a>> + std::fmt::Debug,
    {
        // build the path to our object directory
        path.push("objects");
        // create the dir for object storage if doesn't yet exist
        create_private_dir(path).await?;
        // build the style for our progress bar
        let bar_style = ProgressStyle::with_template(
            "{spinner:.green} Backed Up S3 Objects: {msg} {bytes} {binary_bytes_per_sec}",
        )
        .unwrap()
        .tick_strings(&[
            "🦀🌽     📦",
            " 🦀🌽    📦",
            "  🦀🌽   📦",
            "   🦀🌽  📦",
            "    🦀🌽 📦",
            "     🦀🌽📦",
            "       🦀📦",
            "      🦀 📦",
            "     🦀  📦",
            "    🦀   📦",
            "   🦀    📦",
            "  🦀     📦",
            " 🦀      📦",
            "🦀       📦",
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
            let worker = S3BackupWorker::<S>::new(
                &self.conf,
                &self.updates_tx,
                bar,
                path,
                &self.shared,
                index,
                self.component,
                log,
            );
            // spawn this worker on its share of the orders
            let handle = spawn_worker(&self.orders_rx, |orders| worker.backup(orders));
            // add this task to our futures set
            self.active.push(handle);
        }
        Ok(())
    }

    /// Start our global progress tracker
    fn start_global_tracker(&self) -> JoinHandle<()> {
        // clone our update channel
        let update_rx = self.updates_rx.clone();
        // build the style for our progress bar
        let bar_style = ProgressStyle::with_template(
            "{spinner:.green} {elapsed_precise} Total Backed Up S3 Objects: {msg} {bytes} {binary_bytes_per_sec}",
        )
        .unwrap()
        .tick_strings(&[
            "🦀📋       ",
            " 🦀📋      ",
            "  🦀📋     ",
            "   🦀📋    ",
            "    🦀📋   ",
            "     🦀📋  ",
            "       🦀📋",
            "      🦀📋 ",
            "     🦀📋  ",
            "    🦀📋   ",
            "   🦀📋    ",
            "  🦀📋     ",
            " 🦀📋      ",
            "🦀📋       ",
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

    /// Start backing up objects from s3: the objects the table's archives name, or those a
    /// listing of the bucket names
    ///
    /// # Arguments
    ///
    /// * `path` - The path for our workers to store archives at
    /// * `log` - The run's failure log
    async fn start(&mut self, path: &mut PathBuf, log: &FailureLog) -> Result<(), Error> {
        // a component that copies a whole bucket lists it instead of reading archives
        if let Some(role) = S::listed_role() {
            return self.start_listing(role, log).await;
        }
        // pop our objects directory
        path.pop();
        // switch to our map directory
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
            // record any map that couldn't be listed, since its objects won't be backed up
            match entry {
                Ok(entry) => self.orders_tx.send(S3Order::Archive(entry.path())).await?,
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
            .then(|| joined.step_error(&format!("the {} backup", self.component)));
        // shut down the workers that finished
        for worker in std::mem::take(&mut joined.finished) {
            worker.shutdown();
        }
        // record each worker that ended early once, with the map it was copying objects from
        for failure in &joined.errors {
            let record = FailureRecord::worker_ended(
                &self.component.to_string(),
                Operation::Download,
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

    /// Sends the workers every key a listing of a bucket names, a page at a time, recording a
    /// page that can't be listed after its retries, which ends the listing
    ///
    /// # Arguments
    ///
    /// * `role` - The bucket role to list
    /// * `log` - The run's failure log
    async fn start_listing(&mut self, role: &'static str, log: &FailureLog) -> Result<(), Error> {
        // the bucket the config names for the role
        let bucket = bucket_for(&self.conf, role)
            .ok_or_else(|| Error::new(format!("the config names no bucket for {role}")))?;
        let s3 = self.shared.client();
        let mut token: Option<String> = None;
        loop {
            // send no more orders once --abort-on-failure stopped the run
            if log.stopped() {
                return Ok(());
            }
            // list the next page, retrying what may pass
            let listed = self
                .shared
                .retry
                .run(|| async {
                    s3.list_objects_v2()
                        .bucket(&bucket)
                        .set_continuation_token(token.clone())
                        .send()
                        .await
                        .map_err(|error| Error::from_s3(&error))
                })
                .await;
            let page = match listed.result {
                Ok(page) => page,
                Err(error) => {
                    let record = FailureRecord::failed(
                        self.component.to_string(),
                        Operation::Download,
                        format!(
                            "failed to list bucket {bucket}, so the objects it holds past this \
                             point aren't backed up: {error}"
                        ),
                    )
                    .bucket(&bucket)
                    .attempts(listed.attempts);
                    log.record(&record).await?;
                    return Ok(());
                }
            };
            // hand the page's keys to the workers
            let keys: Vec<String> = page
                .contents()
                .iter()
                .filter_map(|object| object.key().map(ToOwned::to_owned))
                .collect();
            if !keys.is_empty() {
                let order = S3Order::Listed {
                    role,
                    bucket: bucket.clone(),
                    keys,
                };
                self.orders_tx.send(order).await?;
            }
            match page.next_continuation_token() {
                Some(next) if page.is_truncated() == Some(true) => token = Some(next.to_owned()),
                _ => return Ok(()),
            }
        }
    }

    /// Start backing up this table
    ///
    /// # Arguments
    ///
    /// * `path` - The path to store all of this Thorium clusters backups in
    /// * `resume` - Whether this run resumes an interrupted backup, whose files of objects this
    ///   run doesn't land are removed
    /// * `log` - The run's failure log
    pub async fn backup(
        &mut self,
        mut path: PathBuf,
        resume: bool,
        log: &Arc<FailureLog>,
    ) -> Result<(), Error>
    where
        <S as Archive>::Archived:
            for<'a> bytecheck::CheckBytes<DefaultValidator<'a>> + std::fmt::Debug,
    {
        // get our name without '_'
        let pretty_name = S::name().replace('_', " ");
        // log the objects we are backing up
        info!("Backing up the objects of {pretty_name}");
        // nest our path by our table name
        path.push(S::name());
        // start the object index, reading the one an interrupted run left
        create_private_dir(&path).await?;
        let index_path = path.join(INDEX_FILE);
        let index =
            Arc::new(ComponentIndex::start(&index_path, self.shared.sinks.as_ref(), resume).await?);
        // claim objects afresh; the components before this one read other buckets
        self.shared.forget_claims();
        // empty the staging directory of downloads an interrupted run never finished
        let staging = path.join(STAGING_DIR);
        clear_staging(&staging).await?;
        let objects = path.join("objects");
        // start our archive map updater
        let handle = self.start_global_tracker();
        // build our workers
        self.spawn_workers(&mut path, &index, log).await?;
        // start backing up data
        self.start(&mut path, log).await?;
        // wait for all of our workers to finish
        self.wait_for_workers(log).await?;
        // every download has finished, so nothing staged is still needed
        remove_staging(&staging).await?;
        // a resumed run that reached its end keeps only the objects it landed; a stopped one
        // keeps every file for the run that resumes it
        if !log.stopped() {
            let removed = index.prune(&objects, self.shared.sinks.as_ref()).await?;
            if removed > 0 {
                info!("Removed {removed} files of objects this backup no longer names");
            }
        }
        // make every move into place durable before the index lists the objects
        sync_dirs(
            self.shared.sinks.as_ref(),
            index.take_unsynced(),
            &self.component.to_string(),
            S::name(),
            log,
        )
        .await?;
        // sync the index and list it, pinned by its sha256, for the manifest
        let (objects, bytes) = index.finish().await?;
        self.entry = Some(S3Entry {
            bucket_roles: self
                .component
                .bucket_roles()
                .iter()
                .map(|role| (*role).to_owned())
                .collect(),
            objects,
            bytes,
            index: format!("{}/{INDEX_FILE}", S::name()),
            index_sha256: file_sha256(&index_path).await?,
        });
        // tell our map updater to finish
        self.updates_tx.send(MonitorUpdate::Finished).await?;
        // wait for our map updater to finish
        handle.await?;
        // log what this component handled, skipped, and failed
        log.summarize(&self.component.to_string());
        Ok(())
    }
}

impl<S: S3Backup> std::fmt::Debug for S3BackupController<S> {
    /// Allow TableBackup to be debug printed
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Backup")
            .field("namespace", &self.namespace)
            .field("table", &S::name())
            .finish()
    }
}

/// The backup controller for a single Thorium cluster
pub struct BackupController {
    /// The Thorctl config
    ctl_conf: CtlConf,
    /// The file-system seam the backup writes through
    sinks: Arc<dyn SinkFactory>,
    /// How failed reads, downloads, and API requests are retried
    retry: RetryPolicy,
    /// Whether this run resumes an interrupted backup in its directory
    resume: bool,
    /// The Redis state's manifest entry, once `redis.json` was written
    redis: Option<RedisEntry>,
    /// Whether the API drops the user and group references that stay dangling
    prune_dangling: bool,
    /// The manifest entry of `pruned.jsonl`, once the API dropped references and it was written
    pruned: Option<PrunedEntry>,
    /// The number of chunks to split our token range into
    chunks: u64,
    /// The samples list table
    samples_list: TableBackup<SamplesList>,
    /// The s3 ids table
    s3_ids: TableBackup<S3Id>,
    /// The comments table
    comments: TableBackup<Comment>,
    /// The results table
    results: TableBackup<Output>,
    /// The results stream table
    results_stream: TableBackup<OutputStream>,
    /// The tags table
    tags: TableBackup<Tag>,
    /// The repo data table
    repo_data: TableBackup<RepoData>,
    /// The repo list table
    repos_list: TableBackup<RepoList>,
    /// The commitish table
    commitish: TableBackup<Commitish>,
    /// The commitish list table
    commitish_list: TableBackup<CommitishList>,
    /// The nodes tables
    nodes: TableBackup<Node>,
    /// The network policies table
    network_policies: TableBackup<NetworkPolicy>,
    /// The entities table
    entities: TableBackup<Entity>,
    /// The associations table
    associations: TableBackup<Association>,
    /// The notifications table
    notifications: TableBackup<Notification>,
    /// The s3 ids S3 backup
    s3_ids_objects: S3BackupController<S3Id>,
    /// The comments S3 backup
    comment_attachments: S3BackupController<Comment>,
    /// The result files S3 backup
    result_files: S3BackupController<Output>,
    /// The graphics S3 backup
    graphics: S3BackupController<Graphic>,
}

impl std::fmt::Debug for BackupController {
    /// Allow a backup controller to be debug printed without its clients and open files
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // show the chunk count and leave out the clients and open files
        f.debug_struct("BackupController")
            .field("chunks", &self.chunks)
            .finish_non_exhaustive()
    }
}

impl BackupController {
    /// Build a new backup controller for a single Thorium cluster
    ///
    /// # Arguments
    ///
    /// * `config` - The Thorium config for the cluster to backup
    /// * `ctl_conf` - The Thorctl config to use for the backup
    /// * `scylla` - The client to use with scylla
    /// * `workers` - The number of workers to spawn
    /// * `multiplier` - The multiplier to use with our worker count
    /// * `sinks` - The file-system seam the backup writes through
    /// * `retry` - How failed reads and downloads are retried
    /// * `s3_concurrency` - The most S3 downloads in flight at once across the run
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: &Conf,
        ctl_conf: CtlConf,
        scylla: &Arc<Session>,
        workers: usize,
        multiplier: u64,
        sinks: Arc<dyn SinkFactory>,
        retry: RetryPolicy,
        s3_concurrency: usize,
    ) -> Self {
        // short names keep one backup per line
        use BackupComponents as Components;
        // every backup reads the same namespace with the same workers, file seam, and retries
        let (ns, count, r) = (&config.thorium.namespace, workers, retry);
        // every S3 component shares one client, built only if one runs, and one bound on
        // downloads in flight
        let s3 = S3BackupShared::new(config, sinks.clone(), retry, s3_concurrency);
        // build the cluster backup controller with one backup per table and per bucket
        BackupController {
            ctl_conf,
            retry,
            resume: false,
            redis: None,
            prune_dangling: false,
            pruned: None,
            chunks: workers as u64 * multiplier,
            samples_list: TableBackup::new(Components::SamplesList, ns, scylla, count, &sinks, r),
            s3_ids: TableBackup::new(Components::S3Ids, ns, scylla, count, &sinks, r),
            comments: TableBackup::new(Components::Comments, ns, scylla, count, &sinks, r),
            results: TableBackup::new(Components::Results, ns, scylla, count, &sinks, r),
            results_stream: TableBackup::new(
                Components::ResultsStream,
                ns,
                scylla,
                count,
                &sinks,
                r,
            ),
            tags: TableBackup::new(Components::Tags, ns, scylla, count, &sinks, r),
            repo_data: TableBackup::new(Components::RepoData, ns, scylla, count, &sinks, r),
            repos_list: TableBackup::new(Components::RepoList, ns, scylla, count, &sinks, r),
            commitish: TableBackup::new(Components::Commitish, ns, scylla, count, &sinks, r),
            commitish_list: TableBackup::new(
                Components::CommitishList,
                ns,
                scylla,
                count,
                &sinks,
                r,
            ),
            nodes: TableBackup::new(Components::Nodes, ns, scylla, count, &sinks, r),
            network_policies: TableBackup::new(
                Components::NetworkPolicies,
                ns,
                scylla,
                count,
                &sinks,
                r,
            ),
            entities: TableBackup::new(Components::Entities, ns, scylla, count, &sinks, r),
            associations: TableBackup::new(Components::Associations, ns, scylla, count, &sinks, r),
            notifications: TableBackup::new(
                Components::Notifications,
                ns,
                scylla,
                count,
                &sinks,
                r,
            ),
            s3_ids_objects: S3BackupController::new(
                Components::S3IdsObjects,
                ns,
                config,
                count,
                &s3,
            ),
            comment_attachments: S3BackupController::new(
                Components::CommentAttachments,
                ns,
                config,
                count,
                &s3,
            ),
            result_files: S3BackupController::new(Components::ResultFiles, ns, config, count, &s3),
            graphics: S3BackupController::new(Components::Graphics, ns, config, count, &s3),
            sinks,
        }
    }

    /// Backup this clusters redis data to disk
    ///
    /// The state is fetched through the API, retrying transient failures (5xx, a reset
    /// connection, a timeout, a truncated or unparsable body). A fetch that still fails, or a
    /// write that fails, is recorded and the rest of the backup carries on. When the API dropped
    /// dangling references, they are listed in `pruned.jsonl`, written before `redis.json` so
    /// a state with references dropped is never kept without its list.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to write this backup too
    /// * `log` - The run's failure log
    async fn backup_redis(&mut self, path: PathBuf, log: &FailureLog) -> Result<(), Error> {
        // the Redis state is one object under its own component name
        let component = BackupComponents::Redis.to_string();
        info!("Backing up redis");
        // create our data dir
        create_private_dir(&path).await?;
        // the state is fetched again, so an interrupted run's files never outlive a failed fetch
        self.sinks.remove(&path.join(REDIS_FILE)).await?;
        self.sinks
            .remove(&path.join(format!("{REDIS_FILE}.tmp")))
            .await?;
        self.sinks.remove(&path.join(PRUNED_FILE)).await?;
        self.sinks
            .remove(&path.join(format!("{PRUNED_FILE}.tmp")))
            .await?;
        self.sinks.sync_dir(&path).await?;
        // fetch the Redis state through the API, retrying what may pass
        let params = BackupParams {
            prune_dangling: self.prune_dangling,
        };
        let fetched = self
            .retry
            .run(|| async {
                let client = Thorium::from_ctl_conf(self.ctl_conf.clone()).await?;
                client.system.backup_with(&params).await
            })
            .await;
        let mut backup = match fetched.result {
            Ok(backup) => backup,
            Err(error) => {
                // records the API couldn't parse or that don't fit together are each recorded under their own key
                let records = match unparsable_records(&error) {
                    Some(problems) => problems
                        .into_iter()
                        .map(|problem| {
                            FailureRecord::failed(&component, Operation::Redis, problem.error)
                                .key(problem.key)
                                .attempts(fetched.attempts)
                        })
                        .collect(),
                    None => vec![
                        FailureRecord::failed(&component, Operation::Redis, error)
                            .key(REDIS_FILE)
                            .attempts(fetched.attempts),
                    ],
                };
                for record in &records {
                    log.record(record).await?;
                }
                log.summarize(&component);
                return Ok(());
            }
        };
        // what the API dropped is listed beside the state, never in it
        let pruned = std::mem::take(&mut backup.pruned);
        if !pruned.is_empty() && !self.write_pruned(&path, &pruned, log).await? {
            log.summarize(&component);
            return Ok(());
        }
        // serialize our backup to json
        // we use json instead of rkyv because datetimes seem to be causing problems
        let backup_str = serde_json::to_string(&backup)?;
        // write it privately and atomically through the seam, recording a write that fails
        match write_atomic(
            &path,
            REDIS_FILE,
            backup_str.as_bytes(),
            self.sinks.as_ref(),
        )
        .await
        {
            Ok(()) => {
                log.handled(&component, 1);
                // list the file and its records per type for the manifest
                let counts = [
                    ("users", backup.users.len()),
                    ("groups", backup.groups.len()),
                    ("images", backup.images.len()),
                    ("pipelines", backup.pipelines.len()),
                ];
                self.redis = Some(RedisEntry {
                    len: backup_str.len() as u64,
                    sha256: HEXLOWER.encode(&Sha256::digest(backup_str.as_bytes())),
                    counts: counts
                        .into_iter()
                        .map(|(kind, count)| (kind.to_owned(), count as u64))
                        .collect(),
                });
            }
            Err(error) => {
                let record = FailureRecord::failed(&component, Operation::WriteArchive, error)
                    .key(path.join(REDIS_FILE).to_string_lossy());
                log.record(&record).await?;
                // without the state, the list of what was dropped from it describes nothing
                if self.pruned.take().is_some() {
                    self.sinks.remove(&path.join(PRUNED_FILE)).await?;
                    self.sinks.sync_dir(&path).await?;
                }
            }
        }
        // log what this component handled and failed
        log.summarize(&component);
        Ok(())
    }

    /// Writes `pruned.jsonl`, one JSON line per change the API made, and logs each change,
    /// returning whether it was written; a write that fails is recorded
    ///
    /// # Arguments
    ///
    /// * `path` - The backup root
    /// * `pruned` - The changes the API made
    /// * `log` - The run's failure log
    async fn write_pruned(
        &mut self,
        path: &Path,
        pruned: &[PrunedReference],
        log: &FailureLog,
    ) -> Result<bool, Error> {
        // one line per change, each logged as the API made it
        let mut lines = String::new();
        for change in pruned {
            let line = serde_json::to_string(change)?;
            warn!("Pruned a dangling reference (--prune-dangling): {line}");
            lines.push_str(&line);
            lines.push('\n');
        }
        // write it privately and atomically, recording a write that fails
        match write_atomic(path, PRUNED_FILE, lines.as_bytes(), self.sinks.as_ref()).await {
            Ok(()) => {
                self.pruned = Some(PrunedEntry {
                    count: pruned.len() as u64,
                    sha256: HEXLOWER.encode(&Sha256::digest(lines.as_bytes())),
                });
                Ok(true)
            }
            Err(error) => {
                let component = BackupComponents::Redis.to_string();
                let record = FailureRecord::failed(&component, Operation::WriteArchive, error)
                    .key(path.join(PRUNED_FILE).to_string_lossy());
                log.record(&record).await?;
                Ok(false)
            }
        }
    }

    /// Backs up one component
    ///
    /// # Arguments
    ///
    /// * `component` - The component to back up
    /// * `path` - The path to write this backup too
    /// * `log` - The run's failure log
    #[rustfmt::skip]
    async fn backup_component(&mut self, component: BackupComponents, path: &Path, log: &Arc<FailureLog>) -> Result<(), Error> {
        let path = path.to_path_buf();
        // run the component's own backup
        match component {
            BackupComponents::All | BackupComponents::NoS3 => Ok(()),
            BackupComponents::Redis => self.backup_redis(path, log).await,
            BackupComponents::Associations => self.associations.backup(path, self.chunks, self.resume, log).await,
            BackupComponents::Comments => self.comments.backup(path, self.chunks, self.resume, log).await,
            BackupComponents::Commitish => self.commitish.backup(path, self.chunks, self.resume, log).await,
            BackupComponents::CommitishList => self.commitish_list.backup(path, self.chunks, self.resume, log).await,
            BackupComponents::Entities => self.entities.backup(path, self.chunks, self.resume, log).await,
            BackupComponents::NetworkPolicies => self.network_policies.backup(path, self.chunks, self.resume, log).await,
            BackupComponents::Nodes => self.nodes.backup(path, self.chunks, self.resume, log).await,
            BackupComponents::Notifications => self.notifications.backup(path, self.chunks, self.resume, log).await,
            BackupComponents::RepoData => self.repo_data.backup(path, self.chunks, self.resume, log).await,
            BackupComponents::RepoList => self.repos_list.backup(path, self.chunks, self.resume, log).await,
            BackupComponents::Results => self.results.backup(path, self.chunks, self.resume, log).await,
            BackupComponents::ResultsStream => self.results_stream.backup(path, self.chunks, self.resume, log).await,
            BackupComponents::S3Ids => self.s3_ids.backup(path, self.chunks, self.resume, log).await,
            BackupComponents::SamplesList => self.samples_list.backup(path, self.chunks, self.resume, log).await,
            BackupComponents::Tags => self.tags.backup(path, self.chunks, self.resume, log).await,
            BackupComponents::CommentAttachments => self.comment_attachments.backup(path, self.resume, log).await,
            BackupComponents::ResultFiles => self.result_files.backup(path, self.resume, log).await,
            BackupComponents::S3IdsObjects => self.s3_ids_objects.backup(path, self.resume, log).await,
            BackupComponents::Graphics => self.graphics.backup(path, self.resume, log).await,
        }
    }

    /// Backup this cluster to disk
    ///
    /// Every table component runs first, then Redis, then every S3 component, because the S3
    /// components find their objects in the archives the table components write (`graphics`
    /// lists its bucket instead). An S3 component whose table component isn't part of this
    /// backup is recorded instead of run.
    /// Each component's failed objects are recorded and the backup carries on, unless
    /// `--abort-on-failure` stopped it. A component that can't run at all (its directory can't be
    /// created, its statement can't be prepared) is recorded and ends the backup.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to write this backup too
    /// * `components` - The components to backup, with `all` and `no-s3` already expanded
    /// * `log` - The run's failure log
    pub async fn backup(
        &mut self,
        path: &Path,
        components: &[BackupComponents],
        log: &Arc<FailureLog>,
    ) -> Result<(), Error> {
        // log the components this backup leaves out
        for component in BackupComponents::iter() {
            if !component.is_filter() && !components.contains(&component) {
                info!("Skipping {component} backup...");
            }
        }
        // tables first, then Redis, then the S3 components that read the tables' archives
        let ordered = BackupComponents::iter()
            .filter(|component| !component.is_in_s3() && *component != BackupComponents::Redis)
            .chain(std::iter::once(BackupComponents::Redis))
            .chain(BackupComponents::iter().filter(|component| component.is_in_s3()))
            .filter(|component| components.contains(component));
        for component in ordered {
            // run no further components once --abort-on-failure stopped the run
            if log.stopped() {
                warn!(
                    "Stopping before {component}: a failure was recorded under --abort-on-failure"
                );
                break;
            }
            // an S3 component can only find its objects in this backup's archives of its table
            if let Some(table) = component.s3_input()
                && !components.contains(&table)
            {
                let record = FailureRecord::failed(
                    component.to_string(),
                    Operation::ReadArchive,
                    format!(
                        "{component} finds its objects in the {table} archives, which this backup \
                         doesn't hold; back up {table} with it"
                    ),
                );
                log.record(&record).await?;
                log.summarize(&component.to_string());
                continue;
            }
            // a component that can't run at all is recorded and ends the backup; a step whose
            // workers ended early has recorded each of them already
            if let Err(error) = self.backup_component(component, path, log).await {
                let operation = match component {
                    BackupComponents::Redis => Operation::Redis,
                    BackupComponents::CommentAttachments
                    | BackupComponents::ResultFiles
                    | BackupComponents::S3IdsObjects
                    | BackupComponents::Graphics => Operation::Download,
                    _ => Operation::WriteArchive,
                };
                if let Some(record) =
                    FailureRecord::component_failed(&component.to_string(), operation, &error)
                {
                    log.record(&record).await?;
                }
                return Err(error);
            }
        }
        Ok(())
    }

    /// Returns the manifest entries of the tables and S3 components a run backed up
    ///
    /// # Arguments
    ///
    /// * `components` - The components the run backed up
    /// * `log` - The run's failure log
    fn entries(
        &self,
        components: &[BackupComponents],
        log: &FailureLog,
    ) -> (BTreeMap<String, TableEntry>, BTreeMap<String, S3Entry>) {
        use BackupComponents as C;
        let (mut tables, mut s3) = (BTreeMap::new(), BTreeMap::new());
        for component in components {
            // each table is listed by its name
            let table = match component {
                C::All
                | C::NoS3
                | C::Redis
                | C::CommentAttachments
                | C::ResultFiles
                | C::S3IdsObjects
                | C::Graphics => None,
                C::Associations => Some((Association::name(), self.associations.entry(log))),
                C::Comments => Some((Comment::name(), self.comments.entry(log))),
                C::Commitish => Some((Commitish::name(), self.commitish.entry(log))),
                C::CommitishList => Some((CommitishList::name(), self.commitish_list.entry(log))),
                C::Entities => Some((Entity::name(), self.entities.entry(log))),
                C::NetworkPolicies => {
                    Some((NetworkPolicy::name(), self.network_policies.entry(log)))
                }
                C::Nodes => Some((Node::name(), self.nodes.entry(log))),
                C::Notifications => Some((Notification::name(), self.notifications.entry(log))),
                C::RepoData => Some((RepoData::name(), self.repo_data.entry(log))),
                C::RepoList => Some((RepoList::name(), self.repos_list.entry(log))),
                C::Results => Some((Output::name(), self.results.entry(log))),
                C::ResultsStream => Some((OutputStream::name(), self.results_stream.entry(log))),
                C::S3Ids => Some((S3Id::name(), self.s3_ids.entry(log))),
                C::SamplesList => Some((SamplesList::name(), self.samples_list.entry(log))),
                C::Tags => Some((Tag::name(), self.tags.entry(log))),
            };
            if let Some((name, entry)) = table {
                tables.insert(name.to_owned(), entry);
            }
            // each S3 component that ran is listed by its component name
            let object_entry = match component {
                C::CommentAttachments => self.comment_attachments.entry.clone(),
                C::ResultFiles => self.result_files.entry.clone(),
                C::S3IdsObjects => self.s3_ids_objects.entry.clone(),
                C::Graphics => self.graphics.entry.clone(),
                _ => None,
            };
            if let Some(entry) = object_entry {
                s3.insert(component.to_string(), entry);
            }
        }
        (tables, s3)
    }
}

/// Logs how a run ended and where its failures are listed
///
/// # Arguments
///
/// * `command` - The command that ran (`backup`, `restore`, `scrub`)
/// * `outcome` - How the run ended
/// * `log` - The run's failure log
pub fn log_outcome(command: &str, outcome: RunOutcome, log: &FailureLog) {
    // one closing line that a script or an admin can read without the progress bars
    match outcome {
        RunOutcome::Complete => info!("{command} complete: nothing failed or was skipped"),
        RunOutcome::CompletedWithFailures => warn!(
            "{command} finished with {} failed or skipped objects, listed in {}",
            log.total(),
            log.path().display()
        ),
        RunOutcome::Aborted => warn!(
            "{command} stopped early under --abort-on-failure; failures are listed in {}",
            log.path().display()
        ),
    }
}

/// Returns the components a backup runs: those asked for, with `all` and `no-s3` expanded, and
/// every component of the interrupted run it resumes
///
/// # Arguments
///
/// * `requested` - The components given on the command line
/// * `resumed` - The marker of the interrupted run being resumed, if any
fn effective_components(
    requested: &[BackupComponents],
    resumed: Option<&InProgressMarker>,
) -> Vec<BackupComponents> {
    // keep the declaration order, so the list reads the same in every manifest
    BackupComponents::iter()
        .filter(|component| !component.is_filter())
        .filter(|component| {
            component.should_backup(requested)
                || resumed.is_some_and(|marker| marker.components.contains(component))
        })
        .collect()
}

/// Returns the partition size of each partitioned data type in a config, by config name
///
/// # Arguments
///
/// * `config` - The config of the cluster being backed up
fn partition_sizes(config: &Conf) -> BTreeMap<String, u32> {
    // every section of the config with a partition size
    let thorium = &config.thorium;
    [
        ("associations", thorium.associations.partition_size),
        ("entities", thorium.entities.partition_size),
        ("events", thorium.events.partition_size),
        ("files", thorium.files.partition_size),
        ("repos", thorium.repos.partition_size),
        ("results", thorium.results.partition_size),
        ("tags.entities", thorium.tags.entities.partition_size),
        ("tags.files", thorium.tags.files.partition_size),
        ("tags.repos", thorium.tags.repos.partition_size),
    ]
    .into_iter()
    .map(|(name, size)| (name.to_owned(), u32::from(size)))
    .collect()
}

/// Returns the bucket name of each bucket role in a config
///
/// # Arguments
///
/// * `config` - The config of the cluster being backed up
fn buckets(config: &Conf) -> BTreeMap<String, String> {
    // every bucket role the config names
    let thorium = &config.thorium;
    [
        ("attachments", &thorium.attachments.bucket),
        ("ephemeral", &thorium.ephemeral.bucket),
        ("files", &thorium.files.bucket),
        ("graphics", &thorium.graphics.bucket),
        ("reaction_cache", &thorium.reaction_cache.bucket),
        ("repos", &thorium.repos.bucket),
        ("results", &thorium.results.bucket),
    ]
    .into_iter()
    .map(|(role, bucket)| (role.to_owned(), bucket.clone()))
    .collect()
}

/// Returns the interrupted run a backup root's layout resumes, or refuses a root no run may write
/// into
///
/// # Arguments
///
/// * `layout` - What the root holds
/// * `root` - The backup root
fn resumable(layout: Layout, root: &Path) -> Result<Option<InProgressMarker>, Error> {
    // an empty root starts a backup and an interrupted one resumes; anything else is refused
    match layout {
        Layout::Empty => Ok(None),
        Layout::Interrupted(marker) => Ok(Some(marker)),
        Layout::Finished => Err(Error::new(format!(
            "{} holds a finished backup ({MANIFEST_FILE}), which is never changed; back up into a \
             new directory",
            root.display()
        ))),
        Layout::Legacy => Err(Error::new(format!(
            "{} holds files but neither {MANIFEST_FILE} nor {MARKER_FILE}, so it holds no backup \
             this run can resume; back up into a new or empty directory",
            root.display()
        ))),
    }
}

/// Takes the exclusive lock on a backup root that the run writing into it holds until it ends,
/// refusing a root another run holds
///
/// The lock is an `flock` on `backup.lock` in the root, released when the returned file is
/// dropped or the process exits, however it ends. The file itself stays.
///
/// # Arguments
///
/// * `root` - The backup root, which exists
fn lock_root(root: &Path) -> Result<std::fs::File, Error> {
    // open the lock file, creating it private to this user
    let path = root.join(LOCK_FILE);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(PRIVATE_FILE_MODE)
        .open(&path)
        .map_err(|error| Error::new(format!("{} can't be opened: {error}", path.display())))?;
    // take the lock without waiting, so a second run refuses rather than queues
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(Error::new(format!(
            "another thoradm run is backing up into {} (it holds {}); wait for it to end, or \
             back up into another directory",
            root.display(),
            path.display()
        ))),
        Err(std::fs::TryLockError::Error(error)) => Err(Error::new(format!(
            "{} can't be locked: {error}",
            path.display()
        ))),
    }
}

/// Creates a backup root private to this user, or makes one that exists private
///
/// A root other users can read or enter gets mode 0700 before anything is written in it; a root
/// whose mode can't be set (it belongs to another user, for example) refuses the run.
///
/// # Arguments
///
/// * `root` - The backup root
async fn make_root_private(root: &Path) -> Result<(), Error> {
    // a new root, and any missing parents, are created private
    if !tokio::fs::try_exists(root).await? {
        create_private_dir(root).await?;
        return Ok(());
    }
    // an existing root other users can reach is made private first
    let mode = tokio::fs::metadata(root).await?.permissions().mode();
    if mode & 0o077 != 0 {
        let private = std::fs::Permissions::from_mode(PRIVATE_DIR_MODE);
        tokio::fs::set_permissions(root, private)
            .await
            .map_err(|error| {
                Error::new(format!(
                    "{} can be read by other users ({:o}) and couldn't be made private: {error}; \
                     back up into a directory you own, or make it 0700",
                    root.display(),
                    mode & 0o7777
                ))
            })?;
        info!(
            "Made {} private to this user (it was {:o})",
            root.display(),
            mode & 0o7777
        );
    }
    Ok(())
}

/// Moves an interrupted run's failures file to `reports/backup-<its start time>/`, so the
/// resumed run's own `failures.jsonl` and its manifest count only what it couldn't back up
///
/// An empty or missing failures file is left alone.
///
/// # Arguments
///
/// * `root` - The backup root
/// * `marker` - The interrupted run's marker
/// * `sinks` - The file-system seam to move the file through
async fn set_aside_failures(
    root: &Path,
    marker: &InProgressMarker,
    sinks: &dyn SinkFactory,
) -> Result<(), Error> {
    // only a file holding records is worth keeping
    let failures = root.join(FAILURES_FILE);
    match tokio::fs::metadata(&failures).await {
        Ok(metadata) if metadata.len() > 0 => (),
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    // name the report directory for the interrupted run's start, as restore names its own
    let started = marker.started.format("%Y%m%dT%H%M%S%.3fZ");
    let reports = root.join("reports").join(format!("backup-{started}"));
    create_private_dir(&reports).await?;
    sinks
        .rename(&failures, &reports.join(FAILURES_FILE))
        .await?;
    // make the move durable in both directories
    sinks.sync_dir(&reports).await?;
    sinks.sync_dir(root).await?;
    info!(
        "Moved the interrupted run's failures to {}",
        reports.join(FAILURES_FILE).display()
    );
    Ok(())
}

/// Takes a backup of a cluster and returns how the run ended
///
/// The output directory decides how the run starts: a new or empty directory starts a backup, a
/// directory holding an interrupted backup (`backup-in-progress.json` without `manifest.json`)
/// resumes it, and a finished backup or a directory of other files is refused before anything
/// is written. A run that reaches its end writes `manifest.json` and removes the marker; a run
/// that stops early keeps the marker, so running it again resumes it. A run holds an exclusive
/// lock on `backup.lock` in the directory until it ends, and a directory another run holds is
/// refused.
///
/// # Arguments
///
/// * `config` - The Thorium config of the cluster to back up
/// * `ctl_conf` - The Thorctl config to fetch the Redis state with
/// * `scylla` - The Scylla client to use
/// * `workers` - The number of workers per component
/// * `backup_args` - The `backup new` arguments
/// * `sinks` - The file-system seam the backup writes through
pub async fn run(
    config: &Conf,
    ctl_conf: CtlConf,
    scylla: &Arc<Session>,
    workers: usize,
    backup_args: &NewBackup,
    sinks: Arc<dyn SinkFactory>,
) -> Result<RunOutcome, Error> {
    let started = Utc::now();
    let root = &backup_args.output;
    // a root no run may write into is refused before anything in it is touched
    let found = resumable(manifest::classify(root).await?, root)?;
    // a backup of the Redis state needs an API of thoradm's own version, checked before anything
    // is written
    let version_mismatch = if effective_components(&backup_args.components, found.as_ref())
        .contains(&BackupComponents::Redis)
    {
        version::enforce(
            &ctl_conf,
            backup_args.allow_version_mismatch,
            "back up the Redis state",
        )
        .await
        .map_err(Error::new)?
    } else {
        None
    };
    // the backup root is private to this user, whether this run creates it or not
    make_root_private(root).await?;
    // one run at a time writes into a backup root, holding its lock until the run ends
    let _lock = lock_root(root)?;
    // what the root holds under the lock decides whether the run starts or resumes, since
    // another run may have started or finished in it since it was first looked at
    let resumed = resumable(manifest::classify(root).await?, root)?;
    // the run backs up what was asked for and everything the run it resumes asked for
    let components = effective_components(&backup_args.components, resumed.as_ref());
    if let Some(marker) = &resumed {
        info!(
            "Resuming the backup in {} started at {}",
            root.display(),
            marker.started
        );
        // keep the interrupted run's records apart, so this run's failures describe the backup
        set_aside_failures(root, marker, sinks.as_ref()).await?;
    }
    // mark the directory as holding a backup in progress before anything else is written
    InProgressMarker::new(started, components.clone())
        .write(root, sinks.as_ref())
        .await?;
    let policy = FailurePolicy::from_flag(backup_args.abort_on_failure);
    let log = Arc::new(FailureLog::open(root, policy, sinks.as_ref()).await?);
    // build the controller for this cluster
    let mut controller = BackupController::new(
        config,
        ctl_conf,
        scylla,
        workers,
        backup_args.multiplier,
        sinks.clone(),
        RetryPolicy::new(backup_args.retries),
        usize::try_from(backup_args.s3_concurrency).unwrap_or(usize::MAX),
    );
    controller.resume = resumed.is_some();
    // the API prunes dangling references only when asked
    controller.prune_dangling = backup_args.prune_dangling;
    // backup the selected components to disk
    controller.backup(root, &components, &log).await?;
    // the outcome follows the records the run made; a backup that dropped references doesn't
    // hold all the cluster held, so it finishes with failures too
    let mut outcome = RunOutcome::finished(&log);
    if outcome == RunOutcome::Complete && controller.pruned.is_some() {
        outcome = RunOutcome::CompletedWithFailures;
    }
    // a run that reached its end is finished, failures and all; a stopped one stays resumable
    if outcome != RunOutcome::Aborted {
        let failures_sha256 = if log.total() > 0 {
            Some(file_sha256(log.path()).await?)
        } else {
            None
        };
        let counts = log.totals();
        let (tables, s3) = controller.entries(&components, &log);
        let manifest = Manifest {
            format_version: FORMAT_VERSION,
            thoradm_version: env!("CARGO_PKG_VERSION").to_owned(),
            thoradm_commit: option_env!("THORADM_GIT_COMMIT").map(ToOwned::to_owned),
            namespace: config.thorium.namespace.clone(),
            partition_sizes: partition_sizes(config),
            buckets: buckets(config),
            started,
            finished: Utc::now(),
            components,
            failures: counts.failed,
            skipped: counts.skipped,
            failures_sha256,
            tables,
            s3,
            redis: controller.redis.clone(),
            siblings: manifest::siblings(root).await?,
            api_version_mismatch: version_mismatch,
            pruned: controller.pruned.clone(),
        };
        manifest.write(root, sinks.as_ref()).await?;
    }
    log_outcome("backup", outcome, &log);
    // name what was dropped last, since the failures line doesn't count it
    if let Some(pruned) = &controller.pruned {
        warn!(
            "backup dropped {} dangling user and group references (--prune-dangling), listed in \
             {}; a restore of this backup needs --accept-failures",
            pruned.count,
            root.join(PRUNED_FILE).display()
        );
    }
    Ok(outcome)
}

/// Handle a backup take sub comamnd
///
/// # Arguments
///
/// * `backup_args` - The args for the backup handler
/// * `args` - The Thoradm args
pub async fn handle(backup_args: &NewBackup, args: &Args) -> Result<RunOutcome, Error> {
    // load our config
    let config = Conf::new(&args.cluster_conf)?;
    let ctl_conf = CtlConf::from_path(&args.ctl_conf)?;
    // build a new scylla client
    let scylla = Arc::new(utils::get_scylla_client(&config).await?);
    // take the backup through the real file system, boxing its large future
    Box::pin(run(
        &config,
        ctl_conf,
        &scylla,
        args.workers,
        backup_args,
        Arc::new(TokioFs),
    ))
    .await
}

/// Returns the records a failed Redis backup names as unparsable, one per Redis key
///
/// The API answers a backup of records it can't parse with 422 and a body listing each record's
/// key and what is wrong with it; any other failure names no records.
///
/// # Arguments
///
/// * `error` - Why fetching the Redis state failed
fn unparsable_records(error: &thorium::Error) -> Option<Vec<BackupProblem>> {
    // only a 422 lists records
    match error {
        thorium::Error::Thorium {
            code,
            msg: Some(msg),
        } if code.as_u16() == 422 => serde_json::from_str::<BackupProblems>(msg)
            .ok()
            .map(|body| body.problems)
            .filter(|problems| !problems.is_empty()),
        _ => None,
    }
}
