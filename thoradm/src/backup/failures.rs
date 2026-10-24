//! Failure accounting for backup, restore, and scrub runs
//!
//! Every object a run fails to handle, or deliberately skips, is written as one line of
//! `failures.jsonl` and logged on stderr, and so is every broken link a backup accepts. The same
//! log keeps each component's handled, skipped, failed, and broken-link counts, and carries the
//! stop signal `--abort-on-failure` raises at the first failed or skipped record.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::sink::{FileSink, SinkFactory};
use crate::Error;
use crate::shared::workers::WorkerFailure;

/// The name of the failures file in a backup root or a report directory
pub const FAILURES_FILE: &str = "failures.jsonl";

/// Whether an object failed or was deliberately skipped
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureKind {
    /// The object exhausted its retries or hit a permanent error
    Failed,
    /// A row that couldn't be deserialized into the archive struct
    Skipped,
    /// A reference to an object the source doesn't hold, kept as it is in the source and accepted
    /// by `--accept-broken-links`
    BrokenLink,
}

/// The operation that failed
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Operation {
    /// Reading a token range from Scylla
    ReadRange,
    /// Deserializing one row
    DeserializeRow,
    /// Writing an archive or map file
    WriteArchive,
    /// Reading an archive or map file, or listing a backup directory
    ReadArchive,
    /// Downloading an S3 object
    Download,
    /// Uploading an S3 object
    Upload,
    /// Aborting a failed multipart upload
    AbortUpload,
    /// Inserting rows into Scylla
    Insert,
    /// Fetching or writing the Redis state through the API
    Redis,
    /// A verify check
    Verify,
    /// A restore preflight check
    Preflight,
    /// A step before any row is restored (the schema apply that converges the keyspace)
    PreRestore,
    /// A post-restore step (views, nodes, census)
    PostRestore,
    /// Truncating a table during a `--wipe` restore
    Wipe,
    /// Any operation added later that this reader doesn't know
    #[serde(other)]
    Unknown,
}

/// One failed or skipped object, written as one line of `failures.jsonl`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FailureRecord {
    /// Whether the object failed or was skipped
    pub kind: FailureKind,
    /// The component the object belongs to (`samples-list`, `result-files`, `redis`, …)
    pub component: String,
    /// The Scylla table, when the object is a row, a range, or an archive
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<String>,
    /// The S3 bucket, when the object is an S3 object
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
    /// The token range `[start, end]`, when the object is a range or a row
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_range: Option<(i64, i64)>,
    /// The partition key, S3 key, or Redis key, as much of it as could be read
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// A short identifier of the object for humans (a sha256, an id, a user name)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
    /// The record that names a missing object, as `<table or collection>:<id>`, set on a broken
    /// link
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub referrer: Option<String>,
    /// The operation that failed
    pub operation: Operation,
    /// How many attempts were made, including the first
    pub attempts: u32,
    /// The final error's text, on one line
    pub error: String,
    /// When the last attempt ended
    pub at: DateTime<Utc>,
}

impl FailureRecord {
    /// Starts a record of a failed object made in one attempt, at the current time
    ///
    /// # Arguments
    ///
    /// * `component` - The component the object belongs to
    /// * `operation` - The operation that failed
    /// * `error` - The error the operation returned
    pub fn failed<C: Into<String>, E: std::fmt::Display>(
        component: C,
        operation: Operation,
        error: E,
    ) -> Self {
        // fill in what every record has; callers add what names the object
        FailureRecord {
            kind: FailureKind::Failed,
            component: component.into(),
            table: None,
            bucket: None,
            token_range: None,
            key: None,
            object: None,
            referrer: None,
            operation,
            attempts: 1,
            error: error.to_string(),
            at: Utc::now(),
        }
    }

    /// Starts a record of a row that was skipped because it couldn't be deserialized
    ///
    /// # Arguments
    ///
    /// * `component` - The component the row belongs to
    /// * `error` - The deserialization error
    pub fn skipped<C: Into<String>, E: std::fmt::Display>(component: C, error: E) -> Self {
        // a skip is a failed record of the deserialize operation with the skipped kind
        let mut record = Self::failed(component, Operation::DeserializeRow, error);
        record.kind = FailureKind::Skipped;
        record
    }

    /// Starts a record of a broken link: a reference to an S3 object the source doesn't hold,
    /// which a backup accepts and keeps as it is
    ///
    /// # Arguments
    ///
    /// * `component` - The component whose object the link names
    /// * `error` - Why the object is missing
    pub fn broken_link<C: Into<String>, E: std::fmt::Display>(component: C, error: E) -> Self {
        // a broken link is a failed download of an object that isn't there, with the broken kind
        let mut record = Self::failed(component, Operation::Download, error);
        record.kind = FailureKind::BrokenLink;
        record
    }

