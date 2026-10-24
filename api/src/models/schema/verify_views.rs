//! Comparing each declared materialized view with its base table, read-only
//!
//! `schema diff` compares definitions, not rows, so a view that has the right definition but
//! misses rows its base table gives it, or holds rows no base row gives, is found only here. The
//! token ring is cut into [`TOKEN_RANGES`] equal ranges. The full mode scans the base table and
//! the view once each, and compares, for every range of the base table's tokens, the number of
//! rows and a checksum of every column the view selects; a view row is placed by the token of the
//! base row it comes from. The sampled mode checks only some ranges: each base row in a range is
//! looked up in the view, and each view row in the same range of the view's own tokens is looked
//! up in its base table, and a row found holding other values in any column counts as not found.
//! Either way a view row whose key is right but whose other values differ (a missed update of a
//! non-key column) is out of step, and a base row the view excludes (an unset key column, or a
//! row the view's filter rejects) isn't expected in the view.

use scylla::client::session::Session;
use scylla::response::PagingState;
use scylla::serialize::row::SerializeRow;
use scylla::statement::prepared::PreparedStatement;
use scylla::value::{CqlValue, Row};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::ControlFlow;
use uuid::Uuid;

use super::apply::{ApplyError, read_texts};
use super::views::{ViewError, declared_views, resolve_views};
use super::{ColumnDef, ColumnRole, ObjectDef, SchemaDefinition, SchemaObject, ViewDef};
use crate::Conf;

/// How many equal ranges the token ring is cut into
pub const TOKEN_RANGES: u32 = 256;

/// Which token ranges a check covers
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RangeSelection {
    /// Every range: both tables are scanned in full
    All,
    /// This many ranges, picked at random
    Sample(u32),
    /// These ranges, by index
    Ranges(Vec<u32>),
}

/// What `thoradm schema verify-views` checks
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyOptions {
    /// Only check this declared view
    pub view: Option<String>,
    /// The token ranges to check
    pub ranges: RangeSelection,
}

/// Whose tokens a range is a range of
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RangeSide {
    /// The base table's: the range holds base rows, and the view rows that come from them
    Base,
    /// The view's own: the range holds view rows, looked up in the base table
    View,
}

/// The first token range where a view and its base table differ
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RangeDifference {
    /// Whose tokens the range is a range of
    pub side: RangeSide,
    /// The range's index, from 0 to [`TOKEN_RANGES`] - 1
    pub index: u32,
    /// The range's first token
    pub start: i64,
    /// The range's last token
    pub end: i64,
    /// The base rows the range holds that the view should hold (on the view side, the view rows
    /// whose base row exists and gives them, with the same value in every column)
    pub base_rows: u64,
    /// The view rows the range holds (on the base side in the sampled mode, the base rows whose
    /// view row was found with the same value in every column)
    pub view_rows: u64,
}

/// Whether a view is in step with its base table
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ViewState {
    /// Every range checked holds the same rows in both
    InStep,
    /// At least one range differs
    OutOfStep {
        /// The first range that differs
        first: RangeDifference,
    },
    /// The view or its base table doesn't exist
    Missing,
}

/// The result of checking one view
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewCheck {
    /// The view
    pub view: String,
    /// Its base table
    pub base: String,
    /// Whether it is in step
    pub state: ViewState,
    /// The indexes of the ranges checked, in order
    pub ranges: Vec<u32>,
    /// The base rows read that the view should hold
    pub base_rows: u64,
    /// The view rows read
    pub view_rows: u64,
}

/// The result of checking every view asked for
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyReport {
    /// Each view's result, in declaration order
    pub views: Vec<ViewCheck>,
    /// Whether only some ranges were checked
    pub sampled: bool,
}

impl VerifyReport {
    /// The views that aren't in step, missing ones included
    #[must_use]
    pub fn out_of_step(&self) -> Vec<&ViewCheck> {
        // keep every view not reported in step
        self.views
            .iter()
            .filter(|check| check.state != ViewState::InStep)
            .collect()
    }
}

/// How far a token is shifted right to give its range: each of the [`TOKEN_RANGES`] ranges spans
/// 2^56 tokens, so the 256 of them cover the 2^64 tokens of the ring
const RANGE_SHIFT: u32 = 56;

