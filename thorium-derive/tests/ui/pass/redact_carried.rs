//! Views and projections over a redacted column compile when they redact it too, and print
//! `<redacted>` in its place
#![deny(warnings)]

use thorium::models::schema::{ScyllaProjection, ScyllaTable, ScyllaView};

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

/// Secrets by url, redacting the password
#[derive(ScyllaView)]
#[thorium_view(name = "secrets_by_url", base = SecretsTable)]
pub struct SecretsByUrlView {
    /// The repo url
    #[thorium(partition)]
    pub url: String,
    /// The group
    #[thorium(cluster)]
    pub group: String,
    /// The password
    #[thorium(redact)]
    pub password: String,
}

/// A row reading the password, redacted
#[derive(ScyllaProjection)]
#[thorium_projection(of = SecretsTable)]
pub struct SecretRow {
    /// The group
    pub group: String,
    /// The password
    #[thorium(redact)]
    pub password: String,
}

/// Check the generated `Debug` hides a redacted column
fn main() {
    // the generated Debug hides the password
    let row = SecretRow {
        group: "minerva".to_owned(),
        password: "hunter2".to_owned(),
    };
    let printed = format!("{row:?}");
    assert!(printed.contains("<redacted>") && !printed.contains("hunter2"));
}
