//! The revision snapshots committed under `api/schema/revisions/`, and the catalog checks that
//! hold each revision's recorded fingerprint to its snapshot

use serde_json::{Map, Value};

use super::fingerprint::{Fingerprint, RevisionSnapshot};
use crate::models::upgrades::{Catalog, SchemaBackend};

/// Every committed revision snapshot as `(revision, file text)`, oldest first
///
/// The files are generated, never written by hand:
/// `cargo run -p thorium-api --example schema_report -- --write-snapshot <revision>` writes one
/// (from the baseline for a revision released before the schema registry existed, otherwise from
/// the schema this build declares) and prints the deployment fingerprint its catalog entry records
/// in `schema`.
const COMMITTED: &[(&str, &str)] = &[
    (
        "2026-10-v01",
        include_str!("../../../schema/revisions/2026-10-v01.json"),
    ),
    (
        "2026-10-v02",
        include_str!("../../../schema/revisions/2026-10-v02.json"),
    ),
    (
        "2026-10-v03",
        include_str!("../../../schema/revisions/2026-10-v03.json"),
    ),
];

/// A committed snapshot that is malformed or doesn't match its own canonical forms
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotError {
    /// The file isn't a snapshot
    Parse {
        /// The revision the file is committed for
        revision: String,
        /// Why it didn't parse
        error: String,
    },
    /// The file holds another revision's snapshot
    Revision {
        /// The revision the file is committed for
        revision: String,
        /// The revision the snapshot names
        found: String,
    },
    /// The objects aren't sorted by backend and name, or one is listed twice
    Order {
        /// The snapshot's revision
        revision: String,
    },
    /// An object's fingerprint isn't the hash of its canonical form
    Object {
        /// The snapshot's revision
        revision: String,
        /// The object's backend
        backend: SchemaBackend,
        /// The object's name
        name: String,
    },
    /// The backend fingerprints aren't the ones the objects give
    Backends {
        /// The snapshot's revision
        revision: String,
    },
    /// The deployment fingerprint isn't the one the backend fingerprints give
    Deployment {
        /// The snapshot's revision
        revision: String,
    },
}

/// A catalog revision that breaks a hard check
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogCheckError {
    /// The revision records no schema fingerprint
    NoSchema {
        /// The revision
        revision: String,
    },
    /// No snapshot is committed for the revision
    NoSnapshot {
        /// The revision
        revision: String,
    },
    /// The revision's committed snapshot is malformed
    BadSnapshot(SnapshotError),
    /// The fingerprint the revision records isn't its snapshot's deployment fingerprint
    FingerprintMismatch {
        /// The revision
        revision: String,
        /// The fingerprint its catalog entry records
        recorded: String,
        /// The deployment fingerprint of its committed snapshot
        snapshot: Fingerprint,
    },
    /// A step names a data migration that isn't registered
    UnregisteredMigration {
        /// The step
        step: String,
        /// The migration it names
        migration: String,
    },
}

/// Rebuild a JSON value with every object's keys in sorted order
///
/// # Arguments
///
/// * `value` - The value to rebuild
fn sorted(value: &Value) -> Value {
    match value {
        // insert each object's keys in sorted order, whichever order the map keeps
        Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            Value::Object(
                keys.into_iter()
                    .map(|key| (key.clone(), sorted(&map[key])))
                    .collect::<Map<_, _>>(),
            )
        }
        // an array keeps its order
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        scalar => scalar.clone(),
    }
}

