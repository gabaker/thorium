//! The deployment's record in `schema_migrations`: the applied schema, the data-shaping settings,
//! and the revision with its history

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::{Fingerprint, FinishedStep};
use crate::models::upgrades::{AppliedStep, DriftSummary, RevisionId, SchemaBackend};

/// The schema record in `schema_migrations`: the applied fingerprints and layout versions, which
/// the API startup check reads, backups carry, and restore writes back
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedSchema {
    /// The applied deployment fingerprint
    pub deployment: Fingerprint,
    /// Each backend's applied fingerprint
    pub backends: BTreeMap<SchemaBackend, Fingerprint>,
    /// The layout version of each Redis family and S3 bucket, by backend
    #[serde(default)]
    pub layouts: BTreeMap<SchemaBackend, BTreeMap<String, u32>>,
}

/// The data-shaping settings recorded at the first `schema apply`, compared at every later apply
/// and at API startup
///
/// Its `Debug` output redacts the files password's hash
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigRecord {
    /// The effective partition size of each data type, keyed `files`, `repos`, `results`,
    /// `entities`, `associations`, and `tags.<type>`
    pub partition_sizes: BTreeMap<String, u16>,
    /// Each `earliest` value, keyed like the partition sizes
    pub earliest: BTreeMap<String, i64>,
    /// The files password's identity: an argon2id PHC string with a random salt and
    /// `Params::default()`; a string with other parameters is refused before verifying; never the
    /// password and never a fast hash
    pub files_password: String,
}

impl std::fmt::Debug for ConfigRecord {
    /// Format the record for debugging, printing `<redacted>` in place of the files password's
    /// hash so logs and test failures never carry it
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // print the settings and hide the files password's hash
        f.debug_struct("ConfigRecord")
            .field("partition_sizes", &self.partition_sizes)
            .field("earliest", &self.earliest)
            .field("files_password", &"<redacted>")
            .finish()
    }
}

/// The deployment's whole record, as read from `schema_migrations`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentRecord {
    /// The revision the deployment is at
    pub revision: RevisionId,
    /// The Thorium version that recorded it
    #[serde(default)]
    pub applied_by_version: Option<String>,
    /// The applied history, oldest first; a status Job returns the newest 200 entries (the
    /// mirror's size, for display), or only those newer than the ones the operator already holds
    #[serde(default)]
    pub history: Vec<AppliedStep>,
    /// Every `(revision, step)` the full, uncapped history counts as finished, by the rule that a
    /// `Done` or `Skipped` entry counts only when it is newer than the latest entry that lowered the
    /// revision; the operator plans from this, never from the 200-entry window
    #[serde(default)]
    pub finished: Vec<FinishedStep>,
    /// How many entries the full, uncapped history holds, counted in the same query that read the
    /// history; an input to the lossy confirmation code when a step blocks
    #[serde(default)]
    pub history_len: u64,
    /// The applied fingerprints and layout versions
    pub schema: AppliedSchema,
    /// The data-shaping record
    pub config: ConfigRecord,
}

/// What `thoradm schema status` found, and what every writing thoradm Job's result row carries
///
/// Kept unboxed: one status is read per Job, and `Recorded` is the usual answer
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RecordStatus {
    /// Scylla couldn't be reached; retried, never taken for a fresh install
    Unreachable {
        /// What failed, with no credentials in it
        message: String,
    },
    /// Scylla refused Thorium's login; retried, never taken for a fresh install
    AuthFailed,
    /// There is no Thorium keyspace: a fresh install
    NoKeyspace,
    /// The keyspace exists but `schema_migrations` holds no revision: wait for
    /// `spec.upgrade.starting_revision`
    NoRecord {
        /// The revision today's detection would have picked, offered in the status message only
        suggested: Option<RevisionId>,
    },
    /// The deployment's record, and the capped drift summary when the probe ran
    Recorded {
        /// The record
        record: DeploymentRecord,
        /// Counts and the first 20 unacknowledged drifted objects
        drift: Option<DriftSummary>,
    },
}
