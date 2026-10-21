//! The worker that handles backing up Thorium data

use data_encoding::HEXLOWER;
use indicatif::ProgressBar;
use kanal::AsyncSender;
use rkyv::ser::serializers::AllocSerializer;
use rkyv::{AlignedVec, Archive, Serialize};
use scylla::client::session::Session;
use scylla::deserialize::row::{ColumnIterator, DeserializeRow};
use scylla::deserialize::value::DeserializeValue;
use scylla::deserialize::{DeserializationError, TypeCheckError};
use scylla::errors::PrepareError;
use scylla::frame::response::result::ColumnSpec;
use scylla::statement::prepared::PreparedStatement;
use scylla::value::CqlValue;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use thorium::models::schema::{TableStatements, with_keyspace};
use uuid::Uuid;

use super::failures::{FailureLog, FailureRecord, Operation};
use super::manifest::{ArchiveEntry, RangeCount};
use super::sink::{FileSink, PRIVATE_FILE_MODE, SinkFactory};
use super::{MonitorUpdate, PartitionArchive, Utils};
use crate::Error;
use crate::shared::retry::RetryPolicy;
use crate::shared::scylla::{Page, RangeReader};
use crate::shared::workers::Orders;

/// A row read from Scylla, or what could be read of a row that failed to decode
#[derive(Debug)]
pub enum Decoded<T> {
    /// The row, decoded into its archive struct
    Row(T),
    /// A row the archive struct can't hold
    Undecodable {
        /// The row's partition key columns as `name=value`, as far as they could be read
        key: String,
        /// Why the row couldn't be decoded
        error: String,
    },
}

/// Renders one CQL value for a failure record
///
/// # Arguments
///
/// * `value` - The value to render
fn render_value(value: &CqlValue) -> String {
    // text renders as itself; everything else in its debug form
    match value {
        CqlValue::Text(text) | CqlValue::Ascii(text) => text.clone(),
        other => format!("{other:?}"),
    }
}

/// Renders the partition key columns of a raw row as `name=value` pairs
///
/// # Arguments
///
/// * `row` - The raw row
/// * `columns` - The names of the partition key columns
fn render_key(row: ColumnIterator<'_, '_>, columns: &[&str]) -> String {
    // read each key column on its own, so one unreadable column doesn't hide the rest
    let mut parts = Vec::with_capacity(columns.len());
    for column in row {
        // stop at a row too damaged to walk
        let Ok(column) = column else {
            break;
        };
        // skip the columns outside the partition key
        let name = column.spec.name();
        if !columns.contains(&name) {
            continue;
        }
        // decode the column as a generic CQL value
        let value =
            <Option<CqlValue> as DeserializeValue>::deserialize(column.spec.typ(), column.slice);
        let rendered = match value {
            Ok(Some(value)) => render_value(&value),
            Ok(None) => "null".to_owned(),
            Err(_) => "<unreadable>".to_owned(),
        };
        parts.push(format!("{name}={rendered}"));
    }
    parts.join(", ")
}

impl<'frame, 'metadata, T: Backup> DeserializeRow<'frame, 'metadata> for Decoded<T> {
    /// Checks the columns against the archive struct, as decoding it directly would
    ///
    /// # Arguments
    ///
    /// * `specs` - The result's column specs
    fn type_check(specs: &[ColumnSpec]) -> Result<(), TypeCheckError> {
        // the columns must suit the archive struct
        <T as DeserializeRow<'frame, 'metadata>>::type_check(specs)
    }

    /// Decodes a row, keeping its partition key when the archive struct can't hold it
    ///
    /// # Arguments
    ///
    /// * `row` - The raw row
    fn deserialize(row: ColumnIterator<'frame, 'metadata>) -> Result<Self, DeserializationError> {
        // decode the row from a copy of the iterator so the key can still be read on failure
        match <T as DeserializeRow<'frame, 'metadata>>::deserialize(row.clone()) {
            Ok(decoded) => Ok(Decoded::Row(decoded)),
            Err(error) => Ok(Decoded::Undecodable {
                key: render_key(row, T::partition_key()),
                error: error.to_string(),
            }),
        }
    }
}

/// An archived partition that is ready to be written to disk
#[derive(Debug)]
pub struct PendingArchive {
    /// The number of rows in this partition
    rows: usize,
    /// The bytes for this archive
    bytes: AlignedVec,
}

