//! A u64 can't be stored in an int column
use thorium::models::schema::ScyllaTable;

/// A table storing a u64 in an int
#[derive(ScyllaTable)]
#[thorium_table(name = "counts")]
pub struct CountsTable {
    /// The partition column
    #[thorium(partition)]
    pub group: String,
    /// A u64 annotated with a column type narrower than its range
    #[thorium(unsigned = "int")]
    pub count: u64,
}

fn main() {}
