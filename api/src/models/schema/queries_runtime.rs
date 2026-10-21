//! Runtime support for the generated queries structs: statement markers, statement text, typed
//! binds, rows, and `IN` chunking

use chrono::{DateTime, NaiveDate, Utc};
use scylla::deserialize::row::DeserializeRow;
use scylla::deserialize::{DeserializationError, TypeCheckError};
use scylla::errors::{BadQuery, ExecutionError, PagerExecutionError};
use scylla::frame::response::result::{ColumnType, NativeType};
use scylla::response::query_result::{
    IntoRowsResultError, MaybeFirstRowError, QueryResult, RowsError,
};
use scylla::serialize::SerializationError;
use scylla::serialize::row::{RowSerializationContext, SerializeRow};
use scylla::serialize::value::SerializeValue;
use scylla::serialize::writers::{CellWriter, RowWriter, WrittenCellProof};
use std::future::Future;
use std::marker::PhantomData;
use std::num::NonZeroU32;
use std::ops::Range;

use super::checks::unknown_bind_column;
use super::definition::{BindColumns, ColumnAt, ObjectDef};

/// Marks a queries field as a full or subset `INSERT` (tables only)
pub struct Insert;

/// Marks a queries field as a `SELECT` returning rows of a projection
pub struct Select;

/// Marks a queries field as an `UPDATE` of named columns by key (tables only)
pub struct Update;

/// Marks a queries field as a `DELETE` by key, partition, or clustering prefix (tables only)
pub struct Delete;

/// Marks a queries field as a cursor's statements
pub struct Cursor;

/// Marks a queries field as a token-range `count(*)` grouped by the partition key
pub struct Census;

/// Marks a queries field as a hand-written statement, still prepared, registered, and tested
pub struct Raw;

/// The most combinations Scylla accepts from the `IN` lists on one kind of key column
pub const IN_PRODUCT_LIMIT: usize = 100;

/// The most values bound in one `IN` list by default
pub const DEFAULT_IN_CHUNK: usize = 100;

/// Which kind of key column an `IN` list restricts, since Scylla limits the cartesian product of
/// each kind separately
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InKind {
    /// A partition key column
    PartitionKey,
    /// A clustering column
    Clustering,
}

/// One `IN` list of a statement, in the order the statement binds them
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InList {
    /// How many values the list holds
    pub len: usize,
    /// The kind of key column it restricts
    pub kind: InKind,
    /// The most values bound per execution
    pub chunk: usize,
}

impl InList {
    /// An `IN` list on a partition key column
    ///
    /// # Arguments
    ///
    /// * `len` - How many values the list holds
    #[must_use]
    pub fn partition_key(len: usize) -> Self {
        InList {
            len,
            kind: InKind::PartitionKey,
            chunk: DEFAULT_IN_CHUNK,
        }
    }

    /// An `IN` list on a clustering column
    ///
    /// # Arguments
    ///
    /// * `len` - How many values the list holds
    #[must_use]
    pub fn clustering(len: usize) -> Self {
        InList {
            len,
            kind: InKind::Clustering,
            chunk: DEFAULT_IN_CHUNK,
        }
    }

    /// Set the most values bound per execution
    ///
    /// # Arguments
    ///
    /// * `chunk` - The most values bound per execution
    #[must_use]
    pub fn chunk(mut self, chunk: usize) -> Self {
        self.chunk = chunk;
        self
    }
}