/// What one table's backup wrote, gathered from its workers as archives close and ranges end
#[derive(Debug, Default)]
pub struct TableTally {
    /// Every archive the table's workers closed
    pub archives: Vec<ArchiveEntry>,
    /// The rows read from each token range the workers finished
    pub ranges: Vec<RangeCount>,
}

/// The mode archive and map files are created with, before the umask
const ARCHIVE_MODE: u32 = PRIVATE_FILE_MODE;

/// How many bytes an archive's data file holds before it is closed and a new one started
const ARCHIVE_ROLLOVER: usize = 10_737_418_240;

/// The current archive we are writting too
pub struct ArchiveWriter {
    /// The name of this archive
    pub name: Uuid,
    /// The path to write data too
    data_path: PathBuf,
    /// The path to write map info too
    map_path: PathBuf,
    /// The progress bar to update
    pub progress: ProgressBar,
    /// The buffers we have to write to disk still
    pending: Vec<PendingArchive>,
    /// The buffers for our map entries we have to write to disk still
    pending_maps: Vec<AlignedVec>,
    /// The current size of our pending buffers
    pending_bytes: usize,
    /// The number of bytes we have/will write to our current archive file
    written: usize,
    /// The file to write our archived data too
    data_file: Option<Box<dyn FileSink>>,
    /// The file to write our map data too
    map_file: Option<Box<dyn FileSink>>,
    /// The file-system seam to open archive and map files through
    sinks: Arc<dyn SinkFactory>,
    /// How many bytes of the current archive's data file were written and flushed
    durable_len: u64,
    /// How many map entries of the current archive were written and flushed
    durable_entries: u64,
    /// How many rows the current archive's flushed partitions hold
    durable_rows: u64,
    /// The sha256 of the current archive's flushed map entries
    map_sha256: Sha256,
    /// Where each closed archive is listed for the manifest
    tally: Arc<Mutex<TableTally>>,
    /// How many bytes an archive's data file holds before the archive counts as full
    rollover: usize,
}

impl std::fmt::Debug for ArchiveWriter {
    /// Allow an archive writer to be debug printed without its open files
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // name the paths and leave out the open files
        f.debug_struct("ArchiveWriter")
            .field("name", &self.name)
            .field("data_path", &self.data_path)
            .field("map_path", &self.map_path)
            .field("pending_bytes", &self.pending_bytes)
            .field("written", &self.written)
            .finish_non_exhaustive()
    }
}

impl ArchiveWriter {
    /// Create a new archive writer
    ///
    /// # Arguments
    ///
    /// * `data_path` - The folder to write archive data too
    /// * `map_path` - The folder to write archive map info too
    /// * `progress` - The progress bar to update
    /// * `sinks` - The file-system seam to open archive and map files through
    pub fn new(
        data_path: &Path,
        map_path: &Path,
        progress: ProgressBar,
        sinks: &Arc<dyn SinkFactory>,
    ) -> Self {
        // build our archive writer
        Self {
            name: Uuid::new_v4(),
            data_path: data_path.to_path_buf(),
            map_path: map_path.to_path_buf(),
            progress,
            pending: Vec::with_capacity(100),
            pending_maps: Vec::with_capacity(100),
            pending_bytes: 0,
            written: 0,
            data_file: None,
            map_file: None,
            sinks: sinks.clone(),
            durable_len: 0,
            durable_entries: 0,
            durable_rows: 0,
            map_sha256: Sha256::new(),
            tally: Arc::default(),
            rollover: ARCHIVE_ROLLOVER,
        }
    }

    /// Sets how many bytes an archive's data file holds before the archive counts as full, so
    /// tests can roll archives over without writing gigabytes
    ///
    /// # Arguments
    ///
    /// * `bytes` - The data file length at which an archive is full
    #[cfg(feature = "test-support")]
    #[must_use]
    pub fn with_rollover(mut self, bytes: usize) -> Self {
        // later checks of full compare against this size instead of the default
        self.rollover = bytes;
        self
    }

    /// Lists each archive this writer closes in a table's tally
    ///
    /// # Arguments
    ///
    /// * `tally` - The tally of the table this writer writes
    #[must_use]
    pub fn with_tally(mut self, tally: &Arc<Mutex<TableTally>>) -> Self {
        // share the table's tally instead of this writer's own
        self.tally = tally.clone();
        self
    }

