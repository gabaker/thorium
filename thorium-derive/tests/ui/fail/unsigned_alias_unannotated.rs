//! An alias of an unsigned type fails the same way, through the missing CqlType impl
use thorium::models::schema::ScyllaTable;

/// An alias the derive can't see through
pub type Count = u64;

/// A table with an aliased unsigned column
#[derive(ScyllaTable)]
#[thorium_table(name = "counts")]
pub struct CountsTable {
    /// The partition column
    #[thorium(partition)]
    pub group: String,
    /// An aliased unsigned column without its CQL type
    pub count: Count,
}

fn main() {}