/// Plan the executions of a statement with `IN` lists
///
/// Each execution binds one range of each list. A list's ranges are as long as its chunk size
/// allows while the product of the ranges on each kind of key column stays within
/// [`IN_PRODUCT_LIMIT`]; an empty list means nothing to run.
///
/// # Arguments
///
/// * `lists` - The statement's `IN` lists, in bind order
#[must_use]
pub fn plan_in_chunks(lists: &[InList]) -> Vec<Vec<Range<usize>>> {
    // an empty list matches nothing, so there is nothing to run
    if lists.iter().any(|list| list.len == 0) {
        return Vec::new();
    }
    // pick each list's range length within its chunk size and its kind's remaining product
    let mut products = [1_usize; 2];
    let mut lengths = Vec::with_capacity(lists.len());
    for list in lists {
        let product = &mut products[list.kind as usize];
        let room = (IN_PRODUCT_LIMIT / *product).max(1);
        let length = list.len.min(list.chunk.max(1)).min(room);
        *product *= length;
        lengths.push(length);
    }
    // split each list into ranges of its length
    let splits = lists.iter().zip(&lengths).map(|(list, length)| {
        (0..list.len)
            .step_by(*length)
            .map(|start| start..(start + length).min(list.len))
            .collect::<Vec<_>>()
    });
    // run every combination of one range from each list
    splits.fold(vec![Vec::new()], |plans, ranges| {
        plans
            .into_iter()
            .flat_map(|plan| {
                ranges.iter().map(move |range| {
                    let mut plan = plan.clone();
                    plan.push(range.clone());
                    plan
                })
            })
            .collect()
    })
}

/// Run every planned execution in order, concatenating their rows
///
/// # Arguments
///
/// * `plan` - The executions from [`plan_in_chunks`]
/// * `execute` - Runs one execution, given the range of each list it binds
pub async fn run_chunks<T, E, F, Fut>(
    plan: &[Vec<Range<usize>>],
    mut execute: F,
) -> Result<Vec<T>, E>
where
    F: FnMut(&[Range<usize>]) -> Fut,
    Fut: Future<Output = Result<Vec<T>, E>>,
{
    let mut rows = Vec::new();
    // run each execution in plan order, keeping its rows
    for ranges in plan {
        rows.extend(execute(ranges).await?);
    }
    Ok(rows)
}