    /// Lists the current archive, as far as it was flushed, in the tally and starts a new one
    ///
    /// # Arguments
    ///
    /// * `opened` - Whether the archive's files were opened; only those archives are listed
    fn finish_archive(&mut self, opened: bool) {
        // an archive is listed once its files exist
        if opened {
            let entry = ArchiveEntry {
                uuid: self.name,
                data_len: self.durable_len,
                entries: self.durable_entries,
                rows: self.durable_rows,
                map_sha256: HEXLOWER.encode(&std::mem::take(&mut self.map_sha256).finalize()),
            };
            self.tally
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .archives
                .push(entry);
        }
        // the next archive starts empty under a new name
        self.durable_len = 0;
        self.durable_entries = 0;
        self.durable_rows = 0;
        self.map_sha256 = Sha256::new();
        self.data_file = None;
        self.map_file = None;
        self.written = 0;
        self.name = Uuid::new_v4();
    }

    /// Add a partitions archived buffer to our pending buffer vec
    ///
    /// Returns whether the buffer has enough bytes to write off to disk
    ///
    /// # Arguments
    ///
    /// * `partition` - The hash of the partition we are adding to this archive
    /// * `rows` - The number of rows we archived in this partition
    /// * `sha256` - The sha256 for the archive we archived
    /// * `bytes` - The bytes we are archiving
    pub fn add(&mut self, partition: u64, rows: usize, sha256: String, bytes: AlignedVec) -> bool {
        // calculate where this partitions archived data ends
        let end = self.written + bytes.len();
        // build the map for this partition
        let map = PartitionArchive::new(self.written, end, partition, sha256);
        // archive this partition and log its size
        let archived_map = rkyv::to_bytes::<_, 1024>(&map).unwrap();
        // panic if our len is greater then 96 bytes
        if archived_map.len() != 96 {
            panic!("entry len is {}", archived_map.len());
        }
        // build the pending archive object
        let archive = PendingArchive {
            rows,
            //sha256,
            bytes,
        };
        // add this buffers size to our pending size
        self.pending_bytes += archive.bytes.len();
        // set our new progress bar position
        self.progress.inc(archive.bytes.len() as u64);
        // add this archive to our pending bytes
        self.pending.push(archive);
        // add this map to our pending maps
        self.pending_maps.push(archived_map);
        // update the number of bytes we have written
        self.written = end;
        // determine if we have enough bytes to write this archive to disk
        self.pending_bytes > 1_048_576
    }

    /// Open a file handle to our data file
    async fn open_handles(&mut self) -> Result<(), Error> {
        // loop until we have found an unused id
        loop {
            // cast our archive id to a string
            let id = self.name.to_string();
            // build the new data path without changing the directory we write into, so a failed
            // open leaves this writer able to try again
            let data_path = self.data_path.join(&id);
            // make sure this data file doesn't already exist
            if tokio::fs::try_exists(&data_path).await? {
                // this data file already exists so try a new random id
                self.name = Uuid::new_v4();
                continue;
            }
            // open a file handle to the data file
            self.data_file = Some(self.sinks.create(&data_path, ARCHIVE_MODE).await?);
            // open a file handle to the map file beside it in the map directory
            let map_path = self.map_path.join(format!("{id}.thoriummap"));
            self.map_file = Some(self.sinks.create(&map_path, ARCHIVE_MODE).await?);
            // we have opened our file handles
            break;
        }
        Ok(())
    }

    /// Returns the path of the archive file pending data is written to
    #[must_use]
    pub fn archive_path(&self) -> PathBuf {
        // the archive is named by its id inside the data directory
        self.data_path.join(self.name.to_string())
    }

    /// Drops every pending partition after a failed write, returning how many rows they held
    ///
    /// The archive is closed too, so the next write starts a fresh archive instead of appending
    /// after a partial one.
    pub fn discard_pending(&mut self) -> usize {
        // count the rows that will never reach disk
        let rows = self.pending.iter().map(|archive| archive.rows).sum();
        // forget the pending data and its map entries
        self.pending.clear();
        self.pending_maps.clear();
        self.pending_bytes = 0;
        // close the archive, listing what was flushed, so a partial one is never extended
        let opened = self.data_file.is_some() || self.map_file.is_some();
        self.finish_archive(opened);
        rows
    }

    /// Returns whether the current archive has grown large enough to close
    #[must_use]
    pub fn full(&self) -> bool {
        // an archive is split off once it holds its rollover size, 10 GiB outside tests
        self.written >= self.rollover
    }

