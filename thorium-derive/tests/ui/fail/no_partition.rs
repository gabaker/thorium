//! A table needs a partition key
use thorium::models::schema::ScyllaTable;

/// A table with no partition column
#[derive(ScyllaTable)]
#[thorium_table(name = "nopart")]
pub struct NoPartitionTable {
    /// A regular column
    pub name: String,
}

fn main() {}
