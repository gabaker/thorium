//! The migration runner: workers over slices, checkpoints, attempts, grouped issues, the group
//! limit, re-runs of one group or some objects, checks, and verification
//!
//! A pass is split into a fixed number of slices when it first starts, one worker per slice. Each
//! worker reads batches, classifies every record, writes the ones that convert with an adaptive
//! number of writes in flight, and writes its checkpoint and the issue groups that changed in one
//! single-partition batch. Everything that isn't a data problem (timeouts, unavailable replicas,
//! overload) is retried and never becomes an issue; a run that can't go on stops with its last
//! checkpoint and resumes from it.

use chrono::{DateTime, Utc};
use futures::future::join_all;
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use uuid::Uuid;

use super::lease::WriterLease;
use super::pass_store::{GroupWrite, IssueRow, PassRow, PassStore, WorkerRow};
use super::{
    AppliedReport, CheckReport, IssueReport, MigrateError, MigrationRunStatus, MigrationRunner,
    ObjectClass, Outcome, PassCheckpoint, PassCounts, ReasonSummary, ReportCounts, RunContext,
    RunMode, SkippedItem, Slice,
};
use crate::models::upgrades::Record;

/// The most sample keys an issue group keeps
pub const SAMPLE_KEYS: usize = 20;

/// The most distinct issue groups one worker may record before the run fails as systematic
pub const MAX_GROUPS_PER_WORKER: usize = 100;

/// The most issue groups a check report lists
pub const CHECK_REASONS: usize = 10;

/// The rows a second a table without views takes in a rewrite pass, for estimates
const ROWS_PER_SECOND: u64 = 100_000;

/// The rows a second a table with a view takes in a rewrite pass, for estimates
const VIEW_ROWS_PER_SECOND: u64 = 30_000;

/// The longest pause between two retries
const MAX_BACKOFF: Duration = Duration::from_secs(10);

/// What a pass is, apart from how it runs
#[derive(Debug, Clone)]
pub struct PassSpec {
    /// The pass key: the family a declared transform rewrites, or the migration id of a
    /// hand-written migration; the pass row's id and the issue partition
    pub key: String,
    /// The migration's id, for logs
    pub migration: String,
    /// The step id of the transform, or the migration id, which names the issue rows
    pub transform: String,
    /// The revision the pass moves objects to
    pub revision: Option<String>,
    /// The step ids of the transforms the pass runs, in order
    pub chain: Vec<String>,
    /// The most objects one issue group may hold before the run fails
    pub group_max: u64,
    /// The reason codes an approval accepted, which don't count toward the limit
    pub accept: BTreeSet<String>,
    /// The Job running the pass, if any
    pub job: Option<String>,
    /// The approval's backup reference, if any
    pub backup: Option<String>,
    /// The families the migration covers, for a check report
    pub families: Vec<String>,
    /// Whether a table the migration writes has views, which lowers its write rate
    pub has_views: bool,
}

/// How a pass runs
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// How many slices to split the pass into when it first starts; a pass recorded with another
    /// count starts a new attempt
    pub slices: u32,
    /// Process only these object keys, crediting the groups whose samples hold them; the pass
    /// isn't ended
    pub objects: Vec<String>,
    /// Re-run one issue group: scan the family again, write what now converts, and recount every
    /// group seen; the pass ends
    pub reason: Option<String>,
}

impl Default for RunOptions {
    /// A whole run in the design's 16 slices
    fn default() -> Self {
        // no object or reason filter
        RunOptions {
            slices: 16,
            objects: Vec::new(),
            reason: None,
        }
    }
}

/// One issue group summed across the current attempt's workers
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupTotal {
    /// The step id of the failing transform
    pub transform: String,
    /// The reason code
    pub reason: String,
    /// `unrecognized` or `error`
    pub class: ObjectClass,
    /// How many objects the group holds, summed across workers
    pub count: u64,
    /// How many objects left the group after later runs
    pub resolved: u64,
    /// Whether the group's code was accepted
    pub accepted: bool,
    /// The sample keys merged from every worker, sorted
    pub samples: BTreeSet<String>,
}

/// How a pass run ended
#[derive(Debug, Clone)]
pub struct PassOutcome {
    /// Where the pass is
    pub status: MigrationRunStatus,
    /// Whether SIGTERM stopped the run before the pass ended; it resumes from its checkpoints
    pub interrupted: bool,
    /// Why the pass failed systematically, when it did
    pub failure: Option<String>,
    /// What the pass did, summed across workers
    pub counts: PassCounts,
    /// The issue groups, summed across workers
    pub groups: Vec<GroupTotal>,
    /// The attempt the run worked in
    pub attempt: String,
    /// Whether the run resumed an earlier one of the same attempt
    pub resumed: bool,
    /// What the pass applied
    pub report: AppliedReport,
    /// The largest checkpoint batch any worker wrote, in bytes
    pub largest_batch: usize,
}

/// What a check found
#[derive(Debug, Clone)]
pub struct CheckRun {
    /// The report a Job writes to its result row
    pub report: CheckReport,
    /// Every issue group with its sample keys, which only a hand-run check prints
    pub groups: Vec<GroupTotal>,
    /// What the migration would apply, with its own notes and warnings, which only a hand-run
    /// check prints in full
    pub applied: AppliedReport,
}

/// What `migrate verify` found
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyRun {
    /// How many records were loaded
    pub checked: u64,
    /// The reason codes records failed to load with, and how many each
    pub failures: BTreeMap<String, u64>,
}

/// Why a run stopped before its workers finished their slices
#[derive(Debug, Clone)]
enum Stop {
    /// A worker's group went over the limit
    GroupLimit,
    /// The migration, not the data, is wrong
    Systematic(String),
    /// The writer lease was lost
    LeaseLost,
    /// A backend kept failing past the retries
    Backend,
}

/// What every worker of a run shares
struct Shared<'a> {
    /// The migration
    runner: &'a dyn MigrationRunner,
    /// The pass
    spec: &'a PassSpec,
    /// The run's context
    ctx: &'a RunContext,
    /// The store, absent in a check
    store: Option<&'a PassStore>,
    /// The writer lease, absent in a check
    lease: Option<&'a WriterLease>,
    /// The attempt the run works in
    attempt: String,
    /// Cancelled by SIGTERM or by a worker that stops the run
    cancel: CancellationToken,
    /// Why the run stopped, the first reason winning
    stop: Mutex<Option<Stop>>,
    /// The slice count
    slices: u32,
    /// The checkpoint's largest batch in bytes
    largest: Mutex<usize>,
    /// Only these objects are processed, when set
    objects: Option<BTreeSet<String>>,
    /// Whether the workers re-scan the whole slice and recount their groups
    rescan: bool,
}

