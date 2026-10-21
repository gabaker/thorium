//! A key column can't be optional
use thorium::models::schema::ScyllaTable;

/// A table with an optional partition column
#[derive(ScyllaTable)]
#[thorium_table(name = "optkey")]
pub struct OptionKeyTable {
    /// An optional partition column
    #[thorium(partition)]
    pub group: Option<String>,
}

fn main() {}
