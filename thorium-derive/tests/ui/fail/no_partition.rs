//! A table needs a partition key
use thorium::models::schema::ScyllaTable;

/// A table with no partition column
#[derive(ScyllaTable)]
#[thorium_table(name = "nopart")]
pub struct NoPartitionTable {
    /// A regular column
    pub name: String,
}

/// The entry point of a case that fails to compile, so it is never built or run
fn main() {}
