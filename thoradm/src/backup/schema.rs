//! The backup schema record: every row of both migration-state tables, written beside a backup's
//! archives as `schema.json`
//!
//! The record carries what a restore needs to bring the deployment's schema version and upgrade
//! position back: the applied fingerprints and layout versions, the revision and the full applied
//! history, the data-shaping settings, and each migration pass's rows. A `job_result` row isn't
//! carried: it expires after seven days, which a restore would turn into a permanent row. The
//! record is JSON rather than an rkyv archive so it can gain fields with defaults without
//! making older backups unreadable.

use chrono::{DateTime, Utc};
use futures::StreamExt;
use scylla::client::session::Session;
use scylla::{DeserializeRow, SerializeRow};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use thorium::models::schema::scylla::{
    SchemaMigrationIssueRow, SchemaMigrationIssuesTable, SchemaMigrationRow, SchemaMigrationsTable,
};
use thorium::models::schema::{
    FILES_PASSWORD, MigrationRecordKind, Projection, ScyllaObject, redact_trigger_key,
};
use tracing::warn;

use super::manifest::write_json_atomic;
use super::sink::SinkFactory;
use crate::Error;

/// The name of the file the record is written to, at the backup root
pub const SCHEMA_FILE: &str = "schema.json";

/// The version of the record's layout; a field added with a default doesn't change it
pub const RECORD_FORMAT: u32 = 1;

/// The id of the revision row and of the deployment's backend row
const DEPLOYMENT: &str = "deployment";

/// The rows a backup carries from both migration-state tables, written as `schema.json` beside the
/// archives and restored in full; a restore merges `consumed_code` rows instead of replacing them
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupSchemaRecord {
    /// The version of this record's layout, so later fields can be added with defaults
    pub format: u32,
    /// The thoradm version that wrote the backup, which a restore across revisions uses
    #[serde(default)]
    pub thoradm_version: Option<String>,
    /// Every `schema_migrations` row of a kind this thoradm knows, except `job_result` rows
    pub migrations: Vec<SchemaMigrationsTable>,
    /// Every `schema_migration_issues` row
    #[serde(default)]
    pub issues: Vec<SchemaMigrationIssuesTable>,
    /// Every `schema_migrations` row of a kind this thoradm has no variant for, such as a later
    /// release's audit rows, carried as the cells the table holds so a restore can write it back
    #[serde(default)]
    pub other: Vec<RawRow>,
}

/// A `schema_migrations` row of any kind, read and written cell by cell with no knowledge of what
/// its kind means
///
/// Its columns are the table's, with the kind as text and the JSON cells as the text they are
/// stored as, so a row of a kind added by a later release is carried unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, DeserializeRow, SerializeRow)]
pub struct RawRow {
    /// The row's kind, as the table stores it
    pub kind: String,
    /// The row's id within its kind
    pub id: String,
    /// The applied fingerprint
    pub fingerprint: Option<String>,
    /// The schema-apply trigger key
    pub apply_key: Option<String>,
    /// The layout version of each Redis family or S3 bucket
    pub layouts: Option<BTreeMap<String, i64>>,
    /// The revision of a pass row or the revision row
    pub revision: Option<String>,
    /// The Thorium version that recorded the revision
    pub version: Option<String>,
    /// An applied-history entry, as the JSON text stored
    pub entry: Option<String>,
    /// A Job's result, as the JSON text stored
    pub result: Option<String>,
    /// The recorded value of a data-shaping setting
    pub value: Option<String>,
    /// The pass's status, as stored
    pub status: Option<String>,
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
    /// A pass's counts, as the JSON text stored
    #[serde(default)]
    pub counts: Option<String>,
    /// A pass's applied report, as the JSON text stored
    #[serde(default)]
    pub report: Option<String>,
    /// When this row was last written
    pub updated: Option<DateTime<Utc>>,
}

/// The columns of `schema_migrations`, which [`RawRow`] reads by name
const RAW_COLUMNS: &str = "kind, id, fingerprint, apply_key, layouts, revision, version, entry, \
                           result, value, status, attempt, lease, started, finished, job, backup, \
                           counts, report, updated";