/// The first and last token of a range
///
/// # Arguments
///
/// * `index` - The range's index, from 0 to [`TOKEN_RANGES`] - 1
#[must_use]
pub fn token_range(index: u32) -> (i64, i64) {
    // ranges start at the smallest token and follow each other without gaps
    let start = i128::from(i64::MIN) + (i128::from(index) << RANGE_SHIFT);
    let end = start + (1_i128 << RANGE_SHIFT) - 1;
    (
        i64::try_from(start).unwrap_or(i64::MAX),
        i64::try_from(end).unwrap_or(i64::MAX),
    )
}

/// The index of the range holding a token
///
/// # Arguments
///
/// * `token` - The token
#[must_use]
pub fn range_of(token: i64) -> u32 {
    // count whole ranges from the smallest token
    u32::try_from((i128::from(token) - i128::from(i64::MIN)) >> RANGE_SHIFT)
        .unwrap_or(TOKEN_RANGES - 1)
}

/// The columns a check reads for one view
struct Shape {
    /// The view
    view: ViewDef,
    /// The view's key columns, partition columns first, each in key order
    keys: Vec<&'static str>,
    /// Every column the view selects: its key columns as in `keys`, then the rest in declaration
    /// order, so a key column's position here is its position in `keys`
    columns: Vec<&'static str>,
    /// The view's partition key columns
    view_partition: Vec<&'static str>,
    /// The base table's partition key columns
    base_partition: Vec<&'static str>,
    /// The base table's key columns, partition columns first
    base_primary: Vec<&'static str>,
}

/// The key columns of a set of columns, partition columns first, each in key order
///
/// # Arguments
///
/// * `columns` - The object's columns
/// * `partition_only` - Whether to stop after the partition key
fn key_columns(columns: &[ColumnDef], partition_only: bool) -> Vec<&'static str> {
    // sort the partition and clustering columns by their place in the key
    let mut partition = Vec::new();
    let mut clustering = Vec::new();
    for column in columns {
        match column.role {
            ColumnRole::Partition(at) => partition.push((at, column.name)),
            ColumnRole::Cluster(at, _) if !partition_only => clustering.push((at, column.name)),
            _ => (),
        }
    }
    partition.sort_unstable();
    clustering.sort_unstable();
    partition
        .into_iter()
        .chain(clustering)
        .map(|(_, name)| name)
        .collect()
}

impl Shape {
    /// Work out the columns a check reads for a view, if its base table is declared
    ///
    /// # Arguments
    ///
    /// * `view` - The view
    /// * `objects` - The declared objects
    fn of(view: ViewDef, objects: &[SchemaObject]) -> Option<Self> {
        // find the base table among the declared objects
        let base = objects.iter().find_map(|object| match object.definition {
            SchemaDefinition::Scylla(ObjectDef::Table(table)) if table.name == view.base => {
                Some(table)
            }
            _ => None,
        })?;
        // read the key columns both reads restrict from the two definitions, and select every
        // view column with the key columns first
        let keys = key_columns(view.columns, false);
        let columns = keys
            .iter()
            .copied()
            .chain(
                view.columns
                    .iter()
                    .map(|column| column.name)
                    .filter(|name| !keys.contains(name)),
            )
            .collect();
        Some(Shape {
            view,
            keys,
            columns,
            view_partition: key_columns(view.columns, true),
            base_partition: key_columns(base.columns, true),
            base_primary: key_columns(base.columns, false),
        })
    }