impl RevisionSnapshot {
    /// The ids of every revision with a committed snapshot, oldest first
    pub fn committed_ids() -> impl Iterator<Item = &'static str> {
        // list the committed files' revisions in order
        COMMITTED.iter().map(|(revision, _)| *revision)
    }

    /// Load a revision's committed snapshot, checked against its own canonical forms
    ///
    /// # Arguments
    ///
    /// * `revision` - The revision to load
    #[must_use]
    pub fn committed(revision: &str) -> Option<Result<RevisionSnapshot, SnapshotError>> {
        // find the revision's committed file
        let (_, text) = COMMITTED.iter().find(|(id, _)| *id == revision)?;
        Some(Self::parse(revision, text))
    }

    /// Parse a revision's snapshot file and check it
    ///
    /// # Arguments
    ///
    /// * `revision` - The revision the file is committed for
    /// * `text` - The file's text
    pub(super) fn parse(revision: &str, text: &str) -> Result<RevisionSnapshot, SnapshotError> {
        // parse the file
        let snapshot: RevisionSnapshot =
            serde_json::from_str(text).map_err(|error| SnapshotError::Parse {
                revision: revision.to_owned(),
                error: error.to_string(),
            })?;
        // it must be the snapshot of the revision it is committed for
        if snapshot.revision.as_str() != revision {
            return Err(SnapshotError::Revision {
                revision: revision.to_owned(),
                found: snapshot.revision.as_str().to_owned(),
            });
        }
        // and it must match its own canonical forms
        snapshot.verify()?;
        Ok(snapshot)
    }

    /// Check that every fingerprint in this snapshot is the one its canonical forms give, and
    /// that its objects are sorted by backend and name with no name listed twice
    pub fn verify(&self) -> Result<(), SnapshotError> {
        // every error names this snapshot's revision
        let revision = || self.revision.as_str().to_owned();
        // the objects are strictly sorted by backend and name
        let ordered = self
            .objects
            .windows(2)
            .all(|pair| (pair[0].backend, &pair[0].name) < (pair[1].backend, &pair[1].name));
        if !ordered {
            return Err(SnapshotError::Order {
                revision: revision(),
            });
        }
        // each object's fingerprint hashes its canonical form
        if let Some(object) = self
            .objects
            .iter()
            .find(|object| Fingerprint::of(&object.canonical) != object.fingerprint)
        {
            return Err(SnapshotError::Object {
                revision: revision(),
                backend: object.backend,
                name: object.name.clone(),
            });
        }
        // the backend fingerprints follow from the objects, and the deployment's from them
        if Self::backend_fingerprints(&self.objects) != self.backends {
            return Err(SnapshotError::Backends {
                revision: revision(),
            });
        }
        if Self::deployment_fingerprint(&self.backends) != self.deployment {
            return Err(SnapshotError::Deployment {
                revision: revision(),
            });
        }
        Ok(())
    }

    /// Render this snapshot as its committed file's text: pretty JSON with sorted keys and a
    /// trailing newline
    ///
    /// # Panics
    ///
    /// Panics if the snapshot can't be serialized, which a snapshot of JSON values can't cause.
    #[must_use]
    pub fn to_file_text(&self) -> String {
        // serialize the snapshot, sort its keys, and pretty print it
        let value = serde_json::to_value(self).expect("a snapshot serializes");
        let mut text =
            serde_json::to_string_pretty(&sorted(&value)).expect("a JSON value serializes");
        text.push('\n');
        text
    }
}

/// Check a catalog against its committed snapshots and the registered data migrations
///
/// Each revision must record a fingerprint, have a committed snapshot that matches its own
/// canonical forms, and record that snapshot's deployment fingerprint; with `registered`, each
/// migration a step names must be registered. Errors are listed revision by revision.
///
/// # Arguments
///
/// * `catalog` - The catalog to check
/// * `snapshot` - Loads a revision's committed snapshot
/// * `registered` - Every registered data migration id, or `None` to skip that check
pub fn check_catalog(
    catalog: &Catalog,
    snapshot: impl Fn(&str) -> Option<Result<RevisionSnapshot, SnapshotError>>,
    registered: Option<&[&str]>,
) -> Vec<CatalogCheckError> {
    // collect the errors of every revision in catalog order
    let mut errors = Vec::new();
    for entry in catalog.edges {
        let revision = entry.id.to_owned();
        // the revision records the deployment fingerprint of its committed snapshot
        match (entry.schema, snapshot(entry.id)) {
            (None, _) => errors.push(CatalogCheckError::NoSchema { revision }),
            (Some(_), None) => errors.push(CatalogCheckError::NoSnapshot { revision }),
            (Some(_), Some(Err(error))) => errors.push(CatalogCheckError::BadSnapshot(error)),
            (Some(recorded), Some(Ok(snapshot))) if recorded != snapshot.deployment.as_str() => {
                errors.push(CatalogCheckError::FingerprintMismatch {
                    revision,
                    recorded: recorded.to_owned(),
                    snapshot: snapshot.deployment,
                });
            }
            (Some(_), Some(Ok(_))) => (),
        }
        // every migration its steps name is registered
        let Some(registered) = registered else {
            continue;
        };
        for step in entry.steps {
            if let Some(migration) = step.thoradm_migration
                && !registered.contains(&migration)
            {
                errors.push(CatalogCheckError::UnregisteredMigration {
                    step: step.id.to_owned(),
                    migration: migration.to_owned(),
                });
            }
        }
    }
    errors
}
