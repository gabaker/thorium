//! What `thoradm schema apply` converges to, what one run did, and why it can stop

use serde::{Deserialize, Serialize};

use super::Fingerprint;
use crate::models::upgrades::RevisionId;

/// What `thoradm schema apply` converges to, and whether it runs anything
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyOptions {
    /// The definitions to converge to
    pub target: ApplyTarget,
    /// Return the statements without executing them (`print`, `diff`, and `--dry-run`)
    pub dry_run: bool,
}

/// The definitions the applier converges to; either way it only creates and adds, never drops
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyTarget {
    /// The registry this build declares
    Declared,
    /// A committed revision snapshot, newer or older than the live schema (an older one re-adds
    /// what a contract step dropped)
    Snapshot(RevisionId),
}

/// What one `schema apply` run did, or would do on a dry run
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyReport {
    /// Every statement issued (or, on a dry run, that would be), in order
    pub statements: Vec<String>,
    /// The objects created
    pub created: Vec<String>,
    /// The columns and UDT fields added, as `object.column`
    pub added: Vec<String>,
    /// Table settings changed from the config, which move no fingerprint
    pub config_changes: Vec<ConfigChange>,
    /// The deployment fingerprint recorded in `schema_migrations`; the operator records this one
    /// and raises `SchemaSkew` when it differs from its own
    pub fingerprint: Fingerprint,
    /// The trigger key this run applied for (the image reference, the target revision, and the
    /// hashes of the data-shaping and TTL settings), recorded beside the fingerprint so later
    /// reconciles compare keys, not fingerprints
    pub trigger_key: String,
    /// The views this run created, each reported only once `system.built_views` lists it, so no
    /// reader moves to a view still being built
    #[serde(default)]
    pub views_built: Vec<String>,
    /// Warnings for the status that don't stop the apply, such as a files password left at its
    /// public default
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Whether nothing was executed
    pub dry_run: bool,
}

/// A table setting that `schema apply` brings in line with the config
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigChange {
    /// The table changed
    pub table: String,
    /// The setting changed, such as `default_time_to_live`
    pub setting: String,
    /// The live value before the change
    pub from: String,
    /// The configured value applied
    pub to: String,
}

/// Why `schema apply` stopped without converging
#[derive(Debug)]
pub enum ApplyError {
    /// A data-shaping setting differs from the recorded one while its data exists
    ConfigChanged {
        /// The setting, such as `thorium.files.partition_size`
        setting: String,
        /// The recorded value (`None` for the password, which is never printed)
        recorded: Option<String>,
        /// The configured value (`None` for the password)
        configured: Option<String>,
    },
    /// `--seed-revision` named a revision while a different one is already recorded
    SeedRefused {
        /// The recorded revision
        recorded: String,
    },
    /// The live schema disagrees with the snapshot of the revision `--seed-revision` named, so it
    /// isn't recorded: an object exists but differs, or is missing and `schema apply` can't add
    /// it; objects the next apply would add, and unexpected extras, don't refuse it
    SeedMismatch {
        /// The revision named
        revision: String,
        /// The changed or unaddable objects, as `thoradm schema diff` names them
        differences: Vec<String>,
    },
    /// The revision `--seed-revision` named is below the catalog's support floor
    SeedBelowMinimum {
        /// The revision named
        revision: String,
        /// The oldest revision this build upgrades from
        minimum: String,
    },
    /// Before the first data-shaping record, sampled rows sit in buckets another partition size
    /// gives; nothing is recorded until the step is approved
    SampledSizeMismatch {
        /// The data type, such as `files`
        data_type: String,
        /// How many sampled rows sit in a bucket the configured size doesn't give
        mismatched: u64,
        /// The partition size those rows suggest, when one size explains them all
        suggested: Option<u16>,
    },
    /// A recorded or restored files-password hash isn't argon2id with `Params::default()`, so it
    /// is refused before any hash is computed
    PasswordHashParams,
    /// A statement failed
    Query {
        /// The statement that failed
        statement: String,
        /// The error Scylla returned
        error: String,
    },
}