/// Builds a `schema_migrations` row from the projection a full scan reads it as
///
/// Every field of the table is named, so a column added to the table fails the build here until
/// the record carries it.
///
/// # Arguments
///
/// * `row` - The row as a scan reads it
#[must_use]
pub fn migrations_row(row: SchemaMigrationRow) -> SchemaMigrationsTable {
    // a cell the row left unset stays unset; the collection and timestamp columns have no unset
    SchemaMigrationsTable {
        kind: row.kind,
        id: row.id,
        fingerprint: row.fingerprint,
        apply_key: row.apply_key,
        layouts: row.layouts.unwrap_or_default(),
        revision: row.revision,
        version: row.version,
        entry: row.entry,
        result: row.result,
        value: row.value,
        status: row.status,
        attempt: row.attempt,
        lease: row.lease,
        started: row.started,
        finished: row.finished,
        job: row.job,
        backup: row.backup,
        counts: row.counts,
        report: row.report,
        updated: row.updated.unwrap_or(DateTime::<Utc>::UNIX_EPOCH),
    }
}

/// Builds a `schema_migration_issues` row from the projection a full scan reads it as
///
/// Every field of the table is named, so a column added to the table fails the build here until
/// the record carries it.
///
/// # Arguments
///
/// * `row` - The row as a scan reads it
#[must_use]
pub fn issue_row(row: SchemaMigrationIssueRow) -> SchemaMigrationIssuesTable {
    // a cell the row left unset stays unset; the counters and timestamps have no unset
    SchemaMigrationIssuesTable {
        migration: row.migration,
        transform: row.transform,
        reason: row.reason,
        worker: row.worker,
        attempt: row.attempt.unwrap_or_default(),
        checkpoint: row.checkpoint,
        lease: row.lease,
        class: row.class,
        count: row.count.unwrap_or_default(),
        first_seen: row.first_seen,
        last_seen: row.last_seen,
        samples: row.samples.unwrap_or_default(),
        resolved_count: row.resolved_count.unwrap_or_default(),
        accepted: row.accepted.unwrap_or_default(),
        updated: row.updated.unwrap_or(DateTime::<Utc>::UNIX_EPOCH),
    }
}

/// Whether a keyspace holds a table
///
/// # Arguments
///
/// * `scylla` - The session to ask
/// * `keyspace` - The keyspace
/// * `table` - The table
pub(super) async fn has_table(
    scylla: &Session,
    keyspace: &str,
    table: &str,
) -> Result<bool, Error> {
    // the schema tables list every table of the keyspace
    let rows = scylla
        .query_unpaged(
            "SELECT table_name FROM system_schema.tables WHERE keyspace_name = ? AND table_name = ?",
            (keyspace.to_lowercase(), table),
        )
        .await?
        .into_rows_result()
        .map_err(|error| Error::new(format!("failed to look for {keyspace}.{table}: {error}")))?;
    Ok(rows.rows_num() > 0)
}

/// Reads every row of the pass table, when the keyspace holds it
///
/// # Arguments
///
/// * `scylla` - The session to read with
/// * `namespace` - The keyspace
async fn read_issues(
    scylla: &Session,
    namespace: &str,
) -> Result<Vec<SchemaMigrationIssuesTable>, Error> {
    let issues_table = <SchemaMigrationIssuesTable as ScyllaObject>::NAME;
    let mut issues = Vec::new();
    // a keyspace without the table has no rows
    if has_table(scylla, namespace, issues_table).await? {
        let columns = <SchemaMigrationIssueRow as Projection>::COLUMNS.join(", ");
        let cql = format!("SELECT {columns} FROM {namespace}.{issues_table}");
        let mut rows = scylla
            .query_iter(cql, ())
            .await?
            .rows_stream::<SchemaMigrationIssueRow>()
            .map_err(|error| Error::new(format!("failed to read {issues_table}: {error}")))?;
        while let Some(row) = rows.next().await {
            let row = row.map_err(|error| {
                Error::new(format!("failed to read a {issues_table} row: {error}"))
            })?;
            issues.push(issue_row(row));
        }
    }
    Ok(issues)
}

