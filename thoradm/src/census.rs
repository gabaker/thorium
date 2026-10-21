//! Census commands in thoradm

use bb8_redis::RedisConnectionManager;
use bb8_redis::bb8::Pool;
use indicatif::{ProgressBar, ProgressStyle};
use kanal::AsyncSender;
use scylla::client::session::Session;
use scylla::statement::prepared::PreparedStatement;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use thorium::Conf;
use thorium::models::backends::TagSupport;
use thorium::models::{
    Association, CensusSupport, Commitish, Entity, Repo, Sample, TagCensusCaseInsensitive,
    TagRequest,
};

use crate::Error;
use crate::args::{Args, CensusKinds, CensusSubCommands, NewCensus};
use crate::backup::Utils;
use crate::outcome::RunOutcome;
use crate::shared::monitor::MonitorUpdate;
use crate::shared::retry::RetryPolicy;
use crate::shared::scylla::{RangeReader, ScyllaCrawlController, ScyllaCrawlSupport};
use crate::shared::workers::Orders;

/// What every worker of one census shares
#[derive(Clone)]
pub struct CensusArgs {
    /// The redis client to write census streams with
    pub redis: Pool<RedisConnectionManager>,
    /// How many errors the census's workers logged
    pub errors: Arc<AtomicU64>,
}

