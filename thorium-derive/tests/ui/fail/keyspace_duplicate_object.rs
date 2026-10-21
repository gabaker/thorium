//! Two keyspace objects can't share a name
use thorium::models::schema::ScyllaTable;

/// The first table named samples
#[derive(ScyllaTable)]
#[thorium_table(name = "samples")]
pub struct FirstTable {
    /// The partition column
    #[thorium(partition)]
    pub group: String,
}

/// A second table with the same name
#[derive(ScyllaTable)]
#[thorium_table(name = "samples")]
pub struct SecondTable {
    /// The partition column
    #[thorium(partition)]
    pub sha256: String,
}

/// A keyspace declaring both
#[thorium::keyspace]
pub struct DupeKeyspace {
    /// The first table
    first: FirstTable,
    /// The second table
    second: SecondTable,
}

/// The entry point of a case that fails to compile, so it is never built or run
fn main() {}
