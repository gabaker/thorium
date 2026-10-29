//! The Scylla table for scan stats snapshots
//!
//! Scan stats are TTL'd, disposable telemetry (one JSON snapshot row per quantized
//! `events.stats_snapshot_secs` window) so this table is deliberately not part of the thoradm
//! backup set.

use chrono::{DateTime, Utc};

use crate::models::schema::ScyllaTable;

/// A time series of periodic scan stats snapshots
///
/// Rows expire whole on the config driven `events.stats_retention` default TTL, partitioned by
/// `events.stats_partition_size` seconds and clustered newest first. The year and bucket are
/// hand-planned at the call sites instead of carrying the time-bucket roles because no cursor
/// lists this table and its partition size is not a recorded data-shaping setting: the rows age
/// out on their own, so a size change only strands rows until they expire.
#[derive(ScyllaTable)]
#[thorium_table(name = "scan_stats", ttl = "thorium.events.stats_retention")]
pub struct ScanStatsTable {
    /// The year this snapshot was taken in
    #[thorium(partition)]
    pub year: i32,
    /// The time bucket within the year, from `events.stats_partition_size`
    #[thorium(partition)]
    pub bucket: i32,
    /// The quantized window this snapshot covers
    #[thorium(cluster = "desc")]
    pub snapshot_at: DateTime<Utc>,
    /// The serialized snapshot (JSON text)
    pub data: String,
}
