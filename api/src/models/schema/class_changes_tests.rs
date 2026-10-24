//! Tests for classifying schema changes by the changed object's data class

use serde_json::{Value, json};

use super::{
    ChangeClass, ChangeKind, DataClass, Fingerprint, RevisionSnapshot, SchemaBackend, SchemaChange,
    SnapshotObject, ViewChange,
};
use crate::models::upgrades::{SchemaTarget, StepDef, StepKind};

/// Get a copy of every declared object's snapshot entry
fn declared() -> Vec<SnapshotObject> {
    // copy the declared entries so a test can change them
    SnapshotObject::declared().to_vec()
}

/// Change one object's canonical form in the declared snapshot entries, refreshing its
/// fingerprint
///
/// # Arguments
///
/// * `backend` - The changed object's backend
/// * `name` - The changed object's name
/// * `change` - How to change the object's canonical form
fn edit(
    backend: SchemaBackend,
    name: &str,
    change: impl FnOnce(&mut Value),
) -> Vec<SnapshotObject> {
    // find the object and change its canonical form
    let mut objects = declared();
    let object = objects
        .iter_mut()
        .find(|object| object.backend == backend && object.name == name)
        .unwrap_or_else(|| panic!("{name} is declared"));
    change(&mut object.canonical);
    object.fingerprint = Fingerprint::of(&object.canonical);
    objects
}

/// Remove one object from the declared snapshot entries
///
/// # Arguments
///
/// * `backend` - The removed object's backend
/// * `name` - The removed object's name
fn without(backend: SchemaBackend, name: &str) -> Vec<SnapshotObject> {
    // drop the object from a copy of the declared entries
    let mut objects = declared();
    objects.retain(|object| !(object.backend == backend && object.name == name));
    objects
}

/// Change the type of a Scylla object's column to `int`
///
/// # Arguments
///
/// * `column` - The regular text column to change
fn retype(column: &str) -> impl FnOnce(&mut Value) + '_ {
    // find the column by name and give it another type
    move |canonical: &mut Value| {
        let columns = canonical["columns"].as_array_mut().expect("columns");
        let found = columns
            .iter_mut()
            .find(|found| found["name"] == column)
            .expect("the column exists");
        found["type"] = json!("int");
    }
}

/// Remove a column from a Scylla object
///
/// # Arguments
///
/// * `column` - The regular column to remove
fn drop_column(column: &str) -> impl FnOnce(&mut Value) + '_ {
    // keep every other column
    move |canonical: &mut Value| {
        let columns = canonical["columns"].as_array_mut().expect("columns");
        columns.retain(|found| found["name"] != column);
    }
}

/// Flip a Scylla object's first clustering order
///
/// # Arguments
///
/// * `canonical` - The object's canonical form
fn flip_order(canonical: &mut Value) {
    // swap ascending and descending
    let first = &mut canonical["clustering_key"][0];
    let flipped = if first["order"] == "asc" {
        "desc"
    } else {
        "asc"
    };
    first["order"] = json!(flipped);
}

/// Bump a layout's version past what the new code reads
///
/// # Arguments
///
/// * `family` - The layout family
fn bump(family: &str) -> impl FnOnce(&mut Value) + '_ {
    // a layout's canonical form maps its family to its version
    move |canonical: &mut Value| {
        let version = canonical[family].as_u64().expect("a version");
        *canonical = json!({ family: version + 1 });
    }
}

/// Diff the declared schema to a changed one, treating `samples_list` as `Expiring` and every
/// other object as the class table says
///
/// # Arguments
///
/// * `new` - The changed schema
fn classes_with_expiring_samples(new: &[SnapshotObject]) -> Vec<ChangeClass> {
    // classify with a lookup that marks samples_list as expiring
    SchemaChange::between_classified(
        &declared(),
        new,
        |_, _| None,
        |backend, name| {
            if backend == SchemaBackend::Scylla && name == "samples_list" {
                Some(DataClass::Expiring)
            } else {
                DataClass::of(backend, name)
            }
        },
    )
    .into_iter()
    .map(|change| change.class)
    .collect()
}

/// Lists the classes of the changes between the declared schema and a changed one, using the
/// class table
///
/// # Arguments
///
/// * `new` - The changed schema
fn classes(new: &[SnapshotObject]) -> Vec<ChangeClass> {
    // classify with the class table
    SchemaChange::between(&declared(), new)
        .into_iter()
        .map(|change| change.class)
        .collect()
}

