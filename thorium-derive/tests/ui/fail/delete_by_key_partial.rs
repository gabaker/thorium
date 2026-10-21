//! A by-key delete restricts every key column
use thorium::models::schema::{ScyllaProjection, ScyllaTable};
use uuid::Uuid;

/// The table
#[derive(ScyllaTable)]
#[thorium_table(name = "base")]
pub struct BaseTable {
    /// The first partition column
    #[thorium(partition)]
    pub group: String,
    /// The second partition column
    #[thorium(partition)]
    pub year: i32,
    /// The clustering column
    #[thorium(cluster)]
    pub id: Uuid,
    /// A regular column
    pub crop: String,
}

/// The crop of a row
#[derive(ScyllaProjection)]
#[thorium_projection(of = BaseTable)]
pub struct CropRow {
    /// The crop
    pub crop: String,
}

/// Statements on the table
#[thorium::table_queries(table = BaseTable)]
pub struct BaseQueries {
    /// A by-key delete missing the clustering column
    #[delete(by_key, eq(group, year))]
    delete: Delete,
}

fn main() {}