    /// Names the record that names a missing object
    ///
    /// # Arguments
    ///
    /// * `referrer` - The referring record, as `<table or collection>:<id>`
    #[must_use]
    pub fn referrer<T: Into<String>>(mut self, referrer: T) -> Self {
        // set the referrer
        self.referrer = Some(referrer.into());
        self
    }

    /// Names the table the object belongs to
    ///
    /// # Arguments
    ///
    /// * `table` - The Scylla table
    #[must_use]
    pub fn table<T: Into<String>>(mut self, table: T) -> Self {
        // set the table
        self.table = Some(table.into());
        self
    }

    /// Names the bucket the object is in
    ///
    /// # Arguments
    ///
    /// * `bucket` - The S3 bucket
    #[must_use]
    pub fn bucket<T: Into<String>>(mut self, bucket: T) -> Self {
        // set the bucket
        self.bucket = Some(bucket.into());
        self
    }

    /// Names the token range the object came from
    ///
    /// # Arguments
    ///
    /// * `range` - The token range `(start, end)`
    #[must_use]
    pub fn token_range(mut self, range: (i64, i64)) -> Self {
        // set the token range
        self.token_range = Some(range);
        self
    }

    /// Names the object's key
    ///
    /// # Arguments
    ///
    /// * `key` - The partition key, S3 key, Redis key, or file path
    #[must_use]
    pub fn key<T: Into<String>>(mut self, key: T) -> Self {
        // set the key
        self.key = Some(key.into());
        self
    }

    /// Names the object for humans
    ///
    /// # Arguments
    ///
    /// * `object` - A short identifier (a sha256, an id, a partition hash)
    #[must_use]
    pub fn object<T: Into<String>>(mut self, object: T) -> Self {
        // set the object
        self.object = Some(object.into());
        self
    }

    /// Sets how many attempts were made
    ///
    /// # Arguments
    ///
    /// * `attempts` - The attempts, including the first
    #[must_use]
    pub fn attempts(mut self, attempts: u32) -> Self {
        // set the attempt count
        self.attempts = attempts;
        self
    }
}

/// An order a step hands its workers, which names what a worker that ended early lost
pub trait OrderKey {
    /// Adds what this order names to a record
    ///
    /// # Arguments
    ///
    /// * `record` - The record to name the order in
    #[must_use]
    fn name_in(&self, record: FailureRecord) -> FailureRecord;
}

impl OrderKey for (i64, i64) {
    /// Names the token range a worker was reading
    ///
    /// # Arguments
    ///
    /// * `record` - The record to name the range in
    fn name_in(&self, record: FailureRecord) -> FailureRecord {
        // a range order is a token range
        record.token_range(*self)
    }
}

impl OrderKey for PathBuf {
    /// Names the archive map a worker was reading
    ///
    /// # Arguments
    ///
    /// * `record` - The record to name the map in
    fn name_in(&self, record: FailureRecord) -> FailureRecord {
        // a map order is the map file's path
        record.key(self.to_string_lossy())
    }
}

impl FailureRecord {
    /// Builds the one record of a worker that ended early, naming the order it held
    ///
    /// # Arguments
    ///
    /// * `component` - The component the worker handled
    /// * `operation` - The operation the worker's orders perform
    /// * `table` - The table the worker handled
    /// * `failure` - How the worker ended and the order it held
    pub fn worker_ended<O: OrderKey>(
        component: &str,
        operation: Operation,
        table: &str,
        failure: &WorkerFailure<O>,
    ) -> Self {
        // the worker's error, with the range or map it didn't finish
        let record = FailureRecord::failed(component, operation, &failure.error).table(table);
        match &failure.order {
            Some(order) => order.name_in(record),
            None => record,
        }
    }

    /// Builds the record of a component that couldn't run, or `None` when its error is a step
    /// whose workers ended early and were each recorded already
    ///
    /// # Arguments
    ///
    /// * `component` - The component that couldn't run
    /// * `operation` - The component's operation
    /// * `error` - The component's error
    #[must_use]
    pub fn component_failed(component: &str, operation: Operation, error: &Error) -> Option<Self> {
        // one worker that ended early is one record, made when its step joined it
        match error {
            Error::WorkersEnded(_) => None,
            error => Some(FailureRecord::failed(component, operation, error)),
        }
    }
}