/// A layout change, column type change, key change, or removal of an `Expiring` object is
/// informational, covered without a step and quiet in the CI report, while the same change of a
/// `Core` object is breaking or contract as before
#[test]
fn expiring_changes_are_informational() {
    use ChangeClass::{Breaking, Contract, Informational};
    use SchemaBackend::{Redis, Scylla};
    // a layout version the new code can't read, on the expiring cursors family
    let changes = SchemaChange::between(&declared(), &edit(Redis, "cursors", bump("cursors")));
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].class, Informational);
    assert_eq!(
        changes[0].kind,
        ChangeKind::LayoutChanged {
            from: 2,
            to: 3,
            reads_old: false
        }
    );
    assert!(changes[0].is_covered(std::iter::empty()));
    assert!(!changes[0].class.warns());
    assert_eq!(changes[0].default_step, StepKind::Infra);
    assert!(!changes[0].default_step.needs_approval());
    // removing the expiring family is informational too
    assert_eq!(classes(&without(Redis, "cursors")), [Informational]);
    // the same bump and removal of a core family stay breaking and contract
    assert_eq!(classes(&edit(Redis, "users", bump("users"))), [Breaking]);
    assert_eq!(classes(&without(Redis, "users")), [Contract]);
    // a column type change, a key change, a column removal and a table removal of an expiring
    // table are informational, and breaking or contract on the same table as core data
    let retyped = edit(Scylla, "samples_list", retype("submitter"));
    let flipped = edit(Scylla, "samples_list", flip_order);
    let dropped = edit(Scylla, "samples_list", drop_column("submitter"));
    let removed = without(Scylla, "samples_list");
    for (new, core) in [
        (&retyped, Breaking),
        (&flipped, Breaking),
        (&dropped, Contract),
    ] {
        assert_eq!(classes_with_expiring_samples(new), [Informational]);
        assert_eq!(classes(new), [core]);
    }
    // removing the expiring table is informational, and a contract change as core data
    assert_eq!(classes_with_expiring_samples(&removed), [Informational]);
    assert_eq!(classes(&removed), [Contract]);
    // every class but informational warns
    for class in [ChangeClass::Additive, Breaking, Contract] {
        assert!(class.warns(), "{class:?}");
    }
    // the committed v02 snapshot holds cursors at layout 1, so the report lists the bump to 2 as
    // informational, and so is the removal of the legacy events table and entities_by_name view
    let v02 = RevisionSnapshot::committed("2026-10-v02")
        .expect("listed")
        .expect("valid");
    let unreleased = SchemaChange::unreleased(&v02);
    let class_of = |object: &str| {
        unreleased
            .iter()
            .find(|change| change.object == object && change.field.is_none())
            .map(|change| change.class)
    };
    assert_eq!(class_of("cursors"), Some(Informational));
    assert_eq!(class_of("events"), Some(Informational));
    assert_eq!(class_of("entities_by_name"), Some(Informational));
}

/// Removing an object registered in `LEGACY` is informational, needing no step and raising no
/// warning, while removing an object nothing registers is a contract change
#[test]
fn legacy_removal_is_informational_and_unregistered_removal_is_contract() {
    use ChangeClass::{Contract, Informational};
    // the released snapshot still lists the two legacy objects, which this build doesn't declare
    let v02 = RevisionSnapshot::committed("2026-10-v02")
        .expect("listed")
        .expect("valid");
    let whole = |changes: &[SchemaChange], object: &str| {
        changes
            .iter()
            .filter(|change| change.object == object && change.field.is_none())
            .cloned()
            .collect::<Vec<_>>()
    };
    let changes = SchemaChange::between(&v02.objects, &declared());
    for object in ["events", "entities_by_name"] {
        let found = whole(&changes, object);
        assert_eq!(found.len(), 1, "{object}");
        assert_eq!(found[0].kind, ChangeKind::Removed, "{object}");
        assert_eq!(found[0].class, Informational, "{object}");
        assert_eq!(found[0].default_step, StepKind::Infra, "{object}");
        assert!(found[0].is_covered(std::iter::empty()), "{object}");
        assert!(!found[0].class.warns(), "{object}");
    }
    // the same table under a name nobody registered is a contract removal
    let mut unregistered = v02.objects.clone();
    for object in &mut unregistered {
        if object.backend == SchemaBackend::Scylla && object.name == "events" {
            object.name = "tool_runs".to_owned();
        }
    }
    let changes = SchemaChange::between(&unregistered, &declared());
    let found = whole(&changes, "tool_runs");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].class, Contract);
    assert!(!found[0].is_covered(std::iter::empty()));
    // a Redis family named like the legacy table isn't registered: its removal is a contract change
    let changes = SchemaChange::between(&declared(), &without(SchemaBackend::Redis, "events"));
    let found = whole(&changes, "events");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].class, Contract);
}

/// Build a `Data` step targeting one object
///
/// # Arguments
///
/// * `targets` - The objects the step targets
fn data_step(targets: &'static [SchemaTarget]) -> StepDef {
    // a data step running a migration on its targets
    StepDef {
        id: "class-step",
        kind: StepKind::Data,
        description: "A fixture step for data class coverage",
        thoradm_migration: Some("fixture-migration"),
        targets,
        ..StepDef::BASE
    }
}

