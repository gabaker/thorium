//! Tests for diffing two schemas and classifying each difference

use serde_json::{Value, json};

use super::{
    ChangeClass, ChangeKind, Fingerprint, RevisionSnapshot, SchemaBackend, SchemaChange,
    SnapshotObject, ViewChange, rebuilds, views_read,
};
use crate::models::upgrades::{SchemaTarget, StepDef, StepKind};

/// A step target naming the `submitter` column of `samples_list`
const SUBMITTER: &[SchemaTarget] = &[SchemaTarget {
    backend: SchemaBackend::Scylla,
    object: "samples_list",
    field: Some("submitter"),
}];

/// A step target naming the whole `samples_list` table
const SAMPLES_LIST: &[SchemaTarget] = &[SchemaTarget {
    backend: SchemaBackend::Scylla,
    object: "samples_list",
    field: None,
}];

/// A step target naming another column of `samples_list`, its `origin`
const ORIGIN: &[SchemaTarget] = &[SchemaTarget {
    backend: SchemaBackend::Scylla,
    object: "samples_list",
    field: Some("origin"),
}];

/// A step target naming another table, `nodes`
const NODES: &[SchemaTarget] = &[SchemaTarget {
    backend: SchemaBackend::Scylla,
    object: "nodes",
    field: None,
}];

/// Get a copy of every declared object's snapshot entry
fn declared() -> Vec<SnapshotObject> {
    // copy the declared entries so a test can change them
    SnapshotObject::declared().to_vec()
}

/// Change one object's canonical form in a list of snapshot entries, refreshing its fingerprint
///
/// # Arguments
///
/// * `objects` - The entries to change
/// * `backend` - The changed object's backend
/// * `name` - The changed object's name
/// * `change` - How to change the object's canonical form
fn edit(
    mut objects: Vec<SnapshotObject>,
    backend: SchemaBackend,
    name: &str,
    change: impl FnOnce(&mut Value),
) -> Vec<SnapshotObject> {
    // find the object and change its canonical form
    let object = objects
        .iter_mut()
        .find(|object| object.backend == backend && object.name == name)
        .unwrap_or_else(|| panic!("{name} is declared"));
    change(&mut object.canonical);
    object.fingerprint = Fingerprint::of(&object.canonical);
    objects
}

/// Remove one object from a list of snapshot entries
///
/// # Arguments
///
/// * `objects` - The entries to remove the object from
/// * `backend` - The removed object's backend
/// * `name` - The removed object's name
fn without(
    mut objects: Vec<SnapshotObject>,
    backend: SchemaBackend,
    name: &str,
) -> Vec<SnapshotObject> {
    // drop the object, which must have been there
    let before = objects.len();
    objects.retain(|object| !(object.backend == backend && object.name == name));
    assert_eq!(objects.len(), before - 1, "{name} is declared");
    objects
}

/// Get a Scylla object's column list from its canonical form
///
/// # Arguments
///
/// * `canonical` - The object's canonical form
fn columns(canonical: &mut Value) -> &mut Vec<Value> {
    // the columns are a list of name and type pairs
    canonical["columns"].as_array_mut().expect("columns")
}

/// Change the `submitter` column of `samples_list` in the declared snapshot entries
///
/// # Arguments
///
/// * `change` - How to change the column's canonical form
fn edit_submitter(change: impl FnOnce(&mut Value)) -> Vec<SnapshotObject> {
    // find the column in the table's canonical form and change it
    edit(
        declared(),
        SchemaBackend::Scylla,
        "samples_list",
        |canonical| {
            let column = columns(canonical)
                .iter_mut()
                .find(|column| column["name"] == "submitter")
                .expect("submitter");
            change(column);
        },
    )
}

/// Flip the order of a Scylla object's first clustering column in its canonical form
///
/// # Arguments
///
/// * `canonical` - The object's canonical form
fn flip_first_order(canonical: &mut Value) {
    // swap ascending and descending
    let first = &mut canonical["clustering_key"][0];
    let flipped = if first["order"] == "asc" {
        "desc"
    } else {
        "asc"
    };
    first["order"] = json!(flipped);
}

/// Build the change a test expects
///
/// # Arguments
///
/// * `backend` - The changed object's backend
/// * `object` - The changed object's name
/// * `field` - The changed field, if any
/// * `kind` - What changed
/// * `class` - The expected class
fn change(
    backend: SchemaBackend,
    object: &str,
    field: Option<&str>,
    kind: ChangeKind,
    class: ChangeClass,
) -> SchemaChange {
    // a change's default step follows from its class
    let default_step = match class {
        ChangeClass::Additive | ChangeClass::Informational => StepKind::Infra,
        ChangeClass::Breaking | ChangeClass::Contract => StepKind::Data,
    };
    SchemaChange {
        backend,
        object: object.to_owned(),
        field: field.map(str::to_owned),
        kind,
        class,
        default_step,
    }
}

