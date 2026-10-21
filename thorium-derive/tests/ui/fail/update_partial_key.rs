//! An update restricts every key column
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
    /// An update by partition only
    #[update(set(crop), by_key, eq(group))]
    set_crop: Update,
}

/// The entry point of a case that fails to compile, so it is never built or run
fn main() {}
