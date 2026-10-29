//! Tests of the comparison of an observed schema with a revision snapshot: each kind of
//! difference, view filters as sets, and registered legacy objects

use serde_json::{Value, json};

use super::super::{Fingerprint, RevisionSnapshot, SnapshotObject};
use super::{DriftReport, ObservedSchema, converged_revision, view_differences};
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
    let absent = edited(
        &SnapshotObject {
            backend: SchemaBackend::Scylla,
            name: "tool_runs".to_owned(),
            fingerprint: Fingerprint::of(&old),
            canonical: old.clone(),
        },
        |form| form["name"] = json!("tool_runs"),
    );
    expected.objects.push(absent);
    let mut live = SnapshotObject::declared().to_vec();
    let other = SnapshotObject {
        backend: SchemaBackend::Scylla,
        name: "events".to_owned(),
        fingerprint: Fingerprint::of(&old),
        canonical: old,
    };
    live.push(other.clone());
    let observed = observe(live);
    // registered, the absent one is expected and the present one acknowledged
    let legacy = |backend: SchemaBackend, name: &str| {
        backend == SchemaBackend::Scylla && matches!(name, "events" | "tool_runs")
    };
    let report = DriftReport::compare(&expected, &observed, &legacy);
    let scylla = &report.backends[&SchemaBackend::Scylla];
    assert_eq!(scylla.status, BackendSchemaStatus::Ok);
    assert_eq!(scylla.objects.len(), 1, "{:?}", scylla.objects);
    assert_eq!(scylla.objects[0].object, "events");
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
            ("events", DriftKind::Unexpected, false),
            ("tool_runs", DriftKind::Missing, false),
        ]
    );
}