/// What a run does after it records a failed or skipped object
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FailurePolicy {
    /// Record the object and carry on to the end of the run (the default)
    #[default]
    Continue,
    /// Record the object and stop the run (`--abort-on-failure`)
    AbortOnFailure,
}

impl FailurePolicy {
    /// Builds the policy from the `--abort-on-failure` flag
    ///
    /// # Arguments
    ///
    /// * `abort_on_failure` - Whether `--abort-on-failure` was given
    #[must_use]
    pub fn from_flag(abort_on_failure: bool) -> Self {
        // the flag is the only way to stop early; there is no count or threshold
        if abort_on_failure {
            FailurePolicy::AbortOnFailure
        } else {
            FailurePolicy::Continue
        }
    }
}

/// What one component handled, skipped, failed, and accepted as broken links
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentCounts {
    /// Objects written to the backup or the target
    pub handled: u64,
    /// Rows skipped because they couldn't be deserialized
    pub skipped: u64,
    /// Broken links accepted and kept as they are in the source; left out of a summary when none
    #[serde(default, skip_serializing_if = "is_zero")]
    pub broken_links: u64,
    /// Objects that failed after their retries
    pub failed: u64,
}

/// Returns true for zero, so a count that is zero can be left out of a serialized summary
///
/// # Arguments
///
/// * `count` - The count to check
// serde's `skip_serializing_if` passes the field by reference
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(count: &u64) -> bool {
    // serde hands over a reference to the field
    *count == 0
}

/// An append-only `failures.jsonl` writer shared by every worker of a run
pub struct FailureLog {
    /// Where the failures file is
    path: PathBuf,
    /// The open file, behind a lock so lines never interleave
    file: tokio::sync::Mutex<Box<dyn FileSink>>,
    /// Set when a write or sync failed part way, which may have left part of a line that any
    /// later record would be joined to
    torn: AtomicBool,
    /// What the run does after a record
    policy: FailurePolicy,
    /// Raised by the first record under `FailurePolicy::AbortOnFailure`; the controller stops sending
    /// orders and each worker stops after its current object
    stop: CancellationToken,
    /// How many failed records were written
    failed: AtomicU64,
    /// How many skipped records were written
    skipped: AtomicU64,
    /// How many broken-link records were written
    broken: AtomicU64,
    /// Each component's counts, by component name
    counts: Mutex<BTreeMap<String, ComponentCounts>>,
}

impl std::fmt::Debug for FailureLog {
    /// Allow a failure log to be debug printed without its open file
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // show the path, policy, and counts and leave out the open file
        f.debug_struct("FailureLog")
            .field("path", &self.path)
            .field("policy", &self.policy)
            .field("failed", &self.failed)
            .field("skipped", &self.skipped)
            .field("broken", &self.broken)
            .finish_non_exhaustive()
    }
}

impl FailureLog {
    /// Opens (or creates) the failures file in a directory
    ///
    /// # Arguments
    ///
    /// * `dir` - The backup root or the report directory
    /// * `policy` - What the run does after a record
    /// * `sinks` - The file-system seam to open the file through
    pub async fn open(
        dir: &Path,
        policy: FailurePolicy,
        sinks: &dyn SinkFactory,
    ) -> Result<Self, Error> {
        // drop a record an earlier run left half written, so the next record starts its own line
        let path = dir.join(FAILURES_FILE);
        drop_torn_tail(&path).await?;
        // open the file for appending through the seam so tests can inject faults
        let file = sinks.append(&path).await?;
        // sync the directory so the file's entry is as durable as the lines synced into it
        sinks.sync_dir(dir).await?;
        Ok(FailureLog {
            path,
            file: tokio::sync::Mutex::new(file),
            torn: AtomicBool::new(false),
            policy,
            stop: CancellationToken::new(),
            failed: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            broken: AtomicU64::new(0),
            counts: Mutex::default(),
        })
    }

    /// Returns the path of the failures file
    #[must_use]
    pub fn path(&self) -> &Path {
        // the path was fixed when the log was opened
        &self.path
    }

    /// Returns the policy this run follows
    #[must_use]
    pub fn policy(&self) -> FailurePolicy {
        // the policy was fixed when the log was opened
        self.policy
    }

