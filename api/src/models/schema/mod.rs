//! Thorium's Scylla schema, declared with derives
//!
//! Each table and view is a struct deriving [`ScyllaTable`] or [`ScyllaView`], each row a query
//! reads is a struct deriving [`ScyllaProjection`], and `#[thorium::keyspace]` collects every
//! object in creation order. This module holds the types and traits the generated code builds
//! definitions from and calls into.

mod apply;
mod baseline;
mod changes;
mod check;
mod checks;
mod classes;
mod config_record;
mod cql_type;
mod cursors;
mod definition;
mod elastic;
#[cfg(feature = "upgrades-runtime")]
mod engine;
mod fingerprint;
#[cfg(feature = "upgrades-runtime")]
mod lease;
mod legacy;
mod markers;
mod migrations;
#[cfg(feature = "upgrades-runtime")]
mod pass_store;
mod probe;
pub mod queries;
mod queries_runtime;
#[cfg(feature = "upgrades-runtime")]
mod rebucket;
mod record;
mod registry;
#[cfg(feature = "upgrades-runtime")]
mod runner;
#[cfg(feature = "upgrades-runtime")]
mod scan;
pub mod scylla;
mod session;
mod snapshots;
pub mod statements;
mod unsigned;
#[cfg(feature = "upgrades-runtime")]
mod users_theme;
#[cfg(all(test, feature = "upgrades-runtime"))]
mod users_theme_tests;
mod validate;
mod verify_views;
mod views;

#[cfg(test)]
mod apply_tests;
#[cfg(test)]
mod changes_tests;
#[cfg(test)]
mod check_tests;
#[cfg(test)]
mod class_changes_tests;
#[cfg(test)]
mod classes_tests;
#[cfg(test)]
mod config_record_tests;
#[cfg(test)]
mod cursor_tests;
#[cfg(test)]
mod ddl_tests;
#[cfg(test)]
mod elastic_tests;
#[cfg(test)]
mod fingerprint_tests;
#[cfg(all(test, feature = "upgrades-runtime"))]
mod lease_tests;
#[cfg(test)]
mod legacy_tests;
#[cfg(test)]
mod queries_tests;
#[cfg(test)]
mod record_tests;
#[cfg(test)]
mod report_tests;
#[cfg(test)]
mod snapshot_tests;
#[cfg(test)]
mod state_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod validate_tests;

