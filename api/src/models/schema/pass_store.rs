//! The rows of a migration pass: its pass row in `schema_migrations`, and each worker's checkpoint
//! and issue groups in `schema_migration_issues`
//!
//! A worker writes its checkpoint row and the issue rows of the groups that changed in a batch as
//! one single-partition batch, which Scylla applies atomically without a batchlog or a size check.
//! A batch updates only `count`, `samples`, `first_seen`, and `last_seen` of an issue row, never
//! `resolved_count` or `accepted`. Nothing here is a counter or a lightweight transaction.

use chrono::{DateTime, Utc};
use scylla::client::session::Session;
use scylla::serialize::SerializationError;
use scylla::serialize::row::{RowSerializationContext, SerializeRow};
use scylla::serialize::writers::RowWriter;
use scylla::statement::batch::{Batch, BatchType};
use scylla::statement::prepared::PreparedStatement;
use std::collections::BTreeSet;
use std::sync::Arc;

use super::{
    AppliedReport, CheckReport, JsonText, MigrateError, MigrationRunStatus, ObjectClass,
    PassCheckpoint, PassCounts,
};

/// How long a Job's result row lives, in seconds
const RESULT_TTL_SECS: i32 = 7 * 24 * 60 * 60;

/// The transform of a worker row, which sorts it apart from the issue groups
pub const WORKER_TRANSFORM: &str = "~worker";

/// The reason of a worker row
pub const WORKER_REASON: &str = "~";

/// A migration pass's row in `schema_migrations`
#[derive(Debug, Clone, PartialEq)]
pub struct PassRow {
    /// Where the pass is
    pub status: MigrationRunStatus,
    /// The pass's current attempt; rows of any other attempt are ignored
    pub attempt: String,
    /// When the pass first started
    pub started: Option<DateTime<Utc>>,
    /// When the pass finished
    pub finished: Option<DateTime<Utc>>,
    /// The writer lease holder that last wrote the pass
    pub lease: Option<String>,
    /// The Job that last wrote the pass
    pub job: Option<String>,
    /// The approval's backup reference
    pub backup: Option<String>,
    /// The revision the pass moves objects to
    pub revision: Option<String>,
    /// What the pass did, once it ended
    pub counts: Option<PassCounts>,
    /// What the pass applied, once it ended
    pub report: Option<AppliedReport>,
}

/// What one worker of a pass wrote last
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerRow {
    /// The worker (`w00`, `w01`, …)
    pub worker: String,
    /// The attempt the row belongs to
    pub attempt: String,
    /// The worker's checkpoint
    pub checkpoint: PassCheckpoint,
    /// The fencing token the worker held when it wrote the row
    pub lease: Option<String>,
}

/// One worker's tally of one issue group, as stored
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueRow {
    /// The step id of the failing transform
    pub transform: String,
    /// The reason code
    pub reason: String,
    /// The worker that counted the group
    pub worker: String,
    /// The attempt the row belongs to
    pub attempt: String,
    /// `unrecognized` or `error`
    pub class: ObjectClass,
    /// How many objects the worker counts in the group now
    pub count: u64,
    /// When the group was first recorded
    pub first_seen: Option<DateTime<Utc>>,
    /// When the group was last seen
    pub last_seen: Option<DateTime<Utc>>,
    /// Up to 20 of the group's object keys
    pub samples: BTreeSet<String>,
    /// How many objects left the group because a later run converted them
    pub resolved_count: u64,
    /// Whether an approval accepted the group's code
    pub accepted: bool,
}

/// The values of a worker row's checkpoint update: the attempt, the checkpoint, the fencing
/// token, the update time, the pass, and the worker
type WorkerBind<'a> = (
    &'a str,
    JsonText<&'a PassCheckpoint>,
    &'a str,
    DateTime<Utc>,
    &'a str,
    &'a str,
);

/// The values of an issue row's count update: the attempt, the class, the count, the samples, the
/// first and last seen, the update time, and the row's key
type IssueBind<'a> = (
    &'a str,
    &'a str,
    i64,
    &'a BTreeSet<String>,
    DateTime<Utc>,
    DateTime<Utc>,
    DateTime<Utc>,
    &'a str,
    &'a str,
    &'a str,
    &'a str,
);

