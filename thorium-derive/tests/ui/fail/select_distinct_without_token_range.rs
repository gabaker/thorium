//! `distinct` is generated only on a token-range select
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

/// A harvest group
#[derive(ScyllaProjection)]
#[thorium_projection(of = HarvestsTable)]
pub struct GroupRow {
    /// The group
    pub group: String,
}

/// Statements on the table
#[thorium::table_queries(table = HarvestsTable)]
pub struct HarvestsQueries {
    /// A distinct select restricted by key instead of a token range
    #[select(project = GroupRow, eq(group), distinct)]
    groups: Select,
}

fn main() {}