/// Run planned executions in order until one finds a hit
///
/// # Arguments
///
/// * `plan` - The executions from [`plan_in_chunks`]
/// * `execute` - Runs one execution, given the range of each list it binds, returning whether
///   it found a hit
pub async fn run_chunks_any<E, F, Fut>(
    plan: &[Vec<Range<usize>>],
    mut execute: F,
) -> Result<bool, E>
where
    F: FnMut(&[Range<usize>]) -> Fut,
    Fut: Future<Output = Result<bool, E>>,
{
    for ranges in plan {
        if execute(ranges).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The error a generated statement's method returns: the driver error from running the
/// statement or reading its result
#[derive(Debug)]
pub enum QueryError {
    /// Running the statement failed
    Execution(Box<ExecutionError>),
    /// The result wasn't rows
    IntoRows(Box<IntoRowsResultError>),
    /// The rows didn't type check against the projection
    Rows(RowsError),
    /// The first row didn't type check or deserialize
    FirstRow(MaybeFirstRowError),
    /// Running a paged statement failed
    Pager(Box<PagerExecutionError>),
    /// A paged statement's rows didn't type check against the projection
    TypeCheck(TypeCheckError),
    /// A bucket didn't fit the column it's bound to
    Bucket(BucketError),
}

impl std::fmt::Display for QueryError {
    /// Show the driver's error
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueryError::Execution(error) => write!(f, "{error}"),
            QueryError::IntoRows(error) => write!(f, "{error}"),
            QueryError::Rows(error) => write!(f, "{error}"),
            QueryError::FirstRow(error) => write!(f, "{error}"),
            QueryError::Pager(error) => write!(f, "{error}"),
            QueryError::TypeCheck(error) => write!(f, "{error}"),
            QueryError::Bucket(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for QueryError {}

impl From<ExecutionError> for QueryError {
    /// Wrap an error from running a statement
    ///
    /// # Arguments
    ///
    /// * `error` - The driver's error
    fn from(error: ExecutionError) -> Self {
        QueryError::Execution(Box::new(error))
    }
}

impl From<SerializationError> for QueryError {
    /// Wrap an error from writing a bound value the way the driver wraps it when a statement's
    /// values fail to serialize
    ///
    /// # Arguments
    ///
    /// * `error` - The serialization error
    fn from(error: SerializationError) -> Self {
        QueryError::Execution(Box::new(ExecutionError::BadQuery(
            BadQuery::SerializationError(error),
        )))
    }
}

impl From<IntoRowsResultError> for QueryError {
    /// Wrap an error from reading a result as rows
    ///
    /// # Arguments
    ///
    /// * `error` - The driver's error
    fn from(error: IntoRowsResultError) -> Self {
        QueryError::IntoRows(Box::new(error))
    }
}

impl From<RowsError> for QueryError {
    /// Wrap an error from type checking rows
    ///
    /// # Arguments
    ///
    /// * `error` - The driver's error
    fn from(error: RowsError) -> Self {
        QueryError::Rows(error)
    }
}

impl From<MaybeFirstRowError> for QueryError {
    /// Wrap an error from reading the first row
    ///
    /// # Arguments
    ///
    /// * `error` - The driver's error
    fn from(error: MaybeFirstRowError) -> Self {
        QueryError::FirstRow(error)
    }
}

impl From<PagerExecutionError> for QueryError {
    /// Wrap an error from running a paged statement
    ///
    /// # Arguments
    ///
    /// * `error` - The driver's error
    fn from(error: PagerExecutionError) -> Self {
        QueryError::Pager(Box::new(error))
    }
}

impl From<TypeCheckError> for QueryError {
    /// Wrap an error from type checking a paged statement's rows
    ///
    /// # Arguments
    ///
    /// * `error` - The driver's error
    fn from(error: TypeCheckError) -> Self {
        QueryError::TypeCheck(error)
    }
}

impl From<BucketError> for QueryError {
    /// Wrap a bucket that didn't fit its column
    ///
    /// # Arguments
    ///
    /// * `error` - The bucket's error
    fn from(error: BucketError) -> Self {
        QueryError::Bucket(error)
    }
}

/// One row a generated select read, or the error deserializing it, so each caller decides
/// whether a bad row fails the request or is skipped
pub type RowResult<P> = Result<P, DeserializationError>;

/// How a statement restricts one key column
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restriction {
    /// `column = ?`
    Eq,
    /// `column IN ?`
    In,
    /// `column >= ?`
    Ge,
    /// `column > ?`
    Gt,
    /// `column <= ?`
    Le,
    /// `column < ?`
    Lt,
}

/// One value bound to a column, written the way the column's object writes it
pub struct BindValue<'v, 'b, O: ColumnAt<N>, const N: usize>(
    /// The value
    pub &'v O::Bind<'b>,
);

impl<O: ColumnAt<N>, const N: usize> SerializeValue for BindValue<'_, '_, O, N> {
    /// Write the value through its column's conversion
    ///
    /// # Arguments
    ///
    /// * `typ` - The bind marker's CQL type
    /// * `writer` - The writer for the value
    fn serialize<'w>(
        &self,
        typ: &ColumnType,
        writer: CellWriter<'w>,
    ) -> Result<WrittenCellProof<'w>, SerializationError> {
        O::serialize_bind(self.0, typ, writer)
    }
}

/// A list of values bound to a key column's `IN` restriction
pub struct BindList<'v, O: ColumnAt<N>, const N: usize>(
    /// The values
    pub &'v [O::Owned],
    /// Ties the list to its column
    pub PhantomData<O>,
);

impl<O: ColumnAt<N>, const N: usize> SerializeValue for BindList<'_, O, N> {
    /// Write the list through its column's conversion
    ///
    /// # Arguments
    ///
    /// * `typ` - The bind marker's CQL type
    /// * `writer` - The writer for the list
    fn serialize<'w>(
        &self,
        typ: &ColumnType,
        writer: CellWriter<'w>,
    ) -> Result<WrittenCellProof<'w>, SerializationError> {
        O::serialize_list(self.0, typ, writer)
    }
}

/// Read every row of a select's result as a projection
///
/// # Arguments
///
/// * `result` - The select's result
pub fn rows<P>(result: QueryResult) -> Result<Vec<RowResult<P>>, QueryError>
where
    P: for<'f, 'm> DeserializeRow<'f, 'm>,
{
    // type check the result once, then read each row
    let rows = result.into_rows_result()?;
    Ok(rows.rows::<P>()?.collect())
}

