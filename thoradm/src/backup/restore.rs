//! The worker that handles restoring data to a Thorium cluster

use futures::stream::{FuturesUnordered, StreamExt};
use indicatif::ProgressBar;
use kanal::AsyncSender;
use scylla::client::session::Session;
use scylla::errors::PrepareError;
use scylla::serialize::row::SerializeRow;
use scylla::statement::prepared::PreparedStatement;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::Arc;
use thorium::Conf;

use super::archive::check_partition;
use super::failures::{FailureLog, FailureRecord, Operation};
use super::tables::{Versioned, read_rows};
use super::{ArchiveReader, MonitorUpdate, Utils};
use crate::Error;
use crate::shared::workers::Orders;

/// The most inserts a worker keeps in flight
const MAX_IN_FLIGHT: usize = 1000;

/// The in-flight inserts below which a worker queues more
const REFILL_BELOW: usize = 100;

/// Inserts rows with a prepared statement, keeping at most about a thousand in flight, and counts
/// each one that lands
///
/// # Arguments
///
/// * `scylla` - The client to insert with
/// * `prepared` - The prepared insert
/// * `values` - The values each insert binds
/// * `rows_restored` - The count of restored rows to add to
/// * `progress` - The bar to report progress with
async fn insert_rows<V, I>(
    scylla: &Session,
    prepared: &PreparedStatement,
    values: I,
    rows_restored: &mut usize,
    progress: &ProgressBar,
) -> Result<(), Error>
where
    V: SerializeRow + Send,
    I: IntoIterator<Item = V>,
    I::IntoIter: Send,
{
    // queue each insert, waiting for most of them to land whenever too many are in flight
    let mut futures = FuturesUnordered::new();
    for value in values {
        futures.push(scylla.execute_unpaged(prepared, value));
        if futures.len() > MAX_IN_FLIGHT {
            while let Some(result) = futures.next().await {
                result?;
                *rows_restored += 1;
                progress.set_message(rows_restored.to_string());
                if futures.len() < REFILL_BELOW {
                    break;
                }
            }
        }
    }
    // wait for the rest
    while let Some(result) = futures.next().await {
        result?;
        *rows_restored += 1;
        progress.set_message(rows_restored.to_string());
    }
    Ok(())
}

/// The worker that handles restoring data to a Thorium cluster
pub struct RestoreWorker<R: Restore> {
    /// The scylla client to connect with
    scylla: Arc<Session>,
    /// The prepared statement to use
    prepared: PreparedStatement,
    /// The type we are restoring
    phantom: PhantomData<R>,
    /// The kanal channel workers should send restore updates over
    updates: AsyncSender<MonitorUpdate>,
    /// The progress bar to track progress with
    progress: ProgressBar,
    /// Track the total number of rows restored
    rows_restored: usize,
    /// The partition config to use for this table
    partition_conf: R::PartitionConf,
    /// The component this worker restores
    component: String,
    /// The run's failure log
    log: Arc<FailureLog>,
    /// The format version the backup's archives were written in
    format: u32,
}

impl<R: Restore> RestoreWorker<R> {
    /// Create a new restore worker
    ///
    /// # Arguments
    ///
    /// * `scylla` - The scylla client to use when restoring data
    /// * `conf` - A Thorium config
    /// * `updates` - The kanal channel to send restore updates over
    /// * `progress` - The progress bar to track progress with
    /// * `component` - The component this worker restores
    /// * `log` - The run's failure log
    /// * `format` - The format version the backup's archives were written in
    pub async fn new(
        scylla: &Arc<Session>,
        conf: &Conf,
        updates: AsyncSender<MonitorUpdate>,
        progress: ProgressBar,
        component: &str,
        log: &Arc<FailureLog>,
        format: u32,
    ) -> Result<Self, Error> {
        // get our namespace
        let namespace = &conf.thorium.namespace;
        // get our prepared statement
        let prepared = R::prepared_statement(&scylla, namespace).await?;
        // get our partition size
        let partition_conf = R::partition_conf(conf);
        // build our restore worker
        let worker = RestoreWorker {
            scylla: scylla.clone(),
            prepared,
            phantom: PhantomData::default(),
            updates,
            progress,
            rows_restored: 0,
            partition_conf,
            component: component.to_owned(),
            log: log.clone(),
            format,
        };
        Ok(worker)
    }

