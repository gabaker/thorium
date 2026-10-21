//! A select's projection redacts every column the object it reads redacts
use thorium::models::schema::{ScyllaProjection, ScyllaTable};

/// A table that keeps the password in the clear
#[derive(ScyllaTable)]
#[thorium_table(name = "plain")]
pub struct PlainTable {
    /// The partition column
    #[thorium(partition)]
    pub group: String,
    /// An unredacted column
    pub password: String,
}

/// A table that redacts the password
#[derive(ScyllaTable)]
#[thorium_table(name = "secrets")]
pub struct SecretsTable {
    /// The partition column
    #[thorium(partition)]
    pub group: String,
    /// A secret column
    #[thorium(redact)]
    pub password: String,
}

/// The password of a row, unredacted because its own object doesn't redact it
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = PlainTable)]
pub struct PasswordRow {
    /// The password
    pub password: String,
}

/// Statements on the table that redacts the password
#[thorium::table_queries(table = SecretsTable)]
pub struct SecretsQueries {
    /// A projection of the plain table used on the secrets table
    #[select(project = PasswordRow, eq(group))]
    get: Select,
}

fn main() {}
