//! The Scylla tables and views for reaction stage logs

use uuid::Uuid;

use crate::models::schema::ScyllaTable;

/// Stage logs for reactions, bucketed by 2500 lines
#[derive(ScyllaTable)]
#[thorium_table(name = "logs", ttl = "thorium.retention.logs")]
pub struct LogsTable {
    /// The reaction these logs are for
    #[thorium(partition)]
    pub reaction: Uuid,
    /// The stage these logs are for
    #[thorium(partition)]
    pub stage: String,
    /// The line bucket (`position / 2500`; not a time bucket, so no `bucket` role); `INT`, so
    /// declared `i32` and computed with a checked conversion at the call site
    #[thorium(partition)]
    pub bucket: i32,
    /// The line's position in the stage's log
    #[thorium(cluster = "asc", unsigned = "bigint")]
    pub position: u64,
    /// The log line
    pub line: String,
}