impl CensusArgs {
    /// Starts the shared arguments of one census, with no errors logged yet
    ///
    /// # Arguments
    ///
    /// * `redis` - The redis client to write census streams with
    #[must_use]
    pub fn new(redis: Pool<RedisConnectionManager>) -> Self {
        CensusArgs {
            redis,
            errors: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Returns how many errors the census's workers logged
    #[must_use]
    pub fn errors(&self) -> u64 {
        // workers only ever add to the count
        self.errors.load(Ordering::Relaxed)
    }
}

/// A worker that counts the partitions of the token ranges it is sent for one census kind
pub struct CensusWorker<T: CensusSupport> {
    /// The keyspace/namespace for this worker
    namespace: String,
    /// The scylla client to talk to Scylla with
    scylla: Arc<Session>,
    /// The redis client to talk to redis with
    redis: Pool<RedisConnectionManager>,
    /// The prepared statement to use
    prepared: PreparedStatement,
    /// The kanal channel workers should send backup updates over
    updates: AsyncSender<MonitorUpdate>,
    /// The current number of partitions this worker has counted
    partitions_counted: u64,
    /// The progress bar to write error messages with
    progress: ProgressBar,
    /// Whether to write data to the db or not
    dry_run: bool,
    /// The table we are taking a census of
    phantom: PhantomData<T>,
    /// How many errors this census's workers logged, shared by them all
    errors: Arc<AtomicU64>,
}

impl<T: CensusSupport> CensusWorker<T> {
    /// Builds a census worker
    ///
    /// # Arguments
    ///
    /// * `scylla` - The scylla client to read partitions with
    /// * `args` - What every worker of this census shares
    /// * `namespace` - The keyspace/namespace to take a census of
    /// * `updates` - The channel to send progress updates on
    /// * `dry_run` - Whether to leave the census streams unwritten
    /// * `progress` - The progress bar to update
    pub async fn new(
        scylla: &Arc<Session>,
        args: &CensusArgs,
        namespace: &str,
        updates: AsyncSender<MonitorUpdate>,
        dry_run: bool,
        progress: ProgressBar,
    ) -> Result<Self, Error> {
        // get our prepared statement
        let prepared = T::scan_prepared_statement(scylla, namespace).await?;
        // build this backup worker
        let worker = CensusWorker {
            namespace: namespace.to_owned(),
            scylla: scylla.clone(),
            redis: args.redis.clone(),
            prepared,
            updates,
            partitions_counted: 0,
            dry_run,
            progress,
            phantom: PhantomData,
            errors: args.errors.clone(),
        };
        Ok(worker)
    }

    /// Start counting the partitions of each token range it is sent
    ///
    /// Each range is read a page at a time; a page that fails to arrive is requested again from
    /// where the last page ended, and a range whose page still fails after every attempt is
    /// logged and counted as an error.
    ///
    /// # Arguments
    ///
    /// * `orders` - The token ranges to count
    ///
    /// # Panics
    ///
    /// Panics if no Redis connection can be taken from the pool.
    pub async fn start(mut self, orders: Orders<(i64, i64)>) -> Result<Self, Error> {
        // the session and statement outlive each range's reader, which borrows them
        let (scylla, prepared) = (self.scylla.clone(), self.prepared.clone());
        // handle messages in our channel until its closed
        loop {
            // get the next message in the queue
            let (start, end) = match orders.recv().await {
                Ok(range) => range,
                Err(kanal::ReceiveError::Closed | kanal::ReceiveError::SendClosed) => break,
            };
            // build the redis command to insert info into redis
            let mut pipe = redis::pipe();
            // track how many partitions we counted before we handle this range
            let old_counted = self.partitions_counted;
            // read the range a page at a time, resuming a page that fails to arrive
            let mut reader =
                RangeReader::new(&scylla, &prepared, (start, end), RetryPolicy::default());
            loop {
                let page = match reader.next_page::<T::Row>().await {
                    Ok(Some(page)) => page,
                    Ok(None) => break,
                    Err(failed) => {
                        // log the range whose rest couldn't be read, and count it
                        tracing::error!(range = ?(start, end), "Census failure: {failed}");
                        self.errors.fetch_add(1, Ordering::Relaxed);
                        break;
                    }
                };
                for typed_row in page {
                    // log and count a row that can't be read, then go on with the rest
                    let typed_row = match typed_row {
                        Ok(typed_row) => typed_row,
                        Err(error) => {
                            tracing::error!("Census failure: {error}");
                            self.errors.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                    };
                    // increment our partition count
                    self.partitions_counted += 1;
                    // skip building a pipeline if this is a dry run
                    if !self.dry_run {
                        // add this partition's bucket to its census stream
                        let bucket = T::get_bucket(&typed_row);
                        let stream_key = T::stream_key_from_row(&self.namespace, &typed_row);
                        pipe.cmd("zadd").arg(stream_key).arg(bucket).arg(bucket);
                    }
                }
            }
            // skip saving things to redis if this is a dry run
            if !self.dry_run {
                // get a connection to our redis db
                let mut conn = self.redis.get().await.unwrap();
                // execute our redis pipeline
                let _: () = pipe.atomic().query_async(&mut *conn).await?;
            }
            // set our current row count progress message
            self.progress
                .set_message(self.partitions_counted.to_string());
            // calculate how many partitions we counted in this range
            let items =
                usize::try_from(self.partitions_counted - old_counted).unwrap_or(usize::MAX);
            // build the update to send to our controller
            let update = MonitorUpdate::Update { items, bytes: 0 };
            // send an update to our controller
            self.updates.send(update).await?;
        }
        Ok(self)
    }
}

impl<T: CensusSupport + Utils> ScyllaCrawlSupport for CensusWorker<T> {
    /// The arguments to specify when creating workers
    type WorkerArgs = CensusArgs;

    /// Set the progress bars style for workers
    fn bar_style() -> Result<ProgressStyle, Error> {
        // build the style string for this
        let style_str = format!("{{spinner:.green}} {} Censused: {{msg}}", T::name());
        // build the style for our progress bar
        let bar_style = ProgressStyle::with_template(&style_str)
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
        Ok(bar_style)
    }

    /// Set the progress bar style for this controllers monitor
    fn monitor_bar_style() -> Result<ProgressStyle, Error> {
        // build the style string for our monitor
        let style_str = format!(
            "{{spinner:.green}} {{elapsed_precise}} Total Censused {}: {{msg}}",
            T::name()
        );
        // build the style for our progress bar
        let bar_style = ProgressStyle::with_template(&style_str)
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
        Ok(bar_style)
    }

    /// Build a single worker for this controller
    async fn build_worker(
        scylla: &Arc<Session>,
        namespace: &str,
        updates: AsyncSender<MonitorUpdate>,
        args: &Self::WorkerArgs,
        dry_run: bool,
        bar: ProgressBar,
    ) -> Result<Self, Error> {
        // build our census worker
        CensusWorker::new(scylla, args, namespace, updates, dry_run, bar).await
    }

    /// Start crawling data in scylla
    ///
    /// # Arguments
    ///
    /// * `rx` - The channel to receive token ranges to crawl on
    async fn start(self, rx: Orders<(i64, i64)>) -> Result<Self, Error> {
        // run the worker's own crawl, returning its error to the controller
        self.start(rx).await
    }

    /// Shutdown this worker
    fn shutdown(self) {}
}

/// Take a new full census of tag data
///
/// # Arguments
///
/// * `config` - The config of the cluster to take a census of
/// * `workers` - The number of workers to use
/// * `dry_run` - Whether this is a dry run or not
///
/// Returns how many errors were logged while the census ran.
async fn new_tags(config: &Conf, workers: usize, dry_run: bool) -> Result<u64, Error> {
    // build a new scylla client
    let scylla = Arc::new(crate::shared::scylla::get_client(config).await?);
    // build a new redis client, shared with an error count by every worker
    let args = CensusArgs::new(crate::shared::redis::get_client(config).await?);
    // build a new tag census controller
    // we are using sample but the type doesn't matter for tag scans actually
    let mut controller = ScyllaCrawlController::<CensusWorker<TagRequest<Sample>>>::new(
        &config.thorium.namespace,
        &scylla,
        args.clone(),
        workers,
    )?;
    // start taking a census of tag data
    let ended = controller.start(100_000, dry_run).await?;
    // count the errors the workers logged and the workers that ended early
    Ok(args.errors() + ended as u64)
}

impl<T: TagSupport> Utils for TagCensusCaseInsensitive<T> {
    fn name() -> &'static str {
        "tags_case_insensitive"
    }
}

/// Take a new full census of tag data
///
/// # Arguments
///
/// * `config` - The config of the cluster to take a census of
/// * `workers` - The number of workers to use
/// * `dry_run` - Whether this is a dry run or not
///
/// Returns how many errors were logged while the census ran.
async fn new_tags_case_insensitive(
    config: &Conf,
    workers: usize,
    dry_run: bool,
) -> Result<u64, Error> {
    // build a new scylla client
    let scylla = Arc::new(crate::shared::scylla::get_client(config).await?);
    // build a new redis client, shared with an error count by every worker
    let args = CensusArgs::new(crate::shared::redis::get_client(config).await?);
    // build a new tag census controller
    // we are using sample but the type doesn't matter for tag scans actually
    let mut controller =
        ScyllaCrawlController::<CensusWorker<TagCensusCaseInsensitive<Sample>>>::new(
            &config.thorium.namespace,
            &scylla,
            args.clone(),
            workers,
        )?;
    // start taking a census of tag data
    let ended = controller.start(1000, dry_run).await?;
    // count the errors the workers logged and the workers that ended early
    Ok(args.errors() + ended as u64)
}

impl Utils for Sample {
    fn name() -> &'static str {
        "samples_list"
    }
}

/// Take a new full census of sample data
///
/// # Arguments
///
/// * `config` - The config of the cluster to take a census of
/// * `workers` - The number of workers to use
/// * `dry_run` - Whether this is a dry run or not
///
/// Returns how many errors were logged while the census ran.
async fn new_samples(config: &Conf, workers: usize, dry_run: bool) -> Result<u64, Error> {
    // build a new scylla client
    let scylla = Arc::new(crate::shared::scylla::get_client(config).await?);
    // build a new redis client, shared with an error count by every worker
    let args = CensusArgs::new(crate::shared::redis::get_client(config).await?);
    // build a new sample census controller
    let mut controller = ScyllaCrawlController::<CensusWorker<Sample>>::new(
        &config.thorium.namespace,
        &scylla,
        args.clone(),
        workers,
    )?;
    // start taking a census of tag data
    let ended = controller.start(1000, dry_run).await?;
    // count the errors the workers logged and the workers that ended early
    Ok(args.errors() + ended as u64)
}

impl Utils for Repo {
    fn name() -> &'static str {
        "repos_list"
    }
}

