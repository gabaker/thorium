//! The runner API every data migration implements, and what the runner gives it

use async_trait::async_trait;
use bb8_redis::RedisConnectionManager;
use bb8_redis::bb8::Pool;
use scylla::client::session::Session;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use super::{AppliedReport, CheckWait, IssueReport, PassCounts};
use crate::Conf;
use crate::models::upgrades::{IssueReason, Record};

/// What a migration supplies to the runner; `Send + Sync` so it sits in a `const` registry as
/// `&'static dyn MigrationRunner`
#[async_trait]
pub trait MigrationRunner: Send + Sync {
    /// Split the family into `n` contiguous slices (Scylla token ranges, a Redis index set's
    /// members by hash, or one `SCAN` slice, as the family's `LayoutDef` declares, or S3 prefixes)
    ///
    /// # Arguments
    ///
    /// * `n` - How many slices to make
    /// * `ctx` - The backends, settings, and mode of this run
    async fn slices(&self, n: u32, ctx: &RunContext) -> Result<Vec<Slice>, MigrateError>;

    /// Read the next batch of a slice after a position, at most 10,000 records
    ///
    /// # Arguments
    ///
    /// * `slice` - The slice to read
    /// * `after` - The last position done, or the slice's start
    /// * `ctx` - The backends, settings, and mode of this run
    async fn next_batch(
        &self,
        slice: &Slice,
        after: &serde_json::Value,
        ctx: &RunContext,
    ) -> Result<Batch, MigrateError>;

    /// Classify one record and, when it converts, return what to write; in `Check` mode nothing is
    /// written and the early stop doesn't apply
    ///
    /// # Arguments
    ///
    /// * `record` - The stored record
    /// * `mode` - `Run` or `Check`
    /// * `ctx` - The backends, settings, and mode of this run
    async fn process(
        &self,
        record: &Record,
        mode: RunMode,
        ctx: &RunContext,
    ) -> Result<Outcome, MigrateError>;

    /// Work after every slice is done, such as rebuilding a census or recording layouts
    ///
    /// # Arguments
    ///
    /// * `ctx` - The backends, settings, and mode of this run
    async fn finish(&self, ctx: &RunContext) -> Result<(), MigrateError>;

    /// In `Check` mode, after every slice is classified, whether the step must wait before it can
    /// be judged; the runner puts the answer in the report's `wait`. Most migrations never wait,
    /// so the default returns `None`
    ///
    /// # Arguments
    ///
    /// * `counts` - The counts the check's classification found
    /// * `ctx` - The backends, settings, and mode of this run
    async fn check_wait(
        &self,
        counts: &PassCounts,
        ctx: &RunContext,
    ) -> Result<Option<CheckWait>, MigrateError> {
        // most migrations never wait, so the counts and the context go unused
        let _ = (counts, ctx);
        Ok(None)
    }

    /// Write one converted record to its backend
    ///
    /// A timeout, an unavailable replica, or an overloaded node is a [`MigrateError::Backend`],
    /// which the runner retries with fewer writes in flight; a refusal that retrying can't change
    /// is a [`MigrateError::Rejected`], which the runner records as a `write_failed` issue.
    ///
    /// # Arguments
    ///
    /// * `record` - The converted record
    /// * `ctx` - The backends, settings, and mode of this run
    async fn write(&self, record: &Record, ctx: &RunContext) -> Result<(), MigrateError> {
        // a migration that converts records must say how they are written
        let _ = (record, ctx);
        Err(MigrateError::NotImplemented(
            "this migration doesn't write records".to_owned(),
        ))
    }

    /// Read the named objects directly, for `migrate run --objects`
    ///
    /// Returns `None` when the migration can't look objects up by key, so the runner scans the
    /// family and keeps the named ones.
    ///
    /// # Arguments
    ///
    /// * `keys` - The objects' keys, as `IssueReport::key` renders them
    /// * `ctx` - The backends, settings, and mode of this run
    async fn fetch(
        &self,
        keys: &[String],
        ctx: &RunContext,
    ) -> Result<Option<Vec<Record>>, MigrateError> {
        // most migrations scan instead
        let _ = (keys, ctx);
        Ok(None)
    }

    /// Check that a migrated record loads through the API's own deserialization, for
    /// `migrate verify`
    ///
    /// # Arguments
    ///
    /// * `record` - The stored record
    /// * `ctx` - The backends, settings, and mode of this run
    async fn verify(&self, record: &Record, ctx: &RunContext) -> Result<(), IssueReason> {
        // a migration with nothing to verify accepts every record
        let _ = (record, ctx);
        Ok(())
    }

    /// Add what only this migration knows to the applied report when a pass ends: notes,
    /// warnings, and the mappings it made
    ///
    /// # Arguments
    ///
    /// * `report` - The report the runner built from the pass's counts and issues
    /// * `ctx` - The backends, settings, and mode of this run
    async fn finish_report(
        &self,
        report: &mut AppliedReport,
        ctx: &RunContext,
    ) -> Result<(), MigrateError> {
        // most migrations have nothing to add
        let _ = (report, ctx);
        Ok(())
    }
}

/// Whether a run writes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    /// Convert and write, stopping early when a group exceeds `group_max`
    Run,
    /// Classify only, write nothing, and never stop early (`migrate check`)
    Check,
}

/// One contiguous part of a family, worked by one worker
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slice {
    /// The slice index, which names the worker (`w00`, …)
    pub index: u32,
    /// Where the slice starts, in the runner's position form
    pub start: serde_json::Value,
    /// Where the slice ends
    pub end: serde_json::Value,
}