/// The values of an issue row's recount: the attempt, the class, the count, the resolved count, the
/// samples, the last seen, the update time, and the row's key
type RecountBind<'a> = (
    &'a str,
    &'a str,
    i64,
    i64,
    &'a BTreeSet<String>,
    DateTime<Utc>,
    DateTime<Utc>,
    &'a str,
    &'a str,
    &'a str,
    &'a str,
);

/// The columns of a pass row as they are read
type PassColumns = (
    Option<MigrationRunStatus>,
    Option<String>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<JsonText<PassCounts>>,
    Option<JsonText<AppliedReport>>,
);

/// The columns of a worker or issue row as they are read
type RowColumns = (
    String,
    String,
    String,
    Option<String>,
    Option<JsonText<PassCheckpoint>>,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
    Option<BTreeSet<String>>,
    Option<i64>,
    Option<bool>,
);

/// The values of one statement in a batch, whichever statement it is
enum Bind<'a> {
    /// A worker row's checkpoint update
    Worker(WorkerBind<'a>),
    /// An issue row's count update
    Issue(IssueBind<'a>),
    /// An issue row's recount, which also sets its resolved count
    Recount(RecountBind<'a>),
}

impl SerializeRow for Bind<'_> {
    /// Serialize the values of whichever statement this is
    ///
    /// # Arguments
    ///
    /// * `ctx` - The statement's bind markers
    /// * `writer` - The writer for the row's values
    fn serialize(
        &self,
        ctx: &RowSerializationContext<'_>,
        writer: &mut RowWriter,
    ) -> Result<(), SerializationError> {
        // each statement's tuple knows its own order
        match self {
            Bind::Worker(values) => values.serialize(ctx, writer),
            Bind::Issue(values) => values.serialize(ctx, writer),
            Bind::Recount(values) => values.serialize(ctx, writer),
        }
    }

    /// Every statement binds values
    fn is_empty(&self) -> bool {
        // a batch statement here always has markers
        false
    }
}

/// One issue group's values to write
#[derive(Debug, Clone)]
pub struct GroupWrite<'a> {
    /// The step id of the failing transform
    pub transform: &'a str,
    /// The reason code
    pub reason: &'a str,
    /// `unrecognized` or `error`
    pub class: ObjectClass,
    /// How many objects the worker counts in the group
    pub count: u64,
    /// The group's sample keys
    pub samples: &'a BTreeSet<String>,
    /// When the worker first recorded the group
    pub first_seen: DateTime<Utc>,
    /// When the worker last recorded the group
    pub last_seen: DateTime<Utc>,
    /// The resolved count, for a recount that sets it; `None` for a batch that must leave it alone
    pub resolved_count: Option<u64>,
}

/// The prepared statements on a pass's rows, in one keyspace
pub struct PassStore {
    /// The Scylla session
    session: Arc<Session>,
    /// The keyspace
    ns: String,
    /// Read one pass row
    select_pass: PreparedStatement,
    /// Write a pass row for a new attempt
    insert_pass: PreparedStatement,
    /// Mark a resumed pass running again
    resume_pass: PreparedStatement,
    /// Record how a pass ended
    finish_pass: PreparedStatement,
    /// Read every worker and issue row of a pass
    select_rows: PreparedStatement,
    /// Update a worker row
    update_worker: PreparedStatement,
    /// Update an issue row's count and samples
    update_issue: PreparedStatement,
    /// Update an issue row's count, samples, and resolved count
    recount_issue: PreparedStatement,
    /// Mark an issue group accepted
    accept_issue: PreparedStatement,
    /// Delete every row of a pass
    delete_pass: PreparedStatement,
    /// Write a Job's result row with a TTL
    insert_result: PreparedStatement,
}