impl BackupSchemaRecord {
    /// Reads every row of both migration-state tables, leaving out the `job_result` rows
    ///
    /// # Arguments
    ///
    /// * `scylla` - The session to read with
    /// * `namespace` - The namespace, which is the keyspace of the tables
    ///
    /// # Returns
    ///
    /// The record, or `None` when the keyspace holds no `schema_migrations` table: a deployment
    /// that never recorded its schema has nothing to back up
    pub async fn read(scylla: &Session, namespace: &str) -> Result<Option<Self>, Error> {
        // a keyspace without the record table has no record
        let migrations_table = <SchemaMigrationsTable as ScyllaObject>::NAME;
        if !has_table(scylla, namespace, migrations_table).await? {
            return Ok(None);
        }
        // the kinds the table holds, each one a partition, so a kind this thoradm has no variant
        // for can be read apart from the ones it has
        let columns = <SchemaMigrationRow as Projection>::COLUMNS.join(", ");
        let mut kinds = Vec::new();
        let mut stream = scylla
            .query_iter(
                format!("SELECT DISTINCT kind FROM {namespace}.{migrations_table}"),
                (),
            )
            .await?
            .rows_stream::<(String,)>()
            .map_err(|error| Error::new(format!("failed to read {migrations_table}: {error}")))?;
        while let Some(row) = stream.next().await {
            let (kind,) = row.map_err(|error| {
                Error::new(format!("failed to read a {migrations_table} kind: {error}"))
            })?;
            kinds.push(kind);
        }
        let mut migrations = Vec::new();
        let mut other = Vec::new();
        for kind in kinds {
            match kind.parse::<MigrationRecordKind>() {
                // a Job's result expires, so it isn't carried
                Ok(MigrationRecordKind::JobResult) => (),
                // every row of a kind this thoradm knows, as the projection its readers use
                Ok(_) => {
                    let cql = format!(
                        "SELECT {columns} FROM {namespace}.{migrations_table} WHERE kind = ?"
                    );
                    let mut rows = scylla
                        .query_iter(cql, (kind.as_str(),))
                        .await?
                        .rows_stream::<SchemaMigrationRow>()
                        .map_err(|error| {
                            Error::new(format!("failed to read {migrations_table}: {error}"))
                        })?;
                    while let Some(row) = rows.next().await {
                        let row = row.map_err(|error| {
                            Error::new(format!("failed to read a {migrations_table} row: {error}"))
                        })?;
                        let mut row = migrations_row(row);
                        // an earlier writer's key may still hold the password's unsalted hash
                        row.apply_key = row.apply_key.map(|key| redact_trigger_key(&key));
                        migrations.push(row);
                    }
                }
                // a kind a later release added is carried raw, so what it records isn't lost
                Err(_) => {
                    warn!(
                        "{migrations_table} holds rows of kind {kind}, which this thoradm doesn't \
                         know; carrying them in {SCHEMA_FILE} as they are"
                    );
                    let cql = format!(
                        "SELECT {RAW_COLUMNS} FROM {namespace}.{migrations_table} WHERE kind = ?"
                    );
                    let mut rows = scylla
                        .query_iter(cql, (kind.as_str(),))
                        .await?
                        .rows_stream::<RawRow>()
                        .map_err(|error| {
                            Error::new(format!("failed to read {migrations_table}: {error}"))
                        })?;
                    while let Some(row) = rows.next().await {
                        let mut row = row.map_err(|error| {
                            Error::new(format!("failed to read a {migrations_table} row: {error}"))
                        })?;
                        row.apply_key = row.apply_key.map(|key| redact_trigger_key(&key));
                        other.push(row);
                    }
                }
            }
        }
        other.sort_by(|left, right| (&left.kind, &left.id).cmp(&(&right.kind, &right.id)));
        // every row of the pass table, when the keyspace holds it
        let mut issues = read_issues(scylla, namespace).await?;
        // order the rows by key, so two reads of one state write the same file
        migrations.sort_by(|left, right| {
            (left.kind.as_str(), &left.id).cmp(&(right.kind.as_str(), &right.id))
        });
        issues.sort_by(|left, right| {
            (&left.migration, &left.transform, &left.reason, &left.worker).cmp(&(
                &right.migration,
                &right.transform,
                &right.reason,
                &right.worker,
            ))
        });
        Ok(Some(BackupSchemaRecord {
            format: RECORD_FORMAT,
            thoradm_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            migrations,
            issues,
            other,
        }))
    }

