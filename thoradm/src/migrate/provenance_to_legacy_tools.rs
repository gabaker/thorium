//! The `provenance-to-legacy-tools` migration
//!
//! Entities of the three kinds that remember which tool produced them (`FileSystem`,
//! `DecompiledFunction`, and `WindowsProcessTree`) stored free-form tool names in `tools` before
//! tool instances existed. The API reads every stored string that isn't a tool instance id
//! into `legacy_tools`, so this migration rewrites each such row's `kind_data` through the same
//! reading and writes it back: the names move, every other column and every other row stays as it
//! was. The conversion and its classification are pure functions of the stored text in
//! `thorium::client::migrations`; this runner reads the `entities` table in token order, hands each
//! row to them, and updates the one column that changed. A row that is already moved classifies
//! as current, so a rerun finds nothing to do and an interrupted run resumes from its checkpoints.

use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use scylla::value::{CqlTimestamp, CqlValue};
use thorium::client::migrations::{
    MigrationCategory, MigrationReason, ProvenanceClass, ProvenanceRow, classify, up,
};
use thorium::models::schema::{
    Batch, IssueReport, MigrateError, MigrationRunner, ObjectClass, Outcome, RunContext, RunMode,
    ScyllaScan, Slice, backend_error, is_rejection,
};
use thorium::models::upgrades::{IssueCategory, IssueReason, Record};

use super::registry::{Backend, MigrationDef, Support};

/// The step id issues of this migration are recorded under
const TRANSFORM: &str = "provenance-to-legacy-tools";

/// The migration's registration: the `entities` rows of the provenance kinds
pub const PROVENANCE_TO_LEGACY_TOOLS: MigrationDef = MigrationDef {
    id: TRANSFORM,
    backend: Backend::Scylla,
    // checked only until the release's catalog step runs it; flips to `Support::Yes` with that step
    support: Support::CheckOnly,
    runner: &ProvenanceToLegacyTools,
    pass: TRANSFORM,
    chain: &[TRANSFORM],
    group_max: 5,
    families: &["entities"],
    has_views: true,
};

/// The runner that moves stored tool names of provenance rows into `legacy_tools`
pub struct ProvenanceToLegacyTools;

/// The columns that make up the primary key of an `entities` row, partition first
const KEY_COLUMNS: [&str; 6] = ["kind", "group", "year", "bucket", "created", "id"];

/// The columns a scan reads: the key and the one column that changes
const READ_COLUMNS: [&str; 7] = [
    "kind",
    "group",
    "year",
    "bucket",
    "created",
    "id",
    "kind_data",
];

/// Build the scan of the entities table
///
/// # Arguments
///
/// * `ctx` - The run's context, for the keyspace
fn scan(ctx: &RunContext) -> ScyllaScan {
    // the partition is the kind, group, year, and bucket; the rest of the key clusters
    ScyllaScan {
        keyspace: ctx.backends.namespace.clone(),
        table: "entities".to_owned(),
        partition_key: KEY_COLUMNS[..4]
            .iter()
            .map(|name| (*name).to_owned())
            .collect(),
        columns: READ_COLUMNS.iter().map(|name| (*name).to_owned()).collect(),
        key_columns: KEY_COLUMNS.iter().map(|name| (*name).to_owned()).collect(),
        cuts: Vec::new(),
    }
}

/// Map a conversion reason to the issue reason the runner records
///
/// # Arguments
///
/// * `reason` - The conversion's reason
fn issue_reason(reason: &MigrationReason) -> IssueReason {
    // each conversion category has one runner category
    let category = match reason.category {
        MigrationCategory::MissingField => IssueCategory::MissingField,
        MigrationCategory::InvalidValue => IssueCategory::InvalidValue,
        MigrationCategory::MalformedJson => IssueCategory::MalformedJson,
        MigrationCategory::WriteFailed => IssueCategory::WriteFailed,
    };
    IssueReason::new(category, reason.field.clone())
}

/// Render a column of a row key as text
///
/// # Arguments
///
/// * `value` - The column's value, if the row has one
fn key_part(value: Option<&Option<CqlValue>>) -> String {
    // a missing or null column leaves its place in the key empty
    match value {
        Some(Some(CqlValue::Text(text) | CqlValue::Ascii(text))) => text.clone(),
        Some(Some(CqlValue::Int(value))) => value.to_string(),
        Some(Some(CqlValue::BigInt(value))) => value.to_string(),
        Some(Some(CqlValue::SmallInt(value))) => value.to_string(),
        Some(Some(CqlValue::TinyInt(value))) => value.to_string(),
        Some(Some(CqlValue::Uuid(value))) => value.to_string(),
        // a creation time is RFC 3339 UTC with nanoseconds so it parses back to the same instant
        Some(Some(CqlValue::Timestamp(CqlTimestamp(millis)))) => {
            DateTime::<Utc>::from_timestamp_millis(*millis).map_or_else(String::new, |time| {
                time.to_rfc3339_opts(SecondsFormat::Nanos, true)
            })
        }
        _ => String::new(),
    }
}