/// Read the first row of a select's result as a projection, if it returned one
///
/// # Arguments
///
/// * `result` - The select's result
pub fn first_row<P>(result: QueryResult) -> Result<Option<P>, QueryError>
where
    P: for<'f, 'm> DeserializeRow<'f, 'm>,
{
    Ok(result.into_rows_result()?.maybe_first_row::<P>()?)
}

/// Whether a select's result returned any row
///
/// # Arguments
///
/// * `result` - The select's result
pub fn any_row(result: QueryResult) -> Result<bool, QueryError> {
    Ok(result.into_rows_result()?.rows_num() > 0)
}

/// Render a statement's `WHERE` clause
///
/// # Arguments
///
/// * `restrictions` - The restricted columns in key order, with how each is restricted
fn where_clause(restrictions: &[(&str, Restriction)]) -> String {
    restrictions
        .iter()
        .map(|(column, restriction)| match restriction {
            Restriction::Eq => format!("{column} = ?"),
            Restriction::In => format!("{column} IN ?"),
            Restriction::Ge => format!("{column} >= ?"),
            Restriction::Gt => format!("{column} > ?"),
            Restriction::Le => format!("{column} <= ?"),
            Restriction::Lt => format!("{column} < ?"),
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// The limits a select can end with
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Limits {
    /// A literal `PER PARTITION LIMIT`
    pub per_partition: Option<u32>,
    /// Whether the `PER PARTITION LIMIT` is bound instead
    pub per_partition_bound: bool,
    /// A literal `LIMIT`
    pub limit: Option<u32>,
}

/// Render a select
///
/// # Arguments
///
/// * `ns` - The keyspace
/// * `object` - The table or view read
/// * `columns` - The selected columns
/// * `restrictions` - The restricted columns in key order, with how each is restricted
/// * `group_by` - The grouped columns, if any
/// * `limits` - The literal limits, if any
#[must_use]
pub fn select_cql(
    ns: &str,
    object: &str,
    columns: &[&str],
    restrictions: &[(&str, Restriction)],
    group_by: &[&str],
    limits: Limits,
) -> String {
    let mut clauses = vec![format!(
        "SELECT {} FROM {ns}.{object} WHERE {}",
        columns.join(", "),
        where_clause(restrictions)
    )];
    // add each trailing clause in the order CQL requires
    if !group_by.is_empty() {
        clauses.push(format!("GROUP BY {}", group_by.join(", ")));
    }
    if let Some(limit) = limits.per_partition {
        clauses.push(format!("PER PARTITION LIMIT {limit}"));
    } else if limits.per_partition_bound {
        clauses.push("PER PARTITION LIMIT ?".to_owned());
    }
    if let Some(limit) = limits.limit {
        clauses.push(format!("LIMIT {limit}"));
    }
    clauses.join(" ")
}

/// Render a select over one token range of an object's partition key, as scans, partition
/// enumerations, and census counts read; it binds the range's first and last token
///
/// # Arguments
///
/// * `ns` - The keyspace
/// * `object` - The table or view read
/// * `columns` - The selected columns
/// * `distinct` - Whether to select each partition once
/// * `group_by` - The grouped columns, if any
#[must_use]
pub fn token_range_cql(
    ns: &str,
    object: &ObjectDef,
    columns: &[&str],
    distinct: bool,
    group_by: &[&str],
) -> String {
    // the range restricts the token of the whole partition key
    let partition = object.partition_key().join(", ");
    let distinct = if distinct { "DISTINCT " } else { "" };
    let mut cql = format!(
        "SELECT {distinct}{} FROM {ns}.{} WHERE token({partition}) >= ? AND token({partition}) <= ?",
        columns.join(", "),
        object.name(),
    );
    // a census counts the rows of each group
    if !group_by.is_empty() {
        cql.push_str(" GROUP BY ");
        cql.push_str(&group_by.join(", "));
    }
    cql
}

/// Render a delete
///
/// # Arguments
///
/// * `ns` - The keyspace
/// * `table` - The table deleted from
/// * `restrictions` - The restricted columns in key order, with how each is restricted
#[must_use]
pub fn delete_cql(ns: &str, table: &str, restrictions: &[(&str, Restriction)]) -> String {
    format!(
        "DELETE FROM {ns}.{table} WHERE {}",
        where_clause(restrictions)
    )
}

/// Put a statement's values in the order its restrictions are rendered
///
/// # Arguments
///
/// * `order` - The declared index of each restriction, in key order
/// * `values` - The values, in declaration order
#[must_use]
pub fn in_key_order<const K: usize>(
    order: [usize; K],
    values: [&(dyn SerializeValue + Sync); K],
) -> [&(dyn SerializeValue + Sync); K] {
    order.map(|index| values[index])
}

/// Render an insert of some columns
///
/// # Arguments
///
/// * `ns` - The keyspace
/// * `table` - The table written to
/// * `columns` - The columns written, in bind order
#[must_use]
pub fn insert_cql(ns: &str, table: &str, columns: &[&str]) -> String {
    let binds = vec!["?"; columns.len()].join(", ");
    format!(
        "INSERT INTO {ns}.{table} ({}) VALUES ({binds})",
        columns.join(", ")
    )
}

/// Render an update of named columns by key
///
/// # Arguments
///
/// * `ns` - The keyspace
/// * `table` - The table updated
/// * `set` - The columns set, in bind order
/// * `restrictions` - The restricted columns in key order, with how each is restricted
#[must_use]
pub fn update_cql(
    ns: &str,
    table: &str,
    set: &[&str],
    restrictions: &[(&str, Restriction)],
) -> String {
    let set = set
        .iter()
        .map(|column| format!("{column} = ?"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "UPDATE {ns}.{table} SET {set} WHERE {}",
        where_clause(restrictions)
    )
}

/// Bind a statement's values in their markers' order: an update's set values, then the
/// restrictions in key order, then a bound limit
///
/// # Arguments
///
/// * `set` - The set values, in the order the statement sets them
/// * `restrictions` - The restriction values, in key order
/// * `trailing` - The values bound after the restrictions
#[must_use]
pub fn chain_binds<'v, const K: usize>(
    set: &[&'v (dyn SerializeValue + Sync)],
    restrictions: [&'v (dyn SerializeValue + Sync); K],
    trailing: &[&'v (dyn SerializeValue + Sync)],
) -> Vec<&'v (dyn SerializeValue + Sync)> {
    set.iter()
        .copied()
        .chain(restrictions)
        .chain(trailing.iter().copied())
        .collect()
}

/// The name Scylla gives a statement's `USING TTL ?` marker
const TTL_MARKER: &str = "[ttl]";

/// A table's borrowed row bound with a TTL, for a full-row insert ending in `USING TTL ?`
pub struct WithTtl<'r, R: BindColumns> {
    /// The row's columns
    pub row: &'r R,
    /// The seconds until the row expires, or 0 for never
    pub ttl: i32,
}

impl<R: BindColumns> SerializeRow for WithTtl<'_, R> {
    /// Write each bind marker by name: the TTL marker from the TTL, the others from the row
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
        for spec in ctx.columns() {
            let cell = writer.make_cell_writer();
            // the TTL marker isn't a column, so the row can't answer it
            if spec.name() == TTL_MARKER {
                self.ttl.serialize(spec.typ(), cell)?;
                continue;
            }
            match self.row.serialize_column(spec.name(), spec.typ(), cell) {
                Some(written) => {
                    written?;
                }
                None => return Err(unknown_bind_column(R::TABLE, spec.name())),
            }
        }
        Ok(())
    }

    /// A row with a TTL always binds values
    fn is_empty(&self) -> bool {
        false
    }
}

/// A value that should have been stored as text but wasn't
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotText;

impl std::fmt::Display for NotText {
    /// Say what went wrong
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the value doesn't serialize to non-null UTF-8 text")
    }
}

impl std::error::Error for NotText {}

/// The text a value is stored as in a `text` column, written through its own serialization
///
/// A type stored as JSON or by name in a text column binds as that text, so its stored text is
/// what a statement binding the column as a `&str` must bind.
///
/// # Arguments
///
/// * `value` - The value to write
pub fn text_of<T: SerializeValue + ?Sized>(value: &T) -> Result<String, SerializationError> {
    let typ = ColumnType::Native(NativeType::Text);
    let mut buf = Vec::new();
    value.serialize(&typ, CellWriter::new(&mut buf))?;
    // a cell is its length as a four byte integer and then its bytes; a null has length -1
    let Some((len, bytes)) = buf.split_first_chunk::<4>() else {
        return Err(SerializationError::new(NotText));
    };
    if i32::from_be_bytes(*len) < 0 {
        return Err(SerializationError::new(NotText));
    }
    String::from_utf8(bytes.to_vec()).map_err(|_| SerializationError::new(NotText))
}

/// The most rows a listing returns in one page unless its route sets its own cap
pub const DEFAULT_LIMIT_CAP: u32 = 10_000;

/// The most stage-log lines one request returns, and the largest cap any route sets
pub const STAGE_LOG_LIMIT_CAP: u32 = 250_000;

/// A row limit from a request that its route can't serve
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitError {
    /// The request parameter the limit came from
    pub name: &'static str,
    /// The largest limit the route allows
    pub cap: u32,
    /// The limit the request asked for
    pub got: usize,
}

impl std::fmt::Display for LimitError {
    /// Say which limits the route allows
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} must be between 1 and {}, got {}",
            self.name, self.cap, self.got
        )
    }
}

