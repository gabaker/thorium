//! Tests of the comparison of an observed schema with a revision snapshot: each kind of
//! difference, view filters as sets, and registered legacy objects

use serde_json::{Value, json};

use super::super::{Fingerprint, RevisionSnapshot, SnapshotObject};
use super::{DriftReport, ObservedSchema, converged_revision};
use crate::models::upgrades::{
    BackendSchemaStatus, Catalog, CatalogEntry, DriftKind, RevisionId, SchemaBackend,
};

/// The snapshot of the declared schema, under a fixed revision
fn snapshot() -> RevisionSnapshot {
    // the revision only labels the report
    RevisionSnapshot::declared(RevisionId::try_from("2026-10-v02".to_owned()).unwrap())
}

/// Observe a set of objects on Scylla and the layout backends
///
/// # Arguments
///
/// * `objects` - The objects observed
fn observe(objects: Vec<SnapshotObject>) -> ObservedSchema {
    // Elastic is left unprobed
    ObservedSchema::new(
        objects,
        &[
            SchemaBackend::Scylla,
            SchemaBackend::Redis,
            SchemaBackend::S3,
        ],
    )
}

/// Rebuild a snapshot entry after changing its canonical form
///
/// # Arguments
///
/// * `object` - The entry to change
/// * `change` - The change to its canonical form
fn edited(object: &SnapshotObject, change: impl FnOnce(&mut Value)) -> SnapshotObject {
    // change the form, then fingerprint it again
    let mut object = object.clone();
    change(&mut object.canonical);
    object.fingerprint = Fingerprint::of(&object.canonical);
    object
}

/// An observed schema equal to the snapshot drifts nowhere, and Elastic, which wasn't probed, is
/// left out rather than reported missing
#[test]
fn a_matching_schema_reports_no_drift() {
    // observe exactly the declared objects
    let report = DriftReport::compare(
        &snapshot(),
        &observe(SnapshotObject::declared().to_vec()),
        &|_, _: &str| false,
    );
    assert_eq!(
        report.backends.keys().copied().collect::<Vec<_>>(),
        [
            SchemaBackend::Scylla,
            SchemaBackend::Redis,
            SchemaBackend::S3
        ]
    );
    // every probed backend is ok and the report names the revision
    for drift in report.backends.values() {
        assert_eq!(drift.status, BackendSchemaStatus::Ok);
        assert_eq!(drift.objects, []);
    }
    assert_eq!(report.revision.as_str(), "2026-10-v02");
    assert_eq!(report.expected, snapshot().deployment);
}

