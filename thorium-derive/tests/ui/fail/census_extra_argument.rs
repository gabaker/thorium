//! A census takes only a projection and grouped columns
use chrono::{DateTime, Utc};
use thorium::models::schema::{ScyllaProjection, ScyllaTable};
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
    /// A regular column
    pub crop: String,
}

/// A count of each group's harvests
#[derive(ScyllaProjection)]
#[thorium_projection(of = HarvestsTable)]
pub struct CountRow {
    /// The group
    pub group: String,
    /// How many harvests the partition holds
    #[thorium(expr = "count(*)")]
    pub count: i64,
}

/// Statements on the table
#[thorium::table_queries(table = HarvestsTable)]
pub struct HarvestsQueries {
    /// A census that also restricts a key column
    #[census(project = CountRow, eq(group))]
    census: Census,
}

fn main() {}