/// The records of one batch
#[derive(Debug, Clone)]
pub struct Batch {
    /// The records read
    pub records: Vec<Record>,
    /// The position after the last record, recorded in the checkpoint
    pub next: serde_json::Value,
    /// Whether the slice is exhausted
    pub done: bool,
}

/// What processing one record decided
#[derive(Debug, Clone)]
pub enum Outcome {
    /// Already in the target layout; skipped
    Current,
    /// Converted; write this record
    Write(Record),
    /// Left untouched and recorded as an issue
    Issue(IssueReport),
}

/// What the runner gives a migration's methods
#[derive(Clone)]
pub struct RunContext {
    /// `Run` or `Check`
    pub mode: RunMode,
    /// The Scylla, Redis, and S3 clients the migration reads and writes through
    pub backends: RunBackends,
    /// Set by SIGTERM, a lost writer lease, or another worker's early stop; the workers check it
    /// between batches
    pub cancel: CancellationToken,
    /// The bounds the runner works within
    pub limits: RunLimits,
    /// The attempt of the pass this context runs in, set by the runner when it ends the pass, so
    /// a migration's own progress can be tied to the attempt that made it
    pub attempt: Option<String>,
}

impl RunContext {
    /// A context with the default limits
    ///
    /// # Arguments
    ///
    /// * `mode` - `Run` or `Check`
    /// * `backends` - The clients the migration reads and writes through
    /// * `cancel` - Cancelled by SIGTERM
    #[must_use]
    pub fn new(mode: RunMode, backends: RunBackends, cancel: CancellationToken) -> Self {
        // every limit starts at its default
        RunContext {
            mode,
            backends,
            cancel,
            limits: RunLimits::default(),
            attempt: None,
        }
    }
}

/// The bounds a run works within; each default is the design's
#[derive(Debug, Clone)]
pub struct RunLimits {
    /// The most records a batch holds
    pub batch_rows: usize,
    /// The longest a migration spends reading one batch
    pub batch_time: Duration,
    /// How many writes a worker starts with in flight, and the most it grows to
    pub writes_initial: usize,
    /// The most writes a worker has in flight
    pub writes_max: usize,
    /// How many times a read or write that timed out is retried before the run stops resumable
    pub retries: u32,
    /// The first pause before a retry, which doubles up to ten seconds
    pub backoff: Duration,
    /// The least time between two checkpoints of one worker
    pub checkpoint_every: Duration,
    /// How many keys one census `SCAN` step asks for
    pub scan_count: usize,
    /// Stops a census rebuild with a cancellation after this many batches, which leaves its
    /// progress for the next run; unset in every real run, which tests use to interrupt a census
    pub census_batches: Option<usize>,
}

impl Default for RunLimits {
    /// The design's defaults: batches of 10,000 rows or 5 seconds, 16 writes in flight, a
    /// checkpoint at most once a second
    fn default() -> Self {
        // the values the runner page gives
        RunLimits {
            batch_rows: 10_000,
            batch_time: Duration::from_secs(5),
            writes_initial: 16,
            writes_max: 64,
            retries: 12,
            backoff: Duration::from_millis(250),
            checkpoint_every: Duration::from_secs(1),
            scan_count: 10_000,
            census_batches: None,
        }
    }
}

/// The clients a migration reads and writes through, connected by thoradm
#[derive(Clone)]
pub struct RunBackends {
    /// The Thorium namespace, which is also the Scylla keyspace and the prefix of Redis keys
    pub namespace: String,
    /// The partition size the config gives each data type: `files`, `repos`, `results`,
    /// `entities`, `associations`, `tags.files`, `tags.repos`, and `tags.entities`
    pub partition_sizes: std::collections::BTreeMap<String, u16>,
    /// The Scylla session
    pub scylla: Arc<Session>,
    /// The Redis connection pool
    pub redis: Pool<RedisConnectionManager>,
    /// The S3 client
    pub s3: aws_sdk_s3::Client,
    /// The cluster's loaded config, for the settings a migration reads beyond the namespace and
    /// the partition sizes
    pub conf: Arc<Conf>,
}

/// Why the runner or a migration couldn't go on
#[derive(Debug)]
pub enum MigrateError {
    /// The runner or this migration isn't built yet
    NotImplemented(String),
    /// A backend refused a read or write, or couldn't be reached
    Backend(String),
    /// The run was cancelled by SIGTERM, a lost writer lease, or another worker's early stop
    Cancelled,
    /// A backend rejected a write for good, which the runner records as a `write_failed` issue
    Rejected(String),
    /// The migration, not the data, is wrong: more than 100 distinct issue groups in one worker,
    /// or an error no retry can change; the pass is `Failed` at once
    Systematic(String),
    /// The writer lease was broken or lost, so this worker's token is stale
    LeaseLost,
}

impl fmt::Display for MigrateError {
    /// Write the error for the person who ran the command
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // each variant says what stopped the run
        match self {
            MigrateError::NotImplemented(what) => write!(f, "not built yet: {what}"),
            MigrateError::Backend(what) => write!(f, "a backend failed: {what}"),
            MigrateError::Cancelled => f.write_str("the run was cancelled"),
            MigrateError::Rejected(what) => write!(f, "a backend rejected a write: {what}"),
            MigrateError::Systematic(what) => {
                write!(f, "the migration failed systematically: {what}")
            }
            MigrateError::LeaseLost => f.write_str("the writer lease was lost"),
        }
    }
}

impl std::error::Error for MigrateError {}