/// A dropped table, a dropped column, an extra table, an extra column, and a changed layout version
/// each come out as their kind, with the expected and observed values and the observed
/// fingerprint, while a view filter with the same conjuncts in another order and case matches
#[test]
fn each_difference_is_reported_as_its_kind() {
    // drop `nodes` and `samples_list.origin`, add `samples_list.preview` and a `tools` table,
    // reorder a view filter, and bump the users layout
    let mut objects = SnapshotObject::declared().to_vec();
    objects.retain(|object| object.name != "nodes");
    for object in &mut objects {
        match (object.backend, object.name.as_str()) {
            (SchemaBackend::Scylla, "samples_list") => {
                *object = edited(object, |form| {
                    let columns = form["columns"].as_array_mut().unwrap();
                    columns.retain(|column| column["name"] != "origin");
                    columns.push(json!({ "name": "preview", "type": "text" }));
                });
            }
            (SchemaBackend::Scylla, "network_policies_default") => {
                *object = edited(object, |form| {
                    form["filter"] = json!("default_policy = TRUE AND name IS NOT NULL");
                });
            }
            (SchemaBackend::Redis, "users") => {
                *object = edited(object, |form| form["users"] = json!(9));
            }
            _ => (),
        }
    }
    let tools = json!({
        "kind": "table", "name": "tools", "columns": [{ "name": "id", "type": "uuid" }],
        "partition_key": ["id"], "clustering_key": [], "static": []
    });
    objects.push(SnapshotObject {
        backend: SchemaBackend::Scylla,
        name: "tools".to_owned(),
        fingerprint: Fingerprint::of(&tools),
        canonical: tools,
    });
    // compare and list each entry with its fields
    let observed = observe(objects);
    let report = DriftReport::compare(&snapshot(), &observed, &|_, _: &str| false);
    let found = |backend: SchemaBackend| {
        report.backends[&backend]
            .objects
            .iter()
            .map(|entry| {
                let field = entry
                    .fields
                    .iter()
                    .map(|field| {
                        (
                            field.field.clone(),
                            field.expected.clone(),
                            field.observed.clone(),
                        )
                    })
                    .collect::<Vec<_>>();
                (
                    entry.object.clone(),
                    entry.kind,
                    field,
                    entry.fingerprint.is_some(),
                )
            })
            .collect::<Vec<_>>()
    };
    // each Scylla difference as its kind, the layout as changed, and S3 untouched
    let some = |text: &str| Some(text.to_owned());
    assert_eq!(
        found(SchemaBackend::Scylla),
        [
            ("nodes".to_owned(), DriftKind::Missing, vec![], false),
            (
                "samples_list.origin".to_owned(),
                DriftKind::Missing,
                vec![("origin".to_owned(), some("text"), None)],
                false
            ),
            (
                "samples_list.preview".to_owned(),
                DriftKind::Unexpected,
                vec![("preview".to_owned(), None, some("text"))],
                true
            ),
            ("tools".to_owned(), DriftKind::Unexpected, vec![], true),
        ]
    );
    assert_eq!(
        found(SchemaBackend::Redis),
        [(
            "users".to_owned(),
            DriftKind::Changed,
            vec![("version".to_owned(), some("2"), some("9"))],
            true
        )]
    );
    assert_eq!(
        report.backends[&SchemaBackend::S3].status,
        BackendSchemaStatus::Ok
    );
    assert_eq!(
        report.backends[&SchemaBackend::Scylla].status,
        BackendSchemaStatus::Drift
    );
}

/// A changed column type is reported once for its field, naming both types and the column's place
/// in the key
#[test]
fn a_changed_column_type_names_both_types() {
    // observe `samples_list.uploaded` as text
    let mut objects = SnapshotObject::declared().to_vec();
    for object in &mut objects {
        if object.backend == SchemaBackend::Scylla && object.name == "samples_list" {
            *object = edited(object, |form| {
                for column in form["columns"].as_array_mut().unwrap() {
                    if column["name"] == "uploaded" {
                        column["type"] = json!("text");
                    }
                }
            });
        }
    }
    // it is one changed field
    let report = DriftReport::compare(&snapshot(), &observe(objects), &|_, _: &str| false);
    let entries = &report.backends[&SchemaBackend::Scylla].objects;
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0].object, "samples_list.uploaded");
    assert_eq!(entries[0].kind, DriftKind::Changed);
    assert_eq!(
        entries[0].fields[0].expected.as_deref(),
        Some("timestamp, clustering key 0 desc")
    );
    assert_eq!(
        entries[0].fields[0].observed.as_deref(),
        Some("text, clustering key 0 desc")
    );
}