impl Shared<'_> {
    /// Stop the run for a reason, unless an earlier reason already did
    ///
    /// # Arguments
    ///
    /// * `reason` - Why the run stops
    fn halt(&self, reason: Stop) {
        // the first reason is the one reported
        if let Ok(mut stop) = self.stop.lock()
            && stop.is_none()
        {
            *stop = Some(reason);
        }
        // every worker stops at its next batch
        self.cancel.cancel();
    }

    /// Why the run stopped, if it did
    fn stopped(&self) -> Option<Stop> {
        // read the recorded reason
        self.stop.lock().ok().and_then(|stop| stop.clone())
    }
}

/// What one worker ends with
struct WorkerEnd {
    /// The worker's checkpoint as last written
    checkpoint: PassCheckpoint,
    /// The worker's issue groups
    tallies: BTreeMap<(String, String), Tally>,
    /// Whether the worker reached the end of its slice
    done: bool,
}

/// One worker's tally of one issue group
#[derive(Debug, Clone)]
struct Tally {
    /// `unrecognized` or `error`
    class: ObjectClass,
    /// How many objects the worker counts
    count: u64,
    /// The sample keys
    samples: BTreeSet<String>,
    /// When the worker first recorded the group
    first_seen: DateTime<Utc>,
    /// When the worker last recorded the group
    last_seen: DateTime<Utc>,
}

/// The key of a record, as issues and `--objects` render it
///
/// # Arguments
///
/// * `record` - The record
#[must_use]
pub fn record_key(record: &Record) -> &str {
    // every kind of record carries its key
    match record {
        Record::Scylla { key, .. } | Record::Redis { key, .. } | Record::S3 { key } => key,
    }
}

/// Name the worker of a slice
///
/// # Arguments
///
/// * `index` - The slice's index
#[must_use]
pub fn worker_name(index: u32) -> String {
    // zero padded, so workers sort in slice order
    format!("w{index:02}")
}