/// Each additive row of the classification table (a new table, view, column, Elastic index,
/// mapping property, bucket, and a layout version that still reads the old layout) is classified
/// `Additive` with the default step `Infra`
#[test]
fn additive_changes_converge_automatically() {
    use ChangeClass::Additive;
    use ChangeKind::Added;
    use SchemaBackend::{Elastic, S3, Scylla};
    // the same schema on both sides has no changes
    assert_eq!(SchemaChange::between(&declared(), &declared()), Vec::new());
    // a new table
    let old = without(declared(), Scylla, "nodes");
    assert_eq!(
        SchemaChange::between(&old, &declared()),
        [change(Scylla, "nodes", None, Added, Additive)]
    );
    // a new view
    let old = without(declared(), Scylla, "samples");
    assert_eq!(
        SchemaChange::between(&old, &declared()),
        [change(Scylla, "samples", None, Added, Additive)]
    );
    // a new column
    let new = edit(declared(), Scylla, "samples_list", |canonical| {
        columns(canonical).push(json!({ "name": "note", "type": "text" }));
    });
    assert_eq!(
        SchemaChange::between(&declared(), &new),
        [change(
            Scylla,
            "samples_list",
            Some("note"),
            Added,
            Additive
        )]
    );
    // a new Elastic index
    let old = without(declared(), Elastic, "RepoTags");
    assert_eq!(
        SchemaChange::between(&old, &declared()),
        [change(Elastic, "RepoTags", None, Added, Additive)]
    );
    // a new mapping property, nested properties named by their path
    let new = edit(declared(), Elastic, "SampleResults", |canonical| {
        canonical["body"]["mappings"]["properties"]["origin"] =
            json!({ "properties": { "tool": { "type": "keyword" } } });
    });
    assert_eq!(
        SchemaChange::between(&declared(), &new),
        [change(
            Elastic,
            "SampleResults",
            Some("origin"),
            Added,
            Additive
        )]
    );
    let deeper = edit(new.clone(), Elastic, "SampleResults", |canonical| {
        canonical["body"]["mappings"]["properties"]["origin"]["properties"]["kind"] =
            json!({ "type": "keyword" });
    });
    assert_eq!(
        SchemaChange::between(&new, &deeper),
        [change(
            Elastic,
            "SampleResults",
            Some("origin.kind"),
            Added,
            Additive
        )]
    );
    // a new bucket role
    let old = without(declared(), S3, "graphics");
    assert_eq!(
        SchemaChange::between(&old, &declared()),
        [change(S3, "graphics", None, Added, Additive)]
    );
    // a layout version the new code writes while it still reads the old one
    let new = edit(declared(), SchemaBackend::Redis, "users", |canonical| {
        *canonical = json!({ "users": 3 });
    });
    let changes = SchemaChange::between_with(&declared(), &new, |backend, family| {
        (backend == SchemaBackend::Redis && family == "users").then_some(2)
    });
    let bump = ChangeKind::LayoutChanged {
        from: 2,
        to: 3,
        reads_old: true,
    };
    assert_eq!(
        changes,
        [change(SchemaBackend::Redis, "users", None, bump, Additive)]
    );
    // the report's diff from the latest snapshot reads the declared layouts the same way
    let latest = RevisionSnapshot::declared("2030-01-v01".parse().expect("a valid revision"));
    assert_eq!(SchemaChange::unreleased_to(&latest, &new), changes);
    // every additive change converges without an approval
    assert!(
        changes
            .iter()
            .all(|change| !change.default_step.needs_approval())
    );
    assert_eq!(ChangeClass::Additive.default_step(), StepKind::Infra);
}