/// A registered legacy object absent from the live schema isn't missing against a snapshot that
/// lists it, one present where the snapshot doesn't list it is unexpected but acknowledged, and an
/// object that isn't registered is drift either way
#[test]
fn registered_legacy_objects_are_expected_extras() {
    // a snapshot listing an old table, and a live schema with another old table instead
    let old = json!({
        "kind": "table", "name": "events", "columns": [{ "name": "id", "type": "uuid" }],
        "partition_key": ["id"], "clustering_key": [], "static": []
    });
    let mut expected = snapshot();
    expected.objects.push(SnapshotObject {
        backend: SchemaBackend::Scylla,
        name: "events".to_owned(),
        fingerprint: Fingerprint::of(&old),
        canonical: old.clone(),
    });
    let mut live = SnapshotObject::declared().to_vec();
    let other = edited(
        &SnapshotObject {
            backend: SchemaBackend::Scylla,
            name: "entities_by_name".to_owned(),
            fingerprint: Fingerprint::of(&old),
            canonical: old,
        },
        |form| form["name"] = json!("entities_by_name"),
    );
    live.push(other.clone());
    let observed = observe(live);
    // registered, the absent one is expected and the present one acknowledged
    let legacy = |backend: SchemaBackend, name: &str| {
        backend == SchemaBackend::Scylla && matches!(name, "events" | "entities_by_name")
    };
    let report = DriftReport::compare(&expected, &observed, &legacy);
    let scylla = &report.backends[&SchemaBackend::Scylla];
    assert_eq!(scylla.status, BackendSchemaStatus::Ok);
    assert_eq!(scylla.objects.len(), 1, "{:?}", scylla.objects);
    assert_eq!(scylla.objects[0].object, "entities_by_name");
    assert_eq!(scylla.objects[0].kind, DriftKind::Unexpected);
    assert!(scylla.objects[0].acknowledged);
    assert_eq!(scylla.objects[0].fingerprint, Some(other.fingerprint));
    // unregistered, both are drift
    let report = DriftReport::compare(&expected, &observed, &|_, _: &str| false);
    let scylla = &report.backends[&SchemaBackend::Scylla];
    assert_eq!(scylla.status, BackendSchemaStatus::Drift);
    let kinds = scylla
        .objects
        .iter()
        .map(|entry| (entry.object.as_str(), entry.kind, entry.acknowledged))
        .collect::<Vec<_>>();
    assert_eq!(
        kinds,
        [
            ("entities_by_name", DriftKind::Unexpected, false),
            ("events", DriftKind::Missing, false),
        ]
    );
}

/// The dead `events` table and `entities_by_name` view an older release created are unexpected
/// but acknowledged by the legacy registry against a snapshot that doesn't list them, so they
/// raise no condition; against the `2026-10-v01` and `2026-10-v02` snapshots they match; absent
/// from a keyspace this release created they aren't missing; and the registry holds 18 tables and
/// 12 views and declares neither
#[test]
fn the_dead_objects_are_acknowledged_extras_and_never_declared() {
    // the derives declare 18 tables and 12 views, and neither dead object
    let declared = SnapshotObject::declared();
    let count = |kind: &str| {
        declared
            .iter()
            .filter(|object| {
                object.backend == SchemaBackend::Scylla && object.canonical["kind"] == kind
            })
            .count()
    };
    assert_eq!((count("table"), count("view")), (18, 12));
    assert!(
        declared
            .iter()
            .filter(|object| object.backend == SchemaBackend::Scylla)
            .all(|object| !matches!(object.name.as_str(), "events" | "entities_by_name"))
    );
    // a keyspace an older release created holds both, as the older snapshots list them
    let committed = |revision: &str| {
        RevisionSnapshot::committed(revision)
            .expect("the revision has a snapshot")
            .expect("the snapshot is valid")
    };
    let v02 = committed("2026-10-v02");
    let dead = v02
        .objects
        .iter()
        .filter(|object| {
            object.backend == SchemaBackend::Scylla
                && matches!(object.name.as_str(), "events" | "entities_by_name")
        })
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(dead.len(), 2);
    let mut live = declared.to_vec();
    live.extend(dead);
    let older = observe(live);
    // against the current snapshot, both are unexpected and acknowledged by the registry
    let report = DriftReport::compare(&snapshot(), &older, &crate::models::upgrades::is_legacy);
    let scylla = &report.backends[&SchemaBackend::Scylla];
    assert_eq!(scylla.status, BackendSchemaStatus::Ok);
    let found = scylla
        .objects
        .iter()
        .map(|object| (object.object.as_str(), object.kind, object.acknowledged))
        .collect::<Vec<_>>();
    assert_eq!(
        found,
        [
            ("entities_by_name", DriftKind::Unexpected, true),
            ("events", DriftKind::Unexpected, true),
        ]
    );
    assert!(!report.summary(&[]).holds());
    // against the older snapshots, which list them, they match whether or not they are registered
    for revision in ["2026-10-v01", "2026-10-v02"] {
        let report = DriftReport::compare(&committed(revision), &older, &|_, _: &str| false);
        let scylla = &report.backends[&SchemaBackend::Scylla];
        let named = scylla.objects.iter().map(|object| object.object.as_str());
        assert!(
            !named
                .clone()
                .any(|name| matches!(name, "events" | "entities_by_name")),
            "{revision}: {:?}",
            named.collect::<Vec<_>>()
        );
    }
    // absent from a keyspace this release created, they aren't missing against the older snapshot
    let newer = observe(declared.to_vec());
    let report = DriftReport::compare(&v02, &newer, &crate::models::upgrades::is_legacy);
    let scylla = &report.backends[&SchemaBackend::Scylla];
    assert!(
        scylla
            .objects
            .iter()
            .all(|object| !matches!(object.object.as_str(), "events" | "entities_by_name")),
        "{:?}",
        scylla.objects
    );
}