/// Turn a driver error into the runner's error, keeping what is worth retrying apart from what
/// the backend refused for good
///
/// # Arguments
///
/// * `context` - What was being done
/// * `err` - The driver's error, as text
/// * `rejected` - Whether the backend rejected the statement deterministically
pub fn backend_error(context: &str, err: &impl std::fmt::Display, rejected: bool) -> MigrateError {
    // a deterministic rejection won't succeed on a retry; everything else may
    let message = format!("{context}: {err}");
    if rejected {
        MigrateError::Rejected(message)
    } else {
        MigrateError::Backend(message)
    }
}

/// Whether the driver's error is one a retry can't get past
///
/// # Arguments
///
/// * `err` - The driver's execution error
#[must_use]
pub fn is_rejection(err: &scylla::errors::ExecutionError) -> bool {
    use scylla::errors::{DbError, ExecutionError, RequestAttemptError};
    // only the server's own refusal of the statement or the caller is final
    match err {
        ExecutionError::LastAttemptError(RequestAttemptError::DbError(db, _)) => matches!(
            db,
            DbError::Invalid
                | DbError::SyntaxError
                | DbError::Unauthorized
                | DbError::ConfigError
                | DbError::AlreadyExists { .. }
                | DbError::FunctionFailure { .. }
        ),
        ExecutionError::BadQuery(_) => true,
        _ => false,
    }
}

impl PassStore {
    /// Prepare the statements on a pass's rows
    ///
    /// # Arguments
    ///
    /// * `session` - The Scylla session
    /// * `ns` - The keyspace
    pub async fn prepare(session: Arc<Session>, ns: &str) -> Result<Self, MigrateError> {
        // prepare each statement against the keyspace, naming the one that fails
        let prepare = |cql: String| {
            let session = session.clone();
            async move {
                session.prepare(cql.clone()).await.map_err(|err| {
                    backend_error("prepare a migration state statement", &err, false)
                })
            }
        };
        let pass_columns =
            "status, attempt, started, finished, lease, job, backup, revision, counts, report";
        let select_pass = prepare(format!(
            "SELECT {pass_columns} FROM {ns}.schema_migrations WHERE kind = 'run' AND id = ?"
        ))
        .await?;
        let insert_pass = prepare(format!(
            "INSERT INTO {ns}.schema_migrations (kind, id, status, attempt, started, finished, \
             lease, job, backup, revision, counts, report, updated) \
             VALUES ('run', ?, ?, ?, ?, null, ?, ?, ?, ?, null, null, ?)"
        ))
        .await?;
        let resume_pass = prepare(format!(
            "UPDATE {ns}.schema_migrations SET status = ?, finished = null, lease = ?, job = ?, \
             backup = ?, updated = ? WHERE kind = 'run' AND id = ?"
        ))
        .await?;
        let finish_pass = prepare(format!(
            "UPDATE {ns}.schema_migrations SET status = ?, finished = ?, lease = ?, counts = ?, \
             report = ?, updated = ? WHERE kind = 'run' AND id = ?"
        ))
        .await?;
        let select_rows = prepare(format!(
            "SELECT transform, reason, worker, attempt, checkpoint, lease, class, count, \
             first_seen, last_seen, samples, resolved_count, accepted \
             FROM {ns}.schema_migration_issues WHERE migration = ?"
        ))
        .await?;
        let update_worker = prepare(format!(
            "UPDATE {ns}.schema_migration_issues SET attempt = ?, checkpoint = ?, lease = ?, \
             updated = ? WHERE migration = ? AND transform = '{WORKER_TRANSFORM}' \
             AND reason = '{WORKER_REASON}' AND worker = ?"
        ))
        .await?;
        let update_issue = prepare(format!(
            "UPDATE {ns}.schema_migration_issues SET attempt = ?, class = ?, count = ?, \
             samples = ?, first_seen = ?, last_seen = ?, updated = ? \
             WHERE migration = ? AND transform = ? AND reason = ? AND worker = ?"
        ))
        .await?;
        let recount_issue = prepare(format!(
            "UPDATE {ns}.schema_migration_issues SET attempt = ?, class = ?, count = ?, \
             resolved_count = ?, samples = ?, last_seen = ?, updated = ? \
             WHERE migration = ? AND transform = ? AND reason = ? AND worker = ?"
        ))
        .await?;
        let accept_issue = prepare(format!(
            "UPDATE {ns}.schema_migration_issues SET accepted = ?, updated = ? \
             WHERE migration = ? AND transform = ? AND reason = ? AND worker = ?"
        ))
        .await?;
        let delete_pass = prepare(format!(
            "DELETE FROM {ns}.schema_migration_issues WHERE migration = ?"
        ))
        .await?;
        let insert_result = prepare(format!(
            "INSERT INTO {ns}.schema_migrations (kind, id, result, updated) \
             VALUES ('job_result', ?, ?, ?) USING TTL {RESULT_TTL_SECS}"
        ))
        .await?;
        Ok(PassStore {
            session,
            ns: ns.to_owned(),
            select_pass,
            insert_pass,
            resume_pass,
            finish_pass,
            select_rows,
            update_worker,
            update_issue,
            recount_issue,
            accept_issue,
            delete_pass,
            insert_result,
        })
    }

