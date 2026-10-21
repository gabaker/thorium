//! A table with a redacted field gets a generated Debug, so it can't derive its own
use thorium::models::schema::ScyllaTable;

/// A table that holds a secret and also derives Debug
#[derive(Debug, ScyllaTable)]
#[thorium_table(name = "secrets")]
pub struct SecretsTable {
    /// The partition column
    #[thorium(partition)]
    pub group: String,
    /// A secret column
    #[thorium(redact)]
    pub password: String,
}

/// The entry point of a case that fails to compile, so it is never built or run
fn main() {}
