//! A projection reading an unsigned column repeats the table's annotation
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
    pub count: u64,
}

/// A row reading the unsigned column without its annotation
#[derive(ScyllaProjection)]
#[thorium_projection(of = CountsTable)]
pub struct CountRow {
    /// The unsigned column, unannotated
    pub count: u64,
}

fn main() {}