/// Each breaking row (a type change, a key change, a removal, a rename, and a layout version that
/// can't read the old layout) is classified `Breaking` with an approved `Data` step, and removals
/// and renames are marked contract
#[test]
fn breaking_changes_need_an_approved_data_step() {
    use ChangeClass::{Additive, Breaking, Contract};
    use ChangeKind::{Added, KeyChanged, Removed};
    use SchemaBackend::Scylla;
    // a column type change
    let new = edit_submitter(|column| column["type"] = json!("int"));
    let typed = ChangeKind::TypeChanged {
        from: "text".to_owned(),
        to: "int".to_owned(),
    };
    assert_eq!(
        SchemaChange::between(&declared(), &new),
        [change(
            Scylla,
            "samples_list",
            Some("submitter"),
            typed,
            Breaking
        )]
    );
    // a regular column moved into the clustering key
    let new = edit(declared(), Scylla, "samples_list", |canonical| {
        canonical["clustering_key"]
            .as_array_mut()
            .expect("clustering key")
            .push(json!({ "name": "submitter", "order": "asc" }));
    });
    assert_eq!(
        SchemaChange::between(&declared(), &new),
        [change(
            Scylla,
            "samples_list",
            Some("submitter"),
            KeyChanged,
            Breaking
        )]
    );
    // a flipped clustering order
    let new = edit(declared(), Scylla, "samples_list", |canonical| {
        flip_first_order(canonical);
    });
    let changes = SchemaChange::between(&declared(), &new);
    assert_eq!(changes.len(), 1);
    assert_eq!(
        (changes[0].kind.clone(), changes[0].class),
        (KeyChanged, Breaking)
    );
    // a removed column and a removed table are contract changes
    let new = edit(declared(), Scylla, "samples_list", |canonical| {
        columns(canonical).retain(|column| column["name"] != "submitter");
    });
    assert_eq!(
        SchemaChange::between(&declared(), &new),
        [change(
            Scylla,
            "samples_list",
            Some("submitter"),
            Removed,
            Contract
        )]
    );
    let new = without(declared(), Scylla, "nodes");
    assert_eq!(
        SchemaChange::between(&declared(), &new),
        [change(Scylla, "nodes", None, Removed, Contract)]
    );
    // a renamed column shows as a contract removal and an additive addition
    let new = edit_submitter(|column| column["name"] = json!("uploader"));
    assert_eq!(
        SchemaChange::between(&declared(), &new),
        [
            change(Scylla, "samples_list", Some("submitter"), Removed, Contract),
            change(Scylla, "samples_list", Some("uploader"), Added, Additive),
        ]
    );
    // a layout version that can't read the old layout
    let new = edit(declared(), SchemaBackend::Redis, "users", |canonical| {
        *canonical = json!({ "users": 3 });
    });
    let unreadable = ChangeKind::LayoutChanged {
        from: 2,
        to: 3,
        reads_old: false,
    };
    let expected = [change(
        SchemaBackend::Redis,
        "users",
        None,
        unreadable,
        Breaking,
    )];
    assert_eq!(
        SchemaChange::between_with(&declared(), &new, |_, _| Some(3)),
        expected
    );
    // without knowing what the new code reads, a bump is treated as unreadable
    assert_eq!(SchemaChange::between(&declared(), &new), expected);
    // every breaking and contract change needs an approved data step
    assert_eq!(ChangeClass::Breaking.default_step(), StepKind::Data);
    assert_eq!(ChangeClass::Contract.default_step(), StepKind::Data);
    assert!(StepKind::Data.needs_approval());
}

/// A column added to or removed from a table's key is a breaking key change rather than an
/// addition or a contract removal, and a removed mapping property is a contract change that
/// rebuilds its index
#[test]
fn key_columns_and_mapping_removals_are_classified() {
    use ChangeClass::{Breaking, Contract};
    use ChangeKind::{KeyChanged, Removed};
    use SchemaBackend::Scylla;
    // a column added to the partition key, or removed from the clustering key, changes the key
    let new = edit(declared(), Scylla, "samples_list", |canonical| {
        columns(canonical).push(json!({ "name": "shard", "type": "int" }));
        canonical["partition_key"]
            .as_array_mut()
            .expect("partition key")
            .push(json!("shard"));
    });
    assert_eq!(
        SchemaChange::between(&declared(), &new),
        [change(
            Scylla,
            "samples_list",
            Some("shard"),
            KeyChanged,
            Breaking
        )]
    );
    let new = edit(declared(), Scylla, "samples_list", |canonical| {
        columns(canonical).retain(|column| column["name"] != "id");
        canonical["clustering_key"]
            .as_array_mut()
            .expect("clustering key")
            .retain(|column| column["name"] != "id");
    });
    assert_eq!(
        SchemaChange::between(&declared(), &new),
        [change(
            Scylla,
            "samples_list",
            Some("id"),
            KeyChanged,
            Breaking
        )]
    );
    // a removed mapping property is a contract change, and its index is rebuilt
    let new = edit(
        declared(),
        SchemaBackend::Elastic,
        "SampleResults",
        |canonical| {
            canonical["body"]["mappings"]["properties"]
                .as_object_mut()
                .expect("properties")
                .remove("streamed");
        },
    );
    let removed = SchemaChange::between(&declared(), &new);
    assert_eq!(
        removed,
        [change(
            SchemaBackend::Elastic,
            "SampleResults",
            Some("streamed"),
            Removed,
            Contract
        )]
    );
    assert_eq!(rebuilds(&removed), ["SampleResults"]);
}