    /// Closes the current archive, syncing its data and map files and then their directories,
    /// so the next write starts a new archive
    ///
    /// The writer starts afresh even when a sync fails, so a failed archive is never extended.
    pub async fn close(&mut self) -> Result<(), Error> {
        // take the open files and list the archive, so the next write opens a new archive
        // whatever happens below
        let files = (self.data_file.take(), self.map_file.take());
        self.finish_archive(files.0.is_some() || files.1.is_some());
        // a writer with no open archive has nothing to sync
        let (Some(mut data_file), Some(mut map_file)) = files else {
            return Ok(());
        };
        // force both files to disk, then make their directory entries durable
        data_file.sync().await?;
        map_file.sync().await?;
        self.sinks.sync_dir(&self.data_path).await?;
        self.sinks.sync_dir(&self.map_path).await?;
        Ok(())
    }

    /// Write any pending archived data to disk, returning how many rows were written
    ///
    /// # Arguments
    ///
    /// * `update_tx` - The channel to send updates on
    pub async fn archive(
        &mut self,
        update_tx: &mut AsyncSender<MonitorUpdate>,
    ) -> Result<usize, Error> {
        // if we have no pending bytes then just return
        if self.pending_bytes == 0 {
            return Ok(0);
        }
        // get our open data file handle
        let (data_file, map_file) = match (self.data_file.as_mut(), self.map_file.as_mut()) {
            (Some(data_file), Some(map_file)) => (data_file, map_file),
            _ => {
                // open our new file handles
                self.open_handles().await?;
                // get our opened file handles
                (
                    self.data_file.as_mut().unwrap(),
                    self.map_file.as_mut().unwrap(),
                )
            }
        };
        // write all of our pending partition data in order
        for archive in &self.pending {
            data_file.write_all(&archive.bytes).await?;
        }
        // write the map entries for those partitions in the same order, hashing them for the
        // manifest; the hash only counts once the batch is flushed
        let mut map_sha256 = self.map_sha256.clone();
        for map in &self.pending_maps {
            map_file.write_all(map).await?;
            map_sha256.update(map);
        }
        let entries = self.pending_maps.len() as u64;
        // count the rows we just wrote
        let rows_written: usize = self.pending.iter().map(|archive| archive.rows).sum();
        // build and send the updates for all of our written archives
        for archive in self.pending.drain(..) {
            // build the update for our monitor
            let update = MonitorUpdate::Update {
                items: archive.rows,
                bytes: archive.bytes.len() as u64,
            };
            // send our update to our controller
            update_tx.send(update).await?;
        }
        // clear our map updates
        self.pending_maps.clear();
        // flush our map and data files
        data_file.flush().await?;
        map_file.flush().await?;
        // the batch is in the files, so the archive's listing covers it
        self.durable_len = self.written as u64;
        self.durable_entries += entries;
        self.durable_rows += rows_written as u64;
        self.map_sha256 = map_sha256;
        // clear our pending bytes
        self.pending_bytes = 0;
        Ok(rows_written)
    }
}

/// A single backup worker for Thorium
pub struct BackupWorker<T: Backup> {
    /// The scylla client to connect with
    scylla: Arc<Session>,
    /// The prepared statement to use
    prepared: PreparedStatement,
    /// The kanal channel workers should send backup updates over
    updates: AsyncSender<MonitorUpdate>,
    /// Our current parititons key
    partition: Option<u64>,
    /// The rows for our current partition
    rows: Vec<T>,
    /// The sha256 hasher we are using for partition hashes
    hasher: Sha256,
    /// The archive we are writting too
    writer: ArchiveWriter,
    /// The current number of rows this worker has backed up
    rows_backed_up: u64,
    /// The component this worker backs up
    component: String,
    /// The run's failure log
    log: Arc<FailureLog>,
    /// The token range this worker is reading, for failure records
    range: Option<(i64, i64)>,
    /// How a page that fails to arrive is retried
    retry: RetryPolicy,
    /// Where each finished range's row count is listed for the manifest
    tally: Arc<Mutex<TableTally>>,
}