    /// Writes the record into a backup root as `schema.json`, atomically
    ///
    /// # Arguments
    ///
    /// * `root` - The backup root
    /// * `sinks` - The file-system seam to write through
    pub async fn write(&self, root: &Path, sinks: &dyn SinkFactory) -> Result<(), Error> {
        // the file holds the settings' password hash, so it is written as private as the rest
        write_json_atomic(root, SCHEMA_FILE, self, sinks).await
    }

    /// Loads the record a backup holds, if it holds one
    ///
    /// # Arguments
    ///
    /// * `root` - The backup root
    pub async fn load(root: &Path) -> Result<Option<Self>, Error> {
        // a backup taken before the record existed has no file
        let path = root.join(SCHEMA_FILE);
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(Error::new(format!(
                    "failed to read {}: {error}",
                    path.display()
                )));
            }
        };
        // a record of a newer layout may hold fields this thoradm would drop
        let record: BackupSchemaRecord = serde_json::from_slice(&bytes).map_err(|error| {
            Error::new(format!("{} isn't a schema record: {error}", path.display()))
        })?;
        if record.format > RECORD_FORMAT {
            return Err(Error::new(format!(
                "{} is schema record format {}, which this thoradm (up to format {RECORD_FORMAT}) \
                 doesn't read; restore it with the thoradm version that took the backup",
                path.display(),
                record.format
            )));
        }
        Ok(Some(record))
    }

    /// Finds a row of the record
    ///
    /// # Arguments
    ///
    /// * `kind` - The row's kind
    /// * `id` - The row's id
    #[must_use]
    pub fn row(&self, kind: MigrationRecordKind, id: &str) -> Option<&SchemaMigrationsTable> {
        // the record holds at most one row of a kind and id
        self.migrations
            .iter()
            .find(|row| row.kind == kind && row.id == id)
    }

    /// The revision the backed-up deployment was at, from its revision row
    #[must_use]
    pub fn revision(&self) -> Option<&str> {
        // the revision row's revision column
        self.row(MigrationRecordKind::Revision, DEPLOYMENT)
            .and_then(|row| row.revision.as_deref())
    }

    /// The deployment fingerprint the backed-up deployment had applied
    #[must_use]
    pub fn deployment_fingerprint(&self) -> Option<&str> {
        // the deployment's backend row's fingerprint
        self.row(MigrationRecordKind::Backend, DEPLOYMENT)
            .and_then(|row| row.fingerprint.as_deref())
    }

    /// The files password's identity the backed-up deployment recorded, as its argon2id string
    #[must_use]
    pub fn files_password(&self) -> Option<&str> {
        // the config row of the password setting
        self.row(MigrationRecordKind::Config, FILES_PASSWORD)
            .and_then(|row| row.value.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::{BackupSchemaRecord, RECORD_FORMAT, SCHEMA_FILE, issue_row, migrations_row};
    use serde_json::json;
    use thorium::models::schema::scylla::{SchemaMigrationIssueRow, SchemaMigrationRow};
    use thorium::models::schema::{MigrationRecordKind, ObjectClass};

    /// Builds a record from JSON, with its rows' every column as the tables hold them
    ///
    /// # Arguments
    ///
    /// * `rows` - The `schema_migrations` rows
    fn record(rows: &serde_json::Value) -> BackupSchemaRecord {
        // a record of the current format
        serde_json::from_value(json!({"format": RECORD_FORMAT, "migrations": rows}))
            .expect("a record parses")
    }

    /// The name serde gives a record kind, which is the variant's name
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind as the table stores it
    fn serde_kind(kind: &str) -> String {
        // each stored word starts a capital
        kind.split('_')
            .map(|word| {
                let mut chars = word.chars();
                chars.next().map_or_else(String::new, |first| {
                    first.to_uppercase().chain(chars).collect()
                })
            })
            .collect()
    }

    /// One `schema_migrations` row as JSON
    ///
    /// # Arguments
    ///
    /// * `kind` - The row's kind
    /// * `id` - The row's id
    /// * `cells` - The cells to set, as a JSON object
    fn row(kind: &str, id: &str, cells: &serde_json::Value) -> serde_json::Value {
        // every column of the table, unset unless a cell sets it
        let mut row = json!({
            "kind": serde_kind(kind), "id": id, "fingerprint": null, "apply_key": null, "layouts": {},
            "revision": null, "version": null, "entry": null, "result": null, "value": null,
            "status": null, "attempt": null, "lease": null, "started": null, "finished": null,
            "job": null, "backup": null, "updated": "2026-10-01T00:01:00Z",
        });
        for (name, value) in cells.as_object().unwrap() {
            row[name] = value.clone();
        }
        row
    }

    /// A record needs only its format and its rows; every other field defaults
    #[test]
    fn a_record_with_only_the_required_fields_loads() {
        // no thoradm version and no issues, as a record of an earlier writer holds
        let loaded: BackupSchemaRecord =
            serde_json::from_value(json!({"format": 1, "migrations": []})).unwrap();
        assert_eq!(loaded.thoradm_version, None);
        assert!(loaded.issues.is_empty() && loaded.migrations.is_empty());
        // a field a later writer adds is ignored, so the record can grow
        let grown: BackupSchemaRecord =
            serde_json::from_value(json!({"format": 1, "migrations": [], "later": [1, 2]}))
                .unwrap();
        assert_eq!(grown, loaded);
    }

    /// The record's accessors read the revision, the fingerprint, and the files password from
    /// their rows, and none from a record that lacks them
    #[test]
    fn accessors_read_their_rows() {
        // the revision row, the deployment's backend row, and the password's config row
        let full = record(&json!([
            row(
                "revision",
                "deployment",
                &json!({"revision": "2026-10-v02"})
            ),
            row("backend", "deployment", &json!({"fingerprint": "fp"})),
            row("backend", "scylla", &json!({"fingerprint": "other"})),
            row(
                "config",
                "files.password",
                &json!({"value": "$argon2id$..."})
            ),
            row("config", "partition_size.files", &json!({"value": "10000"})),
        ]));
        assert_eq!(full.revision(), Some("2026-10-v02"));
        assert_eq!(full.deployment_fingerprint(), Some("fp"));
        assert_eq!(full.files_password(), Some("$argon2id$..."));
        assert!(full.row(MigrationRecordKind::Backend, "scylla").is_some());
        assert!(full.row(MigrationRecordKind::Backend, "elastic").is_none());
        // an empty record answers none
        let empty = record(&json!([]));
        assert_eq!(
            (
                empty.revision(),
                empty.deployment_fingerprint(),
                empty.files_password()
            ),
            (None, None, None)
        );
    }

    /// A projection read converts to the table's row with every cell it set, and an unset
    /// collection or counter becomes the empty one
    #[test]
    fn scanned_rows_convert_to_table_rows() {
        // a row with only its key and kind set
        let key_only = SchemaMigrationRow {
            kind: MigrationRecordKind::Revision,
            id: "deployment".to_owned(),
            fingerprint: None,
            apply_key: None,
            layouts: None,
            revision: Some("2026-10-v01".to_owned()),
            version: None,
            entry: None,
            result: None,
            value: None,
            status: None,
            attempt: None,
            lease: None,
            started: None,
            finished: None,
            job: None,
            backup: None,
            counts: None,
            report: None,
            updated: None,
        };
        let table = migrations_row(key_only);
        assert_eq!(table.revision.as_deref(), Some("2026-10-v01"));
        assert!(table.layouts.is_empty());
        // an issue row's counters and samples default the same way
        let issue = issue_row(SchemaMigrationIssueRow {
            migration: "pass".to_owned(),
            transform: "step".to_owned(),
            reason: "unrecognized:field".to_owned(),
            worker: "w00".to_owned(),
            attempt: None,
            checkpoint: None,
            lease: None,
            class: Some(ObjectClass::Unrecognized),
            count: Some(4),
            first_seen: None,
            last_seen: None,
            samples: None,
            resolved_count: None,
            accepted: Some(true),
            updated: None,
        });
        assert_eq!(
            (issue.count, issue.resolved_count, issue.accepted),
            (4, 0, true)
        );
        assert!(issue.samples.is_empty() && issue.attempt.is_empty());
    }

    /// A record round trips through JSON, `layouts` and entries included
    #[test]
    fn a_record_round_trips_through_json() {
        // a history row holding an entry, and a backend row holding layouts
        let original = record(&json!([
            row(
                "backend",
                "redis",
                &json!({"fingerprint": "fp", "layouts": {"users": 2}})
            ),
            row(
                "history",
                "2026-10-01T00:00:00.000000000Z/2026-10-v02/step/done",
                &json!({"entry": {"revision": "2026-10-v02", "step": "step", "outcome": "Done"}}),
            ),
        ]));
        let text = serde_json::to_string(&original).unwrap();
        let again: BackupSchemaRecord = serde_json::from_str(&text).unwrap();
        assert_eq!(again, original);
        assert_eq!(again.migrations[0].layouts.get("users"), Some(&2));
    }

    /// Loading a backup root reads `schema.json`, finds none in a backup that has none, and
    /// refuses a record of a newer format, naming the thoradm that can read it
    #[tokio::test]
    async fn loading_reads_the_file_and_refuses_a_newer_format() {
        // a scratch backup root
        let root = std::env::temp_dir().join(format!("thoradm-record-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        // no file, no record
        assert_eq!(BackupSchemaRecord::load(&root).await.unwrap(), None);
        // a record of this format loads
        let text = json!({"format": RECORD_FORMAT, "migrations": []}).to_string();
        std::fs::write(root.join(SCHEMA_FILE), &text).unwrap();
        assert!(BackupSchemaRecord::load(&root).await.unwrap().is_some());
        // a newer format is refused with a reason that names the format
        let newer = json!({"format": RECORD_FORMAT + 1, "migrations": []}).to_string();
        std::fs::write(root.join(SCHEMA_FILE), newer).unwrap();
        let refused = BackupSchemaRecord::load(&root)
            .await
            .unwrap_err()
            .to_string();
        // text that isn't a record is an error too
        std::fs::write(root.join(SCHEMA_FILE), "not json").unwrap();
        let garbled = BackupSchemaRecord::load(&root)
            .await
            .unwrap_err()
            .to_string();
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            refused.contains("format 2") && refused.contains("thoradm version"),
            "{refused}"
        );
        assert!(garbled.contains("isn't a schema record"), "{garbled}");
    }
    /// Rows of a kind this thoradm has no variant for round trip through the record as raw cells,
    /// and an older record without any loads with none
    #[test]
    fn rows_of_unknown_kinds_round_trip_raw() {
        // a record carrying a later release's audit row and a typed revision row
        let text = json!({
            "format": 1, "migrations": [row("revision", "deployment", &json!({"revision": "2026-10-v02"}))],
            "other": [{
                "kind": "legacy_drop", "id": "2026-10-02T00:00:00Z/scylla:events", "fingerprint": null,
                "apply_key": null, "layouts": {"users": 2}, "revision": null, "version": null,
                "entry": "{\"step\":\"drop\"}", "result": null, "value": "42", "status": null,
                "attempt": null, "lease": null, "started": null, "finished": null, "job": null,
                "backup": null, "updated": "2026-10-01T00:01:00Z",
            }],
        });
        let loaded: BackupSchemaRecord = serde_json::from_value(text).unwrap();
        assert_eq!(loaded.other.len(), 1);
        assert_eq!(loaded.other[0].kind, "legacy_drop");
        assert_eq!(
            loaded.other[0].layouts.as_ref().unwrap().get("users"),
            Some(&2)
        );
        // writing and reading it again changes nothing
        let again: BackupSchemaRecord =
            serde_json::from_str(&serde_json::to_string(&loaded).unwrap()).unwrap();
        assert_eq!(again, loaded);
        // a record written before the list existed has none
        let older: BackupSchemaRecord =
            serde_json::from_value(json!({"format": 1, "migrations": []})).unwrap();
        assert_eq!(older.other, Vec::new());
    }
}