pub use crate::models::upgrades::{
    BackendSchemaStatus, DriftKind, Enumeration, LayoutDef, SchemaBackend,
};
pub use apply::{
    ApplyError, ApplyOptions, ApplyReport, ApplyTarget, ConfigChange, SeedRequest, SnapshotRun,
    apply, apply_accepting, apply_objects, apply_seeding, apply_snapshot, fingerprints,
    keyspace_cql, redact_trigger_key, render, table_cql, view_cql,
};
pub use baseline::BASELINE_REVISIONS;
pub use changes::{ChangeClass, ChangeKind, SchemaChange, ViewChange, rebuilds, views_read};
pub use check::{SchemaCheckError, SchemaCheckPass, check_startup, judge_applied};
pub use classes::{
    BackupCoverage, CLASSES, ClassEntry, ColumnCoverage, DataClass, FamilyClass, REDIS_FAMILIES,
    Store,
};
pub use config_record::{
    ConfigAccept, DATA_TYPES, FILES_PASSWORD, check_files_password_hash, default_password_warning,
    hash_files_password, setting_id, setting_path, verify_files_password,
};
pub use cql_type::{CqlDesc, CqlNative, CqlType, JsonText};
pub use cursors::{
    BucketLimitError, CursorFuture, CursorLimitClause, ExistsStatement, GroupedCursorStatements,
    Order, PULL_PRODUCT_LIMIT, PagedTimeCursorStatements, SimpleCursorStatements,
    TimeCursorStatements, bucket_limit,
};
pub use definition::{
    BindColumns, ClusterOrder, ColumnAt, ColumnDef, ColumnRole, ObjectDef, ProjectedAt, Projection,
    ScyllaObject, TableDef, TableStatements, TtlSource, ViewDef, with_keyspace,
};
pub use elastic::{ElasticIndexDef, REBUILD_MIGRATION, RebuildProgress, RebuildTarget, reingests};
#[cfg(feature = "upgrades-runtime")]
pub use engine::{
    CHECK_REASONS, CheckRun, GroupTotal, MAX_GROUPS_PER_WORKER, PassOutcome, PassSpec, RunOptions,
    SAMPLE_KEYS, VerifyRun, check_pass, issue_groups, record_key, run_pass, verify_pass,
    worker_name,
};
pub use fingerprint::{
    Fingerprint, RevisionSnapshot, SnapshotObject, TypeParseError, canonical_json, canonical_type,
};
#[cfg(feature = "upgrades-runtime")]
pub use lease::{LEASE_TTL_MS, LeaseError, WriterLease, lease_key};
pub use legacy::{
    COUNT_TIMEOUT, DowngradeOutcome, DowngradeTarget, DropPlan, DropRecord, Droppable, LegacyError,
    LegacyListing, LegacyRefusal, LegacySources, LegacyStatus, NotRegistered, PlannedDrop,
    classify, count_statement, declared_scylla, downgrades, drop_planned, drop_records,
    drop_statement, droppable, inspect, parse_id, plan as plan_drops, record_id,
    resolve as resolve_legacy, support_window,
};
pub use markers::{AppliedSchema, ConfigRecord, DeploymentRecord, RecordStatus};
pub use migrations::{
    AppliedReport, CatalogIdentity, CheckReport, CheckWait, FinishedStep, GroupTally, IssueReport,
    MAX_REPORT_BYTES, MAX_REPORT_NOTES, MAX_REPORT_SKIPPED, MAX_REPORT_STRING, MigrationRecordKind,
    MigrationRunStatus, ObjectClass, PassCheckpoint, PassCounts, ReasonSummary, ReportCounts,
    ReportNote, ReportSummary, SkippedItem, StatusReport,
};
#[cfg(feature = "upgrades-runtime")]
pub use pass_store::{
    GroupWrite, IssueRow, PassRow, PassStore, WORKER_REASON, WORKER_TRANSFORM, WorkerRow,
    backend_error, is_rejection,
};
pub use probe::{
    Acknowledgement, BackendDrift, DriftReport, FieldDrift, ObjectDrift, ObservedSchema,
};
pub use probe::{
    ColumnRow, IndexRow, LegacyObjects, ProbeError, SchemaRows, TypeRow, ViewRow, canonical_filter,
    canonical_replication, converged_revision, layout_objects, mapping_object, probe_scylla,
    recorded_layouts,
};
#[cfg(feature = "client")]
pub use probe::{diff as diff_live, elastic_client, probe_configured};
#[cfg(any(feature = "api", feature = "client"))]
pub use probe::{probe, probe_elastic};
pub use queries_runtime::{
    Bucket, BucketError, Census, Cursor, CursorLimit, DEFAULT_IN_CHUNK, DEFAULT_LIMIT_CAP, Delete,
    IN_PRODUCT_LIMIT, InKind, InList, Insert, LimitError, Limits, NotText, QueryError, Raw,
    Restriction, RowResult, STAGE_LOG_LIMIT_CAP, Select, Update, WithTtl, plan_in_chunks,
    run_chunks, run_chunks_any, text_of,
};
#[cfg(feature = "upgrades-runtime")]
pub use rebucket::{
    ASSOCIATIONS, CensusFamily, ENTITIES, FILES, REPOS, RESULTS, Rebucket, RebucketTable, TAGS,
    prepared_statements, scan_table,
};
pub use record::{
    FINALIZE_STEP, RECORD_REVISION_STEP, RESULT_PREFIX, RESULT_TTL_SECS, REVISION_ROW, RecordError,
    SEED_REVISION_STEP, append_history, catalog_identity, claim_consumed_code,
    copy_operator_entries, finished_steps, history_id, read_job_result, read_status,
    record_family_layout, record_revision, seed_from_state, write_job_result,
};
pub use registry::{Keyspace, RegistryError, SchemaDefinition, SchemaObject, validate};
#[cfg(feature = "upgrades-runtime")]
pub use runner::{
    Batch, MigrateError, MigrationRunner, Outcome, RunBackends, RunContext, RunLimits, RunMode,
    Slice,
};
#[cfg(feature = "upgrades-runtime")]
pub use scan::{ScyllaScan, range_count, ring_slices, token_ranges};
pub use session::{connect, session_builder};
pub use snapshots::{CatalogCheckError, SnapshotError, check_catalog};
pub use thorium_derive::{
    ScyllaProjection, ScyllaStoreAsStr, ScyllaStoreJson, ScyllaTable, ScyllaView,
};
pub use unsigned::{
    ConversionDirection, UnsignedError, UnsignedField, UnsignedRead, UnsignedTypeError, Width,
    widths,
};
#[cfg(feature = "upgrades-runtime")]
pub use users_theme::{
    USERS_THEME_REVERSE, USERS_THEME_UP, UsersTheme, verify_user,
    write_commands as users_theme_commands,
};
pub use validate::{Rule, StatementError, validate_statement, validate_statements};
pub use verify_views::{
    RangeDifference, RangeSelection, RangeSide, TOKEN_RANGES, VerifyOptions, VerifyReport,
    ViewCheck, ViewState, range_of, token_range, verify_views,
};
pub use views::{
    RecreateReport, ViewError, declared_views, recreate_statements, recreate_views,
    recreate_warning, resolve_views,
};

/// Re-exports and helpers for generated code only
#[doc(hidden)]
pub mod __private {
    pub use super::checks::{
        ColumnTypeError, Coverage, MissingColumn, NotAKeyColumn, NotAnInColumn, UnknownBindColumn,
        assert_census_columns, assert_cursor_clustering, assert_cursor_partition, assert_distinct,
        assert_group_by, assert_insert_columns, assert_newest_first, assert_projection_in_any,
        assert_redacted, assert_restrictions, assert_selects_from, assert_selects_redacted,
        assert_set_columns, assert_tie_orders, assert_unique_objects, assert_view_column,
        assert_view_key, column_index, column_type_error, fail, field_index, find_column, in_kind,
        is_redacted, key_order, missing_column, missing_value, not_a_key_column, not_an_in_column,
        str_eq, unknown_bind_column,
    };
    pub use super::checks::{ColumnValueError, column_value_error};
    pub use super::cursors::{
        BindOwned, bucket_ints, cmp_column, cmp_consumed, cursor_cql, cursor_order, declared_order,
        order_by, resume_after, tie_from, tie_order, tie_predicates,
    };
    pub use super::queries_runtime::{
        BindList, BindValue, any_row, chain_binds, delete_cql, first_row, in_key_order, insert_cql,
        rows, select_cql, token_range_cql, update_cql,
    };
    pub use crate::models::scylla_utils::errors::DeserializationError;
    pub use chrono;
    pub use scylla;
    pub use serde_json;
}
