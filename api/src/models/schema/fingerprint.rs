//! Fingerprints of canonical schema forms and the revision snapshots built from them

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::models::upgrades::{RevisionId, SchemaBackend};

/// The sha256 of a canonical form, as lowercase hex
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Fingerprint(String);

impl Fingerprint {
    /// Get this fingerprint as lowercase hex
    #[must_use]
    pub fn as_str(&self) -> &str {
        // borrow the hex digest
        &self.0
    }
}

/// A released revision's full canonical schema, committed as
/// `api/schema/revisions/<revision>.json`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RevisionSnapshot {
    /// The revision this snapshot belongs to
    pub revision: RevisionId,
    /// The deployment fingerprint: the sha256 of the backend fingerprints
    pub deployment: Fingerprint,
    /// Each backend's fingerprint: the sha256 of its sorted `(name, object fingerprint)` list
    pub backends: BTreeMap<SchemaBackend, Fingerprint>,
    /// Every object's canonical definition and fingerprint, sorted by backend and name
    pub objects: Vec<SnapshotObject>,
}

/// One object of a snapshot (also the form the probe reports what it observed in)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotObject {
    /// The backend the object belongs to
    pub backend: SchemaBackend,
    /// The object's name
    pub name: String,
    /// The sha256 of `canonical`
    pub fingerprint: Fingerprint,
    /// The canonical JSON form: sorted keys and columns, one spelling per type, server defaults
    /// stripped; for a layout family, its version number
    pub canonical: serde_json::Value,
}
