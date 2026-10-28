//! A backup scrubbing worker for Thoradm
use indicatif::ProgressBar;
use kanal::AsyncSender;
use std::marker::PhantomData;
use std::path::PathBuf;
use std::sync::Arc;

use super::archive::check_partition;
use super::failures::{FailureLog, FailureRecord, Operation};
use super::{ArchiveReader, MonitorUpdate, Utils};
use crate::Error;
use crate::shared::workers::Orders;

/// The worker that handles scrubbing data in Thorium
pub struct ScrubWorker<S: Scrub> {
    /// The type we are scrubbing
    phantom: PhantomData<S>,
    /// The kanal channel workers should send scrub updates over
    updates: AsyncSender<MonitorUpdate>,
    /// The progress bar to track progress with
    progress: ProgressBar,
    /// The component this worker scrubs
    component: String,
    /// The run's failure log
    log: Arc<FailureLog>,
}

impl<S: Scrub> ScrubWorker<S> {
    /// Create a new ScrubWorker
    ///
    /// # Arguments
    ///
    /// * `updates` - The kanal channel to send scrub updates over
    /// * `progress` - The progress bar to track progress with
    /// * `component` - The component this worker scrubs
    /// * `log` - The run's failure log
    pub fn new(
        updates: AsyncSender<MonitorUpdate>,
        progress: ProgressBar,
        component: String,
        log: &Arc<FailureLog>,
    ) -> Self {
        ScrubWorker {
            phantom: PhantomData,
            updates,
            progress,
            component,
            log: log.clone(),
        }
    }

    /// Scrub an archive for bitrot
    ///
    /// # Arguments
    ///
    /// * `orders` - The channel to receive map paths on
    pub async fn scrub(mut self, orders: Orders<PathBuf>) -> Result<Self, Error> {
        // handle messages in our channel until its closed or the run is stopped
        loop {
            // take no new archive once --abort-on-failure stopped the run
            if self.log.stopped() {
                break;
            }
            // get the next message in the queue
            let map_path = match orders.recv().await {
                Ok(path) => path,
                Err(kanal::ReceiveError::Closed) => break,
                Err(kanal::ReceiveError::SendClosed) => break,
            };
            // scrub this archive, recording one that can't be read
            if let Err(error) = self.scrub_archive(&map_path).await {
                let record = FailureRecord::failed(&self.component, Operation::ReadArchive, error)
                    .table(S::name())
                    .key(map_path.to_string_lossy());
                self.log.record(&record).await?;
            }
        }
        Ok(self)
    }

    /// Checks every partition of one archive, recording partitions whose checksum differs
    ///
    /// # Arguments
    ///
    /// * `map_path` - The map of the archive to check
    async fn scrub_archive(&mut self, map_path: &std::path::Path) -> Result<(), Error> {
        // build our reader for this archive
        let mut reader = ArchiveReader::new(map_path.to_path_buf()).await?;
        // crawl over the data for our partitions and scrub them
        while let Some((scrub_slice, partition)) = reader.next_partition().await? {
            // stop after the current partition once --abort-on-failure stopped the run
            if self.log.stopped() {
                break;
            }
            // set our new writer position
            self.progress.inc(scrub_slice.len() as u64);
            // record a partition whose bytes no longer match its checksum
            match check_partition(scrub_slice, &partition) {
                Ok(()) => self.log.handled(&self.component, 1),
                Err(mismatch) => {
                    let record =
                        FailureRecord::failed(&self.component, Operation::Verify, mismatch)
                            .table(S::name())
                            .key(map_path.to_string_lossy())
                            .object(partition.partition.to_string());
                    self.log.record(&record).await?;
                }
            }
            // build our monitor update
            let update = MonitorUpdate::Update {
                items: 1,
                bytes: scrub_slice.len() as u64,
            };
            // send our update to the global tracker
            self.updates.send(update).await?;
        }
        Ok(())
    }

    /// Shutdown this worker
    pub fn shutdown(self) {
        // shutdown our progress bar
        self.progress.finish();
    }
}

pub trait Scrub: Utils + std::fmt::Debug + 'static + Send {}