/// The dead `events` table an older release created is unexpected but acknowledged by the
/// legacy registry against a snapshot that doesn't list it, so it raises no condition; against
/// the `2026-10-v01` and `2026-10-v02` snapshots the dead objects match; absent from a keyspace
/// this release created they aren't missing; and the registry holds 19 tables and 13 views,
/// declaring the `entities_by_name` view that came back but never the `events` table
#[test]
fn the_dead_objects_are_acknowledged_extras_and_never_declared() {
    // the derives declare 19 tables and 13 views, and never the dead events table
    let declared = SnapshotObject::declared();
    let count = |kind: &str| {
        declared
            .iter()
            .filter(|object| {
                object.backend == SchemaBackend::Scylla && object.canonical["kind"] == kind
            })
            .count()
    };
    assert_eq!((count("table"), count("view")), (19, 13));
    assert!(
        declared
            .iter()
            .filter(|object| object.backend == SchemaBackend::Scylla)
            .all(|object| object.name != "events")
    );
    // the view yara's exact-name reads brought back is declared again
    assert!(
        declared
            .iter()
            .any(|object| object.backend == SchemaBackend::Scylla
                && object.name == "entities_by_name")
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
        .filter(|object| object.backend == SchemaBackend::Scylla && object.name == "events")
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(dead.len(), 1);
    let mut live = declared.to_vec();
    live.extend(dead);
    let older = observe(live);
    // against the current snapshot, the dead table is unexpected and acknowledged
    let report = DriftReport::compare(&snapshot(), &older, &crate::models::upgrades::is_legacy);
    let scylla = &report.backends[&SchemaBackend::Scylla];
    assert_eq!(scylla.status, BackendSchemaStatus::Ok);
    let found = scylla
        .objects
        .iter()
        .map(|object| (object.object.as_str(), object.kind, object.acknowledged))
        .collect::<Vec<_>>();
    assert_eq!(found, [("events", DriftKind::Unexpected, true)]);
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

/// A Scylla table entry with one key column, standing in for a table an older release created
///
/// # Arguments
///
/// * `name` - The table's name
fn retired_table(name: &str) -> SnapshotObject {
    // only the name matters to the comparison of an object the snapshot doesn't list
    let canonical = json!({
        "kind": "table", "name": name, "columns": [{ "name": "id", "type": "uuid" }],
        "partition_key": ["id"], "clustering_key": [], "static": []
    });
    SnapshotObject {
        backend: SchemaBackend::Scylla,
        name: name.to_owned(),
        fingerprint: Fingerprint::of(&canonical),
        canonical,
    }
}

/// A keyspace an older release's setup created, converted by `thoradm schema record` and recorded
/// at `2026-10-v01`, holds the migration-state tables and the tables and view releases before the
/// first revision created; against the `2026-10-v01` snapshot, which lists none of them, the
/// state tables aren't reported and the retired objects are acknowledged legacy, so the keyspace
/// drifts only when it holds a table nothing registers
#[test]
fn a_converted_keyspace_drifts_only_for_unregistered_tables() {
    // the objects the setup statements created are what the v01 snapshot lists
    let v01 = RevisionSnapshot::committed("2026-10-v01")
        .expect("the revision has a snapshot")
        .expect("the snapshot is valid");
    let mut live = v01.objects.clone();
    // `schema record` creates the migration-state tables as this build declares them
    live.extend(
        SnapshotObject::declared()
            .iter()
            .filter(|object| {
                matches!(
                    object.name.as_str(),
                    "schema_migrations" | "schema_migration_issues"
                )
            })
            .cloned(),
    );
    assert!(
        !v01.objects
            .iter()
            .any(|object| object.name.starts_with("schema_migration"))
    );
    // the tables and view releases before the first revision created
    let retired = [
        "commits",
        "commits_list",
        "cursors",
        "simple_cursors",
        "workers",
        "exports",
        "export_errors",
        "export_error_ids",
    ];
    live.extend(retired.iter().map(|name| retired_table(name)));
    // compared with the v01 snapshot, nothing drifts
    let legacy = &crate::models::upgrades::is_legacy;
    let report = DriftReport::compare(&v01, &observe(live.clone()), legacy);
    let scylla = &report.backends[&SchemaBackend::Scylla];
    assert_eq!(
        scylla.status,
        BackendSchemaStatus::Ok,
        "{:?}",
        scylla.objects
    );
    let mut found = scylla
        .objects
        .iter()
        .map(|object| (object.object.as_str(), object.kind, object.acknowledged))
        .collect::<Vec<_>>();
    found.sort_unstable_by_key(|(name, _, _)| *name);
    let mut expected = retired
        .iter()
        .map(|name| (*name, DriftKind::Unexpected, true))
        .collect::<Vec<_>>();
    expected.sort_unstable_by_key(|(name, _, _)| *name);
    assert_eq!(found, expected);
    // a table nothing registers still drifts
    live.push(retired_table("tools"));
    let report = DriftReport::compare(&v01, &observe(live), legacy);
    let scylla = &report.backends[&SchemaBackend::Scylla];
    assert_eq!(scylla.status, BackendSchemaStatus::Drift);
    let unacknowledged = scylla
        .objects
        .iter()
        .filter(|object| !object.acknowledged)
        .map(|object| (object.object.as_str(), object.kind))
        .collect::<Vec<_>>();
    assert_eq!(unacknowledged, [("tools", DriftKind::Unexpected)]);
}

/// The migration-state tables are never drift, whether the snapshot lists them and the keyspace
/// lacks them or the keyspace holds them in another form than the snapshot's
#[test]
fn the_migration_state_tables_are_never_drift() {
    // a snapshot that lists the state tables, against a keyspace without them
    let declared = snapshot();
    let without = SnapshotObject::declared()
        .iter()
        .filter(|object| !object.name.starts_with("schema_migration"))
        .cloned()
        .collect::<Vec<_>>();
    let report = DriftReport::compare(&declared, &observe(without), &|_, _: &str| false);
    assert_eq!(report.backends[&SchemaBackend::Scylla].objects, []);
    // and against a keyspace whose record table has an extra column
    let changed = SnapshotObject::declared()
        .iter()
        .map(|object| match object.name.as_str() {
            "schema_migrations" => edited(object, |form| {
                let columns = form["columns"].as_array_mut().unwrap();
                columns.push(json!({ "name": "extra", "type": "text" }));
            }),
            _ => object.clone(),
        })
        .collect::<Vec<_>>();
    let report = DriftReport::compare(&declared, &observe(changed), &|_, _: &str| false);
    assert_eq!(report.backends[&SchemaBackend::Scylla].objects, []);
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

/// The declared canonical form of a view
///
/// # Arguments
///
/// * `name` - The view's name
fn declared_view(name: &str) -> Value {
    // the declared entries hold every view
    SnapshotObject::declared()
        .iter()
        .find(|object| object.backend == SchemaBackend::Scylla && object.name == name)
        .unwrap_or_else(|| panic!("{name} is declared"))
        .canonical
        .clone()
}

/// A live view with the declared definition differs in nothing, even when Scylla renders its
/// filter with every key column's `IS NOT NULL` in another order and case
#[test]
fn a_view_as_declared_has_no_differences() {
    // the live form spells out the key columns' mandatory filters
    let declared = declared_view("committed_repo_data");
    let mut live = declared.clone();
    live["filter"] = json!(
        "timestamp IS NOT NULL AND KIND is not null AND \"group\" IS NOT NULL AND repo IS NOT \
         NULL AND repo_data IS NOT NULL AND key IS NOT NULL AND year IS NOT NULL AND bucket IS \
         NOT NULL"
    );
    assert_eq!(view_differences(&declared, &live), Vec::<String>::new());
    assert_eq!(view_differences(&declared, &declared), Vec::<String>::new());
}

/// Every declared view compared with itself differs in nothing
#[test]
fn every_declared_view_matches_itself() {
    // each declared view's form against itself
    for object in SnapshotObject::declared() {
        if object.canonical["kind"] == "view" {
            assert!(
                view_differences(&object.canonical, &object.canonical).is_empty(),
                "{}",
                object.name
            );
        }
    }
}

/// The old `committed_repo_data` on `commits_list` lists its base, key, and columns as
/// differences, live first and declared second
#[test]
fn an_old_view_lists_its_base_key_and_columns() {
    // the view as releases before commitishes created it
    let declared = declared_view("committed_repo_data");
    let live = json!({
        "kind": "view",
        "name": "committed_repo_data",
        "base": "commits_list",
        "columns": [
            {"name": "bucket", "type": "int"},
            {"name": "group", "type": "text"},
            {"name": "hash", "type": "text"},
            {"name": "repo", "type": "text"},
            {"name": "repo_data", "type": "text"},
            {"name": "timestamp", "type": "timestamp"},
            {"name": "year", "type": "int"},
        ],
        "partition_key": ["repo_data", "year", "bucket"],
        "clustering_key": [
            {"name": "group", "order": "asc"},
            {"name": "repo", "order": "asc"},
            {"name": "hash", "order": "asc"},
            {"name": "timestamp", "order": "asc"},
        ],
        "filter": "group IS NOT NULL AND repo IS NOT NULL AND repo_data IS NOT NULL AND hash IS \
                   NOT NULL AND year IS NOT NULL AND bucket IS NOT NULL AND timestamp IS NOT NULL",
    });
    let differences = view_differences(&declared, &live);
    assert_eq!(
        differences,
        vec![
            "base: commits_list -> commitish_list".to_owned(),
            "clustering_key: (group asc, repo asc, hash asc, timestamp asc) -> (kind asc, group \
             asc, repo asc, key asc, timestamp asc)"
                .to_owned(),
            "columns: -hash, +key, +kind".to_owned(),
        ]
    );
}

/// A filter on a column outside the key, as views created by older releases carry, is a
/// difference
#[test]
fn a_filter_on_a_column_outside_the_key_differs() {
    // the samples view with an extra `IS NOT NULL` on a regular column
    let declared = declared_view("samples");
    let mut live = declared.clone();
    live["filter"] = json!("sha256 IS NOT NULL AND sha1 IS NOT NULL");
    let differences = view_differences(&declared, &live);
    assert_eq!(differences.len(), 1, "{differences:?}");
    assert!(
        differences[0].starts_with("filter: sha1 is not null -> "),
        "{differences:?}"
    );
}

/// A clustering order or a column type that differs is a difference
#[test]
fn an_order_or_type_change_differs() {
    // flip the first clustering column's order and change one column's type
    let declared = declared_view("samples");
    let mut live = declared.clone();
    live["clustering_key"][0]["order"] =
        json!(if declared["clustering_key"][0]["order"] == "asc" {
            "desc"
        } else {
            "asc"
        });
    live["columns"][0]["type"] = json!("blob");
    let differences = view_differences(&declared, &live);
    assert_eq!(differences.len(), 2, "{differences:?}");
    assert!(differences[0].starts_with("clustering_key: "));
    assert!(differences[1].starts_with("columns: "));
}
