//! An unsigned field needs an explicit CQL type
use thorium::models::schema::ScyllaTable;

/// A table with an unannotated unsigned column
#[derive(ScyllaTable)]
#[thorium_table(name = "counts")]
pub struct CountsTable {
    /// The partition column
    #[thorium(partition)]
    pub group: String,
    /// An unsigned column without its CQL type
    pub count: u64,
}

fn main() {}
