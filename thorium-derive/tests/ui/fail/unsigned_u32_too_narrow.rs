//! A u32 can't be stored in an int column
use thorium::models::schema::ScyllaTable;

/// A table storing a u32 in an int
#[derive(ScyllaTable)]
#[thorium_table(name = "counts")]
pub struct CountsTable {
    /// The partition column
    #[thorium(partition)]
    pub group: String,
    /// A u32 annotated with a column type narrower than its range
    #[thorium(unsigned = "int")]
    pub count: u32,
}

/// The entry point of a case that fails to compile, so it is never built or run
fn main() {}