/// Run an operation that may fail for reasons that pass, backing off between tries
///
/// # Arguments
///
/// * `sh` - What the run shares
/// * `what` - What the operation does, for logs
/// * `op` - The operation
async fn retrying<T, F, Fut>(sh: &Shared<'_>, what: &str, mut op: F) -> Result<T, MigrateError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, MigrateError>>,
{
    // try, pausing longer after each timeout until the retries run out
    let limits = &sh.ctx.limits;
    let mut pause = limits.backoff;
    let mut tries = 0;
    loop {
        match op().await {
            Err(MigrateError::Backend(message)) => {
                tries += 1;
                if tries > limits.retries {
                    return Err(MigrateError::Backend(message));
                }
                warn!("{what} failed, retrying in {pause:?}: {message}");
                tokio::select! {
                    () = tokio::time::sleep(pause) => {}
                    () = sh.ctx.cancel.cancelled() => return Err(MigrateError::Cancelled),
                }
                pause = (pause * 2).min(MAX_BACKOFF);
            }
            other => return other,
        }
    }
}

/// Add one object to an issue group, logging it without the object's contents
///
/// # Arguments
///
/// * `sh` - What the run shares
/// * `tallies` - The worker's groups
/// * `changed` - The groups this batch changed
/// * `counts` - The worker's counts
/// * `issue` - The object's issue
fn flag(
    sh: &Shared<'_>,
    tallies: &mut BTreeMap<(String, String), Tally>,
    changed: &mut BTreeSet<(String, String)>,
    counts: &mut PassCounts,
    issue: &IssueReport,
) {
    // count the object by its class
    counts.flagged += 1;
    match issue.class {
        ObjectClass::Error => counts.error += 1,
        _ => counts.unrecognized += 1,
    }
    // log the issue with what identifies it, never the object's contents
    error!(
        migration = %sh.spec.migration,
        revision = sh.spec.revision.as_deref().unwrap_or(""),
        key = %issue.key,
        class = %issue.class.as_str(),
        reason = %issue.reason,
        "an object can't be migrated and is left untouched"
    );
    // add the object to its group, keeping the first sample keys
    let now = Utc::now();
    let group = (issue.transform.clone(), issue.reason.to_string());
    let tally = tallies.entry(group.clone()).or_insert_with(|| Tally {
        class: issue.class,
        count: 0,
        samples: BTreeSet::new(),
        first_seen: now,
        last_seen: now,
    });
    tally.count += 1;
    tally.last_seen = now;
    if tally.samples.len() < SAMPLE_KEYS {
        tally.samples.insert(issue.key.clone());
    }
    changed.insert(group);
}

/// Write one chunk of converted records with the writes in flight the worker has now
///
/// Returns each record's result in order. A timeout lowers the window and retries the records
/// that failed; a rejection is returned for the caller to record.
///
/// # Arguments
///
/// * `sh` - What the run shares
/// * `window` - The writes in flight, adjusted by what happens
/// * `records` - The converted records
async fn write_all(
    sh: &Shared<'_>,
    window: &mut usize,
    records: &[Record],
) -> Result<Vec<Result<(), MigrateError>>, MigrateError> {
    // results by position, filled in as writes succeed or are rejected
    let mut results: Vec<Option<Result<(), MigrateError>>> =
        (0..records.len()).map(|_| None).collect();
    let limits = &sh.ctx.limits;
    let mut pause = limits.backoff;
    let mut tries = 0;
    loop {
        // the positions still to write
        let pending = results
            .iter()
            .enumerate()
            .filter_map(|(at, result)| result.is_none().then_some(at))
            .collect::<Vec<_>>();
        if pending.is_empty() {
            break;
        }
        let mut timed_out = false;
        for chunk in pending.chunks((*window).max(1)) {
            // write the chunk's records at the same time
            let writes = chunk
                .iter()
                .map(|at| sh.runner.write(&records[*at], sh.ctx));
            for (at, outcome) in chunk.iter().zip(join_all(writes).await) {
                match outcome {
                    Ok(()) => results[*at] = Some(Ok(())),
                    Err(MigrateError::Backend(message)) => {
                        timed_out = true;
                        warn!("a write failed and will be retried: {message}");
                    }
                    Err(other) => results[*at] = Some(Err(other)),
                }
            }
        }
        if timed_out {
            // fewer writes in flight, a pause, and the same records again
            *window = (*window / 2).max(1);
            tries += 1;
            if tries > limits.retries {
                return Err(MigrateError::Backend(
                    "writes kept failing after the retries".to_owned(),
                ));
            }
            tokio::select! {
                () = tokio::time::sleep(pause) => {}
                () = sh.ctx.cancel.cancelled() => return Err(MigrateError::Cancelled),
            }
            pause = (pause * 2).min(MAX_BACKOFF);
        } else if *window < limits.writes_max {
            // writes that all succeed let the window grow slowly
            *window += 1;
        }
    }
    Ok(results
        .into_iter()
        .map(|result| result.unwrap_or(Ok(())))
        .collect())
}

/// What one batch found
#[derive(Default)]
struct Found {
    /// The batch's counts by class
    counts: PassCounts,
    /// The batch's counts for the applied report's group
    report: ReportCounts,
}

/// Classify every record of a batch and write the ones that convert
///
/// # Arguments
///
/// * `sh` - What the run shares
/// * `records` - The batch's records
/// * `tallies` - The worker's groups
/// * `changed` - The groups this batch changed
/// * `window` - The writes in flight
async fn classify_batch(
    sh: &Shared<'_>,
    records: &[Record],
    tallies: &mut BTreeMap<(String, String), Tally>,
    changed: &mut BTreeSet<(String, String)>,
    window: &mut usize,
) -> Result<Found, MigrateError> {
    let mut found = Found::default();
    let mut pending: Vec<Record> = Vec::new();
    // classify every record
    for record in records {
        found.counts.read += 1;
        let key = record_key(record);
        // objects outside `--objects` are skipped
        if let Some(objects) = &sh.objects
            && !objects.contains(key)
        {
            found.counts.skipped += 1;
            continue;
        }
        let outcome = retrying(sh, "classify a record", || {
            sh.runner.process(record, sh.ctx.mode, sh.ctx)
        })
        .await?;
        match outcome {
            Outcome::Current => {
                found.counts.current += 1;
                found.counts.skipped += 1;
                found.report.already_migrated += 1;
            }
            Outcome::Write(converted) => {
                found.counts.migratable += 1;
                if sh.ctx.mode == RunMode::Run {
                    pending.push(converted);
                } else {
                    found.report.changed += 1;
                }
            }
            Outcome::Issue(issue) => {
                found.report.skipped += 1;
                flag(sh, tallies, changed, &mut found.counts, &issue);
            }
        }
    }
    // write the converted records, recording the ones a backend rejects
    if pending.is_empty() {
        return Ok(found);
    }
    let results = write_all(sh, window, &pending).await?;
    for (record, result) in pending.iter().zip(results) {
        match result {
            Ok(()) => {
                found.counts.written += 1;
                found.report.changed += 1;
            }
            Err(MigrateError::Rejected(_)) => {
                found.report.skipped += 1;
                let issue = IssueReport {
                    key: record_key(record).to_owned(),
                    transform: sh.spec.transform.clone(),
                    class: ObjectClass::Error,
                    reason: crate::models::upgrades::ReasonBuilder::write_failed(true),
                };
                flag(sh, tallies, changed, &mut found.counts, &issue);
            }
            Err(other) => return Err(other),
        }
    }
    Ok(found)
}

/// Add a batch's counts to a worker's checkpoint
///
/// # Arguments
///
/// * `checkpoint` - The worker's checkpoint
/// * `found` - What the batch found
/// * `group` - The applied report's group the counts belong to
fn add_found(checkpoint: &mut PassCheckpoint, found: &Found, group: &str) {
    // every counter adds up
    let counts = &mut checkpoint.counts;
    counts.read += found.counts.read;
    counts.current += found.counts.current;
    counts.migratable += found.counts.migratable;
    counts.unrecognized += found.counts.unrecognized;
    counts.error += found.counts.error;
    counts.written += found.counts.written;
    counts.skipped += found.counts.skipped;
    counts.flagged += found.counts.flagged;
    let report = checkpoint
        .report_counts
        .entry(group.to_owned())
        .or_default();
    report.changed += found.report.changed;
    report.already_migrated += found.report.already_migrated;
    report.skipped += found.report.skipped;
}

/// Write a worker's checkpoint and the groups that changed in one single-partition batch
///
/// # Arguments
///
/// * `sh` - What the run shares
/// * `worker` - The worker
/// * `checkpoint` - The worker's checkpoint
/// * `tallies` - The worker's groups
/// * `changed` - The groups to write
/// * `old_rows` - The worker's issue rows from before this run
/// * `recount` - Whether this is a re-scan's final write, which sets the resolved counts
async fn store_checkpoint(
    sh: &Shared<'_>,
    worker: &str,
    checkpoint: &PassCheckpoint,
    tallies: &BTreeMap<(String, String), Tally>,
    changed: &BTreeSet<(String, String)>,
    old_rows: &[IssueRow],
    recount: bool,
) -> Result<(), MigrateError> {
    let (Some(store), Some(lease)) = (sh.store, sh.lease) else {
        return Ok(());
    };
    // a takeover during the batch makes this worker's token stale, so nothing is written
    ensure_lease(sh).await?;
    let empty = BTreeSet::new();
    let mut writes: Vec<GroupWrite<'_>> = Vec::new();
    for group in changed {
        let tally = tallies.get(group);
        let old = old_rows.iter().find(|row| {
            (row.transform.as_str(), row.reason.as_str()) == (group.0.as_str(), group.1.as_str())
        });
        let (class, count, samples, first_seen, last_seen) = match (tally, old) {
            (Some(t), _) => (t.class, t.count, &t.samples, t.first_seen, t.last_seen),
            (None, Some(row)) => (
                row.class,
                0,
                &empty,
                row.first_seen.unwrap_or_else(Utc::now),
                row.last_seen.unwrap_or_else(Utc::now),
            ),
            (None, None) => continue,
        };
        // a re-scan sets the resolved count to what left the group; a batch leaves it
        let resolved_count = recount.then(|| {
            let before = old.map_or(0, |row| row.count);
            let resolved = old.map_or(0, |row| row.resolved_count);
            resolved + before.saturating_sub(count)
        });
        writes.push(GroupWrite {
            transform: &group.0,
            reason: &group.1,
            class,
            count,
            samples,
            first_seen,
            last_seen,
            resolved_count,
        });
    }
    let size = retrying(sh, "write a checkpoint", || {
        store.write_batch(
            &sh.spec.key,
            &sh.attempt,
            worker,
            lease.token(),
            checkpoint,
            &writes,
        )
    })
    .await?;
    if let Ok(mut largest) = sh.largest.lock() {
        *largest = (*largest).max(size);
    }
    Ok(())
}

/// Rebuild a worker's tallies from the issue rows of its earlier run
///
/// # Arguments
///
/// * `old_rows` - The worker's issue rows from the current attempt
fn tallies_of(old_rows: &[IssueRow]) -> BTreeMap<(String, String), Tally> {
    // each row holds one group as the worker last counted it
    let now = Utc::now();
    old_rows
        .iter()
        .map(|row| {
            (
                (row.transform.clone(), row.reason.clone()),
                Tally {
                    class: row.class,
                    count: row.count,
                    samples: row.samples.clone(),
                    first_seen: row.first_seen.unwrap_or(now),
                    last_seen: row.last_seen.unwrap_or(now),
                },
            )
        })
        .collect()
}

/// Whether any of a worker's groups holds more objects than the step allows and wasn't accepted
///
/// # Arguments
///
/// * `spec` - The pass
/// * `tallies` - The worker's groups
fn over_limit(spec: &PassSpec, tallies: &BTreeMap<(String, String), Tally>) -> bool {
    // a group's count counts toward the limit unless its code was accepted
    tallies
        .iter()
        .any(|((_, reason), tally)| tally.count > spec.group_max && !spec.accept.contains(reason))
}

/// Stop the worker if the writer lease was broken or lost
///
/// # Arguments
///
/// * `sh` - What the run shares
async fn ensure_lease(sh: &Shared<'_>) -> Result<(), MigrateError> {
    // a check holds no lease
    let Some(lease) = sh.lease else {
        return Ok(());
    };
    // a worker whose token is stale must not write anything more
    let current = lease
        .is_current()
        .await
        .map_err(|err| MigrateError::Backend(format!("check the writer lease: {err}")))?;
    if !current || lease.lost().is_cancelled() {
        sh.halt(Stop::LeaseLost);
        return Err(MigrateError::LeaseLost);
    }
    Ok(())
}

/// The checkpoint a worker starts from: where its earlier run left off, or the slice's start
///
/// # Arguments
///
/// * `sh` - What the run shares
/// * `resume` - The worker's row from the current attempt, if there is one
fn starting_checkpoint(sh: &Shared<'_>, resume: Option<WorkerRow>) -> PassCheckpoint {
    // a re-scan starts over at the slice's start; a resume starts where the checkpoint is
    let mut checkpoint = match resume {
        Some(row) if !sh.rescan => row.checkpoint,
        _ => PassCheckpoint::default(),
    };
    checkpoint.chain.clone_from(&sh.spec.chain);
    checkpoint.slices = sh.slices;
    if sh.rescan {
        checkpoint.position = serde_json::Value::Null;
        checkpoint.done = false;
    }
    checkpoint
}

/// Work one slice to its end, or until the run stops
///
/// # Arguments
///
/// * `sh` - What the run shares
/// * `slice` - The slice to work
/// * `resume` - The worker's row from the current attempt, if there is one
/// * `old_rows` - The worker's issue rows from the current attempt
async fn work(
    sh: &Shared<'_>,
    slice: Slice,
    resume: Option<WorkerRow>,
    old_rows: Vec<IssueRow>,
) -> Result<WorkerEnd, MigrateError> {
    let worker = worker_name(slice.index);
    let spec = sh.spec;
    let limits = &sh.ctx.limits;
    let mut checkpoint = starting_checkpoint(sh, resume);
    // a resumed worker rebuilds its tallies from its own issue rows
    let mut tallies = if sh.rescan {
        BTreeMap::new()
    } else {
        tallies_of(&old_rows)
    };
    let end = |checkpoint, tallies, done| {
        Ok(WorkerEnd {
            checkpoint,
            tallies,
            done,
        })
    };
    // a worker that already reached the end of its slice has nothing left to do
    if checkpoint.done && !sh.rescan {
        return end(checkpoint, tallies, true);
    }
    // the writes in flight start low on a table with views
    let mut window = if spec.has_views {
        (limits.writes_initial / 4).max(1)
    } else {
        limits.writes_initial.max(1)
    };
    let mut last_checkpoint: Option<Instant> = None;
    let mut dirty = false;
    loop {
        // SIGTERM or another worker's stop ends the run at a batch boundary, after the batches
        // finished since the last write are checkpointed
        if sh.cancel.is_cancelled() {
            if dirty {
                let none = BTreeSet::new();
                store_checkpoint(sh, &worker, &checkpoint, &tallies, &none, &old_rows, false)
                    .await?;
            }
            return end(checkpoint, tallies, false);
        }
        // a worker whose lease was broken or lost stops before it writes anything more
        ensure_lease(sh).await?;
        // read the next batch after the last position done, and classify and write it
        let batch = retrying(sh, "read a batch", || {
            sh.runner.next_batch(&slice, &checkpoint.position, sh.ctx)
        })
        .await?;
        let mut changed: BTreeSet<(String, String)> = BTreeSet::new();
        let found =
            classify_batch(sh, &batch.records, &mut tallies, &mut changed, &mut window).await?;
        add_found(&mut checkpoint, &found, &spec.key);
        checkpoint.position.clone_from(&batch.next);
        checkpoint.done = batch.done;
        // a worker that records too many distinct groups says the migration is wrong
        if sh.ctx.mode == RunMode::Run && tallies.len() > MAX_GROUPS_PER_WORKER {
            sh.halt(Stop::Systematic(format!(
                "worker {worker} recorded more than {MAX_GROUPS_PER_WORKER} distinct issue groups"
            )));
        }
        // write the checkpoint and the changed groups, a finished re-scan rewriting every group it
        // saw or had before with exact counts
        let recount = sh.rescan && checkpoint.done;
        if recount {
            changed.extend(
                old_rows
                    .iter()
                    .map(|row| (row.transform.clone(), row.reason.clone())),
            );
            changed.extend(tallies.keys().cloned());
        }
        let due = last_checkpoint.is_none_or(|at| at.elapsed() >= limits.checkpoint_every);
        let rows = !batch.records.is_empty() || checkpoint.done;
        let write_now = (rows && (due || checkpoint.done || !changed.is_empty())) || recount;
        dirty = !write_now && (dirty || rows);
        if write_now {
            store_checkpoint(
                sh,
                &worker,
                &checkpoint,
                &tallies,
                &changed,
                &old_rows,
                recount,
            )
            .await?;
            last_checkpoint = Some(Instant::now());
        }
        info!(
            migration = %spec.migration,
            worker = %worker,
            read = checkpoint.counts.read,
            written = checkpoint.counts.written,
            flagged = checkpoint.counts.flagged,
            done = checkpoint.done,
            "migration batch finished"
        );
        // a group over the limit that wasn't accepted stops this worker, and the others at their
        // next batch
        if sh.ctx.mode == RunMode::Run && !sh.rescan && over_limit(spec, &tallies) {
            sh.halt(Stop::GroupLimit);
            return end(checkpoint, tallies, false);
        }
        // the worker stops for a systematic failure too
        if let Some(Stop::Systematic(_)) = sh.stopped() {
            return end(checkpoint, tallies, false);
        }
        if checkpoint.done {
            return end(checkpoint, tallies, true);
        }
    }
}

/// Sum a pass's issue rows into groups
///
/// # Arguments
///
/// * `issues` - The issue rows of the current attempt
/// * `accept` - The accepted reason codes
fn totals(issues: &[IssueRow], accept: &BTreeSet<String>) -> Vec<GroupTotal> {
    // sum each group across workers
    let mut groups: BTreeMap<(String, String), GroupTotal> = BTreeMap::new();
    for row in issues {
        let total = groups
            .entry((row.transform.clone(), row.reason.clone()))
            .or_insert_with(|| GroupTotal {
                transform: row.transform.clone(),
                reason: row.reason.clone(),
                class: row.class,
                count: 0,
                resolved: 0,
                accepted: false,
                samples: BTreeSet::new(),
            });
        total.count += row.count;
        total.resolved += row.resolved_count;
        total.accepted |= row.accepted || accept.contains(&row.reason);
        total.samples.extend(row.samples.iter().cloned());
    }
    groups.into_values().collect()
}

/// Add up the counts of every worker
///
/// # Arguments
///
/// * `workers` - The worker rows of the current attempt
fn sum_counts(workers: &[WorkerRow]) -> PassCounts {
    // every counter sums across workers
    workers.iter().fold(PassCounts::default(), |mut sum, row| {
        let counts = &row.checkpoint.counts;
        sum.read += counts.read;
        sum.current += counts.current;
        sum.migratable += counts.migratable;
        sum.unrecognized += counts.unrecognized;
        sum.error += counts.error;
        sum.written += counts.written;
        sum.skipped += counts.skipped;
        sum.flagged += counts.flagged;
        sum
    })
}

/// Build the report a pass stores from its workers' counts and its issue groups
///
/// # Arguments
///
/// * `spec` - The pass
/// * `workers` - The worker rows of the current attempt
/// * `groups` - The issue groups
fn build_report(spec: &PassSpec, workers: &[WorkerRow], groups: &[GroupTotal]) -> AppliedReport {
    // sum each report group's counts across workers
    let mut report = AppliedReport::default();
    for row in workers {
        for (name, counts) in &row.checkpoint.report_counts {
            let sum = report.groups.entry(name.clone()).or_default();
            sum.changed += counts.changed;
            sum.already_migrated += counts.already_migrated;
            sum.unchanged += counts.unchanged;
            sum.skipped += counts.skipped;
        }
    }
    // the skipped items are the issue groups' samples, with their reason codes
    for group in groups {
        report.skipped_total += group.count;
        for key in &group.samples {
            let (name, item) = key
                .split_once('/')
                .map_or((spec.key.as_str(), key.as_str()), |(name, item)| {
                    (name, item)
                });
            report.skipped.push(SkippedItem {
                group: name.to_owned(),
                item: item.to_owned(),
                reason: group.reason.clone(),
            });
        }
    }
    report.bound();
    report
}

/// Whether a pass may resume from the rows an attempt left
///
/// # Arguments
///
/// * `row` - The pass row
/// * `workers` - The worker rows of its attempt
/// * `spec` - The pass
/// * `slices` - The slice count the run wants
fn can_resume(row: &PassRow, workers: &[WorkerRow], spec: &PassSpec, slices: u32) -> bool {
    // only a pass that is running or failed resumes, and only for its own chain and slicing
    matches!(
        row.status,
        MigrationRunStatus::Running | MigrationRunStatus::Failed
    ) && workers
        .iter()
        .all(|worker| worker.checkpoint.chain == spec.chain && worker.checkpoint.slices == slices)
}

/// Run a pass: resume the current attempt or start a new one, work every slice, and end the pass
///
/// The caller holds the writer lease. A pass that ended `Done` or `DoneWithIssues`, or was
/// recorded for another chain or slice count, starts a new attempt; a `Running` or `Failed` pass
/// of the same chain and slicing resumes from its checkpoints. With `opts.objects` only the named
/// objects are processed and the pass isn't ended; with `opts.reason` the family is scanned
/// again and the pass ends.
///
/// # Arguments
///
/// * `runner` - The migration
/// * `spec` - The pass
/// * `opts` - How to run it
/// * `ctx` - The run's context, in `Run` mode
/// * `store` - The pass rows
/// * `lease` - The held writer lease
pub async fn run_pass(
    runner: &dyn MigrationRunner,
    spec: &PassSpec,
    opts: &RunOptions,
    ctx: &RunContext,
    store: &PassStore,
    lease: &WriterLease,
) -> Result<PassOutcome, MigrateError> {
    // `--objects` fixes named objects within the current attempt
    if !opts.objects.is_empty() {
        return run_objects(runner, spec, opts, ctx, store, lease).await;
    }
    // decide whether to resume the recorded attempt or start a new one
    let slices_wanted = opts.slices.max(1);
    let rescan = opts.reason.is_some();
    let (attempt, resumed, workers, issues) =
        attempt_for(store, spec, lease, slices_wanted, rescan).await?;
    // a re-scan keeps the slicing the pass started with
    let slice_count = if rescan {
        workers
            .first()
            .map_or(slices_wanted, |worker| worker.checkpoint.slices.max(1))
    } else {
        slices_wanted
    };
    let shared = Shared {
        runner,
        spec,
        ctx,
        store: Some(store),
        lease: Some(lease),
        attempt: attempt.clone(),
        cancel: ctx.cancel.child_token(),
        stop: Mutex::new(None),
        slices: slice_count,
        largest: Mutex::new(0),
        objects: None,
        rescan,
    };
    // work every slice at the same time
    let all_done = run_workers(&shared, slice_count, &workers, &issues).await?;
    let largest_batch = shared.largest.lock().map_or(0, |largest| *largest);
    // SIGTERM leaves the pass running, resumable from the checkpoints just written
    let (workers, issues) = store.rows(&spec.key, &attempt).await?;
    let groups = totals(&issues, &spec.accept);
    let counts = sum_counts(&workers);
    if ctx.cancel.is_cancelled() {
        return Ok(PassOutcome {
            status: MigrationRunStatus::Running,
            interrupted: true,
            failure: None,
            counts,
            groups,
            attempt,
            resumed,
            report: AppliedReport::default(),
            largest_batch,
        });
    }
    // end the pass: mark accepted groups, decide the status, advance the layout, and record it
    finish_pass(
        runner,
        spec,
        ctx,
        store,
        lease,
        &shared,
        attempt,
        resumed,
        workers,
        issues,
        all_done,
        largest_batch,
    )
    .await
}

/// Decide whether a run resumes the recorded attempt or starts a new one
///
/// # Arguments
///
/// * `store` - The pass rows
/// * `spec` - The pass
/// * `lease` - The held writer lease
/// * `slices` - The slice count the run wants
/// * `rescan` - Whether the run re-scans one group, which needs an attempt to work in
async fn attempt_for(
    store: &PassStore,
    spec: &PassSpec,
    lease: &WriterLease,
    slices: u32,
    rescan: bool,
) -> Result<(String, bool, Vec<WorkerRow>, Vec<IssueRow>), MigrateError> {
    // a pass that was never started has nothing to resume or to re-scan
    let Some(row) = store.pass_row(&spec.key).await? else {
        if rescan {
            return Err(MigrateError::Systematic(
                "there is no pass to re-run a group of".to_owned(),
            ));
        }
        return begin(store, spec, lease).await;
    };
    // a re-scan or a resumable pass continues its attempt; anything else starts a new one
    let (workers, issues) = store.rows(&spec.key, &row.attempt).await?;
    if !rescan && !can_resume(&row, &workers, spec, slices) {
        return begin(store, spec, lease).await;
    }
    store
        .resume(
            &spec.key,
            lease.holder(),
            spec.job.as_deref(),
            spec.backup.as_deref(),
        )
        .await?;
    Ok((row.attempt, true, workers, issues))
}

/// Work every slice at the same time, and say whether every worker reached the end of its slice
///
/// # Arguments
///
/// * `shared` - What the run shares
/// * `slice_count` - How many slices to split the family into
/// * `workers` - The worker rows of the current attempt
/// * `issues` - The issue rows of the current attempt
async fn run_workers(
    shared: &Shared<'_>,
    slice_count: u32,
    workers: &[WorkerRow],
    issues: &[IssueRow],
) -> Result<bool, MigrateError> {
    let slices = retrying(shared, "split the family into slices", || {
        shared.runner.slices(slice_count, shared.ctx)
    })
    .await?;
    let ends = join_all(slices.into_iter().map(|slice| {
        let name = worker_name(slice.index);
        let resume = workers.iter().find(|row| row.worker == name).cloned();
        let mine = issues
            .iter()
            .filter(|row| row.worker == name)
            .cloned()
            .collect::<Vec<_>>();
        async move {
            let end = work(shared, slice, resume, mine).await;
            // a worker that fails stops the others
            if let Err(err) = &end {
                match err {
                    MigrateError::Cancelled | MigrateError::LeaseLost => {}
                    MigrateError::Systematic(message) => {
                        shared.halt(Stop::Systematic(message.clone()));
                    }
                    _ => shared.halt(Stop::Backend),
                }
            }
            end
        }
    }))
    .await;
    // a worker's failure ends the run without ending the pass: it resumes from its checkpoint
    let mut all_done = true;
    for end in ends {
        match end {
            Ok(end) => all_done &= end.done,
            Err(MigrateError::Cancelled | MigrateError::Systematic(_)) => all_done = false,
            Err(other) => return Err(other),
        }
    }
    Ok(all_done)
}

/// Start a new attempt and return its empty state
///
/// # Arguments
///
/// * `store` - The pass rows
/// * `spec` - The pass
/// * `lease` - The held writer lease
async fn begin(
    store: &PassStore,
    spec: &PassSpec,
    lease: &WriterLease,
) -> Result<(String, bool, Vec<WorkerRow>, Vec<IssueRow>), MigrateError> {
    // a fresh attempt: the pass row first, then the old attempt's rows are deleted
    let attempt = Uuid::new_v4().to_string();
    store
        .begin_attempt(
            &spec.key,
            &attempt,
            lease.holder(),
            spec.job.as_deref(),
            spec.backup.as_deref(),
            spec.revision.as_deref(),
        )
        .await?;
    Ok((attempt, false, Vec::new(), Vec::new()))
}

/// End a pass whose workers returned: mark accepted groups, decide how it ended, run the
/// migration's finish step, and record the result
///
/// # Arguments
///
/// * `runner` - The migration
/// * `spec` - The pass
/// * `ctx` - The run's context
/// * `store` - The pass rows
/// * `lease` - The held writer lease
/// * `shared` - What the run shared, which says why it stopped
/// * `attempt` - The attempt
/// * `resumed` - Whether the run resumed
/// * `workers` - The worker rows of the attempt
/// * `issues` - The issue rows of the attempt
/// * `all_done` - Whether every worker reached the end of its slice
/// * `largest_batch` - The largest checkpoint batch in bytes
#[allow(clippy::too_many_arguments)]
async fn finish_pass(
    runner: &dyn MigrationRunner,
    spec: &PassSpec,
    ctx: &RunContext,
    store: &PassStore,
    lease: &WriterLease,
    shared: &Shared<'_>,
    attempt: String,
    resumed: bool,
    workers: Vec<WorkerRow>,
    issues: Vec<IssueRow>,
    all_done: bool,
    largest_batch: usize,
) -> Result<PassOutcome, MigrateError> {
    // the run marks the groups the approval accepted
    for row in &issues {
        let accepted = spec.accept.contains(&row.reason);
        if accepted != row.accepted {
            // marking a group is idempotent, so a dropped connection is retried
            retrying(shared, "mark an issue group accepted", || {
                store.accept(&spec.key, row, accepted)
            })
            .await?;
        }
    }
    let groups = totals(&issues, &spec.accept);
    let counts = sum_counts(&workers);
    // a group over the limit that wasn't accepted, or a stop for one, fails the pass
    let over = groups
        .iter()
        .any(|group| group.count > spec.group_max && !group.accepted);
    let systematic = match shared.stopped() {
        Some(Stop::Systematic(message)) => Some(message),
        _ => None,
    };
    let failed = over || systematic.is_some() || !all_done;
    let status = if failed {
        MigrationRunStatus::Failed
    } else if groups.iter().any(|group| group.count > 0) {
        MigrationRunStatus::DoneWithIssues
    } else {
        MigrationRunStatus::Done
    };
    // the layout advances only when the pass doesn't fail
    let mut report = build_report(spec, &workers, &groups);
    if !failed {
        // the migration's own progress is tied to this attempt
        let mut own = ctx.clone();
        own.attempt = Some(attempt.clone());
        runner.finish(&own).await?;
        runner.finish_report(&mut report, &own).await?;
        report.bound();
    }
    // the update rewrites one row to the same values every time, so a retry after a dropped
    // connection is safe
    retrying(shared, "record how the pass ended", || {
        store.finish(&spec.key, status, lease.holder(), &counts, &report)
    })
    .await?;
    Ok(PassOutcome {
        status,
        interrupted: false,
        failure: systematic,
        counts,
        groups,
        attempt,
        resumed,
        report,
        largest_batch,
    })
}

/// Credit the groups whose samples hold a key that was fixed: their count drops by one, their
/// resolved count rises by one, and the key leaves their samples
///
/// # Arguments
///
/// * `store` - The pass rows
/// * `spec` - The pass
/// * `lease` - The held writer lease
/// * `attempt` - The pass's current attempt
/// * `workers` - The worker rows of the attempt, whose checkpoints ride along unchanged
/// * `issues` - The issue rows of the attempt, updated here
/// * `key` - The fixed object's key
async fn credit(
    store: &PassStore,
    spec: &PassSpec,
    lease: &WriterLease,
    attempt: &str,
    workers: &[WorkerRow],
    issues: &mut [IssueRow],
    key: &str,
) -> Result<(), MigrateError> {
    for issue in issues
        .iter_mut()
        .filter(|issue| issue.samples.contains(key))
    {
        issue.samples.remove(key);
        issue.count = issue.count.saturating_sub(1);
        issue.resolved_count += 1;
        let write = GroupWrite {
            transform: &issue.transform,
            reason: &issue.reason,
            class: issue.class,
            count: issue.count,
            samples: &issue.samples,
            first_seen: issue.first_seen.unwrap_or_else(Utc::now),
            last_seen: issue.last_seen.unwrap_or_else(Utc::now),
            resolved_count: Some(issue.resolved_count),
        };
        // the worker row rides along unchanged, so the batch stays on the pass's partition
        let checkpoint = workers
            .iter()
            .find(|worker| worker.worker == issue.worker)
            .map_or_else(PassCheckpoint::default, |worker| worker.checkpoint.clone());
        store
            .write_batch(
                &spec.key,
                attempt,
                &issue.worker,
                lease.token(),
                &checkpoint,
                &[write],
            )
            .await?;
    }
    Ok(())
}

/// Process only the named objects within the current attempt, crediting the groups that held them
///
/// # Arguments
///
/// * `runner` - The migration
/// * `spec` - The pass
/// * `opts` - How to run it
/// * `ctx` - The run's context
/// * `store` - The pass rows
/// * `lease` - The held writer lease
async fn run_objects(
    runner: &dyn MigrationRunner,
    spec: &PassSpec,
    opts: &RunOptions,
    ctx: &RunContext,
    store: &PassStore,
    lease: &WriterLease,
) -> Result<PassOutcome, MigrateError> {
    // the current attempt's rows say which groups held the objects
    let row = store
        .pass_row(&spec.key)
        .await?
        .ok_or_else(|| MigrateError::Systematic("there is no pass to fix objects of".to_owned()))?;
    let (workers, mut issues) = store.rows(&spec.key, &row.attempt).await?;
    let shared = Shared {
        runner,
        spec,
        ctx,
        store: Some(store),
        lease: Some(lease),
        attempt: row.attempt.clone(),
        cancel: ctx.cancel.child_token(),
        stop: Mutex::new(None),
        slices: 1,
        largest: Mutex::new(0),
        objects: Some(opts.objects.iter().cloned().collect()),
        rescan: false,
    };
    // look the objects up directly, or scan the family for them
    let found = match runner.fetch(&opts.objects, ctx).await? {
        Some(records) => records,
        None => scan_for(&shared, &opts.objects).await?,
    };
    let mut counts = PassCounts::default();
    for record in &found {
        counts.read += 1;
        let key = record_key(record).to_owned();
        // a key that now converts, or already has the current layout, leaves its group
        let fixed = match retrying(&shared, "classify an object", || {
            runner.process(record, RunMode::Run, ctx)
        })
        .await?
        {
            Outcome::Current => {
                counts.current += 1;
                true
            }
            Outcome::Write(converted) => {
                counts.migratable += 1;
                let mut window = 1;
                match write_all(&shared, &mut window, &[converted])
                    .await?
                    .remove(0)
                {
                    Ok(()) => {
                        counts.written += 1;
                        true
                    }
                    Err(_) => false,
                }
            }
            Outcome::Issue(issue) => {
                // the object still fails, so it's logged and left in its group
                error!(
                    migration = %spec.migration,
                    key = %key,
                    class = %issue.class.as_str(),
                    reason = %issue.reason,
                    "an object still can't be migrated"
                );
                false
            }
        };
        if !fixed {
            continue;
        }
        // credit the group whose samples hold the key; a key in no samples credits none
        credit(
            store,
            spec,
            lease,
            &row.attempt,
            &workers,
            &mut issues,
            &key,
        )
        .await?;
    }
    let groups = totals(&issues, &spec.accept);
    let sum = sum_counts(&workers);
    Ok(PassOutcome {
        status: row.status,
        interrupted: false,
        failure: None,
        counts: if counts.read > 0 { counts } else { sum },
        groups,
        attempt: row.attempt,
        resumed: true,
        report: AppliedReport::default(),
        largest_batch: 0,
    })
}

/// Scan the whole family for the named objects
///
/// # Arguments
///
/// * `sh` - What the run shares
/// * `keys` - The keys to find
async fn scan_for(sh: &Shared<'_>, keys: &[String]) -> Result<Vec<Record>, MigrateError> {
    // read every slice to its end, keeping only the named records
    let wanted: BTreeSet<&str> = keys.iter().map(String::as_str).collect();
    let mut found = Vec::new();
    let slices = retrying(sh, "split the family into slices", || {
        sh.runner.slices(sh.slices.max(1), sh.ctx)
    })
    .await?;
    for slice in slices {
        let mut position = serde_json::Value::Null;
        loop {
            let batch = retrying(sh, "read a batch", || {
                sh.runner.next_batch(&slice, &position, sh.ctx)
            })
            .await?;
            found.extend(
                batch
                    .records
                    .into_iter()
                    .filter(|record| wanted.contains(record_key(record))),
            );
            position = batch.next;
            if batch.done {
                break;
            }
        }
    }
    Ok(found)
}

/// Check what a pass would touch, writing nothing
///
/// Every record is classified, with no early stop and no checkpoint; the report lists the ten
/// largest issue groups by count with ties broken by code, and carries no sample keys.
///
/// # Arguments
///
/// * `runner` - The migration
/// * `spec` - The pass
/// * `opts` - How to run it
/// * `ctx` - The run's context, in `Check` mode
pub async fn check_pass(
    runner: &dyn MigrationRunner,
    spec: &PassSpec,
    opts: &RunOptions,
    ctx: &RunContext,
) -> Result<CheckRun, MigrateError> {
    // a check classifies on its own, with nothing stored
    let slice_count = opts.slices.max(1);
    let shared = Shared {
        runner,
        spec,
        ctx,
        store: None,
        lease: None,
        attempt: String::new(),
        cancel: ctx.cancel.child_token(),
        stop: Mutex::new(None),
        slices: slice_count,
        largest: Mutex::new(0),
        objects: None,
        rescan: false,
    };
    let slices = retrying(&shared, "split the family into slices", || {
        runner.slices(slice_count, ctx)
    })
    .await?;
    let ends = join_all(
        slices
            .into_iter()
            .map(|slice| work(&shared, slice, None, Vec::new())),
    )
    .await;
    // sum the workers' counts and groups
    let mut workers = Vec::new();
    let mut issues = Vec::new();
    for (index, end) in ends.into_iter().enumerate() {
        let end = end?;
        let name = worker_name(u32::try_from(index).unwrap_or(u32::MAX));
        for ((transform, reason), tally) in &end.tallies {
            issues.push(IssueRow {
                transform: transform.clone(),
                reason: reason.clone(),
                worker: name.clone(),
                attempt: String::new(),
                class: tally.class,
                count: tally.count,
                first_seen: Some(tally.first_seen),
                last_seen: Some(tally.last_seen),
                samples: tally.samples.clone(),
                resolved_count: 0,
                accepted: false,
            });
        }
        workers.push(WorkerRow {
            worker: name,
            attempt: String::new(),
            checkpoint: end.checkpoint,
            lease: None,
        });
    }
    let counts = sum_counts(&workers);
    let mut groups = totals(&issues, &BTreeSet::new());
    // the largest groups first, ties broken by code
    groups.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.reason.cmp(&b.reason)));
    let reasons = groups
        .iter()
        .take(CHECK_REASONS)
        .map(|group| ReasonSummary {
            code: group.reason.clone(),
            class: group.class,
            count: group.count,
        })
        .collect::<Vec<_>>();
    let more = u32::try_from(groups.len().saturating_sub(CHECK_REASONS)).unwrap_or(u32::MAX);
    // the rows a run would rewrite, by family, and how long that takes
    let families = if spec.families.is_empty() {
        vec![spec.key.clone()]
    } else {
        spec.families.clone()
    };
    let rows = families
        .iter()
        .map(|family| (family.clone(), counts.migratable))
        .collect::<BTreeMap<_, _>>();
    let rate = if spec.has_views {
        VIEW_ROWS_PER_SECOND
    } else {
        ROWS_PER_SECOND
    };
    let estimated_seconds = Some(counts.migratable.div_ceil(rate));
    let wait = runner.check_wait(&counts, ctx).await?;
    let mut applied = build_report(spec, &workers, &groups);
    // the migration adds its notes and warnings, and the whole stays within the stored bounds
    runner.finish_report(&mut applied, ctx).await?;
    applied.bound();
    let summary =
        (!applied.groups.is_empty() || applied.warnings_total > 0).then(|| applied.summary());
    Ok(CheckRun {
        report: CheckReport {
            migration: spec.migration.clone(),
            counts,
            families,
            rows,
            estimated_seconds,
            reasons,
            more,
            wait,
            summary,
            loss: None,
        },
        groups,
        applied,
    })
}

