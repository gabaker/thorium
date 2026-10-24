//! Reading a Scylla table in token order for a migration: contiguous slices of the ring, and
//! batches that always end on a partition boundary
//!
//! A slice is a contiguous run of the token ring and its position is the token of the last
//! partition finished, so a resumed worker starts right after it. Neighbouring tokens live on the
//! same shard, so workers walking their own contiguous slices spread over the cluster's cores.

use scylla::value::{CqlValue, Row};
use std::collections::BTreeMap;

use super::pass_store::{backend_error, is_rejection};
use super::{Batch, MigrateError, RunContext, Slice};
use crate::models::upgrades::Record;

/// Split the whole token ring, `i64::MIN` to `i64::MAX`, into contiguous ranges
///
/// Each range is `(start, end)`, both inclusive, and starts one token after the previous one
/// ends, so every token is in exactly one range. The ranges come back in ring order and differ in
/// size by at most one token. Zero chunks is treated as one, the whole ring.
///
/// # Arguments
///
/// * `chunks` - How many ranges to split the ring into
#[must_use]
pub fn token_ranges(chunks: u64) -> Vec<(i64, i64)> {
    // the ring holds 2^64 tokens; split them as evenly as whole tokens allow
    let chunks = u128::from(chunks.max(1));
    let ring: u128 = 1 << 64;
    let (size, extra) = (ring / chunks, ring % chunks);
    // the first `extra` ranges take one more token than the rest, so every token is used
    let mut ranges = Vec::with_capacity(usize::try_from(chunks).unwrap_or(0));
    let mut start: u128 = 0;
    for index in 0..chunks {
        let len = size + u128::from(index < extra);
        let end = start + len - 1;
        ranges.push((ring_token(start), ring_token(end)));
        start = end + 1;
    }
    ranges
}

/// Convert a position on the ring, counted from its first token, into the token itself
///
/// # Arguments
///
/// * `position` - The position, from 0 (`i64::MIN`) to 2^64 - 1 (`i64::MAX`)
fn ring_token(position: u128) -> i64 {
    // positions count up from i64::MIN, so shift them down by 2^63
    let token = i128::try_from(position).unwrap_or(i128::MAX) + i128::from(i64::MIN);
    i64::try_from(token).unwrap_or(i64::MAX)
}

/// Split the ring into `n` contiguous slices, optionally cut at extra tokens
///
/// # Arguments
///
/// * `n` - How many slices to make
/// * `cuts` - Extra tokens that each end a slice and start the next one at the token after it
#[must_use]
pub fn ring_slices(n: u32, cuts: &[i64]) -> Vec<Slice> {
    // the even split, then each cut splits the slice that holds it
    let mut ranges = token_ranges(u64::from(n.max(1)));
    for cut in cuts {
        if let Some(at) = ranges
            .iter()
            .position(|(start, end)| start <= cut && cut < end)
        {
            let (start, end) = ranges[at];
            ranges[at] = (start, *cut);
            ranges.insert(at + 1, (*cut + 1, end));
        }
    }
    ranges
        .into_iter()
        .enumerate()
        .map(|(index, (start, end))| Slice {
            index: u32::try_from(index).unwrap_or(u32::MAX),
            start: serde_json::json!(start),
            end: serde_json::json!(end),
        })
        .collect()
}

/// How many token ranges a pass covers: enough that a checkpoint covers bounded work, without
/// paying for 100,000 empty range queries on a small table
///
/// # Arguments
///
/// * `slices` - How many slices the pass has
/// * `estimated_rows` - The table's estimated rows
#[must_use]
pub fn range_count(slices: u32, estimated_rows: u64) -> u64 {
    // sixteen ranges per slice, or one per 20,000 rows when that is more
    (16 * u64::from(slices)).max(estimated_rows / 20_000)
}

/// A Scylla table read in token order
#[derive(Debug, Clone)]
pub struct ScyllaScan {
    /// The keyspace
    pub keyspace: String,
    /// The table
    pub table: String,
    /// The partition key columns, in key order
    pub partition_key: Vec<String>,
    /// The columns to read, which include the partition key columns
    pub columns: Vec<String>,
    /// The columns whose values, joined with `/`, make a record's key
    pub key_columns: Vec<String>,
    /// Extra tokens that end a slice, for tests that need a slice boundary at a chosen token
    pub cuts: Vec<i64>,
}

/// Render a column value as text for a record's key
///
/// # Arguments
///
/// * `value` - The column's value
fn render(value: &CqlValue) -> String {
    // text as it is, numbers and ids by their display form
    match value {
        CqlValue::Text(text) | CqlValue::Ascii(text) => text.clone(),
        CqlValue::Int(number) => number.to_string(),
        CqlValue::BigInt(number) => number.to_string(),
        CqlValue::SmallInt(number) => number.to_string(),
        CqlValue::TinyInt(number) => number.to_string(),
        CqlValue::Uuid(id) => id.to_string(),
        other => format!("{other:?}"),
    }
}

