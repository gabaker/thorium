//! The reactions a drain cancels, recorded as `schema_migrations` rows so they can be resubmitted
//!
//! A `drain_request` row holds the request as JSON without its arguments, the drain attempt that
//! recorded it, and how far its resubmission got; a `drain_request_args` row with the same id holds
//! the arguments and always expires. The typed [`SchemaMigrationRow`](super::scylla::SchemaMigrationRow)
//! can't hold a drain status, so these rows are read and written as raw text columns, and every
//! write of a `drain_request` row is guarded: a lightweight transaction on a keyspace without
//! tablets, and a read then a plain write under the namespace's writer lease on one with them (see
//! [`DrainRequestStore`]).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::str::FromStr;
use uuid::Uuid;

use crate::models::{InvalidEnum, Pipeline, ReactionStatus, RepoDependencyRequest};

/// The accounts the operator makes for itself, whose reactions (hunts among them) are recorded as
/// refused and never resubmitted; `operator/src/app/bootstrap.rs` keeps its own private list of the
/// same accounts, and the two must stay equal
pub const SERVICE_USERS: [&str; 3] = ["thorium-operator", "thorium", "thorium-kaboom"];

/// The most requests one drain attempt records
pub const MAX_REQUESTS: usize = 10_000;

/// The most bytes of argument JSON one request's args row holds
pub const MAX_ARGS_BYTES: usize = 64 * 1024;

/// How many days without activity make a recorded request stale
pub const IDLE_DAYS: i64 = 30;

/// How long an args row lives, in seconds: 14 days
pub const ARGS_TTL_SECS: i32 = 14 * 24 * 60 * 60;

/// How far a recorded drain request's resubmission got
///
/// Stored as `snake_case` text, which is also its JSON form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DrainStatus {
    /// Recorded by the cancel and not yet resubmitted; stored as `recorded`
    Recorded,
    /// A new reaction id is reserved in `entry` and the reaction may exist; stored as
    /// `resubmitting`
    Resubmitting,
    /// The reaction was created again; `result` names the new id; stored as `resubmitted`
    Resubmitted,
    /// The request can't be resubmitted; `result` holds the reason; stored as `refused`
    Refused,
    /// Recorded outside the current attempt or idle too long, so never resubmitted on its own;
    /// `result` holds the reason; stored as `stale`
    Stale,
}

impl DrainStatus {
    /// Every drain status, in the order they are declared
    pub const ALL: [DrainStatus; 5] = [
        Self::Recorded,
        Self::Resubmitting,
        Self::Resubmitted,
        Self::Refused,
        Self::Stale,
    ];

    /// The text this status is stored as, which never changes once released
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        // map each status to its stored text
        match self {
            Self::Recorded => "recorded",
            Self::Resubmitting => "resubmitting",
            Self::Resubmitted => "resubmitted",
            Self::Refused => "refused",
            Self::Stale => "stale",
        }
    }

    /// How far a request with this status has moved, so a merge never moves one backward: a
    /// final status outranks a reservation, which outranks a recorded or stale request
    #[must_use]
    pub fn rank(&self) -> u8 {
        // the final statuses rank alike, as do the two that hold no reservation
        match self {
            Self::Resubmitted | Self::Refused => 3,
            Self::Resubmitting => 2,
            Self::Recorded | Self::Stale => 1,
        }
    }
}

impl FromStr for DrainStatus {
    /// Stored text that names no drain status fails with an [`InvalidEnum`]
    type Err = InvalidEnum;

    /// Parse a drain status from its stored text
    ///
    /// # Arguments
    ///
    /// * `raw` - The stored text to parse
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        // find the status stored this way, rejecting anything else
        Self::ALL
            .into_iter()
            .find(|status| status.as_str() == raw)
            .ok_or_else(|| InvalidEnum(raw.to_owned()))
    }
}

impl fmt::Display for DrainStatus {
    /// Write the status as its stored text
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // the stored text is the display text
        f.write_str(self.as_str())
    }
}

