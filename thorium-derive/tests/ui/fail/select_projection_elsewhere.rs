//! A select's projection reads columns of the object it selects from
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

/// A table without the crop column
#[derive(ScyllaTable)]
#[thorium_table(name = "other")]
pub struct OtherTable {
    /// The partition column
    #[thorium(partition)]
    pub group: String,
}

/// Statements on the other table
#[thorium::table_queries(table = OtherTable)]
pub struct OtherQueries {
    /// A projection of another table
    #[select(project = CropRow, eq(group))]
    get: Select,
}

/// The entry point of a case that fails to compile, so it is never built or run
fn main() {}
