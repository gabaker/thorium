//! The runner API every data migration implements, and what the runner gives it

use async_trait::async_trait;
use bb8_redis::RedisConnectionManager;
use bb8_redis::bb8::Pool;
use scylla::client::session::Session;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use super::{CheckWait, IssueReport, PassCounts};
use crate::models::upgrades::Record;

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
pub struct RunContext {
    /// `Run` or `Check`
    pub mode: RunMode,
    /// The Scylla, Redis, and S3 clients the migration reads and writes through
    pub backends: RunBackends,
    /// Set by SIGTERM, a lost writer lease, or another worker's early stop; the workers check it
    /// between batches
    pub cancel: CancellationToken,
}

/// The clients a migration reads and writes through, connected by thoradm
#[derive(Clone)]
pub struct RunBackends {
    /// The Scylla session
    pub scylla: Arc<Session>,
    /// The Redis connection pool
    pub redis: Pool<RedisConnectionManager>,
    /// The S3 client
    pub s3: aws_sdk_s3::Client,
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
}
