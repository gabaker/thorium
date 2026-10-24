//! The Scylla tables that hold migration state: the deployment's schema record, its revision and
//! history, and each migration pass's working rows

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use crate::models::schema::{
    AppliedReport, JsonText, MigrationRecordKind, MigrationRunStatus, ObjectClass, PassCheckpoint,
    PassCounts, ScyllaProjection, ScyllaTable,
};
use crate::models::upgrades::AppliedStep;

/// Migration state: one row per backend (the applied fingerprint and layout versions), one row per
/// migration pass (status, attempt, and audit fields), the deployment's revision and one row per
/// history entry, and one row per data-shaping setting
///
/// Also serialized as JSON into backups, except the `job_result` rows
#[derive(ScyllaTable, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[thorium_table(name = "schema_migrations")]
pub struct SchemaMigrationsTable {
    /// Which kind of record this row is (`as_str` text via `ScyllaStoreAsStr`)
    #[thorium(partition)]
    pub kind: MigrationRecordKind,
    /// The backend (`scylla`, `elastic`, `redis`, `s3`, or `deployment`) for a backend row; the
    /// pass key for a pass row; `deployment` for the revision row;
    /// `<at>/<revision>/<step>/<outcome>` for a history row, with `at` in RFC 3339 with
    /// nanoseconds, so history rows sort oldest first and need no counter; the setting
    /// (`partition_size.files`, `partition_size.tags.<type>`, `earliest.repos`, `files.password`)
    /// for a config row; the code for a consumed-code row; `<restore time>/<original id>` for an
    /// archived history row; the Job's UID for a Job-result row
    #[thorium(cluster)]
    pub id: String,
    /// The applied fingerprint (backend rows)
    pub fingerprint: Option<String>,
    /// The schema-apply trigger key the fingerprint was applied for (the deployment's backend row)
    pub apply_key: Option<String>,
    /// The layout version of each Redis family or S3 bucket (Redis and S3 backend rows)
    #[thorium(unsigned = "bigint")]
    pub layouts: BTreeMap<String, u32>,
    /// The revision the pass moves objects to (pass rows), or the revision the deployment is at
    /// (the revision row)
    pub revision: Option<String>,
    /// The Thorium version that recorded the revision (the revision row)
    pub version: Option<String>,
    /// One applied-history entry (history rows, and archived history rows); for a consumed-code
    /// row, the step that consumed the code
    pub entry: Option<JsonText<AppliedStep>>,
    /// A Job's full result, a `RecordStatus` or, for a check Job, a `CheckReport` (Job-result rows,
    /// written with a seven-day TTL and left out of backups); the status Job writes none
    pub result: Option<JsonText<serde_json::Value>>,
    /// The recorded value of a data-shaping setting: a partition size or `earliest` as decimal
    /// text, or the files password's argon2id PHC string (config rows)
    pub value: Option<String>,
    /// The pass's status (pass rows; `as_str` text)
    pub status: Option<MigrationRunStatus>,
    /// The pass's current attempt, a UUID minted when the pass starts; worker and issue rows of
    /// any other attempt are ignored (pass rows)
    pub attempt: Option<String>,
    /// The writer lease holder that last wrote the pass (a Job name, or host and OS user)
    pub lease: Option<String>,
    /// When the pass first started (pass rows)
    pub started: Option<DateTime<Utc>>,
    /// When the pass finished (pass rows)
    pub finished: Option<DateTime<Utc>>,
    /// The Job that last wrote this row: the schema-apply Job for backend rows, the migration Job
    /// for pass rows (empty for a run outside Kubernetes)
    pub job: Option<String>,
    /// The approval's backup reference (pass rows)
    pub backup: Option<String>,
    /// What the pass did, counted per object class across its workers, written with the status
    /// when the pass ends (pass rows)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counts: Option<JsonText<PassCounts>>,
    /// What the pass applied, by group, with the skipped items and the notes the admin reads
    /// afterwards, written with the status when the pass ends (pass rows)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<JsonText<AppliedReport>>,
    /// When this row was last written
    pub updated: DateTime<Utc>,
}

