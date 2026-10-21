//! A view column must have its base column's type
use thorium::models::schema::{ScyllaTable, ScyllaView};
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

/// A view reading a bigint column as an int
#[derive(ScyllaView)]
#[thorium_view(name = "by_id", base = BaseTable)]
pub struct ByIdView {
    /// The base clustering column
    #[thorium(partition)]
    pub id: Uuid,
    /// The base partition column
    #[thorium(cluster)]
    pub group: String,
    /// The bigint column read as an int
    pub count: i32,
}

fn main() {}