    /// The keyspace the store writes in
    #[must_use]
    pub fn keyspace(&self) -> &str {
        // the keyspace is fixed at preparation
        &self.ns
    }

    /// Run one prepared statement with its values
    ///
    /// # Arguments
    ///
    /// * `statement` - The statement
    /// * `values` - Its values
    /// * `what` - What the statement does, for errors
    async fn run(
        &self,
        statement: &PreparedStatement,
        values: impl SerializeRow,
        what: &str,
    ) -> Result<scylla::response::query_result::QueryResult, MigrateError> {
        // execute unpaged and classify the failure
        self.session
            .execute_unpaged(statement, values)
            .await
            .map_err(|err| backend_error(what, &err, is_rejection(&err)))
    }

    /// Read a pass's row
    ///
    /// # Arguments
    ///
    /// * `key` - The pass key
    pub async fn pass_row(&self, key: &str) -> Result<Option<PassRow>, MigrateError> {
        // read the one row, if the pass has started
        let result = self
            .run(&self.select_pass, (key,), "read the pass row")
            .await?;
        let rows = result
            .into_rows_result()
            .map_err(|err| backend_error("read the pass row", &err, false))?;
        let row = rows
            .maybe_first_row::<PassColumns>()
            .map_err(|err| backend_error("read the pass row", &err, false))?;
        // a row without a status or attempt isn't a pass
        Ok(row.and_then(
            |(status, attempt, started, finished, lease, job, backup, revision, counts, report)| {
                Some(PassRow {
                    status: status?,
                    attempt: attempt?,
                    started,
                    finished,
                    lease,
                    job,
                    backup,
                    revision,
                    counts: counts.map(|counts| counts.0),
                    report: report.map(|report| report.0),
                })
            },
        ))
    }

    /// Start a pass as a new attempt: write its row first, then delete the old attempt's rows
    ///
    /// A crash between the two writes is harmless, since readers ignore rows of any other
    /// attempt.
    ///
    /// # Arguments
    ///
    /// * `key` - The pass key
    /// * `attempt` - The new attempt
    /// * `holder` - The writer lease holder
    /// * `job` - The Job running the pass, if any
    /// * `backup` - The approval's backup reference, if any
    /// * `revision` - The revision the pass moves objects to
    pub async fn begin_attempt(
        &self,
        key: &str,
        attempt: &str,
        holder: &str,
        job: Option<&str>,
        backup: Option<&str>,
        revision: Option<&str>,
    ) -> Result<(), MigrateError> {
        // the new pass row comes first
        let now = Utc::now();
        self.run(
            &self.insert_pass,
            (
                key,
                MigrationRunStatus::Running,
                attempt,
                now,
                holder,
                job,
                backup,
                revision,
                now,
            ),
            "write the pass row",
        )
        .await?;
        // then the old attempt's rows go
        self.delete_rows(key).await
    }

