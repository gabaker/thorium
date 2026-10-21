//! What the read-only probe observed on the live backends and how it differs from a revision

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::{Fingerprint, SnapshotObject};
use crate::models::upgrades::{BackendSchemaStatus, DriftKind, RevisionId, SchemaBackend};

/// The schema the read-only probe observed on the live backends
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservedSchema {
    /// Every observed object in canonical form; Redis and S3 report their recorded layout versions
    pub objects: Vec<SnapshotObject>,
    /// Each backend's observed fingerprint
    pub backends: BTreeMap<SchemaBackend, Fingerprint>,
    /// The observed deployment fingerprint
    pub deployment: Fingerprint,
}

/// One field of a drifted object
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldDrift {
    /// The column, UDT field, or mapping property
    pub field: String,
    /// The expected canonical value, when the field is expected
    pub expected: Option<String>,
    /// The observed canonical value, when the field exists
    pub observed: Option<String>,
}

/// One object that doesn't match the recorded revision
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectDrift {
    /// The backend the object belongs to
    pub backend: SchemaBackend,
    /// The object's name
    pub object: String,
    /// How it differs
    pub kind: DriftKind,
    /// The differing fields, for a changed object or a field-level missing or unexpected entry
    #[serde(default)]
    pub fields: Vec<FieldDrift>,
    /// Whether an admin or the catalog acknowledged this object, so it raises no condition
    #[serde(default)]
    pub acknowledged: bool,
    /// Whether an acknowledgement named a fingerprint the object no longer has
    #[serde(default)]
    pub lapsed: bool,
    /// The object's observed fingerprint, absent when it is `missing`
    #[serde(default)]
    pub fingerprint: Option<Fingerprint>,
}

/// The probe's findings for one backend
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendDrift {
    /// `ok` when every object matches, `drift` otherwise
    pub status: BackendSchemaStatus,
    /// The objects that don't match
    #[serde(default)]
    pub objects: Vec<ObjectDrift>,
}

/// What `thoradm schema diff` and the operator report against a recorded revision
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriftReport {
    /// The revision the deployment's upgrade state records
    pub revision: RevisionId,
    /// That revision's deployment fingerprint
    pub expected: Fingerprint,
    /// The observed deployment fingerprint
    pub observed: Fingerprint,
    /// The findings for each backend
    pub backends: BTreeMap<SchemaBackend, BackendDrift>,
}