/// What identified the pipeline a request ran when it was recorded; a pipeline has no id or
/// creation time, so a recreated pipeline is told apart by these
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PipelineIdentity {
    /// The pipeline's group
    pub group: String,
    /// The pipeline's name
    pub name: String,
    /// The user who created the pipeline
    pub creator: String,
    /// The sha256 of the pipeline's stage order (the image names by stage) as JSON
    pub order_sha256: String,
}

impl PipelineIdentity {
    /// The identity of a pipeline as it stands: its group, name, creator, and the sha256 of its
    /// stage order as compact JSON
    ///
    /// # Arguments
    ///
    /// * `pipeline` - The pipeline to identify
    #[must_use]
    pub fn of(pipeline: &Pipeline) -> Self {
        // the order is hashed as compact JSON, which a list of lists of strings always renders as
        let order = serde_json::to_string(&pipeline.order).unwrap_or_default();
        PipelineIdentity {
            group: pipeline.group.clone(),
            name: pipeline.name.clone(),
            creator: pipeline.creator.clone(),
            order_sha256: args_sha256(&order),
        }
    }
}

/// Whether a request's creator held a role in its group when the request was recorded
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatorMembership {
    /// Whether the creator was in the group's `owners`, `managers`, or `users`; an admin's or
    /// analyst's bypass doesn't count
    pub held_role: bool,
    /// The creator's role in the group, or `none`
    pub role: String,
}

/// One cancelled reaction's request, stored as JSON in a `drain_request` row's `value`
///
/// It never holds the reaction's arguments: those live in the `drain_request_args` row, and this
/// keeps only their hash and size.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DrainRequest {
    /// The group the reaction ran in
    pub group: String,
    /// The pipeline the reaction ran
    pub pipeline: String,
    /// The samples the reaction ran on
    #[serde(default)]
    pub samples: Vec<String>,
    /// The repos the reaction ran on, as requests
    #[serde(default)]
    pub repos: Vec<RepoDependencyRequest>,
    /// The reaction's tags
    #[serde(default)]
    pub tags: Vec<String>,
    /// The reaction's parent, if it was a sub reaction
    #[serde(default)]
    pub parent: Option<Uuid>,
    /// The reaction's trigger depth
    #[serde(default)]
    pub trigger_depth: Option<u8>,
    /// The user who created the reaction
    pub creator: String,
    /// The reaction's status when it was cancelled
    pub old_status: ReactionStatus,
    /// The reaction's newest activity: its newest status log entry, or its SLA when it has none
    #[serde(default)]
    pub last_activity: Option<DateTime<Utc>>,
    /// Whether the reaction was missing from its pipeline's status set
    #[serde(default)]
    pub orphan: bool,
    /// The sha256 of the reaction's argument JSON, when it had arguments
    #[serde(default)]
    pub args_sha256: Option<String>,
    /// The size of the reaction's argument JSON in bytes
    #[serde(default)]
    pub args_bytes: u64,
    /// Whether the reaction had ephemeral files uploaded with its request
    #[serde(default)]
    pub buffers: bool,
    /// Whether the reaction had a cache
    #[serde(default)]
    pub cache: bool,
    /// The pipeline the request ran when it was recorded
    pub identity: PipelineIdentity,
    /// Whether the creator held a role in the group when the request was recorded
    pub membership: CreatorMembership,
    /// The converted pipeline's tool ids by stage, bound inside the conversion pause before any
    /// user can change the pipeline
    #[serde(default)]
    pub binding: Option<Vec<Vec<String>>>,
}

impl DrainRequest {
    /// Parse a request from a row's `value`
    ///
    /// # Arguments
    ///
    /// * `value` - The JSON text stored in the row
    pub fn from_json(value: &str) -> Result<Self, serde_json::Error> {
        // the value is the request as JSON
        serde_json::from_str(value)
    }

    /// Render the request as the JSON text a row's `value` stores
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        // the value is the request as JSON
        serde_json::to_string(self)
    }
}

/// The lowercase hex sha256 of a request's argument JSON, which its row records
///
/// # Arguments
///
/// * `args_json` - The argument JSON text
#[must_use]
pub fn args_sha256(args_json: &str) -> String {
    // hash the exact text the args row stores
    hex::encode(Sha256::digest(args_json.as_bytes()))
}

