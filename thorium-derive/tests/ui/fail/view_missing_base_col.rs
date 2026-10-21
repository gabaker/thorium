//! A view column must exist in its base table
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

/// A view selecting a column its base doesn't have
#[derive(ScyllaView)]
#[thorium_view(name = "by_nope", base = BaseTable)]
pub struct ByNopeView {
    /// A partition column missing from the base
    #[thorium(partition)]
    pub nope: String,
    /// The base partition column
    #[thorium(cluster)]
    pub group: String,
    /// The base clustering column
    #[thorium(cluster)]
    pub id: Uuid,
}

/// The entry point of a case that fails to compile, so it is never built or run
fn main() {}
