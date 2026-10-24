//! The schema Thorium had before its revisions recorded snapshots, which the snapshots of the
//! first two revisions are generated from
//!
//! `2026-10-v01` (deployments the scripts installed before the Helm charts) and `2026-10-v02` (the
//! first Helm revision) were released before the schema registry existed, so their snapshots
//! record the schema those releases created rather than the one this build declares: the Scylla
//! objects their setup statements created, the mappings the search-streamer created Elastic
//! indexes with, and the layout every Redis family and S3 bucket role had then. Both releases
//! created the same schema.
//!
//! A change to the declared schema that moves this baseline keeps the released definition here
//! instead: the committed snapshots of released revisions never change.

use super::cql_type::{CqlDesc, CqlNative};
use super::definition::{
    ClusterOrder, ColumnDef, ColumnRole, ObjectDef, TableDef, TtlSource, ViewDef,
};
use super::fingerprint::{RevisionSnapshot, SnapshotObject};
use super::registry::{SchemaDefinition, SchemaObject};
use crate::models::upgrades::{LAYOUTS, LayoutDef, RevisionId, SchemaBackend};

/// The revisions released before the schema registry existed, whose snapshots are the baseline
pub const BASELINE_REVISIONS: &[&str] = &["2026-10-v01", "2026-10-v02"];

/// Every declared Scylla object the baseline's setup statements created, in their order; the
/// statements are kept in `goldens/legacy_ddl.cql`, and the declared definitions are held to them
pub(super) const DECLARED_SCYLLA: &[&str] = &[
    "associations",
    "entities",
    "entities_by_id",
    "comments",
    "comments_by_id",
    "commitishes",
    "commitish_list",
    "committed_repo_data",
    "logs",
    "network_policies",
    "network_policies_by_name",
    "network_policies_default",
    "nodes",
    "notifications",
    "repo_data",
    "repos_list",
    "repos",
    "results_stream",
    "results",
    "results_auth",
    "results_ids",
    "s3_ids",
    "s3_sha256s",
    "samples_list",
    "samples",
    "tags",
    "tags_by_item",
    "tags_case_insensitive",
];

/// The `text` type
const TEXT: CqlDesc = CqlDesc::Native(CqlNative::Text);

/// The `int` type
const INT: CqlDesc = CqlDesc::Native(CqlNative::Int);

/// The `timestamp` type
const TIMESTAMP: CqlDesc = CqlDesc::Native(CqlNative::Timestamp);

/// The `uuid` type
const UUID: CqlDesc = CqlDesc::Native(CqlNative::Uuid);

/// Declare a baseline column
///
/// # Arguments
///
/// * `name` - The column's name
/// * `desc` - The column's type
/// * `role` - The column's role in the primary key
const fn column(name: &'static str, desc: CqlDesc, role: ColumnRole) -> ColumnDef {
    // no baseline column is filled restrictively on a downgrade
    ColumnDef {
        name,
        desc,
        role,
        security_sensitive: false,
    }
}

/// The `events` table the baseline's setup created, which the registry doesn't declare
pub(super) const EVENTS: ObjectDef = ObjectDef::Table(TableDef {
    name: "events",
    columns: &[
        column("event_type", TEXT, ColumnRole::Partition(0)),
        column("year", INT, ColumnRole::Partition(1)),
        column("bucket", INT, ColumnRole::Partition(2)),
        column(
            "timestamp",
            TIMESTAMP,
            ColumnRole::Cluster(0, ClusterOrder::Asc),
        ),
        column("id", UUID, ColumnRole::Cluster(1, ClusterOrder::Asc)),
        column("parent", UUID, ColumnRole::Regular),
        column("user", TEXT, ColumnRole::Regular),
        column(
            "trigger_depth",
            CqlDesc::Native(CqlNative::SmallInt),
            ColumnRole::Regular,
        ),
        column("data", TEXT, ColumnRole::Regular),
    ],
    ttl: Some(TtlSource {
        config_path: "thorium.events.retention",
    }),
    partition_size: None,
});

/// The `entities_by_name` view the baseline's setup created on `entities`, which the registry
/// doesn't declare
pub(super) const ENTITIES_BY_NAME: ObjectDef = ObjectDef::View(ViewDef {
    name: "entities_by_name",
    base: "entities",
    columns: &[
        column("name", TEXT, ColumnRole::Partition(0)),
        column("group", TEXT, ColumnRole::Cluster(0, ClusterOrder::Asc)),
        column("kind", TEXT, ColumnRole::Cluster(1, ClusterOrder::Asc)),
        column(
            "created",
            TIMESTAMP,
            ColumnRole::Cluster(2, ClusterOrder::Asc),
        ),
        column("year", INT, ColumnRole::Cluster(3, ClusterOrder::Asc)),
        column("bucket", INT, ColumnRole::Cluster(4, ClusterOrder::Asc)),
        column("id", UUID, ColumnRole::Cluster(5, ClusterOrder::Asc)),
        column("submitter", TEXT, ColumnRole::Regular),
        column("kind_data", TEXT, ColumnRole::Regular),
        column("description", TEXT, ColumnRole::Regular),
        column("image", TEXT, ColumnRole::Regular),
    ],
    filter: None,
});

