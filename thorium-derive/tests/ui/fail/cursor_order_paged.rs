//! A cursor with `order` runs unpaged, so it can't also be `paged`
use chrono::{DateTime, Utc};
use thorium::models::schema::{ScyllaProjection, ScyllaTable};

/// The table, bucketed by time
#[derive(ScyllaTable)]
#[thorium_table(name = "harvests")]
pub struct HarvestsTable {
    /// The partition column
    #[thorium(partition)]
    pub group: String,
    /// The year
    #[thorium(partition, year)]
    pub year: i32,
    /// The bucket
    #[thorium(partition, bucket(partition_size = "files.partition_size"))]
    pub bucket: i32,
    /// The time
    #[thorium(cluster, timestamp)]
    pub at: DateTime<Utc>,
    /// The crop
    #[thorium(cluster)]
    pub crop: String,
}

/// A harvest in a listing
#[derive(ScyllaProjection)]
#[thorium_projection(of = HarvestsTable)]
pub struct HarvestRow {
    /// The group
    pub group: String,
    /// The time
    pub at: DateTime<Utc>,
    /// The crop
    pub crop: String,
}

/// Statements on the table
#[thorium::table_queries(table = HarvestsTable)]
pub struct HarvestsQueries {
    /// A paged cursor read newest first against the declared order
    #[cursor(kind = time, group_by = group, year = year, bucket = bucket, range = at, tie = crop,
             order = "desc", project = HarvestRow, limit = none, paged)]
    list: Cursor,
}

/// The entry point of a case that fails to compile, so it is never built or run
fn main() {}