/// Take a new full census of repo data
///
/// # Arguments
///
/// * `config` - The config of the cluster to take a census of
/// * `workers` - The number of workers to use
/// * `dry_run` - Whether this is a dry run or not
///
/// Returns how many errors were logged while the census ran.
async fn new_repos(config: &Conf, workers: usize, dry_run: bool) -> Result<u64, Error> {
    // build a new scylla client
    let scylla = Arc::new(crate::shared::scylla::get_client(config).await?);
    // build a new redis client, shared with an error count by every worker
    let args = CensusArgs::new(crate::shared::redis::get_client(config).await?);
    // build a new repo census controller
    let mut controller = ScyllaCrawlController::<CensusWorker<Repo>>::new(
        &config.thorium.namespace,
        &scylla,
        args.clone(),
        workers,
    )?;
    // start taking a census of tag data
    let ended = controller.start(1000, dry_run).await?;
    // count the errors the workers logged and the workers that ended early
    Ok(args.errors() + ended as u64)
}

impl Utils for Commitish {
    fn name() -> &'static str {
        "commitish_list"
    }
}

/// Take a new full census of commitishes data
///
/// # Arguments
///
/// * `config` - The config of the cluster to take a census of
/// * `workers` - The number of workers to use
/// * `dry_run` - Whether this is a dry run or not
///
/// Returns how many errors were logged while the census ran.
async fn new_commitishes(config: &Conf, workers: usize, dry_run: bool) -> Result<u64, Error> {
    // build a new scylla client
    let scylla = Arc::new(crate::shared::scylla::get_client(config).await?);
    // build a new redis client, shared with an error count by every worker
    let args = CensusArgs::new(crate::shared::redis::get_client(config).await?);
    // build a new commitishes census controller
    let mut controller = ScyllaCrawlController::<CensusWorker<Commitish>>::new(
        &config.thorium.namespace,
        &scylla,
        args.clone(),
        workers,
    )?;
    // start taking a census of tag data
    let ended = controller.start(100_000, dry_run).await?;
    // count the errors the workers logged and the workers that ended early
    Ok(args.errors() + ended as u64)
}