/// A pass's working rows: one worker row per worker (its checkpoint) and one row per issue group a
/// worker found, all in the pass's partition
///
/// A worker writes its worker row and the issue rows of the groups that changed in a batch in one
/// single-partition batch, which Scylla applies atomically without a batchlog or a size check; a
/// batch updates only `count`, `samples`, `first_seen`, and `last_seen` of an issue row, never
/// `resolved_count` or `accepted`; no counters and no lightweight transactions
///
/// Also serialized as JSON into backups
#[derive(ScyllaTable, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[thorium_table(name = "schema_migration_issues")]
pub struct SchemaMigrationIssuesTable {
    /// The pass key the rows belong to: the family a declared transform rewrites, or the
    /// migration id of a hand-written migration
    #[thorium(partition)]
    pub migration: String,
    /// The step id of the transform that failed, which for a fused chain names its link;
    /// `~worker` for a worker row
    #[thorium(cluster)]
    pub transform: String,
    /// The reason code, `<category>:<field>` or `<category>`; `~` for a worker row
    #[thorium(cluster)]
    pub reason: String,
    /// The worker that wrote this row (`w00`, `w01`, …)
    #[thorium(cluster)]
    pub worker: String,
    /// The pass attempt this row belongs to; rows of another attempt are ignored
    pub attempt: String,
    /// The worker's checkpoint, written after each batch that had rows (worker rows)
    pub checkpoint: Option<JsonText<PassCheckpoint>>,
    /// The writer lease's fencing token the worker held when it wrote the row (worker rows); a
    /// worker whose token is stale stops
    pub lease: Option<String>,
    /// `unrecognized` or `error` (issue rows; `as_str` text)
    pub class: Option<ObjectClass>,
    /// How many objects are in the group now, as this worker last counted (issue rows)
    #[thorium(unsigned = "bigint")]
    pub count: u64,
    /// When the group was first recorded (issue rows)
    pub first_seen: Option<DateTime<Utc>>,
    /// When the group was last seen by a run (issue rows)
    pub last_seen: Option<DateTime<Utc>>,
    /// Up to 20 of the group's object keys, such as usernames or rows' primary keys rendered as
    /// text (issue rows)
    pub samples: BTreeSet<String>,
    /// How many objects left the group because a later run processed them successfully (issue
    /// rows)
    #[thorium(unsigned = "bigint")]
    pub resolved_count: u64,
    /// Whether an approval's `accept_issues` accepted this group's code for the run (issue rows)
    pub accepted: bool,
    /// When this row was last written
    pub updated: DateTime<Utc>,
}

/// One `schema_migrations` row as its readers read it; every column a row of some kind leaves
/// unset is optional
#[derive(Debug, Clone, ScyllaProjection)]
#[thorium_projection(of = SchemaMigrationsTable)]
pub struct SchemaMigrationRow {
    /// Which kind of record this row is
    pub kind: MigrationRecordKind,
    /// The row's id within its kind
    pub id: String,
    /// The applied fingerprint (backend rows)
    pub fingerprint: Option<String>,
    /// The schema-apply trigger key (the deployment's backend row)
    pub apply_key: Option<String>,
    /// The layout version of each Redis family or S3 bucket (Redis and S3 backend rows)
    #[thorium(unsigned = "bigint")]
    pub layouts: Option<BTreeMap<String, u32>>,
    /// The revision of a pass row or the revision row
    pub revision: Option<String>,
    /// The Thorium version that recorded the revision
    pub version: Option<String>,
    /// One applied-history entry
    pub entry: Option<JsonText<AppliedStep>>,
    /// A Job's full result
    pub result: Option<JsonText<serde_json::Value>>,
    /// The recorded value of a data-shaping setting
    pub value: Option<String>,
    /// The pass's status
    pub status: Option<MigrationRunStatus>,
    /// The pass's current attempt
    pub attempt: Option<String>,
    /// The writer lease holder that last wrote the pass
    pub lease: Option<String>,
    /// When the pass first started
    pub started: Option<DateTime<Utc>>,
    /// When the pass finished
    pub finished: Option<DateTime<Utc>>,
    /// The Job that last wrote this row
    pub job: Option<String>,
    /// The approval's backup reference
    pub backup: Option<String>,
    /// The pass's counts
    pub counts: Option<JsonText<PassCounts>>,
    /// The pass's applied report
    pub report: Option<JsonText<AppliedReport>>,
    /// When this row was last written
    pub updated: Option<DateTime<Utc>>,
}

/// One `schema_migration_issues` row as its readers read it; every column a worker or issue row
/// leaves unset is optional
#[derive(Debug, Clone, ScyllaProjection)]
#[thorium_projection(of = SchemaMigrationIssuesTable)]
pub struct SchemaMigrationIssueRow {
    /// The pass key the row belongs to
    pub migration: String,
    /// The step id of the failing transform, or `~worker`
    pub transform: String,
    /// The reason code, or `~`
    pub reason: String,
    /// The worker that wrote the row
    pub worker: String,
    /// The pass attempt the row belongs to
    pub attempt: Option<String>,
    /// The worker's checkpoint (worker rows)
    pub checkpoint: Option<JsonText<PassCheckpoint>>,
    /// The writer lease's fencing token (worker rows)
    pub lease: Option<String>,
    /// `unrecognized` or `error` (issue rows)
    pub class: Option<ObjectClass>,
    /// How many objects are in the group (issue rows)
    #[thorium(unsigned = "bigint")]
    pub count: Option<u64>,
    /// When the group was first recorded (issue rows)
    pub first_seen: Option<DateTime<Utc>>,
    /// When the group was last seen (issue rows)
    pub last_seen: Option<DateTime<Utc>>,
    /// Up to 20 of the group's object keys (issue rows)
    pub samples: Option<BTreeSet<String>>,
    /// How many objects left the group after a later run (issue rows)
    #[thorium(unsigned = "bigint")]
    pub resolved_count: Option<u64>,
    /// Whether an approval accepted the group's code (issue rows)
    pub accepted: Option<bool>,
    /// When the row was last written
    pub updated: Option<DateTime<Utc>>,
}