    /// Appends one record, then flushes and syncs it so it survives a crash of this run
    ///
    /// The record is also logged on stderr, counted against its component, and under
    /// `--abort-on-failure` a failed or skipped record raises the stop signal once it is durable.
    ///
    /// # Arguments
    ///
    /// * `record` - The failed or skipped object
    pub async fn record(&self, record: &FailureRecord) -> Result<(), Error> {
        // log the record on stderr whether or not stderr is a terminal
        match record.kind {
            FailureKind::Failed => error!(
                component = record.component,
                table = record.table,
                bucket = record.bucket,
                key = record.key,
                operation = ?record.operation,
                attempts = record.attempts,
                "failed: {}",
                record.error
            ),
            FailureKind::Skipped => warn!(
                component = record.component,
                table = record.table,
                key = record.key,
                operation = ?record.operation,
                "skipped: {}",
                record.error
            ),
            FailureKind::BrokenLink => warn!(
                component = record.component,
                bucket = record.bucket,
                key = record.key,
                referrer = record.referrer,
                "broken link: {}",
                record.error
            ),
        }
        // serialize to one line; serde_json escapes newlines inside the error text
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        // write, flush, and sync under the lock so lines never interleave
        {
            let mut file = self.file.lock().await;
            // a record appended after part of a line would make both unreadable
            if self.torn.load(Ordering::Acquire) {
                return Err(Error::new(format!(
                    "{} ends in a partly written record, so no more records can be added",
                    self.path.display()
                )));
            }
            // a failed write or sync may leave part of the line in the file
            let written = match file.write_all(&line).await {
                Ok(()) => file.sync().await,
                Err(error) => Err(error),
            };
            if let Err(error) = written {
                self.torn.store(true, Ordering::Release);
                return Err(error.into());
            }
        }
        // count the record by kind for the run outcome and by component for the summary
        let mut counts = self
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = counts.entry(record.component.clone()).or_default();
        match record.kind {
            FailureKind::Failed => {
                self.failed.fetch_add(1, Ordering::Relaxed);
                entry.failed += 1;
            }
            FailureKind::Skipped => {
                self.skipped.fetch_add(1, Ordering::Relaxed);
                entry.skipped += 1;
            }
            FailureKind::BrokenLink => {
                self.broken.fetch_add(1, Ordering::Relaxed);
                entry.broken_links += 1;
            }
        }
        drop(counts);
        // under --abort-on-failure, the first failed or skipped record stops the run once it is
        // durable; a broken link is accepted, so it never does
        if self.policy == FailurePolicy::AbortOnFailure && record.kind != FailureKind::BrokenLink {
            self.stop.cancel();
        }
        Ok(())
    }

    /// Counts objects a component handled
    ///
    /// # Arguments
    ///
    /// * `component` - The component that handled them
    /// * `handled` - How many objects were written to the backup or the target
    pub fn handled(&self, component: &str, handled: u64) {
        // add to the component's handled count
        let mut counts = self
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        counts.entry(component.to_owned()).or_default().handled += handled;
    }

    /// Returns a component's counts so far
    ///
    /// # Arguments
    ///
    /// * `component` - The component to look up
    #[must_use]
    pub fn counts(&self, component: &str) -> ComponentCounts {
        // a component that recorded nothing has zero counts
        let counts = self
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        counts.get(component).copied().unwrap_or_default()
    }

    /// Returns every component's counts so far, by component name
    #[must_use]
    pub fn all_counts(&self) -> BTreeMap<String, ComponentCounts> {
        // copy the counts out from under the lock
        self.counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Logs a component's summary line on stderr
    ///
    /// # Arguments
    ///
    /// * `component` - The component that finished
    pub fn summarize(&self, component: &str) {
        // one line per component, whether or not anything failed
        let counts = self.counts(component);
        // broken links are named only when the run accepted some, so other summaries read as ever
        let broken = if counts.broken_links > 0 {
            format!(", {} broken links", counts.broken_links)
        } else {
            String::new()
        };
        info!(
            component,
            handled = counts.handled,
            skipped = counts.skipped,
            failed = counts.failed,
            broken_links = counts.broken_links,
            "{component}: {} handled, {} skipped, {} failed{broken}",
            counts.handled,
            counts.skipped,
            counts.failed
        );
    }

    /// Returns the token the controller and workers watch to stop early
    #[must_use]
    pub fn stop_signal(&self) -> CancellationToken {
        // clones share one state, so every worker sees the same signal
        self.stop.clone()
    }

    /// Returns true when `--abort-on-failure` stopped the run
    #[must_use]
    pub fn stopped(&self) -> bool {
        // only the abort policy ever cancels the token
        self.stop.is_cancelled()
    }

    /// Returns the run's counts: objects handled across every component, and the failed, skipped,
    /// and broken-link records
    #[must_use]
    pub fn totals(&self) -> ComponentCounts {
        // handled objects are counted per component; records are counted by kind
        let handled = self
            .counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|counts| counts.handled)
            .sum();
        ComponentCounts {
            handled,
            broken_links: self.broken.load(Ordering::Relaxed),
            skipped: self.skipped.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
        }
    }