impl ScyllaScan {
    /// The slices of the table: the ring split into `n` contiguous parts
    ///
    /// # Arguments
    ///
    /// * `n` - How many slices to make
    #[must_use]
    pub fn slices(&self, n: u32) -> Vec<Slice> {
        // the ring split, with any extra cuts
        ring_slices(n, &self.cuts)
    }

    /// The token expression of the partition key
    fn token(&self) -> String {
        // `token(a, b)` over the partition key columns
        format!("token({})", self.partition_key.join(", "))
    }

    /// Read rows in a token interval into records
    ///
    /// # Arguments
    ///
    /// * `ctx` - The run's context, for the Scylla session
    /// * `filter` - The `WHERE` clause over the token
    /// * `limit` - The most rows to read, if bounded
    /// * `binds` - The tokens the clause binds
    async fn read(
        &self,
        ctx: &RunContext,
        filter: &str,
        limit: Option<usize>,
        binds: &[i64],
    ) -> Result<Vec<(i64, Record)>, MigrateError> {
        // select the token first, then the table's columns
        let limit = limit.map_or_else(String::new, |rows| format!(" LIMIT {rows}"));
        let cql = format!(
            "SELECT {}, {} FROM {}.{} WHERE {filter}{limit}",
            self.token(),
            self.columns.join(", "),
            self.keyspace,
            self.table
        );
        let result = ctx
            .backends
            .scylla
            .query_unpaged(cql, binds)
            .await
            .map_err(|err| backend_error("read a batch", &err, is_rejection(&err)))?;
        let rows = result
            .into_rows_result()
            .map_err(|err| backend_error("read a batch", &err, false))?;
        let names = rows
            .column_specs()
            .iter()
            .skip(1)
            .map(|spec| spec.name().to_owned())
            .collect::<Vec<_>>();
        let mut records = Vec::new();
        for row in rows
            .rows::<Row>()
            .map_err(|err| backend_error("read a batch", &err, false))?
        {
            let row = row.map_err(|err| backend_error("read a row", &err, false))?;
            // the first column is the token, which the position records
            let mut columns = row.columns.into_iter();
            let Some(Some(CqlValue::BigInt(token))) = columns.next() else {
                return Err(MigrateError::Systematic("a row had no token".to_owned()));
            };
            let columns = names
                .iter()
                .cloned()
                .zip(columns)
                .collect::<BTreeMap<String, Option<CqlValue>>>();
            let key = self
                .key_columns
                .iter()
                .map(|name| {
                    columns
                        .get(name)
                        .and_then(Option::as_ref)
                        .map_or_else(String::new, render)
                })
                .collect::<Vec<_>>()
                .join("/");
            records.push((token, Record::Scylla { key, columns }));
        }
        Ok(records)
    }

    /// Read the next batch of a slice after a position, ending on a partition boundary
    ///
    /// The position is the token of the last partition finished, or null at the slice's start.
    /// A batch holds at most `limits.batch_rows` rows, except that the last partition it reaches
    /// is read whole.
    ///
    /// # Arguments
    ///
    /// * `slice` - The slice to read
    /// * `after` - The last token done, or null at the start
    /// * `ctx` - The run's context
    pub async fn next_batch(
        &self,
        slice: &Slice,
        after: &serde_json::Value,
        ctx: &RunContext,
    ) -> Result<Batch, MigrateError> {
        // the slice's own bounds, and where this batch starts
        let end = slice
            .end
            .as_i64()
            .ok_or_else(|| MigrateError::Systematic("a slice had no end token".to_owned()))?;
        let start = slice
            .start
            .as_i64()
            .ok_or_else(|| MigrateError::Systematic("a slice had no start token".to_owned()))?;
        let token = self.token();
        let rows = ctx.limits.batch_rows.max(1);
        // the first batch includes the slice's first token; later ones start after the last done
        let (filter, binds) = match after.as_i64() {
            Some(last) => (format!("{token} > ? AND {token} <= ?"), vec![last, end]),
            None => (format!("{token} >= ? AND {token} <= ?"), vec![start, end]),
        };
        let mut read = self.read(ctx, &filter, Some(rows), &binds).await?;
        // fewer rows than the limit means the slice is exhausted
        if read.len() < rows {
            return Ok(Batch {
                records: read.into_iter().map(|(_, record)| record).collect(),
                next: serde_json::json!(end),
                done: true,
            });
        }
        // a full batch may end inside a partition, so read the last partition whole
        let last = read.last().map_or(end, |(token, _)| *token);
        read.retain(|(token, _)| *token != last);
        let whole = self
            .read(ctx, &format!("{token} = ?"), None, &[last])
            .await?;
        read.extend(whole);
        Ok(Batch {
            records: read.into_iter().map(|(_, record)| record).collect(),
            next: serde_json::json!(last),
            done: last >= end,
        })
    }
}