/// The layout every Redis family and S3 bucket role had at the baseline: version 1, except the
/// users family at version 2
const LAYOUT_VERSIONS: &[(SchemaBackend, &str, u32)] = &[
    (SchemaBackend::Redis, "users", 2),
    (SchemaBackend::Redis, "groups", 1),
    (SchemaBackend::Redis, "images", 1),
    (SchemaBackend::Redis, "pipelines", 1),
    (SchemaBackend::Redis, "network_policies", 1),
    (SchemaBackend::Redis, "jobs", 1),
    (SchemaBackend::Redis, "reactions", 1),
    (SchemaBackend::Redis, "streams", 1),
    (SchemaBackend::Redis, "system", 1),
    (SchemaBackend::Redis, "workers", 1),
    (SchemaBackend::Redis, "events", 1),
    (SchemaBackend::Redis, "search_events", 1),
    (SchemaBackend::Redis, "search_streamer_init", 1),
    (SchemaBackend::Redis, "oauth", 1),
    (SchemaBackend::Redis, "vendors", 1),
    (SchemaBackend::Redis, "cursors", 1),
    (SchemaBackend::Redis, "census_samples", 1),
    (SchemaBackend::Redis, "census_repos", 1),
    (SchemaBackend::Redis, "census_commitish", 1),
    (SchemaBackend::Redis, "census_tags", 1),
    (SchemaBackend::Redis, "census_tags_case_insensitive", 1),
    (SchemaBackend::Redis, "census_entities", 1),
    (SchemaBackend::Redis, "census_associations", 1),
    (SchemaBackend::S3, "files", 1),
    (SchemaBackend::S3, "repos", 1),
    (SchemaBackend::S3, "results", 1),
    (SchemaBackend::S3, "attachments", 1),
    (SchemaBackend::S3, "ephemeral", 1),
    (SchemaBackend::S3, "reaction_cache", 1),
    (SchemaBackend::S3, "graphics", 1),
];

/// Build a baseline layout, from the declared layout of its family when there is one
///
/// # Arguments
///
/// * `backend` - The layout's backend
/// * `family` - The Redis family or S3 bucket role
/// * `version` - The layout the baseline wrote, which is also the only one it read
fn baseline_layout(backend: SchemaBackend, family: &'static str, version: u32) -> LayoutDef {
    // keep the declared layout's other properties, which aren't schema
    let declared = LAYOUTS
        .iter()
        .find(|layout| layout.backend == backend && layout.family == family);
    match declared {
        Some(declared) => LayoutDef {
            version,
            reads_from: version,
            ..*declared
        },
        None => LayoutDef {
            backend,
            family,
            version,
            reads_from: version,
            security_sensitive: &[],
            enumerate: None,
        },
    }
}

/// Every baseline object: its Scylla objects in creation order, its Elastic indexes, then its
/// layouts
///
/// # Panics
///
/// Panics if a declared baseline object is no longer declared, which the snapshot tests rule out.
#[must_use]
pub(super) fn baseline_objects() -> Vec<SchemaObject> {
    // the declared Scylla objects the baseline created, with the undeclared view after its base
    // and the undeclared table last
    let declared = SchemaObject::declared();
    let mut objects = Vec::new();
    for name in DECLARED_SCYLLA {
        let object = declared
            .iter()
            .find(|object| object.backend == SchemaBackend::Scylla && object.name == *name)
            .unwrap_or_else(|| panic!("baseline object {name} is declared"));
        objects.push(object.clone());
        if *name == "entities_by_id" {
            objects.push(scylla(ENTITIES_BY_NAME));
        }
    }
    objects.push(scylla(EVENTS));
    // the Elastic indexes kept the mappings the search-streamer created them with
    objects.extend(
        declared
            .into_iter()
            .filter(|object| object.backend == SchemaBackend::Elastic),
    );
    // then each family's layout as the baseline wrote it
    objects.extend(
        LAYOUT_VERSIONS
            .iter()
            .map(|(backend, family, version)| SchemaObject {
                backend: *backend,
                name: (*family).to_owned(),
                definition: SchemaDefinition::Layout(baseline_layout(*backend, family, *version)),
            }),
    );
    objects
}

/// Wrap a baseline Scylla definition as a schema object
///
/// # Arguments
///
/// * `def` - The object's definition
fn scylla(def: ObjectDef) -> SchemaObject {
    // name the object after its definition
    SchemaObject {
        backend: SchemaBackend::Scylla,
        name: def.name().to_owned(),
        definition: SchemaDefinition::Scylla(def),
    }
}

impl RevisionSnapshot {
    /// Build the snapshot of a revision released before the schema registry existed
    ///
    /// # Arguments
    ///
    /// * `revision` - The revision the snapshot is recorded for, one of [`BASELINE_REVISIONS`]
    #[must_use]
    pub fn baseline(revision: RevisionId) -> RevisionSnapshot {
        // fingerprint the baseline objects, sorted by backend and name
        let mut objects = baseline_objects()
            .iter()
            .map(SnapshotObject::of)
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

/// The ordered set of the baseline's Scylla objects, used to check its definitions follow the
/// registry's rules
#[cfg(test)]
pub(super) fn baseline_scylla() -> Vec<ObjectDef> {
    // keep the Scylla definitions in creation order
    baseline_objects()
        .into_iter()
        .filter_map(|object| match object.definition {
            SchemaDefinition::Scylla(def) => Some(def),
            _ => None,
        })
        .collect()
}
