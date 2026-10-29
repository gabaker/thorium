//! The statements on the scan stats table

use chrono::{DateTime, Utc};

use crate::models::schema::scylla::ScanStatsTable;

/// Statements on the `scan_stats` table
#[thorium::table_queries(table = ScanStatsTable)]
pub struct ScanStatsQueries {
    /// Insert a scan stats snapshot, expiring on the table's config driven default TTL
    #[insert(columns = all, ttl = none)]
    insert: Insert,
    /// Pull a range of scan stats snapshots from some partitions
    ///
    /// The timestamp comparators are half open (`>= start`, `< end`) so history reads can tile
    /// adjacent windows with zero overlap, and the bindable per partition limit lets one
    /// statement serve both the ranged pull and the single-newest-row stride reads. The two-sided
    /// range isn't expressible as a generated select, so this is a hand-written statement whose
    /// rows call sites read through the prepared statement itself, typed `(DateTime<Utc>, String)`
    /// as `(snapshot_at, data)`.
    #[query(
        "SELECT snapshot_at, data FROM {ns}.scan_stats WHERE year = ? AND bucket IN ? AND \
         snapshot_at < ? AND snapshot_at >= ? PER PARTITION LIMIT ?",
        binds(year: i32, buckets: &[i32], end: DateTime<Utc>, start: DateTime<Utc>, limit: i32)
    )]
    pull: Raw,
}
