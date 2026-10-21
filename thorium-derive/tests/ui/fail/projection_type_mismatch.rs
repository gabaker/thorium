//! A projection field must have its column's type
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

/// A row reading a text column as a uuid
#[derive(ScyllaProjection)]
#[thorium_projection(of = BaseTable)]
pub struct WrongRow {
    /// The text column read as a uuid
    pub sha256: Uuid,
}

fn main() {}
