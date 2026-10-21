//! A projection's unsigned annotation must match its column's type
use thorium::models::schema::{ScyllaProjection, ScyllaTable};

/// A table with an unsigned column
#[derive(ScyllaTable)]
#[thorium_table(name = "counts")]
pub struct CountsTable {
    /// The partition column
    #[thorium(partition)]
    pub group: String,
    /// An unsigned column stored as a bigint
    #[thorium(unsigned = "bigint")]
    pub count: u16,
}

/// A row reading the bigint column as an int
#[derive(ScyllaProjection)]
#[thorium_projection(of = CountsTable)]
pub struct CountRow {
    /// The unsigned column with the wrong annotation
    #[thorium(unsigned = "int")]
    pub count: u16,
}

/// The entry point of a case that fails to compile, so it is never built or run
fn main() {}