/// Load every object of a family through the migration's own verification
///
/// # Arguments
///
/// * `runner` - The migration
/// * `opts` - How to run it
/// * `ctx` - The run's context, in `Check` mode
pub async fn verify_pass(
    runner: &dyn MigrationRunner,
    opts: &RunOptions,
    ctx: &RunContext,
) -> Result<VerifyRun, MigrateError> {
    // read every slice to its end, verifying each record
    let spec = PassSpec {
        key: String::new(),
        migration: String::new(),
        transform: String::new(),
        revision: None,
        chain: Vec::new(),
        group_max: 0,
        accept: BTreeSet::new(),
        job: None,
        backup: None,
        families: Vec::new(),
        has_views: false,
    };
    let shared = Shared {
        runner,
        spec: &spec,
        ctx,
        store: None,
        lease: None,
        attempt: String::new(),
        cancel: ctx.cancel.child_token(),
        stop: Mutex::new(None),
        slices: opts.slices.max(1),
        largest: Mutex::new(0),
        objects: None,
        rescan: false,
    };
    let slices = retrying(&shared, "split the family into slices", || {
        runner.slices(opts.slices.max(1), ctx)
    })
    .await?;
    let mut run = VerifyRun::default();
    for slice in slices {
        let mut position = serde_json::Value::Null;
        loop {
            let batch = retrying(&shared, "read a batch", || {
                runner.next_batch(&slice, &position, ctx)
            })
            .await?;
            for record in &batch.records {
                run.checked += 1;
                if let Err(reason) = runner.verify(record, ctx).await {
                    *run.failures.entry(reason.to_string()).or_default() += 1;
                }
            }
            position = batch.next;
            if batch.done {
                break;
            }
        }
    }
    Ok(run)
}

/// Read a pass's issue groups from the current attempt
///
/// # Arguments
///
/// * `store` - The pass rows
/// * `key` - The pass key
pub async fn issue_groups(store: &PassStore, key: &str) -> Result<Vec<GroupTotal>, MigrateError> {
    // the current attempt's rows, summed across workers
    let Some(row) = store.pass_row(key).await? else {
        return Ok(Vec::new());
    };
    let (_, issues) = store.rows(key, &row.attempt).await?;
    Ok(totals(&issues, &BTreeSet::new()))
}
