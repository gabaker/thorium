//! A census grouped by named columns selects only those columns
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

/// A count that reads a column it doesn't group by
#[derive(ScyllaProjection)]
#[thorium_projection(of = HarvestsTable)]
pub struct CountRow {
    /// The group
    pub group: String,
    /// The id
    pub id: Uuid,
    /// How many harvests the group holds
    #[thorium(expr = "count(*)")]
    pub count: i64,
}

/// Statements on the table
#[thorium::table_queries(table = HarvestsTable)]
pub struct HarvestsQueries {
    /// A census grouped by the group and time, reading the id
    #[census(project = CountRow, group_by(group, at))]
    census: Census,
}

/// The entry point of a case that fails to compile, so it is never built or run
fn main() {}