impl std::error::Error for LimitError {}

/// A validated row limit for cursor statements, from 1 to its route's cap, bound as a CQL `int`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorLimit(NonZeroU32);

impl CursorLimit {
    /// Check a row limit from a request against the most its route allows
    ///
    /// # Arguments
    ///
    /// * `raw` - The limit the request asked for
    /// * `cap` - The largest limit the route allows, at most [`STAGE_LOG_LIMIT_CAP`]
    /// * `name` - The request parameter the limit came from
    pub fn from_user(raw: usize, cap: u32, name: &'static str) -> Result<Self, LimitError> {
        // no route allows more than the stage-log cap, so a limit plus one always fits an int
        let cap = cap.min(STAGE_LOG_LIMIT_CAP);
        // a limit of 0 never fills a page and anything above the cap is too large to serve
        u32::try_from(raw)
            .ok()
            .filter(|limit| *limit <= cap)
            .and_then(NonZeroU32::new)
            .map(CursorLimit)
            .ok_or(LimitError {
                name,
                cap,
                got: raw,
            })
    }

    /// This limit plus the extra row a cursor reads to detect ties and the end of its rows
    #[must_use]
    pub fn plus_one(self) -> Self {
        // a limit is at most the stage-log cap, so adding one never saturates
        CursorLimit(self.0.saturating_add(1))
    }