/// A step target naming the reactions layout family
const REACTIONS: &[SchemaTarget] = &[SchemaTarget {
    backend: SchemaBackend::Redis,
    object: "reactions",
    field: None,
}];

/// A step target naming the samples census layout family
const CENSUS_SAMPLES: &[SchemaTarget] = &[SchemaTarget {
    backend: SchemaBackend::Redis,
    object: "census_samples",
    field: None,
}];

/// A step target naming the logs table
const LOGS: &[SchemaTarget] = &[SchemaTarget {
    backend: SchemaBackend::Scylla,
    object: "logs",
    field: None,
}];

/// A breaking change of a `Runtime` object and of a `Derived` object each needs a `Data` step:
/// without one it is uncovered (the CI report's second warning), and a `Data` step targeting the
/// object covers it
#[test]
fn runtime_and_derived_changes_need_a_data_step() {
    use SchemaBackend::{Redis, Scylla};
    // the runtime reactions family, the derived samples census, and the runtime logs table
    assert_eq!(DataClass::of(Redis, "reactions"), Some(DataClass::Runtime));
    assert_eq!(
        DataClass::of(Redis, "census_samples"),
        Some(DataClass::Derived)
    );
    assert_eq!(DataClass::of(Scylla, "logs"), Some(DataClass::Runtime));
    let cases = [
        (edit(Redis, "reactions", bump("reactions")), REACTIONS),
        (
            edit(Redis, "census_samples", bump("census_samples")),
            CENSUS_SAMPLES,
        ),
        (edit(Scylla, "logs", retype("line")), LOGS),
    ];
    for (new, targets) in cases {
        let changes = SchemaChange::between(&declared(), &new);
        assert_eq!(changes.len(), 1, "{changes:?}");
        let change = &changes[0];
        // breaking, warned about, and needing a data step
        assert_eq!(change.class, ChangeClass::Breaking, "{}", change.object);
        assert!(change.class.warns());
        assert_eq!(change.default_step, StepKind::Data);
        assert!(!change.is_covered(std::iter::empty()), "{}", change.object);
        // a data step targeting the object covers it, one targeting another object doesn't
        assert!(
            change.is_covered([&data_step(targets)]),
            "{}",
            change.object
        );
        let other = if std::ptr::eq(targets, LOGS) {
            REACTIONS
        } else {
            LOGS
        };
        assert!(!change.is_covered([&data_step(other)]), "{}", change.object);
    }
}

/// A change of `Runtime` or `Derived` data defaults to a `Data` step, which needs the same
/// approval naming a backup as one on `Core` data: the approval rule follows the step kind, not
/// the data class
#[test]
fn runtime_and_derived_data_steps_need_the_same_approval() {
    use SchemaBackend::Redis;
    // a breaking change of core, runtime and derived data
    for family in ["users", "reactions", "census_samples"] {
        let changes = SchemaChange::between(&declared(), &edit(Redis, family, bump(family)));
        assert_eq!(changes.len(), 1, "{family}");
        // each defaults to a data step, and every data step needs an approval with a backup
        assert_eq!(changes[0].default_step, StepKind::Data, "{family}");
        assert!(changes[0].default_step.needs_approval(), "{family}");
    }
}

/// Every declared view is `Derived` data: a breaking change of one keeps its class, defaults to a
/// `Data` step that rebuilds the view, and needs the same approval as a change of `Core` data
#[test]
fn derived_view_changes_rebuild_with_the_same_approval() {
    use SchemaBackend::Scylla;
    // every declared view is derived from its base table
    for object in SnapshotObject::declared() {
        if object.backend == Scylla && object.canonical.get("base").is_some() {
            assert_eq!(
                DataClass::of(Scylla, &object.name),
                Some(DataClass::Derived),
                "{}",
                object.name
            );
        }
    }
    // a column added to the samples view is a breaking change of derived data
    let new = edit(Scylla, "samples", |canonical| {
        canonical["columns"]
            .as_array_mut()
            .expect("columns")
            .push(json!({ "name": "note", "type": "text" }));
    });
    let changes = SchemaChange::between(&declared(), &new);
    assert_eq!(changes.len(), 1, "{changes:?}");
    let change = &changes[0];
    assert_eq!(change.class, ChangeClass::Breaking);
    assert!(change.class.warns());
    // it defaults to a data step, which needs the same approval as one on core data
    assert_eq!(change.default_step, StepKind::Data);
    assert!(change.default_step.needs_approval());
    // and it reaches the new definition by rebuilding the view
    assert_eq!(
        ViewChange::for_change(change, &declared(), |_| false),
        Some(ViewChange::Recreate)
    );
    assert_eq!(
        ViewChange::Recreate.steps(),
        [(ChangeClass::Breaking, StepKind::Data)]
    );
}
