//! A clustering order other than asc or desc is refused
use thorium::models::schema::ScyllaTable;

/// A table with an unknown clustering order
#[derive(ScyllaTable)]
#[thorium_table(name = "upwards")]
pub struct UpwardsTable {
    /// The partition column
    #[thorium(partition)]
    pub group: String,
    /// A clustering column with an unknown order
    #[thorium(cluster = "up")]
    pub name: String,
}

fn main() {}