/// A change to a view's key or columns is breaking, and becomes a replacement view under a new
/// name plus a later contract drop when the API reads the view, or a drop and recreate when
/// nothing does
#[test]
fn view_changes_replace_or_recreate_the_view() {
    use ChangeClass::{Additive, Breaking, Contract};
    use ChangeKind::{Added, KeyChanged, Removed};
    use SchemaBackend::Scylla;
    // a change to a view's key, or a column added to a view, is breaking, since views can't be
    // altered
    let new = edit(declared(), Scylla, "samples", |canonical| {
        flip_first_order(canonical);
    });
    let changes = SchemaChange::between(&declared(), &new);
    assert_eq!(changes.len(), 1);
    assert_eq!(
        (changes[0].kind.clone(), changes[0].class),
        (KeyChanged, Breaking)
    );
    let widened = edit(declared(), Scylla, "samples", |canonical| {
        columns(canonical).push(json!({ "name": "note", "type": "text" }));
    });
    let added = SchemaChange::between(&declared(), &widened);
    assert_eq!(
        added,
        [change(Scylla, "samples", Some("note"), Added, Breaking)]
    );
    // the API reads the samples view, so its change is a replacement view, then a contract drop
    assert!(views_read().contains("samples"));
    let read = |view: &str| views_read().contains(view);
    for view_change in [&changes[0], &added[0]] {
        assert_eq!(
            ViewChange::for_change(view_change, &declared(), read),
            Some(ViewChange::Replace)
        );
        // the same change on a view nothing reads drops and recreates it
        assert_eq!(
            ViewChange::for_change(view_change, &declared(), |_| false),
            Some(ViewChange::Recreate)
        );
    }
    assert_eq!(
        ViewChange::Replace.steps(),
        [
            (ChangeClass::Additive, StepKind::Infra),
            (ChangeClass::Contract, StepKind::Data)
        ]
    );
    assert_eq!(
        ViewChange::Recreate.steps(),
        [(ChangeClass::Breaking, StepKind::Data)]
    );
    assert_eq!(ViewChange::for_view(true), ViewChange::Replace);
    assert_eq!(ViewChange::for_view(false), ViewChange::Recreate);
    // a new filter redefines the whole view, which is replaced or recreated the same way
    let filtered = edit(
        declared(),
        Scylla,
        "network_policies_default",
        |canonical| {
            canonical["filter"] = json!("default_policy = false");
        },
    );
    let refiltered = SchemaChange::between(&declared(), &filtered);
    assert_eq!(
        refiltered,
        [change(
            Scylla,
            "network_policies_default",
            None,
            KeyChanged,
            Breaking
        )]
    );
    assert_eq!(
        ViewChange::for_change(&refiltered[0], &declared(), |_| true),
        Some(ViewChange::Replace)
    );
    assert_eq!(
        ViewChange::for_change(&refiltered[0], &declared(), |_| false),
        Some(ViewChange::Recreate)
    );
    // a table change, a new view and a dropped view are not view changes
    let table_change = change(Scylla, "samples_list", Some("submitter"), Removed, Contract);
    let new_view = change(Scylla, "samples_by_tool", None, Added, Additive);
    let dropped_view = change(Scylla, "samples", None, Removed, Contract);
    for other in [table_change, new_view, dropped_view] {
        assert_eq!(ViewChange::for_change(&other, &declared(), read), None);
    }
}

/// Build a fixture step of a kind targeting objects, with or without a data migration
///
/// # Arguments
///
/// * `kind` - The step's kind
/// * `targets` - The objects or fields the step targets
/// * `migrates` - Whether the step names a data migration
fn step(kind: StepKind, targets: &'static [SchemaTarget], migrates: bool) -> StepDef {
    // a step with only the fields coverage reads set
    StepDef {
        id: "coverage-step",
        kind,
        description: "A fixture step for schema change coverage",
        thoradm_migration: migrates.then_some("fixture-migration"),
        targets,
        ..StepDef::BASE
    }
}

