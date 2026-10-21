//! GROUP BY names a prefix of the key
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
    /// The first clustering column
    #[thorium(cluster = "desc")]
    pub at: DateTime<Utc>,
    /// The second clustering column
    #[thorium(cluster)]
    pub id: Uuid,
    /// A regular column
    pub crop: String,
}

/// When a harvest happened
#[derive(ScyllaProjection)]
#[thorium_projection(of = HarvestsTable)]
pub struct AtRow {
    /// The time
    pub at: DateTime<Utc>,
}

/// Statements on the table
#[thorium::table_queries(table = HarvestsTable)]
pub struct HarvestsQueries {
    /// A grouping that skips a clustering column
    #[select(project = AtRow, eq(group), group_by(group, id))]
    get: Select,
}

fn main() {}