/// Build the key of a stored row from its key columns
///
/// # Arguments
///
/// * `columns` - The row's columns
fn row_key(columns: &BTreeMap<String, Option<CqlValue>>) -> String {
    // join the key columns in primary key order
    KEY_COLUMNS
        .iter()
        .map(|name| key_part(columns.get(*name)))
        .collect::<Vec<_>>()
        .join("/")
}

/// Read the provenance view of a stored row
///
/// The `kind` column holds the kind as JSON text, so its quotes are removed.
///
/// # Arguments
///
/// * `key` - The row's key
/// * `record` - The stored row
fn provenance_row(key: &str, record: &Record) -> Result<ProvenanceRow, IssueReason> {
    // both columns must be there for the row to be read at all
    let kind = record
        .text("kind")
        .ok_or_else(|| IssueReason::new(IssueCategory::MissingField, "kind"))?;
    let kind_data = record
        .text("kind_data")
        .ok_or_else(|| IssueReason::new(IssueCategory::MissingField, "kind_data"))?;
    Ok(ProvenanceRow {
        key: key.to_owned(),
        kind: serde_json::from_str::<String>(&kind).unwrap_or(kind),
        kind_data,
    })
}

#[async_trait]
impl MigrationRunner for ProvenanceToLegacyTools {
    /// Split the entities table's token ring into `n` slices
    ///
    /// # Arguments
    ///
    /// * `n` - How many slices to make
    /// * `ctx` - The run's context
    async fn slices(&self, n: u32, ctx: &RunContext) -> Result<Vec<Slice>, MigrateError> {
        // the whole ring, as every Scylla migration splits it
        Ok(scan(ctx).slices(n))
    }

    /// Read the next batch of a slice
    ///
    /// # Arguments
    ///
    /// * `slice` - The slice to read
    /// * `after` - The last token done
    /// * `ctx` - The run's context
    async fn next_batch(
        &self,
        slice: &Slice,
        after: &serde_json::Value,
        ctx: &RunContext,
    ) -> Result<Batch, MigrateError> {
        // a batch ends on a partition boundary
        scan(ctx).next_batch(slice, after, ctx).await
    }

    /// Classify one row and, when it holds tool names, rewrite its kind data
    ///
    /// # Arguments
    ///
    /// * `record` - The stored row
    /// * `mode` - `Run` or `Check`; the runner writes only in `Run`
    /// * `ctx` - The run's context
    async fn process(
        &self,
        record: &Record,
        mode: RunMode,
        ctx: &RunContext,
    ) -> Result<Outcome, MigrateError> {
        // only a Scylla row can be classified
        let _ = (mode, ctx);
        let Record::Scylla { columns, .. } = record else {
            return Ok(Outcome::Current);
        };
        // key the row by its own key columns
        let key = row_key(columns);
        let issue = |reason: IssueReason| {
            Ok(Outcome::Issue(IssueReport {
                key: key.clone(),
                transform: TRANSFORM.to_owned(),
                class: ObjectClass::Unrecognized,
                reason,
            }))
        };
        // a row missing its kind or its data can't be read
        let row = match provenance_row(&key, record) {
            Ok(row) => row,
            Err(reason) => return issue(reason),
        };
        match classify(&row) {
            // another kind, or no names left in `tools`
            ProvenanceClass::Current => Ok(Outcome::Current),
            // the row holds names: rewrite its kind data the way the API reads it
            ProvenanceClass::Migratable { .. } => match up(row) {
                Ok(moved) => {
                    let mut columns = columns.clone();
                    columns.insert(
                        "kind_data".to_owned(),
                        Some(CqlValue::Text(moved.kind_data)),
                    );
                    Ok(Outcome::Write(Record::Scylla {
                        key: key.clone(),
                        columns,
                    }))
                }
                Err(reason) => issue(issue_reason(&reason)),
            },
            // the row doesn't parse: leave it as it is and say why
            ProvenanceClass::Unrecognized { reason } => issue(issue_reason(&reason)),
        }
    }

    /// Update the kind data of one row
    ///
    /// # Arguments
    ///
    /// * `record` - The rewritten row
    /// * `ctx` - The run's context
    async fn write(&self, record: &Record, ctx: &RunContext) -> Result<(), MigrateError> {
        let Record::Scylla { columns, .. } = record else {
            return Err(MigrateError::Systematic(
                "a provenance row isn't a Scylla row".to_owned(),
            ));
        };
        // one update by the row's full key; it changes no key column, so the view follows
        let filter = KEY_COLUMNS
            .iter()
            .map(|name| format!("{name} = ?"))
            .collect::<Vec<_>>()
            .join(" AND ");
        let mut values = vec![columns.get("kind_data").cloned().flatten()];
        values.extend(
            KEY_COLUMNS
                .iter()
                .map(|name| columns.get(*name).cloned().flatten()),
        );
        ctx.backends
            .scylla
            .query_unpaged(
                format!(
                    "UPDATE {}.entities SET kind_data = ? WHERE {filter}",
                    ctx.backends.namespace
                ),
                values,
            )
            .await
            .map_err(|err| {
                backend_error("rewrite an entity's provenance", &err, is_rejection(&err))
            })?;
        Ok(())
    }

