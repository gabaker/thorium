//! Thorium's Scylla schema, declared with derives
//!
//! Each table and view is a struct deriving [`ScyllaTable`] or [`ScyllaView`], each row a query
//! reads is a struct deriving [`ScyllaProjection`], and `#[thorium::keyspace]` collects every
//! object in creation order. This module holds the types and traits the generated code builds
//! definitions from and calls into.

mod checks;
mod cql_type;
mod cursors;
mod definition;
pub mod queries;
mod queries_runtime;
mod registry;
pub mod scylla;
mod session;
pub mod statements;
mod unsigned;
mod validate;

#[cfg(test)]
mod cursor_tests;
#[cfg(test)]
mod ddl_tests;
#[cfg(test)]
mod queries_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod validate_tests;

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
pub use queries_runtime::{
    Bucket, BucketError, Census, Cursor, CursorLimit, DEFAULT_IN_CHUNK, DEFAULT_LIMIT_CAP, Delete,
    IN_PRODUCT_LIMIT, InKind, InList, Insert, LimitError, Limits, NotText, QueryError, Raw,
    Restriction, RowResult, STAGE_LOG_LIMIT_CAP, Select, Update, WithTtl, plan_in_chunks,
    run_chunks, run_chunks_any, text_of,
};
pub use registry::{Keyspace, RegistryError, validate};
pub use session::{connect, session_builder};
pub use thorium_derive::{
    ScyllaProjection, ScyllaStoreAsStr, ScyllaStoreJson, ScyllaTable, ScyllaView,
};
pub use unsigned::{
    ConversionDirection, UnsignedError, UnsignedField, UnsignedRead, UnsignedTypeError, Width,
    widths,
};
pub use validate::{Rule, StatementError, validate_statement, validate_statements};

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
