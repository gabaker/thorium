//! A census selects only the columns it groups by
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
}

/// A count that also reads a clustering column
#[derive(ScyllaProjection)]
#[thorium_projection(of = HarvestsTable)]
pub struct CountRow {
    /// The group
    pub group: String,
    /// The time
    pub at: DateTime<Utc>,
    /// How many harvests the partition holds
    #[thorium(expr = "count(*)")]
    pub count: i64,
}

/// Statements on the table
#[thorium::table_queries(table = HarvestsTable)]
pub struct HarvestsQueries {
    /// A census reading a column it doesn't group by
    #[census(project = CountRow)]
    census: Census,
}

/// The entry point of a case that fails to compile, so it is never built or run
fn main() {}
