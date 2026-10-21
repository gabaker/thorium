//! An unknown key in a field attribute is refused with a spanned darling error
use thorium::models::schema::ScyllaTable;

/// A table with a misspelled key
#[derive(ScyllaTable)]
#[thorium_table(name = "typo")]
pub struct TypoTable {
    /// The partition column, with its key misspelled
    #[thorium(partitoin)]
    pub group: String,
}

fn main() {}