    /// Finish the pass; the rows are the whole migration, so there is nothing left to do
    ///
    /// # Arguments
    ///
    /// * `ctx` - The run's context
    async fn finish(&self, ctx: &RunContext) -> Result<(), MigrateError> {
        // the runner records the pass; no census or marker changes with this migration
        let _ = ctx;
        Ok(())
    }

    /// Check that a stored row no longer holds tool names in `tools`
    ///
    /// # Arguments
    ///
    /// * `record` - The stored row
    /// * `ctx` - The run's context
    async fn verify(&self, record: &Record, ctx: &RunContext) -> Result<(), IssueReason> {
        // only a Scylla row has columns to check
        let _ = ctx;
        let Record::Scylla { columns, .. } = record else {
            return Ok(());
        };
        // key the row by its own key columns
        let row = provenance_row(&row_key(columns), record)?;
        match classify(&row) {
            ProvenanceClass::Current => Ok(()),
            // a name still in `tools` is a row the migration didn't reach
            ProvenanceClass::Migratable { .. } => {
                Err(IssueReason::new(IssueCategory::InvalidValue, "tools"))
            }
            ProvenanceClass::Unrecognized { reason } => Err(issue_reason(&reason)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::{DateTime, SecondsFormat, Utc};
    use scylla::value::{CqlTimestamp, CqlValue};
    use uuid::Uuid;

    use super::row_key;

    /// The columns of one stored row with the given creation time
    ///
    /// # Arguments
    ///
    /// * `millis` - The row's creation time in milliseconds since the epoch
    fn columns(millis: i64) -> BTreeMap<String, Option<CqlValue>> {
        // the key columns, partition first
        BTreeMap::from([
            (
                "kind".to_owned(),
                Some(CqlValue::Text("\"FileSystem\"".to_owned())),
            ),
            ("group".to_owned(), Some(CqlValue::Text("corn".to_owned()))),
            ("year".to_owned(), Some(CqlValue::Int(2026))),
            ("bucket".to_owned(), Some(CqlValue::Int(77))),
            (
                "created".to_owned(),
                Some(CqlValue::Timestamp(CqlTimestamp(millis))),
            ),
            (
                "id".to_owned(),
                Some(CqlValue::Uuid(
                    Uuid::parse_str("0b1f6c2e-3c0a-4a31-9a0b-5f0d3f6f7c11").unwrap(),
                )),
            ),
        ])
    }

    /// A row's creation time is keyed as RFC 3339 UTC with nanoseconds and a `Z`, never a debug
    /// form, and it reads back to the stored instant
    #[test]
    fn a_row_key_carries_its_creation_time_as_rfc_3339() {
        // a creation time with a sub-second part
        let millis = 1_792_000_000_123;
        let key = row_key(&columns(millis));
        // the key holds the six key columns in order
        let parts = key.split('/').collect::<Vec<_>>();
        assert_eq!(parts.len(), 6, "{key}");
        assert_eq!(
            &parts[..4],
            ["\"FileSystem\"", "corn", "2026", "77"],
            "{key}"
        );
        assert_eq!(parts[5], "0b1f6c2e-3c0a-4a31-9a0b-5f0d3f6f7c11", "{key}");
        // the creation time is RFC 3339 UTC, nine fractional digits, and a `Z` suffix
        let created = parts[4];
        assert!(created.ends_with('Z'), "{created}");
        let fraction = created.split('.').nth(1).expect("a fraction");
        assert_eq!(fraction.trim_end_matches('Z').len(), 9, "{created}");
        assert!(!created.contains("Timestamp"), "{created}");
        // it round-trips to the stored instant
        let parsed = DateTime::parse_from_rfc3339(created).expect("RFC 3339");
        assert_eq!(parsed.with_timezone(&Utc).timestamp_millis(), millis);
        let again = parsed
            .with_timezone(&Utc)
            .to_rfc3339_opts(SecondsFormat::Nanos, true);
        assert_eq!(again, created);
    }

    /// A row missing a key column still gets a key, with an empty part in its place
    #[test]
    fn a_row_key_leaves_a_missing_column_empty() {
        // no creation time at all
        let mut missing = columns(0);
        missing.insert("created".to_owned(), None);
        let key = row_key(&missing);
        assert_eq!(key.split('/').nth(4), Some(""), "{key}");
    }
}
