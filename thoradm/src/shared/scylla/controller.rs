//! The controllers used for thoradm working with data in scylla

use futures::stream::FuturesUnordered;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use kanal::{AsyncReceiver, AsyncSender};
use scylla::client::session::Session;
use std::sync::Arc;
use tokio::task::JoinHandle;

use super::build_token_ranges;
use crate::Error;
use crate::shared::monitor::{Monitor, MonitorUpdate};
use crate::shared::workers::{Orders, WorkerHandle, join_workers, spawn_worker};

/// The trait required for scalably crawling data in scylla
pub trait ScyllaCrawlSupport: Sized + Send + 'static {
    /// The arguments to specify when creating workers
    type WorkerArgs: Clone;

    /// Set the progress bars style for workers
    fn bar_style() -> Result<ProgressStyle, Error>;

    /// Set the progress bar style for this controllers monitor
    fn monitor_bar_style() -> Result<ProgressStyle, Error>;

    /// Build a single worker for this controller
    #[allow(
        async_fn_in_trait,
        reason = "public trait of the thoradm library; desugaring to an explicit future changes the signature every implementor sees"
    )]
    async fn build_worker(
        scylla: &Arc<Session>,
        namespace: &str,
        updates: AsyncSender<MonitorUpdate>,
        args: &Self::WorkerArgs,
        dry_run: bool,
        bar: ProgressBar,
    ) -> Result<Self, Error>;

    /// Start crawling data in scylla
    ///
    /// # Arguments
    ///
    /// * `rx` - The channel to receive token ranges to crawl on
    fn start(
        self,
        rx: Orders<(i64, i64)>,
    ) -> impl std::future::Future<Output = Result<Self, Error>> + Send;

    /// Shutdown this worker
    fn shutdown(self);
}

/// A controller that splits the token ring into ranges and hands them to workers that crawl them
pub struct ScyllaCrawlController<S: ScyllaCrawlSupport + Send> {
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
    /// The arguments to specify when creating workers
    pub args: S::WorkerArgs,
    /// The number of workers to spawn
    worker_count: usize,
    /// The currently active workers
    active: FuturesUnordered<WorkerHandle<S, (i64, i64)>>,
}

impl<S: ScyllaCrawlSupport + Send> ScyllaCrawlController<S> {
    /// Create a new controller
    pub fn new<N: Into<String>>(
        namespace: N,
        scylla: &Arc<Session>,
        args: S::WorkerArgs,
        workers: usize,
    ) -> Result<Self, Error> {
        // build our kanal channel for monitor updates
        let (updates_tx, updates_rx) = kanal::unbounded_async();
        // build our kanal channel for orders
        let (orders_tx, orders_rx) = kanal::unbounded_async();
        // build our controller
        let controller = ScyllaCrawlController {
            namespace: namespace.into(),
            scylla: scylla.clone(),
            updates_tx,
            updates_rx,
            orders_tx,
            orders_rx,
            progress: crate::shared::progress::bars().clone(),
            args,
            worker_count: workers,
            active: FuturesUnordered::default(),
        };
        Ok(controller)
    }
    /// Start our monitor
    fn start_monitor(&self) -> Result<JoinHandle<()>, Error> {
        // get this progress bars style
        let bar_style = S::monitor_bar_style()?;
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
        let handle = tokio::spawn(async move { monitor.start().await });
        Ok(handle)
    }

    /// Build and spawn all all of our workers
    ///
    /// # Arguments
    ///
    /// * `dry_run` - Whether this controller is doing a dry run or not
    async fn spawn_all(&mut self, dry_run: bool) -> Result<(), Error> {
        // get this progress bars style
        let bar_style = S::bar_style()?;
        // create the right number of workers
        for _ in 0..self.worker_count {
            // create a new progress bar for this worker
            let bar = ProgressBar::new_spinner();
            // set a steady tick rate
            bar.enable_steady_tick(std::time::Duration::from_millis(120));
            // set this bars style
            bar.set_style(bar_style.clone());
            // add this progress bar to our main bar
            let bar = self.progress.add(bar);
            // build our worker
            let worker = S::build_worker(
                &self.scylla,
                &self.namespace,
                self.updates_tx.clone(),
                &self.args,
                dry_run,
                bar,
            )
            .await?;
            // spawn this worker on its share of the orders
            let handle = spawn_worker(&self.orders_rx, |orders| worker.start(orders));
            // add this task to our futures set
            self.active.push(handle);
        }
        Ok(())
    }

    /// Generate our token ranges to crawl
    ///
    /// # Arguments
    ///
    /// * `chunk_count` - The number of chunks to break our token ring into
    async fn generate_token_ranges(&mut self, chunk_count: u64) -> Result<(), Error> {
        // split the ring into contiguous ranges and send them in ring order
        for segment in build_token_ranges(chunk_count) {
            // add this segment to our channel
            self.orders_tx.send(segment).await?;
        }
        Ok(())
    }

    /// Wait for all of our workers to finish, returning how many ended early
    ///
    /// When every worker ended early with orders left, the crawl fails at once with their errors.
    async fn wait_for_workers(&mut self) -> Result<usize, Error> {
        // stop sending orders and join every worker directly
        let mut joined = join_workers(&mut self.orders_tx, &self.orders_rx, &mut self.active).await;
        // judge the crawl before the finished workers are shut down
        let failed = joined
            .step_failed()
            .then(|| joined.step_error("the census"));
        // shut down the workers that finished
        for worker in std::mem::take(&mut joined.finished) {
            worker.shutdown();
        }
        // log each worker that ended early, with the range it held
        for failure in &joined.errors {
            tracing::error!(
                range = ?failure.order,
                "a census worker ended early: {}",
                failure.error
            );
        }
        // a crawl whose workers all ended early can't be trusted to have counted everything
        if let Some(error) = failed {
            return Err(error);
        }
        Ok(joined.errors.len())
    }

    /// Start crawling data in scylla
    ///
    /// # Arguments
    ///
    /// * `chunk_count` - The number of chunks to split the work into
    /// * `dry_run` - Whether this controller is doing a dry run or not
    ///
    /// Returns how many workers ended early and were logged, while the others finished the crawl.
    pub async fn start(&mut self, chunk_count: u64, dry_run: bool) -> Result<usize, Error> {
        // start this controllers monitor
        let handle = self.start_monitor()?;
        // spawn all of our workers
        self.spawn_all(dry_run).await?;
        // generate our token ranges
        self.generate_token_ranges(chunk_count).await?;
        // wait for all workers to complete
        let ended = self.wait_for_workers().await?;
        // tell our monitor to finish and exit
        self.updates_tx.send(MonitorUpdate::Finished).await?;
        // wait for our map updater to finish
        handle.await?;
        Ok(ended)
    }
}
