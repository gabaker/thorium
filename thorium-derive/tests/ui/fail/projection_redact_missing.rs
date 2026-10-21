//! A projection field over a redacted column must be redacted too
use thorium::models::schema::{ScyllaProjection, ScyllaTable};

/// A table holding a credential
#[derive(ScyllaTable)]
#[thorium_table(name = "secrets")]
pub struct SecretsTable {
    /// The partition column
    #[thorium(partition)]
    pub group: String,
    /// The repo url
    #[thorium(cluster)]
    pub url: String,
    /// The password used to clone the repo
    #[thorium(redact)]
    pub password: String,
}

/// A row reading the password without redacting it
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = SecretsTable)]
pub struct LeakyRow {
    /// The group
    pub group: String,
    /// The password, unredacted
    pub password: String,
}

fn main() {}
