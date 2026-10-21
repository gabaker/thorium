//! A field type with no CQL type is refused by the trait bound
use thorium::models::schema::ScyllaTable;

/// A type that isn't stored in Scylla
pub struct Foo;

/// A table with a column of that type
#[derive(ScyllaTable)]
#[thorium_table(name = "foos")]
pub struct FooTable {
    /// The partition column
    #[thorium(partition)]
    pub group: String,
    /// A column whose type has no CQL type
    pub foo: Foo,
}

/// The entry point of a case that fails to compile, so it is never built or run
fn main() {}