/// One `drain_request` row, read and written as raw text columns
#[derive(Debug, Clone, PartialEq)]
pub struct DrainRequestRow {
    /// The cancelled reaction's id
    pub id: String,
    /// The request as [`DrainRequest`] JSON
    pub value: String,
    /// The `at` of the drain's `Running` entry of the run that recorded it, if that run named one
    pub attempt: Option<String>,
    /// How far its resubmission got
    pub status: DrainStatus,
    /// The reserved new reaction id, set once when the row becomes `resubmitting` and never
    /// cleared or overwritten
    pub entry: Option<String>,
    /// The new reaction id once `resubmitted`, or the reason once `refused` or `stale`
    pub result: Option<String>,
    /// The revision whose drain recorded it
    pub revision: Option<String>,
    /// When the reaction was cancelled
    pub started: Option<DateTime<Utc>>,
    /// When the request was resubmitted, refused, or marked stale
    pub finished: Option<DateTime<Utc>>,
    /// When the row last changed
    pub updated: Option<DateTime<Utc>>,
}

impl DrainRequestRow {
    /// Parse the row's request
    pub fn request(&self) -> Result<DrainRequest, serde_json::Error> {
        // the value is the request as JSON
        DrainRequest::from_json(&self.value)
    }
}

#[cfg(feature = "upgrades-runtime")]
pub use store::DrainRequestStore;

/// The prepared statements on drain request rows
#[cfg(feature = "upgrades-runtime")]
mod store {
    use chrono::{DateTime, Utc};
    use scylla::client::session::Session;
    use scylla::response::query_result::QueryResult;
    use scylla::serialize::row::SerializeRow;
    use scylla::statement::prepared::PreparedStatement;
    use scylla::value::{CqlValue, Row};
    use std::sync::Arc;

    use super::{ARGS_TTL_SECS, DrainRequestRow, DrainStatus};
    use crate::models::schema::queries::migrations::{insert_if_absent, update_status_if};
    use crate::models::schema::{MigrateError, MigrationRecordKind, backend_error, is_rejection};

    /// The stored kind of a request row
    const REQUEST: &str = "drain_request";

    /// The stored kind of an args row
    const ARGS: &str = "drain_request_args";

    /// The columns a request row is read with, in [`RowColumns`] order
    const ROW_COLUMNS: &str =
        "id, value, attempt, status, entry, result, revision, started, finished, updated";

    /// A request row's columns as read: the id, value, attempt, status, entry, result, revision,
    /// and the started, finished, and updated times
    type RowColumns = (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
    );

    /// The statements that write a request row only when a condition holds on the stored row
    enum GuardedWrites {
        /// Lightweight transactions, on a keyspace without tablets: the server checks the
        /// condition and writes in one step
        Conditional {
            /// Insert a request row unless one exists (`IF NOT EXISTS`)
            insert_if_absent: PreparedStatement,
            /// Update a request row while its status is the one the writer read (`IF status = ?`)
            update_status_if: PreparedStatement,
        },
        /// A read of the stored status then a plain write, on a keyspace on tablets, which
        /// refuse lightweight transactions; the namespace's writer lease keeps a second writer
        /// from running between the read and the write
        ReadThenWrite {
            /// Read a request row's stored status, which tells whether the row exists
            status: PreparedStatement,
            /// Insert or overwrite a request row with every column
            insert: PreparedStatement,
            /// Update a request row's status and outcome
            update: PreparedStatement,
        },
    }

