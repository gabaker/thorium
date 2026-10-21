//! A cursor's tie columns must be consumed in one direction
use chrono::{DateTime, Utc};
use thorium::models::schema::{ScyllaProjection, ScyllaTable};

/// The table, bucketed by time, with tie columns clustered in different directions
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
    #[thorium(cluster = "desc", timestamp)]
    pub at: DateTime<Utc>,
    /// The crop
    #[thorium(cluster = "asc")]
    pub crop: String,
    /// The field
    #[thorium(cluster = "desc")]
    pub field: String,
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
    /// The field
    pub field: String,
}

/// Statements on the table
#[thorium::table_queries(table = HarvestsTable)]
pub struct HarvestsQueries {
    /// A cursor whose ties can't be resumed with one comparison
    #[cursor(kind = time, group_by = group, year = year, bucket = bucket, range = at,
             tie = [crop, field], project = HarvestRow, limit = per_partition)]
    list: Cursor,
}

fn main() {}