/// A fingerprint for a fixture revision
///
/// # Arguments
///
/// * `name` - What tells the fingerprints apart
fn print(name: &str) -> Fingerprint {
    // the fingerprint of a canonical form that holds the name
    Fingerprint::of(&json!({ "revision": name }))
}

/// Build a revision of a fixture catalog that reaches `id` from `from` with a schema fingerprint
///
/// # Arguments
///
/// * `from` - The revision it starts from
/// * `id` - The revision it reaches
/// * `schema` - The deployment fingerprint of its snapshot
fn revision(from: Option<&'static str>, id: &'static str, schema: &'static str) -> CatalogEntry {
    // a release edge with no steps
    CatalogEntry {
        from,
        id,
        released_with: "0.0.0",
        description: "A fixture edge",
        schema: Some(schema),
        prerelease: false,
        sibling: false,
        steps: &[],
    }
}

/// While an upgrade runs between the schema apply and the record, the applied fingerprint is the
/// target's and the live schema is compared with the target's snapshot; otherwise with the
/// recorded revision's
#[test]
fn the_applied_fingerprint_picks_the_revision_to_compare_with() {
    // three revisions, the last two sharing a schema, with the fingerprints leaked for the catalog
    let leak = |name: &str| -> &'static str { Box::leak(print(name).as_str().to_owned().into()) };
    let (one, two, three) = (leak("one"), leak("two"), leak("three"));
    let edges: &'static [CatalogEntry] = Box::leak(Box::new([
        revision(None, "2026-10-v01", one),
        revision(Some("2026-10-v01"), "2026-10-v02", two),
        revision(Some("2026-10-v02"), "2026-10-v03", three),
        revision(Some("2026-10-v03"), "2026-10-v04", three),
    ]));
    let catalog = Catalog { edges, floors: &[] };
    let id = |text: &str| RevisionId::try_from(text.to_owned()).unwrap();
    let fingerprint = |name: &str| print(name);
    // the apply of the next revision ran: its fingerprint is applied while the old one is recorded
    assert_eq!(
        converged_revision(&catalog, &id("2026-10-v02"), &fingerprint("three")),
        id("2026-10-v03")
    );
    // the recorded revision's own fingerprint keeps it, even when later revisions share it
    assert_eq!(
        converged_revision(&catalog, &id("2026-10-v03"), &fingerprint("three")),
        id("2026-10-v03")
    );
    assert_eq!(
        converged_revision(&catalog, &id("2026-10-v02"), &fingerprint("two")),
        id("2026-10-v02")
    );
    // a fingerprint no revision names keeps the recorded revision, so the drift shows
    assert_eq!(
        converged_revision(&catalog, &id("2026-10-v02"), &fingerprint("other")),
        id("2026-10-v02")
    );
    // an earlier revision's fingerprint isn't a convergence upward
    assert_eq!(
        converged_revision(&catalog, &id("2026-10-v02"), &fingerprint("one")),
        id("2026-10-v02")
    );
}
