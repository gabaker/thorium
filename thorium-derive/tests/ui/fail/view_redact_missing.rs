//! A view column over a redacted base column must be redacted too
use thorium::models::schema::{ScyllaTable, ScyllaView};

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

/// Secrets by url, reading the password without redacting it
#[derive(Debug, ScyllaView)]
#[thorium_view(name = "secrets_by_url", base = SecretsTable)]
pub struct SecretsByUrlView {
    /// The repo url
    #[thorium(partition)]
    pub url: String,
    /// The group
    #[thorium(cluster)]
    pub group: String,
    /// The password, unredacted
    pub password: String,
}

/// The entry point of a case that fails to compile, so it is never built or run
fn main() {}