    /// The positions in the view's key of some columns, which the view's key always holds
    ///
    /// # Arguments
    ///
    /// * `columns` - The columns to find
    fn positions(&self, columns: &[&'static str]) -> Vec<usize> {
        // every base key column is part of the view's key
        columns
            .iter()
            .filter_map(|column| self.keys.iter().position(|key| key == column))
            .collect()
    }

    /// The `AND <filter> ALLOW FILTERING` a base read adds for a view with a filter, after an
    /// existing restriction
    fn and_filter(&self) -> String {
        // only a filtered view needs it
        self.view.filter.map_or_else(String::new, |filter| {
            format!(" AND {filter} ALLOW FILTERING")
        })
    }
}

/// Hash every column of one view row, so two tables' rows can be compared by a sum per range
///
/// Each value is hashed through its `Debug` rendering, a canonical text that is the same in both
/// tables for the same value: it names the value's type, quotes and escapes text (so an unset
/// value, rendered `None`, differs from an empty string), lists blob bytes in order, and lists
/// collection elements in the order Scylla stores them.
///
/// # Arguments
///
/// * `values` - The row's columns, in [`Shape::columns`] order
fn row_hash(values: &[Option<CqlValue>]) -> u64 {
    // hash each value's rendering in column order
    let mut hasher = DefaultHasher::new();
    for value in values {
        format!("{value:?}").hash(&mut hasher);
    }
    hasher.finish()
}

/// Prepare a statement, failing with its text on error
///
/// # Arguments
///
/// * `session` - The Scylla session to prepare on
/// * `cql` - The statement
async fn prepare(session: &Session, cql: String) -> Result<PreparedStatement, ApplyError> {
    // keep the statement in any error
    session
        .prepare(cql.as_str())
        .await
        .map_err(|err| ApplyError::query(&cql, err))
}

/// Run a read page by page, handing each row's columns to a callback
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `statement` - The prepared read
/// * `values` - The values to bind
/// * `each` - Receives each row's columns
async fn scan(
    session: &Session,
    statement: &PreparedStatement,
    values: &(impl SerializeRow + Sync),
    each: &mut (dyn FnMut(Vec<Option<CqlValue>>) -> Result<(), ApplyError> + Send),
) -> Result<(), ApplyError> {
    // start at the first page and follow the paging state to the last
    let cql = statement.get_statement();
    let mut paging = PagingState::start();
    loop {
        let (result, response) = session
            .execute_single_page(statement, values, paging)
            .await
            .map_err(|err| ApplyError::query(cql, err))?;
        let rows = result
            .into_rows_result()
            .map_err(|err| ApplyError::query(cql, err))?;
        for row in rows
            .rows::<Row>()
            .map_err(|err| ApplyError::query(cql, err))?
        {
            each(row.map_err(|err| ApplyError::query(cql, err))?.columns)?;
        }
        match response.into_paging_control_flow() {
            ControlFlow::Break(()) => return Ok(()),
            ControlFlow::Continue(next) => paging = next,
        }
    }
}

/// Collect every row of a read
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `statement` - The prepared read
/// * `values` - The values to bind
async fn collect(
    session: &Session,
    statement: &PreparedStatement,
    values: &(impl SerializeRow + Sync),
) -> Result<Vec<Vec<Option<CqlValue>>>, ApplyError> {
    // keep each row's columns in order
    let mut rows = Vec::new();
    scan(session, statement, values, &mut |row| {
        rows.push(row);
        Ok(())
    })
    .await?;
    Ok(rows)
}

/// Take some columns of a row as bind values, by position; `None` when one is unset
///
/// # Arguments
///
/// * `row` - The row's columns
/// * `positions` - The columns to take
fn pick(row: &[Option<CqlValue>], positions: &[usize]) -> Option<Vec<CqlValue>> {
    // every column taken must be set
    positions
        .iter()
        .map(|at| row.get(*at).cloned().flatten())
        .collect()
}

/// Split a row of a read selecting a token and then every view column into the token and the
/// columns, skipping a row the view excludes for an unset key column
///
/// # Arguments
///
/// * `row` - The row's columns
/// * `keys` - How many of the columns after the token are the view's key
fn token_and_row(row: Vec<Option<CqlValue>>, keys: usize) -> Option<(i64, Vec<Option<CqlValue>>)> {
    // the token comes first, then the columns, the key columns leading
    let mut columns = row.into_iter();
    let Some(Some(CqlValue::BigInt(token))) = columns.next() else {
        return None;
    };
    let row = columns.collect::<Vec<_>>();
    row.iter()
        .take(keys)
        .all(Option::is_some)
        .then_some((token, row))
}

/// Render `a = ? AND b = ?` for some columns
///
/// # Arguments
///
/// * `columns` - The columns restricted
fn equal_to_binds(columns: &[&str]) -> String {
    // one bind marker per column
    columns
        .iter()
        .map(|column| format!("{column} = ?"))
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// Compare a view with its base table over every range: scan both once, and compare each range's
/// row count and checksum of every view column, placing a view row by its base row's token
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `keyspace` - The keyspace
/// * `shape` - The columns to read
async fn check_full(
    session: &Session,
    keyspace: &str,
    shape: &Shape,
) -> Result<(ViewState, u64, u64), ApplyError> {
    // tally the base rows the view should hold, by range
    let ranges = TOKEN_RANGES as usize;
    let columns = shape.columns.join(", ");
    let filter = shape.view.filter.map_or_else(String::new, |filter| {
        format!(" WHERE {filter} ALLOW FILTERING")
    });
    let base_read = prepare(
        session,
        format!(
            "SELECT token({}), {columns} FROM {keyspace}.{}{filter}",
            shape.base_partition.join(", "),
            shape.view.base
        ),
    )
    .await?;
    let mut base = (vec![0_u64; ranges], vec![0_u64; ranges]);
    scan(session, &base_read, &(), &mut |row| {
        if let Some((token, row)) = token_and_row(row, shape.keys.len()) {
            let at = range_of(token) as usize;
            base.0[at] += 1;
            base.1[at] = base.1[at].wrapping_add(row_hash(&row));
        }
        Ok(())
    })
    .await?;
    // tally the view's rows by the token of the base row each comes from
    let token_of = prepare(
        session,
        format!(
            "SELECT {} FROM {keyspace}.{} WHERE {}",
            shape.base_partition[0],
            shape.view.base,
            equal_to_binds(&shape.base_partition)
        ),
    )
    .await?;
    let partition = shape.positions(&shape.base_partition);
    let view_read = prepare(
        session,
        format!("SELECT {columns} FROM {keyspace}.{}", shape.view.name),
    )
    .await?;
    let mut view = (vec![0_u64; ranges], vec![0_u64; ranges]);
    scan(session, &view_read, &(), &mut |row| {
        let Some(values) = pick(&row, &partition) else {
            return Ok(());
        };
        let token = token_of
            .calculate_token(&values)
            .map_err(|err| ApplyError::query(token_of.get_statement(), err))?
            .map_or(0, |token| token.value());
        let at = range_of(token) as usize;
        view.0[at] += 1;
        view.1[at] = view.1[at].wrapping_add(row_hash(&row));
        Ok(())
    })
    .await?;
    // the first range whose count or checksum differs is reported
    let first = (0..TOKEN_RANGES).find(|index| {
        let at = *index as usize;
        base.0[at] != view.0[at] || base.1[at] != view.1[at]
    });
    let state = match first {
        Some(index) => {
            let (start, end) = token_range(index);
            ViewState::OutOfStep {
                first: RangeDifference {
                    side: RangeSide::Base,
                    index,
                    start,
                    end,
                    base_rows: base.0[index as usize],
                    view_rows: view.0[index as usize],
                },
            }
        }
        None => ViewState::InStep,
    };
    Ok((state, base.0.iter().sum(), view.0.iter().sum()))
}

/// Look a row up in the other table and tell whether it holds the same value in every view column
/// there, its key columns set
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `lookup` - The prepared lookup in the other table, selecting every view column
/// * `values` - The key values the lookup binds
/// * `row` - The row looked up, in [`Shape::columns`] order
/// * `keys` - How many of the row's columns are the view's key
async fn found_alike(
    session: &Session,
    lookup: &PreparedStatement,
    values: &[CqlValue],
    row: &[Option<CqlValue>],
    keys: usize,
) -> Result<bool, ApplyError> {
    // a row found holding other values, or an unset key column, doesn't count
    Ok(collect(session, lookup, &values)
        .await?
        .iter()
        .any(|other| other == row && other.iter().take(keys).all(Option::is_some)))
}

/// Compare a view with its base table over some ranges: look each base row in a range up in the
/// view, and each view row in the same range of the view's tokens up in its base table, a row
/// counting as found only when every view column holds the same value in both
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `keyspace` - The keyspace
/// * `shape` - The columns to read
/// * `indexes` - The ranges to check, in order
async fn check_sampled(
    session: &Session,
    keyspace: &str,
    shape: &Shape,
    indexes: &[u32],
) -> Result<(ViewState, u64, u64), ApplyError> {
    // prepare the range reads and the lookups both ways, each selecting every view column
    let columns = shape.columns.join(", ");
    let (view, base) = (shape.view.name, shape.view.base);
    let base_partition = shape.base_partition.join(", ");
    let view_partition = shape.view_partition.join(", ");
    let base_range = prepare(
        session,
        format!(
            "SELECT token({base_partition}), {columns} FROM {keyspace}.{base} WHERE \
             token({base_partition}) >= ? AND token({base_partition}) <= ?{}",
            shape.and_filter()
        ),
    )
    .await?;
    let in_view = prepare(
        session,
        format!(
            "SELECT {columns} FROM {keyspace}.{view} WHERE {}",
            equal_to_binds(&shape.keys)
        ),
    )
    .await?;
    let view_range = prepare(
        session,
        format!(
            "SELECT {columns} FROM {keyspace}.{view} WHERE token({view_partition}) >= ? AND \
             token({view_partition}) <= ?"
        ),
    )
    .await?;
    let in_base = prepare(
        session,
        format!(
            "SELECT {columns} FROM {keyspace}.{base} WHERE {}{}",
            equal_to_binds(&shape.base_primary),
            shape.and_filter()
        ),
    )
    .await?;
    let primary = shape.positions(&shape.base_primary);
    let key_count = shape.keys.len();
    let (mut base_rows, mut view_rows, mut first) = (0_u64, 0_u64, None);
    for index in indexes {
        // every base row in the range the view should hold must be found in it, with the same
        // value in every column
        let bounds = token_range(*index);
        let expected = collect(session, &base_range, &bounds)
            .await?
            .into_iter()
            .filter_map(|row| token_and_row(row, key_count))
            .collect::<Vec<_>>();
        let mut found = 0_u64;
        for (_, row) in &expected {
            let key = row
                .iter()
                .take(key_count)
                .flatten()
                .cloned()
                .collect::<Vec<_>>();
            if found_alike(session, &in_view, &key, row, key_count).await? {
                found += 1;
            }
        }
        let expected = expected.len() as u64;
        base_rows += expected;
        if found < expected && first.is_none() {
            first = Some(RangeDifference {
                side: RangeSide::Base,
                index: *index,
                start: bounds.0,
                end: bounds.1,
                base_rows: expected,
                view_rows: found,
            });
        }
        // every view row in the range must come from a base row that gives it, with the same
        // value in every column
        let held = collect(session, &view_range, &bounds).await?;
        let mut matched = 0_u64;
        for row in &held {
            let Some(values) = pick(row, &primary) else {
                continue;
            };
            if found_alike(session, &in_base, &values, row, key_count).await? {
                matched += 1;
            }
        }
        let held = held.len() as u64;
        view_rows += held;
        if matched < held && first.is_none() {
            first = Some(RangeDifference {
                side: RangeSide::View,
                index: *index,
                start: bounds.0,
                end: bounds.1,
                base_rows: matched,
                view_rows: held,
            });
        }
    }
    let state = first.map_or(ViewState::InStep, |first| ViewState::OutOfStep { first });
    Ok((state, base_rows, view_rows))
}

/// Pick some distinct ranges at random, in order
///
/// # Arguments
///
/// * `count` - How many ranges, at most [`TOKEN_RANGES`]
fn sample(count: u32) -> Vec<u32> {
    // shuffle the first `count` places of every range index, then sort them
    let mut indexes = (0..TOKEN_RANGES).collect::<Vec<_>>();
    let len = indexes.len();
    for at in 0..(count as usize).min(len) {
        let (high, low) = Uuid::new_v4().as_u64_pair();
        let span = (len - at) as u64;
        let pick = at + usize::try_from((high ^ low) % span).unwrap_or_default();
        indexes.swap(at, pick);
    }
    indexes.truncate(count as usize);
    indexes.sort_unstable();
    indexes
}

/// Turn a range selection into the indexes to check, refusing one that checks nothing or names a
/// range that doesn't exist
///
/// # Arguments
///
/// * `selection` - The ranges asked for
fn select(selection: &RangeSelection) -> Result<Vec<u32>, ViewError> {
    match selection {
        // every range
        RangeSelection::All => Ok((0..TOKEN_RANGES).collect()),
        // a sample of between one and every range
        RangeSelection::Sample(count) if (1..=TOKEN_RANGES).contains(count) => Ok(sample(*count)),
        RangeSelection::Sample(count) => Err(ViewError::BadRanges {
            reason: format!(
                "a sample of {count} ranges can't be checked; ask for 1 to {TOKEN_RANGES}"
            ),
        }),
        // named ranges, each once and in order
        RangeSelection::Ranges(indexes) => {
            if indexes.is_empty() {
                return Err(ViewError::BadRanges {
                    reason: "no token range was named".to_owned(),
                });
            }
            if let Some(index) = indexes.iter().find(|index| **index >= TOKEN_RANGES) {
                return Err(ViewError::BadRanges {
                    reason: format!(
                        "token range {index} doesn't exist; ranges are 0 to {}",
                        TOKEN_RANGES - 1
                    ),
                });
            }
            let mut indexes = indexes.clone();
            indexes.sort_unstable();
            indexes.dedup();
            Ok(indexes)
        }
    }
}

/// Compare each declared view (or the one named) with its base table over the selected token
/// ranges; reads only
///
/// # Arguments
///
/// * `session` - The Scylla session to read with
/// * `conf` - The Thorium config, for the keyspace
/// * `options` - Which views and ranges to check
/// * `progress` - Receives a line as each view is checked
pub async fn verify_views(
    session: &Session,
    conf: &Conf,
    options: &VerifyOptions,
    progress: &(dyn Fn(&str) + Sync),
) -> Result<VerifyReport, ViewError> {
    // refuse an undeclared view or unusable ranges before reading anything
    let mut views = declared_views();
    if let Some(name) = &options.view {
        views = resolve_views(&[name])?;
    }
    let indexes = select(&options.ranges)?;
    let sampled = options.ranges != RangeSelection::All;
    // find which tables and views exist
    let keyspace = conf.thorium.namespace.to_lowercase();
    let tables = read_texts::<1>(
        session,
        "SELECT table_name FROM system_schema.tables WHERE keyspace_name = ?",
        &[&keyspace],
    )
    .await?
    .into_iter()
    .map(|[name]| name)
    .collect::<BTreeSet<_>>();
    let existing = read_texts::<1>(
        session,
        "SELECT view_name FROM system_schema.views WHERE keyspace_name = ?",
        &[&keyspace],
    )
    .await?
    .into_iter()
    .map(|[name]| name)
    .collect::<BTreeSet<_>>();
    // check each view in declaration order
    let objects = SchemaObject::declared();
    let mut report = VerifyReport {
        views: Vec::with_capacity(views.len()),
        sampled,
    };
    for view in views {
        let shape = Shape::of(view, &objects)
            .filter(|_| existing.contains(view.name) && tables.contains(view.base));
        let (state, base_rows, view_rows) = match shape {
            // a view or base table that doesn't exist has nothing to compare
            None => (ViewState::Missing, 0, 0),
            Some(shape) if sampled => check_sampled(session, &keyspace, &shape, &indexes).await?,
            Some(shape) => check_full(session, &keyspace, &shape).await?,
        };
        progress(&match &state {
            ViewState::InStep => {
                format!("view {}: in step over {} ranges", view.name, indexes.len())
            }
            ViewState::OutOfStep { first } => format!(
                "view {}: out of step; first differing range {} of the {:?} table's tokens [{}, {}]: \
                 {} base rows, {} view rows",
                view.name,
                first.index,
                first.side,
                first.start,
                first.end,
                first.base_rows,
                first.view_rows
            ),
            ViewState::Missing => format!("view {}: missing", view.name),
        });
        report.views.push(ViewCheck {
            view: view.name.to_owned(),
            base: view.base.to_owned(),
            state,
            ranges: indexes.clone(),
            base_rows,
            view_rows,
        });
    }
    Ok(report)
}