    /// Returns how many records this log holds, of every kind
    #[must_use]
    pub fn total(&self) -> u64 {
        // sum the kinds; any record makes a run incomplete, and each is a line of the file
        self.failed.load(Ordering::Relaxed)
            + self.skipped.load(Ordering::Relaxed)
            + self.broken.load(Ordering::Relaxed)
    }
}

/// How many bytes `drop_torn_tail` reads at a time while looking back for the last newline
const TAIL_CHUNK: u64 = 65_536;

/// Truncates a failures file to its last complete line, or ends a whole last record that
/// lacks only its newline
///
/// A run that crashed or ran out of space while writing a record leaves part of a line at the
/// end. Appending after it would join that part and the next record into one unreadable line.
///
/// # Arguments
///
/// * `path` - The failures file, which may not exist yet
async fn drop_torn_tail(path: &Path) -> Result<(), Error> {
    // a missing file has nothing torn
    let mut file = match tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .await
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    // look back from the end, one chunk at a time, for the last newline
    let len = file.metadata().await?.len();
    let mut end = len;
    let mut keep = 0;
    let mut chunk = Vec::new();
    while end > 0 {
        let start = end.saturating_sub(TAIL_CHUNK);
        chunk.resize(usize::try_from(end - start).unwrap_or_default(), 0);
        file.seek(std::io::SeekFrom::Start(start)).await?;
        file.read_exact(&mut chunk).await?;
        // a file ending in a newline is whole
        if end == len && chunk.last() == Some(&b'\n') {
            return Ok(());
        }
        if let Some(position) = chunk.iter().rposition(|byte| *byte == b'\n') {
            keep = start + position as u64 + 1;
            break;
        }
        end = start;
    }
    // an empty file has nothing torn
    if len == 0 {
        return Ok(());
    }
    // a whole record missing only its newline is kept, as reading keeps it, and ended
    let mut tail = Vec::new();
    file.seek(std::io::SeekFrom::Start(keep)).await?;
    file.read_to_end(&mut tail).await?;
    if serde_json::from_slice::<FailureRecord>(&tail).is_ok() {
        file.seek(std::io::SeekFrom::End(0)).await?;
        file.write_all(b"\n").await?;
        file.sync_all().await?;
        return Ok(());
    }
    // cut the partial record off and make the cut durable before appending
    warn!(
        path = %path.display(),
        dropped = len - keep,
        "dropping a partly written record at the end of the failures file"
    );
    file.set_len(keep).await?;
    file.sync_all().await?;
    Ok(())
}

/// Reads every record of a failures file, in order
///
/// A partly written last line, left by a run that crashed while writing it, is skipped with a
/// warning; an unreadable line anywhere else is an error.
///
/// # Arguments
///
/// * `path` - The failures file
pub fn read_records(path: &Path) -> Result<Vec<FailureRecord>, Error> {
    // a file that doesn't end in a newline may end in a partly written record
    let text = std::fs::read_to_string(path)?;
    let (whole, tail) = match text.rfind('\n') {
        Some(end) => text.split_at(end + 1),
        None => ("", text.as_str()),
    };
    // parse each non-empty complete line as one record
    let mut records = whole
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).map_err(Error::from))
        .collect::<Result<Vec<FailureRecord>, Error>>()?;
    // keep a last line that parses, and skip one that was cut off
    if !tail.trim().is_empty() {
        match serde_json::from_str(tail) {
            Ok(record) => records.push(record),
            Err(error) => warn!(
                path = %path.display(),
                bytes = tail.len(),
                "skipping a partly written record at the end of the failures file: {error}"
            ),
        }
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::Operation;

    /// The step before any row is restored round-trips as `pre-restore`, and an operation this
    /// reader doesn't know still parses as unknown
    #[test]
    fn the_pre_restore_operation_round_trips() {
        // the operation is written in kebab case and read back as itself
        let written = serde_json::to_string(&Operation::PreRestore).expect("serializes");
        assert_eq!(written, "\"pre-restore\"");
        let read: Operation = serde_json::from_str(&written).expect("parses");
        assert_eq!(read, Operation::PreRestore);
        // an operation added later parses as unknown
        let later: Operation = serde_json::from_str("\"added-later\"").expect("parses");
        assert_eq!(later, Operation::Unknown);
    }
}