    /// The prepared statements on `drain_request` and `drain_request_args` rows, in one keyspace
    ///
    /// The two guarded writes, [`insert_if_absent`](Self::insert_if_absent) and
    /// [`update_status_if`](Self::update_status_if), take the form the keyspace allows, decided
    /// once when the store is prepared:
    ///
    /// * On a keyspace without tablets (vnodes) they are lightweight transactions (`IF NOT EXISTS`
    ///   and `IF status = ?`), so the server applies the condition and the write atomically and
    ///   two writers can never both win.
    /// * On a keyspace on tablets, which Scylla 6.x may create by default and which refuse
    ///   lightweight transactions, each is a read of the stored row, the same condition checked in
    ///   the client, then a plain write of the same columns. That is not atomic: it relies on the
    ///   namespace's writer lease, which `thoradm migrate run` holds for its whole run and a
    ///   restore holds on its own, so only a run that breaks the lease with
    ///   `--force-break-lease` while another writer still runs could interleave between the read
    ///   and the write.
    ///
    /// [`conditional`](Self::conditional) tells which form a store uses.
    pub struct DrainRequestStore {
        /// The Scylla session
        session: Arc<Session>,
        /// The guarded writes of request rows, in the form the keyspace allows
        guarded: GuardedWrites,
        /// Read one request row
        get: PreparedStatement,
        /// Read a page of request rows after an id
        list_page: PreparedStatement,
        /// Write an args row with its time-to-live
        put_args: PreparedStatement,
        /// Read one args row
        get_args: PreparedStatement,
        /// Delete one args row
        delete_args: PreparedStatement,
    }

    /// Turn one read row into a request row, refusing a row with no value or a status that isn't
    /// a drain status
    ///
    /// # Arguments
    ///
    /// * `columns` - The row's columns as read
    fn to_row(columns: RowColumns) -> Result<DrainRequestRow, MigrateError> {
        // name the columns
        let (id, value, attempt, status, entry, result, revision, started, finished, updated) =
            columns;
        // a request row always holds its request and a known status
        let value = value
            .ok_or_else(|| MigrateError::Systematic(format!("drain request {id} has no value")))?;
        let status = status
            .as_deref()
            .unwrap_or_default()
            .parse::<DrainStatus>()
            .map_err(|err| {
                MigrateError::Systematic(format!("drain request {id} has status {}", err.0))
            })?;
        Ok(DrainRequestRow {
            id,
            value,
            attempt,
            status,
            entry,
            result,
            revision,
            started,
            finished,
            updated,
        })
    }

    /// Whether a conditional statement was applied, read from its result's `[applied]` column
    ///
    /// # Arguments
    ///
    /// * `result` - The result of a conditional statement
    /// * `what` - What the statement does, for errors
    fn applied(result: QueryResult, what: &str) -> Result<bool, MigrateError> {
        // a conditional statement always answers with one row whose `[applied]` says how it went
        let rows = result
            .into_rows_result()
            .map_err(|err| backend_error(what, &err, false))?;
        let index = rows
            .column_specs()
            .iter()
            .position(|spec| spec.name() == "[applied]")
            .ok_or_else(|| {
                MigrateError::Backend(format!("{what}: the result has no [applied] column"))
            })?;
        let row = rows
            .first_row::<Row>()
            .map_err(|err| backend_error(what, &err, false))?;
        match row.columns.get(index) {
            Some(Some(CqlValue::Boolean(applied))) => Ok(*applied),
            _ => Err(MigrateError::Backend(format!(
                "{what}: [applied] isn't a boolean"
            ))),
        }
    }

    /// Whether a keyspace uses tablets, read from `system_schema.scylla_keyspaces`; a Scylla
    /// without that table, or a keyspace with no row or no `initial_tablets` in it, uses vnodes
    ///
    /// # Arguments
    ///
    /// * `session` - The Scylla session
    /// * `ns` - The keyspace
    async fn uses_tablets(session: &Session, ns: &str) -> Result<bool, MigrateError> {
        use scylla::errors::{DbError, ExecutionError, RequestAttemptError};
        // read the keyspace's tablet setting, which only a Scylla that supports tablets records
        let what = "read whether the keyspace uses tablets";
        let cql = "SELECT initial_tablets FROM system_schema.scylla_keyspaces \
                   WHERE keyspace_name = ?";
        let result = match session.query_unpaged(cql, (ns,)).await {
            Ok(result) => result,
            // a Scylla that predates tablets has no such table, so its keyspaces use vnodes
            Err(ExecutionError::LastAttemptError(RequestAttemptError::DbError(
                DbError::Invalid,
                _,
            ))) => return Ok(false),
            Err(err) => return Err(backend_error(what, &err, is_rejection(&err))),
        };
        // a row with an initial tablet count means the keyspace is on tablets
        let rows = result
            .into_rows_result()
            .map_err(|err| backend_error(what, &err, false))?;
        let row = rows
            .maybe_first_row::<(Option<i32>,)>()
            .map_err(|err| backend_error(what, &err, false))?;
        Ok(matches!(row, Some((Some(_),))))
    }

