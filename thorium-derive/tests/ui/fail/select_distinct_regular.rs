//! A distinct token-range select reads only partition key columns
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

/// A harvest's crop
#[derive(ScyllaProjection)]
#[thorium_projection(of = HarvestsTable)]
pub struct CropRow {
    /// The crop
    pub crop: String,
}

/// Statements on the table
#[thorium::table_queries(table = HarvestsTable)]
pub struct HarvestsQueries {
    /// A distinct select of a regular column
    #[select(project = CropRow, distinct, token_range)]
    crops: Select,
}

fn main() {}