    /// The rows left of this limit after some have been read, or `None` when none are left
    ///
    /// # Arguments
    ///
    /// * `used` - The rows already read
    #[must_use]
    pub fn remaining(self, used: usize) -> Option<Self> {
        let used = u32::try_from(used).ok()?;
        self.0
            .get()
            .checked_sub(used)
            .and_then(NonZeroU32::new)
            .map(CursorLimit)
    }

    /// The limit as a count of rows
    #[must_use]
    pub fn get(self) -> usize {
        self.0.get() as usize
    }

    /// The limit as the CQL `int` a `LIMIT ?` or `PER PARTITION LIMIT ?` binds
    #[must_use]
    pub fn to_cql(self) -> i32 {
        // a limit is at most the stage-log cap plus one, far below the largest int
        i32::try_from(self.0.get()).unwrap_or(i32::MAX)
    }
}

/// A time bucket that can't be computed or doesn't fit where it's used
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BucketError {
    /// The configured partition size is 0, so no bucket can be computed
    ZeroPartitionSize,
    /// A timestamp falls before the year its bucket is computed in
    BeforeYear {
        /// The timestamp
        timestamp: DateTime<Utc>,
        /// The year the bucket was computed in
        year: i32,
    },
    /// A year whose last second can't be represented
    YearOutOfRange(i32),
    /// A bucket larger than the CQL `int` column it's bound to holds
    TooLarge(u32),
    /// A stored bucket that isn't a bucket number, such as a negative census member
    Stored(String),
}