    /// Mark a pass that resumes running again
    ///
    /// # Arguments
    ///
    /// * `key` - The pass key
    /// * `holder` - The writer lease holder
    /// * `job` - The Job running the pass, if any
    /// * `backup` - The approval's backup reference, if any
    pub async fn resume(
        &self,
        key: &str,
        holder: &str,
        job: Option<&str>,
        backup: Option<&str>,
    ) -> Result<(), MigrateError> {
        // set the status and who runs it, and clear the finish time
        self.run(
            &self.resume_pass,
            (
                MigrationRunStatus::Running,
                holder,
                job,
                backup,
                Utc::now(),
                key,
            ),
            "resume the pass row",
        )
        .await?;
        Ok(())
    }

    /// Record how a pass ended, in one update of its row
    ///
    /// # Arguments
    ///
    /// * `key` - The pass key
    /// * `status` - How it ended
    /// * `holder` - The writer lease holder
    /// * `counts` - What the pass did, summed across workers
    /// * `report` - What the pass applied
    pub async fn finish(
        &self,
        key: &str,
        status: MigrationRunStatus,
        holder: &str,
        counts: &PassCounts,
        report: &AppliedReport,
    ) -> Result<(), MigrateError> {
        // one single-row update carries the status, the finish time, the counts, and the report
        let now = Utc::now();
        self.run(
            &self.finish_pass,
            (
                status,
                now,
                holder,
                JsonText(counts),
                JsonText(report),
                now,
                key,
            ),
            "record how the pass ended",
        )
        .await?;
        Ok(())
    }

    /// Read every worker and issue row of a pass that belongs to an attempt
    ///
    /// # Arguments
    ///
    /// * `key` - The pass key
    /// * `attempt` - The attempt to read; rows of any other attempt are ignored
    pub async fn rows(
        &self,
        key: &str,
        attempt: &str,
    ) -> Result<(Vec<WorkerRow>, Vec<IssueRow>), MigrateError> {
        // read the whole partition, then keep this attempt's rows
        let result = self
            .run(&self.select_rows, (key,), "read the pass rows")
            .await?;
        let rows = result
            .into_rows_result()
            .map_err(|err| backend_error("read the pass rows", &err, false))?;
        let mut workers = Vec::new();
        let mut issues = Vec::new();
        for row in rows
            .rows::<RowColumns>()
            .map_err(|err| backend_error("read the pass rows", &err, false))?
        {
            let (
                transform,
                reason,
                worker,
                row_attempt,
                checkpoint,
                lease,
                class,
                count,
                first_seen,
                last_seen,
                samples,
                resolved,
                accepted,
            ) = row.map_err(|err| backend_error("read a pass row", &err, false))?;
            // rows of another attempt are never counted
            if row_attempt.as_deref() != Some(attempt) {
                continue;
            }
            if transform == WORKER_TRANSFORM {
                // a worker row holds its checkpoint
                workers.push(WorkerRow {
                    worker,
                    attempt: attempt.to_owned(),
                    checkpoint: checkpoint
                        .map_or_else(PassCheckpoint::default, |checkpoint| checkpoint.0),
                    lease,
                });
            } else if let Some(class) = class.and_then(|class| class.parse().ok()) {
                // an issue row holds one worker's tally of one group
                issues.push(IssueRow {
                    transform,
                    reason,
                    worker,
                    attempt: attempt.to_owned(),
                    class,
                    count: u64::try_from(count.unwrap_or_default()).unwrap_or_default(),
                    first_seen,
                    last_seen,
                    samples: samples.unwrap_or_default(),
                    resolved_count: u64::try_from(resolved.unwrap_or_default()).unwrap_or_default(),
                    accepted: accepted.unwrap_or_default(),
                });
            }
        }
        Ok((workers, issues))
    }