    impl DrainRequestStore {
        /// Prepare the statements on drain request rows, choosing lightweight transactions for
        /// the guarded writes on a keyspace without tablets and a read then a plain write on one
        /// with them
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
                    session.prepare(cql).await.map_err(|err| {
                        backend_error("prepare a drain request statement", &err, false)
                    })
                }
            };
            // tablets refuse lightweight transactions, so their guarded writes read then write
            let guarded = if uses_tablets(&session, ns).await? {
                GuardedWrites::ReadThenWrite {
                    status: prepare(format!(
                        "SELECT status FROM {ns}.schema_migrations WHERE kind = '{REQUEST}' \
                         AND id = ?"
                    ))
                    .await?,
                    insert: prepare(insert_row(ns)).await?,
                    update: prepare(update_status(ns)).await?,
                }
            } else {
                GuardedWrites::Conditional {
                    insert_if_absent: prepare(insert_if_absent(ns)).await?,
                    update_status_if: prepare(update_status_if(ns)).await?,
                }
            };
            let get = prepare(format!(
                "SELECT {ROW_COLUMNS} FROM {ns}.schema_migrations WHERE kind = '{REQUEST}' \
                 AND id = ?"
            ))
            .await?;
            let list_page = prepare(format!(
                "SELECT {ROW_COLUMNS} FROM {ns}.schema_migrations WHERE kind = '{REQUEST}' \
                 AND id > ? LIMIT ?"
            ))
            .await?;
            let put_args = prepare(format!(
                "INSERT INTO {ns}.schema_migrations (kind, id, value, updated) \
                 VALUES ('{ARGS}', ?, ?, ?) USING TTL {ARGS_TTL_SECS}"
            ))
            .await?;
            let get_args = prepare(format!(
                "SELECT value FROM {ns}.schema_migrations WHERE kind = '{ARGS}' AND id = ?"
            ))
            .await?;
            let delete_args = prepare(format!(
                "DELETE FROM {ns}.schema_migrations WHERE kind = '{ARGS}' AND id = ?"
            ))
            .await?;
            Ok(DrainRequestStore {
                session,
                guarded,
                get,
                list_page,
                put_args,
                get_args,
                delete_args,
            })
        }

        /// Whether the guarded writes are lightweight transactions (a keyspace without tablets)
        /// rather than a read then a plain write under the writer lease (a keyspace on tablets)
        #[must_use]
        pub fn conditional(&self) -> bool {
            // the form was chosen when the store was prepared
            matches!(self.guarded, GuardedWrites::Conditional { .. })
        }

        /// Read a request row's stored status, or `None` when no row has its id
        ///
        /// # Arguments
        ///
        /// * `statement` - The prepared status read
        /// * `id` - The cancelled reaction's id
        /// * `what` - What the guarded write does, for errors
        async fn stored_status(
            &self,
            statement: &PreparedStatement,
            id: &str,
            what: &str,
        ) -> Result<Option<Option<String>>, MigrateError> {
            // read the one row's status, if the row exists
            let result = self.run(statement, (id,), what).await?;
            let rows = result
                .into_rows_result()
                .map_err(|err| backend_error(what, &err, false))?;
            let row = rows
                .maybe_first_row::<(Option<String>,)>()
                .map_err(|err| backend_error(what, &err, false))?;
            Ok(row.map(|(status,)| status))
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
        ) -> Result<QueryResult, MigrateError> {
            // execute unpaged and classify the failure
            self.session
                .execute_unpaged(statement, values)
                .await
                .map_err(|err| backend_error(what, &err, is_rejection(&err)))
        }

        /// Write a request row unless one with its id exists; returns whether it was written
        ///
        /// An existing row is kept as it is, so a second cancel run never overwrites the first
        /// one's attempt, status, or reserved id. On a keyspace on tablets the check is a read
        /// before the write, which the writer lease guards (see [`DrainRequestStore`]).
        ///
        /// # Arguments
        ///
        /// * `row` - The row to write
        pub async fn insert_if_absent(&self, row: &DrainRequestRow) -> Result<bool, MigrateError> {
            // bind every column the row holds
            let values = (
                MigrationRecordKind::DrainRequest.as_str(),
                row.id.as_str(),
                row.value.as_str(),
                row.attempt.as_deref(),
                row.status.as_str(),
                row.entry.as_deref(),
                row.result.as_deref(),
                row.revision.as_deref(),
                row.started,
                row.finished,
                row.updated.unwrap_or_else(Utc::now),
            );
            let what = "record a drain request";
            match &self.guarded {
                // the server writes only when no row exists
                GuardedWrites::Conditional {
                    insert_if_absent, ..
                } => {
                    let result = self.run(insert_if_absent, values, what).await?;
                    applied(result, what)
                }
                // write only when the read finds no row
                GuardedWrites::ReadThenWrite { status, insert, .. } => {
                    if self.stored_status(status, &row.id, what).await?.is_some() {
                        return Ok(false);
                    }
                    self.run(insert, values, what).await?;
                    Ok(true)
                }
            }
        }

        /// Write a request row's status, entry, result, value, and finish time, only while its
        /// stored status is still `expected`; returns whether it was written
        ///
        /// A write that isn't applied leaves the row as another writer left it; the caller
        /// re-reads it and acts on what it holds. A missing row is never written. On a keyspace
        /// on tablets the check is a read before the write, which the writer lease guards (see
        /// [`DrainRequestStore`]).
        ///
        /// # Arguments
        ///
        /// * `row` - The row with its new values
        /// * `expected` - The status the row must still hold
        pub async fn update_status_if(
            &self,
            row: &DrainRequestRow,
            expected: DrainStatus,
        ) -> Result<bool, MigrateError> {
            // the new values, then the row's key, then the status it must still hold
            let values = (
                row.status.as_str(),
                row.entry.as_deref(),
                row.result.as_deref(),
                row.value.as_str(),
                row.finished,
                row.updated.unwrap_or_else(Utc::now),
                MigrationRecordKind::DrainRequest.as_str(),
                row.id.as_str(),
                expected.as_str(),
            );
            let what = "update a drain request";
            match &self.guarded {
                // the server writes only while the stored status is the expected one
                GuardedWrites::Conditional {
                    update_status_if, ..
                } => {
                    let result = self.run(update_status_if, values, what).await?;
                    applied(result, what)
                }
                // write only when the read finds the row holding the expected status
                GuardedWrites::ReadThenWrite { status, update, .. } => {
                    let stored = self.stored_status(status, &row.id, what).await?;
                    if stored.flatten().as_deref() != Some(expected.as_str()) {
                        return Ok(false);
                    }
                    // the plain update binds the same values without the expected status
                    let (status, entry, result, value, finished, updated, kind, id, _) = values;
                    let values = (status, entry, result, value, finished, updated, kind, id);
                    self.run(update, values, what).await?;
                    Ok(true)
                }
            }
        }

        /// Read one request row
        ///
        /// # Arguments
        ///
        /// * `id` - The cancelled reaction's id
        pub async fn get(&self, id: &str) -> Result<Option<DrainRequestRow>, MigrateError> {
            // read the one row, if it was recorded
            let what = "read a drain request";
            let result = self.run(&self.get, (id,), what).await?;
            let rows = result
                .into_rows_result()
                .map_err(|err| backend_error(what, &err, false))?;
            let row = rows
                .maybe_first_row::<RowColumns>()
                .map_err(|err| backend_error(what, &err, false))?;
            row.map(to_row).transpose()
        }

        /// Read a page of request rows in id order, starting after an id
        ///
        /// # Arguments
        ///
        /// * `after` - The id of the last row of the previous page, or `None` for the first page
        /// * `limit` - The most rows to read
        pub async fn list_page(
            &self,
            after: Option<&str>,
            limit: usize,
        ) -> Result<Vec<DrainRequestRow>, MigrateError> {
            // no id is empty, so the first page starts after the empty string
            let after = after.unwrap_or_default();
            let limit = i32::try_from(limit).unwrap_or(i32::MAX);
            let what = "list drain requests";
            let result = self.run(&self.list_page, (after, limit), what).await?;
            let rows = result
                .into_rows_result()
                .map_err(|err| backend_error(what, &err, false))?;
            rows.rows::<RowColumns>()
                .map_err(|err| backend_error(what, &err, false))?
                .map(|row| {
                    row.map_err(|err| backend_error(what, &err, false))
                        .and_then(to_row)
                })
                .collect()
        }

        /// Write a request's args row, which expires after [`ARGS_TTL_SECS`] whatever else
        /// happens
        ///
        /// # Arguments
        ///
        /// * `id` - The cancelled reaction's id
        /// * `args_json` - The reaction's argument JSON
        pub async fn put_args(&self, id: &str, args_json: &str) -> Result<(), MigrateError> {
            // every write of an args row carries the time-to-live
            self.run(
                &self.put_args,
                (id, args_json, Utc::now()),
                "record drain args",
            )
            .await?;
            Ok(())
        }

        /// Read a request's args, if its row still exists
        ///
        /// # Arguments
        ///
        /// * `id` - The cancelled reaction's id
        pub async fn get_args(&self, id: &str) -> Result<Option<String>, MigrateError> {
            // read the one value, if it hasn't expired or been deleted
            let what = "read drain args";
            let result = self.run(&self.get_args, (id,), what).await?;
            let rows = result
                .into_rows_result()
                .map_err(|err| backend_error(what, &err, false))?;
            let row = rows
                .maybe_first_row::<(Option<String>,)>()
                .map_err(|err| backend_error(what, &err, false))?;
            Ok(row.and_then(|(value,)| value))
        }

        /// Delete a request's args row
        ///
        /// # Arguments
        ///
        /// * `id` - The cancelled reaction's id
        pub async fn delete_args(&self, id: &str) -> Result<(), MigrateError> {
            // a missing row deletes as nothing
            self.run(&self.delete_args, (id,), "delete drain args")
                .await?;
            Ok(())
        }
    }

    /// Insert or overwrite a `schema_migrations` row with every column a drain request row uses,
    /// bound in the order of [`insert_if_absent`]; the plain write of a keyspace on tablets
    ///
    /// # Arguments
    ///
    /// * `ns` - The keyspace the statement targets
    fn insert_row(ns: &str) -> String {
        // the conditional insert without its condition
        insert_if_absent(ns)
            .trim_end_matches(" IF NOT EXISTS")
            .to_owned()
    }

    /// Update a `schema_migrations` row's status and outcome, bound in the order of
    /// [`update_status_if`] without the expected status; the plain write of a keyspace on tablets
    ///
    /// # Arguments
    ///
    /// * `ns` - The keyspace the statement targets
    fn update_status(ns: &str) -> String {
        // the conditional update without its condition
        update_status_if(ns)
            .trim_end_matches(" IF status = ?")
            .to_owned()
    }

    /// The plain writes of a keyspace on tablets are the conditional writes without their
    /// conditions
    #[cfg(test)]
    #[test]
    fn the_plain_writes_drop_only_the_condition() {
        // each plain write is its conditional write minus the condition
        assert_eq!(
            insert_row("ns"),
            insert_if_absent("ns").replace(" IF NOT EXISTS", "")
        );
        assert!(!insert_row("ns").contains(" IF "));
        assert_eq!(
            update_status("ns"),
            update_status_if("ns").replace(" IF status = ?", "")
        );
        assert!(update_status("ns").ends_with("WHERE kind = ? AND id = ?"));
    }

    /// The stored kinds the store's statements name, which must match the record kinds
    #[cfg(test)]
    #[test]
    fn the_store_names_the_drain_kinds() {
        // the literal kinds equal the record kinds' stored text
        assert_eq!(REQUEST, MigrationRecordKind::DrainRequest.as_str());
        assert_eq!(ARGS, MigrationRecordKind::DrainRequestArgs.as_str());
    }
}