impl<T: Backup> BackupWorker<T> {
    /// Create a new backup worker
    ///
    /// # Arguments
    ///
    /// * `scylla` - The scylla client to use when backing up data
    /// * `namespace` - The namespace for this backup
    /// * `updates` - The channel to send partition archive updates on
    /// * `data_path` - The path to write archive data too
    /// * `map_path` - The path to write map data too
    /// * `progress` - The progress bar to update
    /// * `sinks` - The file-system seam to open archive and map files through
    /// * `component` - The component this worker backs up
    /// * `log` - The run's failure log
    /// * `retry` - How a page that fails to arrive is retried
    /// * `tally` - Where the table's archives and range counts are listed for the manifest
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        scylla: &Arc<Session>,
        namespace: &str,
        updates: AsyncSender<MonitorUpdate>,
        data_path: &Path,
        map_path: &Path,
        progress: ProgressBar,
        sinks: &Arc<dyn SinkFactory>,
        component: String,
        log: &Arc<FailureLog>,
        retry: RetryPolicy,
        tally: &Arc<Mutex<TableTally>>,
    ) -> Result<Self, Error> {
        // get our prepared statement
        let prepared = T::prepared_statement(scylla, namespace).await?;
        // build a new archive writer that lists its archives in the table's tally
        let writer = ArchiveWriter::new(data_path, map_path, progress, sinks).with_tally(tally);
        // build our backup worker
        let worker = BackupWorker {
            scylla: scylla.clone(),
            prepared,
            updates,
            partition: None,
            rows: Vec::with_capacity(1000),
            writer,
            rows_backed_up: 0,
            hasher: Sha256::new(),
            component,
            log: log.clone(),
            range: None,
            retry,
            tally: tally.clone(),
        };
        Ok(worker)
    }

    /// Writes pending partitions to disk, recording a write that fails instead of returning it
    ///
    /// The rows of a failed write are dropped from the backup and the record names the archive.
    async fn write_pending(&mut self) -> Result<(), Error> {
        // write what is pending and count the rows that landed
        match self.writer.archive(&mut self.updates).await {
            Ok(rows) => self.log.handled(&self.component, rows as u64),
            Err(error) => {
                // record the archive and how many rows it lost, then start a fresh archive
                let path = self.writer.archive_path();
                let rows = self.writer.discard_pending();
                let mut record = FailureRecord::failed(
                    &self.component,
                    Operation::WriteArchive,
                    format!("{rows} rows were not written: {error}"),
                )
                .table(T::name())
                .key(path.to_string_lossy());
                if let Some(range) = self.range {
                    record = record.token_range(range);
                }
                self.log.record(&record).await?;
            }
        }
        // split a full archive off into its own files
        if self.writer.full() {
            self.close_archive().await?;
        }
        Ok(())
    }

    /// Closes the current archive, recording an archive whose files couldn't be synced
    async fn close_archive(&mut self) -> Result<(), Error> {
        // name the archive before closing it starts a new one
        let path = self.writer.archive_path();
        if let Err(error) = self.writer.close().await {
            let mut record = FailureRecord::failed(
                &self.component,
                Operation::WriteArchive,
                format!("the archive couldn't be synced to disk: {error}"),
            )
            .table(T::name())
            .key(path.to_string_lossy());
            if let Some(range) = self.range {
                record = record.token_range(range);
            }
            self.log.record(&record).await?;
        }
        Ok(())
    }

    /// Archive our current rows
    ///
    /// # Arguments
    ///
    /// * `flush` - Whether we should flush map info to disk regardless
    async fn archive(&mut self, flush: bool) -> Result<(), Error> {
        // only archive data if we have some
        if !self.rows.is_empty() {
            // get our current partition
            if let Some(partition) = self.partition {
                // archive our current rows
                let archived_bytes = rkyv::to_bytes::<_, 1024>(&self.rows).unwrap();
                // hash our bytes
                self.hasher.update(&archived_bytes);
                // finalize our hash and cast it to a string
                let sha256 = HEXLOWER.encode(&self.hasher.finalize_reset());
                // get the number of rows we are archiving
                let row_count = self.rows.len();
                // add this archive and check if we have enough pending bytes to write them to disk
                let ready = self
                    .writer
                    .add(partition, row_count, sha256, archived_bytes);
                if flush || ready {
                    // we have enough pending bytes so write our archived data to disk
                    self.write_pending().await?;
                }
                // empty our current rows
                self.rows.clear();
            } else {
                return Err(Error::new("Tried to archive with no partition set"));
            }
        }
        Ok(())
    }

    /// Check if we started a new partition with this row
    ///
    /// This will flush the old partition to disk.
    ///
    /// # Arguments
    ///
    /// * `row` - The row to check
    async fn check_row(&mut self, row: T) -> Result<(), Error> {
        // Hash this rows partition key
        let hash = row.hash_partition();
        // check our hash to see if this partition changed
        match self.partition {
            // we don't yet have a partition hash so set it
            None => self.partition = Some(hash),
            // we are still in the same partition
            Some(current) if current == hash => (),
            // we have entered a new partition
            _ => {
                // archive our existing partition
                self.archive(false).await?;
                // update our partition hash
                self.partition = Some(hash);
            }
        }
        // add our new row
        self.rows.push(row);
        Ok(())
    }

    /// Start backing up data and streaming it to archives
    ///
    /// Each range is read a page at a time; a page that fails to arrive is requested again from
    /// where the last page ended, and a range whose page still fails after every attempt is
    /// recorded with how many of its rows were read.
    ///
    /// # Arguments
    ///
    /// * `orders` - The channel to receive token ranges on
    pub async fn start(mut self, orders: Orders<(i64, i64)>) -> Result<Self, Error> {
        // the session and statement outlive each range's reader, which borrows them
        let (scylla, prepared) = (self.scylla.clone(), self.prepared.clone());
        // handle messages in our channel until its closed or the run is stopped
        loop {
            // take no new range once --abort-on-failure stopped the run
            if self.log.stopped() {
                break;
            }
            // get the next message in the queue
            let (start, end) = match orders.recv().await {
                Ok(path) => path,
                Err(kanal::ReceiveError::Closed | kanal::ReceiveError::SendClosed) => break,
            };
            self.range = Some((start, end));
            // read the range a page at a time, resuming a page that fails to arrive
            let mut reader = RangeReader::new(&scylla, &prepared, (start, end), self.retry);
            let mut range_rows: u64 = 0;
            loop {
                let page = match reader.next_page::<Decoded<T>>().await {
                    Ok(Some(page)) => page,
                    Ok(None) => break,
                    Err(failed) => {
                        // the rest of the range couldn't be read, so record the range once
                        let record =
                            FailureRecord::failed(&self.component, Operation::ReadRange, &failed)
                                .table(T::name())
                                .token_range((start, end))
                                .attempts(failed.attempts);
                        self.log.record(&record).await?;
                        break;
                    }
                };
                // archive the page's rows, skipping and recording rows that can't be held
                range_rows += self.handle_page(page, (start, end)).await?;
                // stop reading the range once --abort-on-failure stopped the run
                if self.log.stopped() {
                    break;
                }
            }
            // list how many of the range's rows went to the archive writer
            self.tally
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .ranges
                .push(RangeCount {
                    start,
                    end,
                    rows: range_rows,
                });
        }
        // archive any remaining data and close the archive, syncing it to disk
        self.archive(true).await?;
        self.close_archive().await?;
        Ok(self)
    }

    /// Archives the rows of one page, recording each row the archive struct can't hold, and
    /// returns how many rows it handed to the archive writer
    ///
    /// # Arguments
    ///
    /// * `page` - The page's rows
    /// * `range` - The token range the page is part of, for the records
    async fn handle_page(
        &mut self,
        page: Page<Decoded<T>>,
        range: (i64, i64),
    ) -> Result<u64, Error> {
        // count the rows handed to the archive writer; skipped rows aren't counted
        let mut handed = 0;
        for row in page {
            // take each row that decoded, recording the ones that didn't
            let typed_row = match row {
                Ok(Decoded::Row(typed_row)) => typed_row,
                Ok(Decoded::Undecodable { key, error }) => {
                    // a row the archive struct can't hold is skipped and recorded
                    let record = FailureRecord::skipped(&self.component, error)
                        .table(T::name())
                        .token_range(range)
                        .key(key);
                    self.log.record(&record).await?;
                    // stop after this row once --abort-on-failure stopped the run
                    if self.log.stopped() {
                        break;
                    }
                    continue;
                }
                Err(error) => {
                    // a row that couldn't even be walked is skipped and recorded the same way
                    let record = FailureRecord::skipped(&self.component, error)
                        .table(T::name())
                        .token_range(range);
                    self.log.record(&record).await?;
                    // stop after this row once --abort-on-failure stopped the run
                    if self.log.stopped() {
                        break;
                    }
                    continue;
                }
            };
            // increment our row count
            self.rows_backed_up += 1;
            // set our current row count progress message
            self.writer
                .progress
                .set_message(self.rows_backed_up.to_string());
            // flush completed partitions to disk if necessary
            self.check_row(typed_row).await?;
            // count the row for the range's manifest entry
            handed += 1;
        }
        Ok(handed)
    }

    /// Shutdown this worker
    pub fn shutdown(self) {
        // shutdown our progress bar
        self.writer.progress.finish();
    }
}