impl Utils for Entity {
    /// The table this census counts
    fn name() -> &'static str {
        "entities"
    }
}

impl Utils for Association {
    /// The table this census counts
    fn name() -> &'static str {
        "associations"
    }
}

/// Take a new full census of one table, whose census support names its rows and streams
///
/// # Arguments
///
/// * `config` - The config of the cluster to take a census of
/// * `workers` - The number of workers to use
/// * `dry_run` - Whether this is a dry run or not
///
/// Returns how many errors were logged while the census ran.
async fn new_census<T: CensusSupport + Utils>(
    config: &Conf,
    workers: usize,
    dry_run: bool,
) -> Result<u64, Error> {
    // build a new scylla client
    let scylla = Arc::new(crate::shared::scylla::get_client(config).await?);
    // build a new redis client, shared with an error count by every worker
    let args = CensusArgs::new(crate::shared::redis::get_client(config).await?);
    // build a census controller for the table
    let mut controller = ScyllaCrawlController::<CensusWorker<T>>::new(
        &config.thorium.namespace,
        &scylla,
        args.clone(),
        workers,
    )?;
    // count every partition of the table
    let ended = controller.start(1000, dry_run).await?;
    // count the errors the workers logged and the workers that ended early
    Ok(args.errors() + ended as u64)
}

impl CensusKinds {
    /// Every census kind `all` stands for
    pub const ALL: [CensusKinds; 7] = [
        CensusKinds::Tags,
        CensusKinds::TagsCaseInsensitive,
        CensusKinds::Files,
        CensusKinds::Repos,
        CensusKinds::Commitishes,
        CensusKinds::Entities,
        CensusKinds::Associations,
    ];

    /// Returns the census kinds that count a table's rows
    ///
    /// # Arguments
    ///
    /// * `table` - The table
    #[must_use]
    pub fn of_table(table: &str) -> &'static [CensusKinds] {
        // each table's census streams
        match table {
            "tags" => &[CensusKinds::Tags, CensusKinds::TagsCaseInsensitive],
            "samples_list" => &[CensusKinds::Files],
            "repos_list" => &[CensusKinds::Repos],
            "commitish_list" => &[CensusKinds::Commitishes],
            "entities" => &[CensusKinds::Entities],
            "associations" => &[CensusKinds::Associations],
            _ => &[],
        }
    }

    /// Execute a specific census kind
    ///
    /// # Arguments
    ///
    /// * `config` - The config of the cluster to take a census of
    /// * `workers` - The number of workers to use
    /// * `dry_run` - Whether this is a dry run or not
    ///
    /// Returns how many errors were logged while the census ran.
    async fn start(self, config: &Conf, workers: usize, dry_run: bool) -> Result<u64, Error> {
        // perform a specific kind of census
        match self {
            // All gets handled in the new function and shouldn't occur ever
            CensusKinds::All => panic!("All wasn't mapped to all census kinds?"),
            CensusKinds::Tags => new_tags(config, workers, dry_run).await,
            CensusKinds::TagsCaseInsensitive => {
                new_tags_case_insensitive(config, workers, dry_run).await
            }
            CensusKinds::Files => new_samples(config, workers, dry_run).await,
            CensusKinds::Repos => new_repos(config, workers, dry_run).await,
            CensusKinds::Commitishes => new_commitishes(config, workers, dry_run).await,
            CensusKinds::Entities => new_census::<Entity>(config, workers, dry_run).await,
            CensusKinds::Associations => new_census::<Association>(config, workers, dry_run).await,
        }
    }
}

