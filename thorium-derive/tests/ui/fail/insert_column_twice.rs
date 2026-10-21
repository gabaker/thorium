//! A subset insert writes each column once
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
    /// An insert writing a column twice
    #[insert(columns = [group, at, id, crop, crop], ttl = none)]
    i: Insert,
}

fn main() {}