#[async_trait::async_trait]
pub trait Backup:
    Utils
    + std::fmt::Debug
    + 'static
    + Send
    + Archive
    + Serialize<AllocSerializer<1024>>
    + for<'frame, 'metadata> DeserializeRow<'frame, 'metadata>
{
    /// The table this archive struct backs up, read through its generated statements
    type Table: TableStatements;

    /// The text of the statement reading a token range of the table: its generated scan of
    /// every column, which the archive struct reads by name
    ///
    /// # Arguments
    ///
    /// * `ns` - The namespace the table is in
    #[must_use]
    fn scan_cql(ns: &str) -> String {
        // fill the generated scan's keyspace
        with_keyspace(<Self::Table as TableStatements>::SCAN, ns)
    }

    /// The prepared statement to use when retrieving data from Scylla; it binds the first and
    /// last token of a range
    ///
    /// # Arguments
    ///
    /// * `scylla` - The scylla session to prepare the statement with
    /// * `ns` - The namespace the table is in
    async fn prepared_statement(
        scylla: &Session,
        ns: &str,
    ) -> Result<PreparedStatement, PrepareError> {
        // prepare the generated statement
        scylla.prepare(Self::scan_cql(ns)).await
    }

    /// Hash this rows partitions key to see if we have changed partitions
    fn hash_partition(&self) -> u64;

    /// The columns of this table's partition key, used to name rows that fail to decode
    fn partition_key() -> &'static [&'static str];
}

