//! A partition delete restricts only the partition key
use chrono::{DateTime, Utc};
use thorium::models::schema::ScyllaTable;
use uuid::Uuid;

/// The table
#[derive(ScyllaTable)]
#[thorium_table(name = "harvests")]
pub struct HarvestsTable {
    /// The partition column
    #[thorium(partition)]
    pub group: String,
    /// The first clustering column
    #[thorium(cluster = "desc")]
    pub at: DateTime<Utc>,
    /// The second clustering column
    #[thorium(cluster)]
    pub id: Uuid,
    /// A regular column
    pub crop: String,
}

/// Statements on the table
#[thorium::table_queries(table = HarvestsTable)]
pub struct HarvestsQueries {
    /// A partition delete restricting a clustering column
    #[delete(partition, eq(group, at))]
    d: Delete,
}

/// The entry point of a case that fails to compile, so it is never built or run
fn main() {}