/// A breaking change is covered only by a `Data` step that targets its object or its field and
/// runs a migration, a contract change by such a step with or without a migration, and an
/// additive change by no step at all
#[test]
fn changes_are_covered_by_a_data_step_targeting_them() {
    use ChangeClass::{Additive, Breaking, Contract};
    use ChangeKind::{Added, Removed};
    use SchemaBackend::Scylla;
    use StepKind::{Data, Infra};
    // a type change, a removal and an addition of the same column
    let typed = ChangeKind::TypeChanged {
        from: "text".to_owned(),
        to: "int".to_owned(),
    };
    let breaking = change(Scylla, "samples_list", Some("submitter"), typed, Breaking);
    let contract = change(Scylla, "samples_list", Some("submitter"), Removed, Contract);
    let additive = change(Scylla, "samples_list", Some("submitter"), Added, Additive);
    // an additive change needs no step, the others need one
    assert!(additive.is_covered(std::iter::empty()));
    assert!(!breaking.is_covered(std::iter::empty()));
    assert!(!contract.is_covered(std::iter::empty()));
    // a migrating data step targeting the column or the whole table covers both
    for targets in [SUBMITTER, SAMPLES_LIST] {
        assert!(breaking.is_covered([&step(Data, targets, true)]));
        assert!(contract.is_covered([&step(Data, targets, true)]));
    }
    // a data step without a migration covers the contract change but not the breaking one
    assert!(contract.is_covered([&step(Data, SUBMITTER, false)]));
    assert!(!breaking.is_covered([&step(Data, SUBMITTER, false)]));
    // an infra step never covers a change, whatever it targets or runs
    for migrates in [true, false] {
        assert!(!breaking.is_covered([&step(Infra, SUBMITTER, migrates)]));
        assert!(!contract.is_covered([&step(Infra, SUBMITTER, migrates)]));
    }
    // a step targeting another column of the table, or another table, doesn't cover it
    for targets in [ORIGIN, NODES] {
        assert!(!breaking.is_covered([&step(Data, targets, true)]));
        assert!(!contract.is_covered([&step(Data, targets, true)]));
    }
    // one covering step among others is enough
    let steps = [step(Infra, SUBMITTER, true), step(Data, SAMPLES_LIST, true)];
    assert!(breaking.is_covered(&steps));
}

/// A breaking mapping change on an existing Elastic index maps to one rebuild of that index, however
/// many of its properties change
#[test]
fn breaking_elastic_changes_rebuild_the_index_once() {
    use SchemaBackend::Elastic;
    // two properties of one index change type
    let new = edit(declared(), Elastic, "SampleResults", |canonical| {
        let properties = &mut canonical["body"]["mappings"]["properties"];
        properties["group"] = json!({ "type": "text" });
        properties["streamed"] = json!({ "type": "keyword" });
    });
    let changes = SchemaChange::between(&declared(), &new);
    assert_eq!(
        changes,
        [
            change(
                Elastic,
                "SampleResults",
                Some("group"),
                ChangeKind::MappingChanged,
                ChangeClass::Breaking
            ),
            change(
                Elastic,
                "SampleResults",
                Some("streamed"),
                ChangeKind::MappingChanged,
                ChangeClass::Breaking
            ),
        ]
    );
    // the index is rebuilt once
    assert_eq!(rebuilds(&changes), ["SampleResults"]);
    // an analyzer change is a mapping change too
    let analyzed = edit(declared(), Elastic, "SampleTags", |canonical| {
        canonical["body"]["mappings"]["properties"]["tags"]["analyzer"] = json!("whitespace");
    });
    let changes = SchemaChange::between(&declared(), &analyzed);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].kind, ChangeKind::MappingChanged);
    assert_eq!(rebuilds(&changes), ["SampleTags"]);
    // breaking changes on two indexes rebuild each of them once
    let both = edit(new, Elastic, "SampleTags", |canonical| {
        canonical["body"]["mappings"]["properties"]["tags"]["analyzer"] = json!("whitespace");
    });
    assert_eq!(
        rebuilds(&SchemaChange::between(&declared(), &both)),
        ["SampleResults", "SampleTags"]
    );
    // an added property or a new index needs no rebuild
    let added = edit(declared(), Elastic, "RepoTags", |canonical| {
        canonical["body"]["mappings"]["properties"]["origin"] = json!({ "type": "keyword" });
    });
    assert_eq!(
        rebuilds(&SchemaChange::between(&declared(), &added)),
        Vec::<String>::new()
    );
    let old = without(declared(), Elastic, "RepoTags");
    assert_eq!(
        rebuilds(&SchemaChange::between(&old, &declared())),
        Vec::<String>::new()
    );
}
