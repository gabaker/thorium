//! Two fields can't map to one column
use thorium::models::schema::ScyllaTable;
use uuid::Uuid;

/// A table with two fields for the `id` column
#[derive(ScyllaTable)]
#[thorium_table(name = "dupes")]
pub struct DupesTable {
    /// The partition column
    #[thorium(partition)]
    pub id: Uuid,
    /// A second field renamed onto the same column
    #[thorium(column = "id")]
    pub other: Uuid,
}

fn main() {}