    /// Restore all data in an archive
    ///
    /// Each partition that fails to insert, and each archive that can't be read, is recorded and
    /// the worker carries on with the next one.
    ///
    /// # Arguments
    ///
    /// * `orders` - The channel to receive map paths on
    pub async fn restore(mut self, orders: Orders<PathBuf>) -> Result<Self, Error> {
        // handle messages in our channel until its closed or the run is stopped
        loop {
            // take no new archive once --abort-on-failure stopped the run
            if self.log.stopped() {
                break;
            }
            // get the next message in the queue
            let map_path = match orders.recv().await {
                Ok(path) => path,
                Err(kanal::ReceiveError::Closed | kanal::ReceiveError::SendClosed) => break,
            };
            // restore this archive, recording one that can't be read
            if let Err(error) = self.restore_archive(&map_path).await {
                let record = FailureRecord::failed(&self.component, Operation::ReadArchive, error)
                    .table(R::name())
                    .key(map_path.to_string_lossy());
                self.log.record(&record).await?;
            }
        }
        Ok(self)
    }

    /// Restores every partition of one archive, recording partitions that fail their checksum or
    /// fail to insert
    ///
    /// A partition whose bytes no longer match the checksum its map entry records writes none of
    /// its rows, even when it would still decode, since its values can't be trusted.
    ///
    /// # Arguments
    ///
    /// * `map_path` - The map of the archive to restore
    async fn restore_archive(&mut self, map_path: &std::path::Path) -> Result<(), Error> {
        // build our reader for this archive
        let mut reader = ArchiveReader::new(map_path.to_path_buf()).await?;
        // crawl over the partitions in this archive, checking each before restoring it
        while let Some((restore_slice, entry)) = reader.next_partition().await? {
            // stop after the current partition once --abort-on-failure stopped the run
            if self.log.stopped() {
                break;
            }
            // a partition that fails its checksum is recorded and none of its rows written
            if let Err(mismatch) = check_partition(restore_slice, &entry) {
                let record = FailureRecord::failed(&self.component, Operation::Verify, mismatch)
                    .table(R::name())
                    .key(map_path.to_string_lossy())
                    .object(entry.partition.to_string());
                self.log.record(&record).await?;
                continue;
            }
            // get the number of rows we have restored so far
            let restored = self.rows_restored;
            // restore this slices data to scylla, recording a partition that fails
            if let Err(error) = self.restore_partition(restore_slice).await {
                let record = FailureRecord::failed(&self.component, Operation::Insert, error)
                    .table(R::name())
                    .key(map_path.to_string_lossy())
                    .object(entry.partition.to_string());
                self.log.record(&record).await?;
            }
            // count the rows that landed
            let landed = self.rows_restored - restored;
            self.log.handled(&self.component, landed as u64);
            // set our new writer position
            self.progress.inc(restore_slice.len() as u64);
            // build the progress update for this partition
            let update = MonitorUpdate::Update {
                items: landed,
                bytes: restore_slice.len() as u64,
            };
            // send our progress update to the global tracker
            self.updates.send(update).await?;
        }
        Ok(())
    }

    /// Decodes one partition in the backup's format and inserts its rows
    ///
    /// # Arguments
    ///
    /// * `bytes` - The partition's bytes
    async fn restore_partition(&mut self, bytes: &[u8]) -> Result<(), Error> {
        // every row converts to the current struct, then binds its insert
        let rows = read_rows::<R>(bytes, self.format)?;
        let values = rows
            .into_iter()
            .map(|row| row.values(&self.partition_conf, self.format));
        insert_rows(
            &self.scylla,
            &self.prepared,
            values,
            &mut self.rows_restored,
            &self.progress,
        )
        .await
    }

    /// Shutdown this worker
    pub fn shutdown(self) {
        // shutdown our progress bar
        self.progress.finish();
    }
}

/// A table whose archived rows thoradm writes back into Scylla
#[async_trait::async_trait]
pub trait Restore: Versioned + Utils + std::fmt::Debug + 'static + Send {
    /// The type that contains a partition size or sizes depending on the data/table/row
    type PartitionConf: Send + Sync;

    /// The values the restore's insert binds for one row
    type Values: SerializeRow + Send;

    /// The prepared statement to use when restoring data to scylla
    async fn prepared_statement(
        scylla: &Session,
        ns: &str,
    ) -> Result<PreparedStatement, PrepareError>;

    /// Get the partition size for this data type
    ///
    /// # Arguments
    ///
    /// * `conf` - The Thorium config
    fn partition_conf(config: &Conf) -> Self::PartitionConf;

    /// Returns the values the restore's insert binds for this row; every field the row holds
    /// unset binds as a null
    ///
    /// # Arguments
    ///
    /// * `partition_conf` - The partition sizes the restored row is bucketed with
    /// * `format` - The format version the row was archived in
    fn values(self, partition_conf: &Self::PartitionConf, format: u32) -> Self::Values;
}
