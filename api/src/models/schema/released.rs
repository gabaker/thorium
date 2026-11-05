//! The snapshots of the revisions released after the schema registry existed
//!
//! Such a revision's schema is the one this build declares, except where the declared schema has
//! since moved on: a layout the revision wrote that the declared schema has replaced is kept in
//! [`RELEASED_LAYOUTS`], and an object a later revision introduced is listed with that revision in
//! [`LATER_REVISION_OBJECTS`] so the older revisions' snapshots leave it out.
//! [`RevisionSnapshot::released`] builds each revision's snapshot from them.

use super::baseline::baseline_layout;
use super::fingerprint::{RevisionSnapshot, SnapshotObject};
use super::registry::{SchemaDefinition, SchemaObject};
use crate::models::upgrades::{RevisionId, SchemaBackend};

/// The layouts a revision released after the schema registry existed wrote where the declared
/// schema has since moved to another layout, as `(revision, family, layout)`; every other object of
/// such a revision is the declared one
pub const RELEASED_LAYOUTS: &[(&str, &str, u32)] = &[("2026-10-v03", "pipelines", 1)];

/// The declared objects each revision after the first released ones introduced, as `(the revision
/// that introduced it, backend, name)`; the snapshot of every revision before that one leaves the
/// object out, since that revision's schema never created it
///
/// The yara scanning revision adds the scan stats table, re-declares the `entities_by_name` view
/// the baseline created (its own frozen definition stays in the `baseline` module), and adds the
/// yara ruleset cache's bucket role.
pub const LATER_REVISION_OBJECTS: &[(&str, SchemaBackend, &str)] = &[
    ("2026-10-v05", SchemaBackend::Scylla, "entities_by_name"),
    ("2026-10-v05", SchemaBackend::Scylla, "scan_stats"),
    ("2026-10-v05", SchemaBackend::S3, "yara_rulesets"),
];

/// Whether a declared object was introduced by a revision after older revisions were released,
/// that is whether [`LATER_REVISION_OBJECTS`] lists it
///
/// The live drift probe uses this to treat such an object as an expected extra rather than drift
/// when it compares a live schema with an older revision's snapshot. Membership is all it needs: a
/// snapshot that lists the object raises no extra at all, so an extra only arises against the
/// snapshots that predate the object. The offline writer uses it to pick the objects a keyspace
/// may not hold yet, which it reads from the keyspace itself.
///
/// # Arguments
///
/// * `backend` - The object's backend
/// * `name` - The object's name
#[must_use]
pub fn introduced_by_later_revision(backend: SchemaBackend, name: &str) -> bool {
    // membership is enough, since only older snapshots can raise the extra
    LATER_REVISION_OBJECTS
        .iter()
        .any(|(_, later_backend, later)| *later_backend == backend && *later == name)
}

impl RevisionSnapshot {
    /// Build the snapshot of a revision released after the schema registry existed: the declared
    /// schema, without the objects a later revision introduced, and with the layouts the revision
    /// wrote where they differ from the declared ones
    ///
    /// # Arguments
    ///
    /// * `revision` - The revision the snapshot is recorded for
    #[must_use]
    pub fn released(revision: RevisionId) -> RevisionSnapshot {
        // fingerprint the declared objects, leaving out each object a later revision introduced
        // and keeping the layouts this revision wrote differently
        let mut objects = SchemaObject::declared()
            .into_iter()
            .filter(|object| {
                // an object introduced after this revision was released isn't in its schema
                !LATER_REVISION_OBJECTS
                    .iter()
                    .any(|(introduced, backend, name)| {
                        object.backend == *backend
                            && object.name == *name
                            && revision.as_str() < *introduced
                    })
            })
            .map(|object| {
                let written = RELEASED_LAYOUTS.iter().find(|(id, family, _)| {
                    *id == revision.as_str()
                        && object.backend == SchemaBackend::Redis
                        && object.name == *family
                });
                match written {
                    Some(&(_, family, version)) => SnapshotObject::of(&SchemaObject {
                        definition: SchemaDefinition::Layout(baseline_layout(
                            SchemaBackend::Redis,
                            family,
                            version,
                        )),
                        ..object
                    }),
                    None => SnapshotObject::of(&object),
                }
            })
            .collect::<Vec<_>>();
        objects
            .sort_by(|left, right| (left.backend, &left.name).cmp(&(right.backend, &right.name)));
        let backends = Self::backend_fingerprints(&objects);
        RevisionSnapshot {
            revision,
            deployment: Self::deployment_fingerprint(&backends),
            backends,
            objects,
        }
    }
}