/// Lists the census stream member every partition of one table calls for: each partition's stream
/// key and bucket
///
/// # Arguments
///
/// * `scylla` - The session to read the table with
/// * `namespace` - The namespace the table is in
/// * `retry` - How a page that fails to arrive is retried
async fn members_of<T: CensusSupport>(
    scylla: &Session,
    namespace: &str,
    retry: RetryPolicy,
) -> Result<Vec<(String, i32)>, Error> {
    // count every partition of the ring, a page at a time
    let prepared = T::scan_prepared_statement(scylla, namespace).await?;
    let mut reader = RangeReader::new(scylla, &prepared, (i64::MIN, i64::MAX), retry);
    let mut members = Vec::new();
    loop {
        let page = match reader.next_page::<T::Row>().await {
            Ok(Some(page)) => page,
            Ok(None) => return Ok(members),
            Err(failed) => return Err(Error::new(failed.to_string())),
        };
        for row in page {
            let row = row.map_err(|error| Error::new(error.to_string()))?;
            members.push((T::stream_key_from_row(namespace, &row), T::get_bucket(&row)));
        }
    }
}

/// Lists the census stream member every partition counted by a census kind calls for: each
/// partition's stream key and bucket, as a census of that kind would add them
///
/// # Arguments
///
/// * `kind` - The census kind
/// * `scylla` - The session to read the tables with
/// * `namespace` - The namespace the tables are in
/// * `retry` - How a page that fails to arrive is retried
pub async fn expected_members(
    kind: CensusKinds,
    scylla: &Session,
    namespace: &str,
    retry: RetryPolicy,
) -> Result<Vec<(String, i32)>, Error> {
    // each kind counts one table through its census support
    match kind {
        CensusKinds::All => Ok(Vec::new()),
        CensusKinds::Tags => members_of::<TagRequest<Sample>>(scylla, namespace, retry).await,
        CensusKinds::TagsCaseInsensitive => {
            members_of::<TagCensusCaseInsensitive<Sample>>(scylla, namespace, retry).await
        }
        CensusKinds::Files => members_of::<Sample>(scylla, namespace, retry).await,
        CensusKinds::Repos => members_of::<Repo>(scylla, namespace, retry).await,
        CensusKinds::Commitishes => members_of::<Commitish>(scylla, namespace, retry).await,
        CensusKinds::Entities => members_of::<Entity>(scylla, namespace, retry).await,
        CensusKinds::Associations => members_of::<Association>(scylla, namespace, retry).await,
    }
}

/// Execute any requested censuses
///
/// # Arguments
///
/// * `cmd` - The census to take
/// * `config` - The config of the cluster to take a census of
/// * `workers` - The number of workers to use
///
/// A census whose workers logged errors finishes with failures, since what it counted may be
/// short.
pub async fn new(cmd: &NewCensus, config: &Conf, workers: usize) -> Result<RunOutcome, Error> {
    // if we have all set then replace it with all of the census types
    let kinds = if cmd.census_kinds.contains(&CensusKinds::All) {
        // add all possible census kinds
        CensusKinds::ALL.to_vec()
    } else {
        // clone our census kinds
        cmd.census_kinds.clone()
    };
    // iterate over the requested census kinds and perform them, counting logged errors
    let mut errors = 0;
    for kind in &kinds {
        // perform a census on the correct data
        errors += kind.start(config, workers, cmd.dry_run).await?;
    }
    // a census that logged errors may have missed what it couldn't read
    if errors > 0 {
        tracing::warn!("census finished after {errors} errors, logged above");
        return Ok(RunOutcome::CompletedWithFailures);
    }
    Ok(RunOutcome::Complete)
}

/// Handle census commands
///
/// # Arguments
///
/// * `cmd` - The census commands
/// * `args` - The command line arguments
pub async fn handle(cmd: &CensusSubCommands, args: &Args) -> Result<RunOutcome, Error> {
    // load our config
    let config = Conf::new(&args.cluster_conf)?;
    // take the requested census
    match cmd {
        CensusSubCommands::New(cmd) => new(cmd, &config, args.workers).await,
    }
}