    /// Write a worker's checkpoint and the issue rows of the groups that changed in one
    /// single-partition batch
    ///
    /// Returns the size of the batch's values in bytes, so a caller can watch it.
    ///
    /// # Arguments
    ///
    /// * `key` - The pass key
    /// * `attempt` - The pass's current attempt
    /// * `worker` - The worker writing
    /// * `token` - The writer lease's fencing token the worker holds
    /// * `checkpoint` - The worker's checkpoint
    /// * `groups` - The groups that changed in the batch
    pub async fn write_batch(
        &self,
        key: &str,
        attempt: &str,
        worker: &str,
        token: &str,
        checkpoint: &PassCheckpoint,
        groups: &[GroupWrite<'_>],
    ) -> Result<usize, MigrateError> {
        // the batch holds the worker row first, then one statement per changed group
        let now = Utc::now();
        let mut batch = Batch::new(BatchType::Logged);
        let mut binds = Vec::with_capacity(groups.len() + 1);
        let mut bytes = serde_json::to_string(checkpoint).map_or(0, |json| json.len());
        batch.append_statement(self.update_worker.clone());
        binds.push(Bind::Worker((
            attempt,
            JsonText(checkpoint),
            token,
            now,
            key,
            worker,
        )));
        for group in groups {
            let count = i64::try_from(group.count).unwrap_or(i64::MAX);
            bytes += group.transform.len()
                + group.reason.len()
                + group.samples.iter().map(String::len).sum::<usize>();
            match group.resolved_count {
                // a recount also sets the resolved count and keeps the first-seen time
                Some(resolved) => {
                    batch.append_statement(self.recount_issue.clone());
                    binds.push(Bind::Recount((
                        attempt,
                        group.class.as_str(),
                        count,
                        i64::try_from(resolved).unwrap_or(i64::MAX),
                        group.samples,
                        group.last_seen,
                        now,
                        key,
                        group.transform,
                        group.reason,
                        worker,
                    )));
                }
                // a batch leaves the resolved count and the acceptance alone
                None => {
                    batch.append_statement(self.update_issue.clone());
                    binds.push(Bind::Issue((
                        attempt,
                        group.class.as_str(),
                        count,
                        group.samples,
                        group.first_seen,
                        group.last_seen,
                        now,
                        key,
                        group.transform,
                        group.reason,
                        worker,
                    )));
                }
            }
        }
        // one single-partition batch, applied atomically
        self.session
            .batch(&batch, &binds)
            .await
            .map_err(|err| backend_error("write a checkpoint batch", &err, is_rejection(&err)))?;
        Ok(bytes)
    }

    /// Mark one issue row accepted or not
    ///
    /// # Arguments
    ///
    /// * `key` - The pass key
    /// * `row` - The issue row
    /// * `accepted` - Whether the group's code is accepted
    pub async fn accept(
        &self,
        key: &str,
        row: &IssueRow,
        accepted: bool,
    ) -> Result<(), MigrateError> {
        // only the acceptance and the update time change
        self.run(
            &self.accept_issue,
            (
                accepted,
                Utc::now(),
                key,
                row.transform.as_str(),
                row.reason.as_str(),
                row.worker.as_str(),
            ),
            "mark an issue group accepted",
        )
        .await?;
        Ok(())
    }

    /// Delete every worker and issue row of a pass
    ///
    /// # Arguments
    ///
    /// * `key` - The pass key
    pub async fn delete_rows(&self, key: &str) -> Result<(), MigrateError> {
        // one partition delete
        self.run(&self.delete_pass, (key,), "delete the pass rows")
            .await?;
        Ok(())
    }

    /// Write a check Job's report to its own result row, which expires after seven days
    ///
    /// # Arguments
    ///
    /// * `id` - The row's id, the Job's UID
    /// * `report` - The check report
    pub async fn write_result(&self, id: &str, report: &CheckReport) -> Result<(), MigrateError> {
        // the report is the row's only content
        let value = serde_json::to_value(report)
            .map_err(|err| backend_error("encode the check report", &err, true))?;
        self.run(
            &self.insert_result,
            (id, JsonText(value), Utc::now()),
            "write the check result",
        )
        .await?;
        Ok(())
    }

    /// The Scylla session the store writes through
    #[must_use]
    pub fn session(&self) -> &Arc<Session> {
        // shared with the migration's own reads
        &self.session
    }
}
