//! A paged select streams the rows of a projection, so it can't be an existence check
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
    /// The clustering column
    #[thorium(cluster = "desc")]
    pub at: DateTime<Utc>,
    /// The id
    #[thorium(cluster)]
    pub id: Uuid,
}

/// Statements on the table
#[thorium::table_queries(table = HarvestsTable)]
pub struct HarvestsQueries {
    /// A paged existence check
    #[select(eq(group), exists, paged)]
    any: Select,
}

fn main() {}
