//! A projection column must exist in its object
use thorium::models::schema::{ScyllaProjection, ScyllaTable};
use uuid::Uuid;

/// The base table the views select from
#[derive(ScyllaTable)]
#[thorium_table(name = "base")]
pub struct BaseTable {
    /// The partition column
    #[thorium(partition)]
    pub group: String,
    /// The clustering column
    #[thorium(cluster)]
    pub id: Uuid,
    /// A bigint column
    pub count: i64,
    /// A text column
    pub sha256: String,
}

/// A row reading a column the table doesn't have
#[derive(ScyllaProjection)]
#[thorium_projection(of = BaseTable)]
pub struct NopeRow {
    /// A column missing from the table
    pub nope: String,
}

/// The entry point of a case that fails to compile, so it is never built or run
fn main() {}
