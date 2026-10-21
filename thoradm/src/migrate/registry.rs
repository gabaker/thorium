//! Every data migration thoradm knows about
//!
//! A new migration is appended to [`MIGRATIONS`]; every `thoradm_migration` a catalog step names
//! must be registered here.

/// The backend a data migration changes
#[derive(Debug, Clone, Copy)]
pub enum Backend {
    /// Scylla tables
    Scylla,
    /// Elasticsearch
    Elastic,
    /// Redis families
    Redis,
    /// S3 buckets
    S3,
}

/// A data migration thoradm knows about
#[derive(Debug, Clone, Copy)]
pub struct MigrationDef {
    /// The id of this migration, referenced by catalog steps
    pub id: &'static str,
    /// The backend this migration changes
    pub backend: Backend,
    /// Whether thoradm can run this migration yet
    pub implemented: bool,
}

/// Every data migration thoradm knows about, appended in the order they were added
pub const MIGRATIONS: &[MigrationDef] = &[MigrationDef {
    id: "elastic-keyword-mappings",
    backend: Backend::Elastic,
    implemented: false,
}];
