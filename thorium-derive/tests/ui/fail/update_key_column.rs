//! An update can't set a key column
use thorium::models::schema::ScyllaTable;
use uuid::Uuid;

/// The table
#[derive(ScyllaTable)]
#[thorium_table(name = "base")]
pub struct BaseTable {
    /// The partition column
    #[thorium(partition)]
    pub group: String,
    /// The clustering column
    #[thorium(cluster)]
    pub id: Uuid,
    /// A regular column
    pub crop: String,
}

/// Statements on the table
#[thorium::table_queries(table = BaseTable)]
pub struct BaseQueries {
    /// An update of a key column
    #[update(set(id), by_key, eq(group, id))]
    set_id: Update,
}

/// The entry point of a case that fails to compile, so it is never built or run
fn main() {}
