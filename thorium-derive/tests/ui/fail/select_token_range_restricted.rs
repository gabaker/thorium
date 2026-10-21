//! A token-range select's only restriction is its token range
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
    /// A token-range select that also restricts a key column
    #[select(project = GroupRow, token_range, eq(group))]
    groups: Select,
}

fn main() {}
