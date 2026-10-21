//! A census groups by a prefix of the primary key
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

/// A count of each crop's harvests
#[derive(ScyllaProjection)]
#[thorium_projection(of = HarvestsTable)]
pub struct CropCountRow {
    /// The crop
    pub crop: String,
    /// How many harvests grew the crop
    #[thorium(expr = "count(*)")]
    pub count: i64,
}

/// Statements on the table
#[thorium::table_queries(table = HarvestsTable)]
pub struct HarvestsQueries {
    /// A census grouped by a regular column
    #[census(project = CropCountRow, group_by(crop))]
    census: Census,
}

/// The entry point of a case that fails to compile, so it is never built or run
fn main() {}
