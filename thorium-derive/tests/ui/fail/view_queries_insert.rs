//! A view's queries struct can't write: views are written only through their base table
use thorium::models::schema::{ScyllaTable, ScyllaView};
use uuid::Uuid;

/// The base table
#[derive(ScyllaTable)]
#[thorium_table(name = "base")]
pub struct BaseTable {
    /// The partition column
    #[thorium(partition)]
    pub group: String,
    /// The clustering column
    #[thorium(cluster)]
    pub id: Uuid,
}

/// The base table by id
#[derive(ScyllaView)]
#[thorium_view(name = "base_by_id", base = BaseTable)]
pub struct BaseByIdView {
    /// The id as the partition key
    #[thorium(partition)]
    pub id: Uuid,
    /// The group
    #[thorium(cluster)]
    pub group: String,
}

/// Statements on the view
#[thorium::view_queries(view = BaseByIdView)]
pub struct BaseByIdQueries {
    /// A write, which a view can't take
    #[insert(columns = all, ttl = none)]
    write: Insert,
}

/// The entry point of a case that fails to compile, so it is never built or run
fn main() {}