impl std::fmt::Display for BucketError {
    /// Say what went wrong
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BucketError::ZeroPartitionSize => write!(f, "a partition size of 0 has no buckets"),
            BucketError::BeforeYear { timestamp, year } => {
                write!(f, "{timestamp} is before the start of {year}")
            }
            BucketError::YearOutOfRange(year) => write!(f, "the year {year} is out of range"),
            BucketError::TooLarge(bucket) => {
                write!(f, "bucket {bucket} is larger than an int column holds")
            }
            BucketError::Stored(value) => write!(f, "the stored bucket {value:?} isn't a bucket"),
        }
    }
}

impl std::error::Error for BucketError {}

/// A time bucket within a year, counted from the year's start in partition-size steps
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Bucket(u32);

impl Bucket {
    /// The bucket at a position within its year
    ///
    /// # Arguments
    ///
    /// * `bucket` - The bucket's position
    #[must_use]
    pub const fn new(bucket: u32) -> Self {
        Bucket(bucket)
    }

    /// The bucket a timestamp falls in within a year, as rows are bucketed when written
    ///
    /// Buckets count from one second after midnight on January 1st, so the year's first second
    /// would fall one second before bucket 0; it's counted in bucket 0 instead.
    ///
    /// # Arguments
    ///
    /// * `timestamp` - The timestamp
    /// * `year` - The year the bucket is counted in
    /// * `partition_size` - The size of each bucket in seconds
    pub fn of(
        timestamp: DateTime<Utc>,
        year: i32,
        partition_size: u16,
    ) -> Result<Self, BucketError> {
        // a size of 0 would divide by zero
        if partition_size == 0 {
            return Err(BucketError::ZeroPartitionSize);
        }
        // buckets are counted from one second after the year starts
        let base = NaiveDate::from_ymd_opt(year, 1, 1)
            .and_then(|date| date.and_hms_opt(0, 0, 1))
            .ok_or(BucketError::YearOutOfRange(year))?;
        let seconds = (timestamp.naive_utc() - base).num_seconds();
        // only the year's first second falls before the base
        if seconds < -1 {
            return Err(BucketError::BeforeYear { timestamp, year });
        }
        // count the first second in bucket 0, as larger partition sizes already do
        let bucket = seconds.max(0) / i64::from(partition_size);
        u32::try_from(bucket)
            .map(Bucket)
            .map_err(|_| BucketError::YearOutOfRange(year))
    }

    /// The last bucket of a year: the bucket of its final second, which counts the year's own
    /// length, leap years included
    ///
    /// # Arguments
    ///
    /// * `year` - The year
    /// * `partition_size` - The size of each bucket in seconds
    pub fn last_of_year(year: i32, partition_size: u16) -> Result<Self, BucketError> {
        // get the final second of this year
        let last_second = NaiveDate::from_ymd_opt(year, 12, 31)
            .and_then(|date| date.and_hms_opt(23, 59, 59))
            .ok_or(BucketError::YearOutOfRange(year))?
            .and_utc();
        Bucket::of(last_second, year, partition_size)
    }

    /// Parse a bucket stored as text, such as a census member
    ///
    /// # Arguments
    ///
    /// * `stored` - The stored text
    pub fn from_stored(stored: &str) -> Result<Self, BucketError> {
        // a stored bucket is never negative
        stored
            .parse::<u32>()
            .map(Bucket)
            .map_err(|_| BucketError::Stored(stored.to_owned()))
    }

    /// The bucket before this one, or `None` at the year's first bucket
    #[must_use]
    pub fn prev(self) -> Option<Self> {
        self.0.checked_sub(1).map(Bucket)
    }

    /// The bucket's position within its year
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// The bucket as the CQL `int` its column binds
    pub fn to_cql(self) -> Result<i32, BucketError> {
        i32::try_from(self.0).map_err(|_| BucketError::TooLarge(self.0))
    }
}
