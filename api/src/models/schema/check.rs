//! How the API's startup check judges the live schema against the one this build declares

use super::Fingerprint;
use crate::models::upgrades::RevisionId;

/// Why the API refused to start against the live schema
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaCheckError {
    /// The keyspace or `schema_migrations` doesn't exist ("run `thoradm schema apply`")
    Missing,
    /// The applied schema is older than this build needs ("the operator hasn't applied revision
    /// X yet")
    Older {
        /// The fingerprint recorded in `schema_migrations`
        applied: Fingerprint,
        /// The fingerprint this build declares
        declared: Fingerprint,
        /// The revision this build needs applied
        revision: RevisionId,
    },
    /// The applied fingerprint is a later revision this build's catalog knows, and the diff from
    /// the declared schema to that revision's snapshot has a non-additive change
    Newer {
        /// The fingerprint recorded in `schema_migrations`
        applied: Fingerprint,
        /// The fingerprint this build declares
        declared: Fingerprint,
        /// The later revision the applied fingerprint belongs to
        revision: RevisionId,
        /// The objects whose difference isn't additive, as `object` or `object.column`
        breaking: Vec<String>,
    },
    /// The applied fingerprint matches no revision in this build's catalog, such as one a newer
    /// build applied before its image was rolled back
    UnknownFingerprint {
        /// The fingerprint recorded in `schema_migrations`
        applied: Fingerprint,
        /// The fingerprint this build declares
        declared: Fingerprint,
    },
    /// A data-shaping setting differs from the recorded one while its data exists
    ConfigChanged {
        /// The setting, such as `thorium.files.partition_size`
        setting: String,
    },
    /// The recorded files-password hash isn't argon2id with `Params::default()`, so it is refused
    /// before any hash is computed
    PasswordHashParams,
    /// `schema_migrations` exists but its record couldn't be read
    Unreadable(String),
}

/// How the API's startup check let it start
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaCheckPass {
    /// The applied fingerprint is the one this build declares
    Current,
    /// The applied fingerprint is a later revision this build's catalog knows, and every
    /// difference from the declared schema to that revision's snapshot is additive; the API starts
    /// and logs a warning naming the revision
    NewerAdditive {
        /// The fingerprint recorded in `schema_migrations`
        applied: Fingerprint,
        /// The later revision the applied fingerprint belongs to
        revision: RevisionId,
    },
}
