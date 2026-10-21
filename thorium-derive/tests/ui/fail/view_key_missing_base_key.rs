//! A view's key must contain every base key column
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

/// A view whose key leaves out the base's clustering column
#[derive(ScyllaView)]
#[thorium_view(name = "by_sha256", base = BaseTable)]
pub struct BySha256View {
    /// The sha256 column as the partition key
    #[thorium(partition)]
    pub sha256: String,
    /// The base partition column
    #[thorium(cluster)]
    pub group: String,
    /// The base clustering column, left out of the view's key
    pub id: Uuid,
}

fn main() {}