#[cfg(test)]
mod tests {
    use super::ArchiveWriter;
    use crate::backup::sink::{FileSink, SinkFactory, TokioFs};
    use indicatif::ProgressBar;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    /// One call a recording factory or one of its files saw
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        /// A file was created with a mode
        Create(PathBuf, u32),
        /// A file's buffered bytes were handed to the operating system
        Flush(PathBuf),
        /// A file was synced to disk
        Sync(PathBuf),
        /// A directory was synced
        SyncDir(PathBuf),
    }

    /// A factory over the real file system that records the calls made through it
    #[derive(Default)]
    struct RecordingFs {
        /// Every call, in order
        calls: Arc<Mutex<Vec<Call>>>,
    }

    impl RecordingFs {
        /// Returns the calls so far
        fn calls(&self) -> Vec<Call> {
            // copy them out from under the lock
            self.calls.lock().unwrap().clone()
        }
    }

    /// A real file that records its flushes and syncs
    struct RecordingFile {
        /// The file being written
        inner: Box<dyn FileSink>,
        /// Its path
        path: PathBuf,
        /// Where calls are recorded
        calls: Arc<Mutex<Vec<Call>>>,
    }

    #[async_trait::async_trait]
    impl FileSink for RecordingFile {
        /// Writes through to the real file
        ///
        /// # Arguments
        ///
        /// * `buf` - The bytes to write
        async fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
            // writes aren't recorded; only flushes and syncs matter here
            self.inner.write_all(buf).await
        }

        /// Records the flush and flushes the real file
        async fn flush(&mut self) -> std::io::Result<()> {
            // record, then pass through
            self.calls
                .lock()
                .unwrap()
                .push(Call::Flush(self.path.clone()));
            self.inner.flush().await
        }

        /// Records the sync and syncs the real file
        async fn sync(&mut self) -> std::io::Result<()> {
            // record, then pass through
            self.calls
                .lock()
                .unwrap()
                .push(Call::Sync(self.path.clone()));
            self.inner.sync().await
        }
    }

    #[async_trait::async_trait]
    impl SinkFactory for RecordingFs {
        /// Creates a real file and records its path and mode
        ///
        /// # Arguments
        ///
        /// * `path` - The file to create
        /// * `mode` - The mode to create it with
        async fn create(&self, path: &Path, mode: u32) -> std::io::Result<Box<dyn FileSink>> {
            // record the call, then create the file for real
            self.calls
                .lock()
                .unwrap()
                .push(Call::Create(path.to_path_buf(), mode));
            let inner = TokioFs.create(path, mode).await?;
            Ok(Box::new(RecordingFile {
                inner,
                path: path.to_path_buf(),
                calls: self.calls.clone(),
            }))
        }

        /// Opens a real file for appending
        ///
        /// # Arguments
        ///
        /// * `path` - The file to append to
        async fn append(&self, path: &Path) -> std::io::Result<Box<dyn FileSink>> {
            // pass appends straight through
            TokioFs.append(path).await
        }

        /// Renames a real file
        ///
        /// # Arguments
        ///
        /// * `from` - The current path
        /// * `to` - The new path
        async fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
            // pass renames straight through
            TokioFs.rename(from, to).await
        }

        /// Records the directory sync and syncs the real directory
        ///
        /// # Arguments
        ///
        /// * `dir` - The directory to sync
        async fn sync_dir(&self, dir: &Path) -> std::io::Result<()> {
            // record, then pass through
            self.calls
                .lock()
                .unwrap()
                .push(Call::SyncDir(dir.to_path_buf()));
            TokioFs.sync_dir(dir).await
        }

        /// Removes a real file
        ///
        /// # Arguments
        ///
        /// * `path` - The file to remove
        async fn remove(&self, path: &Path) -> std::io::Result<()> {
            // pass removals straight through
            TokioFs.remove(path).await
        }
    }

    /// Builds a writer over fresh scratch directories with a recording factory
    ///
    /// # Arguments
    ///
    /// * `name` - The test's name for the directories
    fn recording_writer(name: &str) -> (PathBuf, Arc<RecordingFs>, ArchiveWriter) {
        // name the directories by test and process so parallel runs never share them
        let root =
            std::env::temp_dir().join(format!("thoradm-writer-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (data, maps) = (root.join("data"), root.join("maps"));
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&maps).unwrap();
        let recording = Arc::new(RecordingFs::default());
        let sinks: Arc<dyn SinkFactory> = recording.clone();
        let writer = ArchiveWriter::new(&data, &maps, ProgressBar::hidden(), &sinks);
        (root, recording, writer)
    }

    /// The archive writer opens its data and map files through the injected factory
    #[tokio::test]
    async fn archive_writer_writes_through_the_sink_factory() {
        // build a writer over scratch directories with a recording factory
        let (root, recording, mut writer) = recording_writer("factory");
        let (data, maps) = (root.join("data"), root.join("maps"));
        // archive two partitions and write them out
        let (mut updates, rx) = kanal::unbounded_async();
        let first = rkyv::to_bytes::<_, 1024>(&vec![1_u64, 2]).unwrap();
        let second = rkyv::to_bytes::<_, 1024>(&vec![3_u64]).unwrap();
        let total = first.len() + second.len();
        writer.add(7, 2, "a".repeat(64), first);
        writer.add(8, 1, "b".repeat(64), second);
        writer.archive(&mut updates).await.unwrap();
        drop(rx);
        // exactly the data file and its map were created through the factory
        let created: Vec<Call> = recording
            .calls()
            .into_iter()
            .filter(|call| matches!(call, Call::Create(..)))
            .collect();
        let id = writer.name.to_string();
        assert_eq!(
            created,
            vec![
                Call::Create(data.join(&id), 0o600),
                Call::Create(maps.join(format!("{id}.thoriummap")), 0o600)
            ]
        );
        // the data file holds both partitions and the map one 96-byte entry per partition
        assert_eq!(std::fs::read(data.join(&id)).unwrap().len(), total);
        let map = std::fs::read(maps.join(format!("{id}.thoriummap"))).unwrap();
        assert_eq!(map.len(), 2 * 96);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Closing an archive syncs its data and map files, then their directories, and the next
    /// write starts a new archive
    #[tokio::test]
    async fn closing_an_archive_syncs_its_files_and_directories() {
        // write one partition and close the archive
        let (root, recording, mut writer) = recording_writer("close");
        let (data, maps) = (root.join("data"), root.join("maps"));
        let (mut updates, rx) = kanal::unbounded_async();
        writer.add(
            7,
            1,
            "a".repeat(64),
            rkyv::to_bytes::<_, 1024>(&vec![1_u64]).unwrap(),
        );
        writer.archive(&mut updates).await.unwrap();
        let id = writer.name.to_string();
        writer.close().await.unwrap();
        // both files were synced after their last write, then both directories
        let data_file = data.join(&id);
        let map_file = maps.join(format!("{id}.thoriummap"));
        let calls = recording.calls();
        let position = |call: &Call| {
            calls
                .iter()
                .position(|seen| seen == call)
                .unwrap_or_else(|| panic!("no {call:?} in {calls:?}"))
        };
        let data_sync = position(&Call::Sync(data_file.clone()));
        let map_sync = position(&Call::Sync(map_file.clone()));
        let data_dir = position(&Call::SyncDir(data.clone()));
        let map_dir = position(&Call::SyncDir(maps.clone()));
        assert!(data_sync < data_dir && map_sync < map_dir, "{calls:?}");
        // a write after closing opens a new archive
        writer.add(
            8,
            1,
            "b".repeat(64),
            rkyv::to_bytes::<_, 1024>(&vec![2_u64]).unwrap(),
        );
        writer.archive(&mut updates).await.unwrap();
        assert_ne!(writer.name.to_string(), id);
        drop(rx);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
