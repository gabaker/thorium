//! Tests of acknowledging drifted objects, the capped drift summary, and the steps drift blocks

use std::collections::BTreeMap;

use super::super::{Fingerprint, SnapshotObject};
use super::{Acknowledgement, BackendDrift, DriftReport, FieldDrift, ObjectDrift};
use crate::models::upgrades::{
    BackendSchemaStatus, DriftKind, DriftSummary, RevisionId, SchemaBackend, SchemaTarget,
};

/// Build a fingerprint from a label
///
/// # Arguments
///
/// * `label` - What makes the fingerprint its own
fn fingerprint(label: &str) -> Fingerprint {
    // fingerprint a small canonical form holding the label
    Fingerprint::of(&serde_json::json!({ "label": label }))
}

/// Build one drifted object
///
/// # Arguments
///
/// * `backend` - The object's backend
/// * `object` - The object's name as `schema diff` names it
/// * `kind` - How it differs
/// * `observed` - The label of the observed fingerprint, none for a missing object
fn drift(
    backend: SchemaBackend,
    object: &str,
    kind: DriftKind,
    observed: Option<&str>,
) -> ObjectDrift {
    // an unacknowledged entry with its observed fingerprint
    ObjectDrift {
        backend,
        object: object.to_owned(),
        kind,
        fields: Vec::<FieldDrift>::new(),
        acknowledged: false,
        lapsed: false,
        fingerprint: observed.map(fingerprint),
    }
}

/// Build a report from drifted objects, setting each backend's status the way the comparison does
///
/// # Arguments
///
/// * `objects` - The drifted objects of every backend
fn report(objects: Vec<ObjectDrift>) -> DriftReport {
    // group the objects by backend
    let mut backends = BTreeMap::<SchemaBackend, Vec<ObjectDrift>>::new();
    for object in objects {
        backends.entry(object.backend).or_default().push(object);
    }
    // every backend with objects drifts
    let backends = backends
        .into_iter()
        .map(|(backend, objects)| {
            let status = BackendSchemaStatus::Drift;
            (backend, BackendDrift { status, objects })
        })
        .collect();
    DriftReport {
        revision: RevisionId::try_from("2026-10-v02".to_owned()).unwrap(),
        expected: fingerprint("expected"),
        observed: fingerprint("observed"),
        backends,
    }
}

/// Acknowledge an object, naming a fingerprint label or none
///
/// # Arguments
///
/// * `backend` - The object's backend
/// * `object` - The object's name
/// * `observed` - The label of the fingerprint acknowledged, if any
fn ack(backend: SchemaBackend, object: &str, observed: Option<&str>) -> Acknowledgement {
    // the fingerprint is written as its hex digest
    Acknowledgement {
        backend,
        object: object.to_owned(),
        fingerprint: observed.map(|label| fingerprint(label).as_str().to_owned()),
    }
}

/// Find one object in a report
///
/// # Arguments
///
/// * `report` - The report to search
/// * `backend` - The object's backend
/// * `object` - The object's name
fn entry<'a>(report: &'a DriftReport, backend: SchemaBackend, object: &str) -> &'a ObjectDrift {
    // the object is in its backend's list
    report.backends[&backend]
        .objects
        .iter()
        .find(|entry| entry.object == object)
        .expect("the object is in the report")
}

/// Acknowledging every `unexpected` object clears the condition and returns the backend to `ok`,
/// while one unacknowledged object keeps both
#[test]
fn acknowledging_every_unexpected_object_clears_the_condition() {
    // two unexpected objects on Scylla
    let objects = vec![
        drift(
            SchemaBackend::Scylla,
            "tools",
            DriftKind::Unexpected,
            Some("a"),
        ),
        drift(
            SchemaBackend::Scylla,
            "samples_list.preview",
            DriftKind::Unexpected,
            Some("b"),
        ),
    ];
    // unacknowledged, they raise the condition
    let unacknowledged = report(objects.clone());
    let summary = unacknowledged.summary(&[]);
    assert!(summary.holds());
    assert_eq!(summary.unexpected, 2);
    assert_eq!(
        summary.backends[&SchemaBackend::Scylla],
        BackendSchemaStatus::Drift
    );
    // acknowledging one keeps the condition and the backend's drift
    let mut partial = report(objects.clone());
    let acks = [ack(SchemaBackend::Scylla, "tools", None)];
    partial.acknowledge(&acks);
    assert!(entry(&partial, SchemaBackend::Scylla, "tools").acknowledged);
    assert!(!entry(&partial, SchemaBackend::Scylla, "samples_list.preview").acknowledged);
    assert_eq!(
        partial.backends[&SchemaBackend::Scylla].status,
        BackendSchemaStatus::Drift
    );
    assert!(partial.summary(&acks).holds());
    // acknowledging both clears the condition and the backend
    let mut cleared = report(objects);
    let acks = [
        ack(SchemaBackend::Scylla, "tools", None),
        ack(SchemaBackend::Scylla, "samples_list.preview", Some("b")),
    ];
    cleared.acknowledge(&acks);
    assert_eq!(
        cleared.backends[&SchemaBackend::Scylla].status,
        BackendSchemaStatus::Ok
    );
    let summary = cleared.summary(&acks);
    assert!(!summary.holds());
    assert_eq!((summary.unexpected, summary.acknowledged), (0, 2));
    assert_eq!(summary.objects, []);
    assert_eq!(
        summary.backends[&SchemaBackend::Scylla],
        BackendSchemaStatus::Ok
    );
}

/// An acknowledgement naming the observed fingerprint holds while it matches and lapses when the
/// object changes, reporting it unacknowledged with a lapsed note and setting the condition; an
/// acknowledgement without a fingerprint still covers the changed object
#[test]
fn an_acknowledgement_lapses_when_the_object_changes() {
    let unchanged = || {
        drift(
            SchemaBackend::Scylla,
            "tools",
            DriftKind::Unexpected,
            Some("a"),
        )
    };
    let changed = || {
        drift(
            SchemaBackend::Scylla,
            "tools",
            DriftKind::Unexpected,
            Some("b"),
        )
    };
    // while the fingerprint matches, the object stays acknowledged
    let acks = [ack(SchemaBackend::Scylla, "tools", Some("a"))];
    let mut held = report(vec![unchanged()]);
    held.acknowledge(&acks);
    let found = entry(&held, SchemaBackend::Scylla, "tools");
    assert!(found.acknowledged && !found.lapsed);
    assert!(!held.summary(&acks).holds());
    // once the object changes, it is unacknowledged again with a lapsed note
    let mut lapsed = report(vec![changed()]);
    lapsed.acknowledge(&acks);
    let found = entry(&lapsed, SchemaBackend::Scylla, "tools");
    assert!(!found.acknowledged && found.lapsed);
    let summary = lapsed.summary(&acks);
    assert!(summary.holds());
    assert_eq!(summary.unexpected, 1);
    assert_eq!(summary.objects.len(), 1);
    assert!(summary.objects[0].lapsed);
    assert!(
        summary.describe().contains("lapsed"),
        "{}",
        summary.describe()
    );
    // without a fingerprint, the acknowledgement covers whatever the object becomes
    let acks = [ack(SchemaBackend::Scylla, "tools", None)];
    let mut covered = report(vec![changed()]);
    covered.acknowledge(&acks);
    let found = entry(&covered, SchemaBackend::Scylla, "tools");
    assert!(found.acknowledged && !found.lapsed);
    assert!(!covered.summary(&acks).holds());
}

/// An acknowledgement names `unexpected` objects only: a `missing` or `changed` object an
/// acknowledgement names stays unacknowledged, and an acknowledgement for another backend's object
/// of the same name covers nothing
#[test]
fn only_unexpected_objects_can_be_acknowledged() {
    // a missing and a changed object, and an unexpected one named like a Scylla object on Elastic
    let mut drifted = report(vec![
        drift(SchemaBackend::Scylla, "nodes", DriftKind::Missing, None),
        drift(
            SchemaBackend::Scylla,
            "samples_list",
            DriftKind::Changed,
            Some("a"),
        ),
        drift(
            SchemaBackend::Elastic,
            "tools",
            DriftKind::Unexpected,
            Some("b"),
        ),
    ]);
    let acks = [
        ack(SchemaBackend::Scylla, "nodes", None),
        ack(SchemaBackend::Scylla, "samples_list", None),
        ack(SchemaBackend::Scylla, "tools", None),
    ];
    drifted.acknowledge(&acks);
    // nothing was acknowledged
    for (backend, object) in [
        (SchemaBackend::Scylla, "nodes"),
        (SchemaBackend::Scylla, "samples_list"),
        (SchemaBackend::Elastic, "tools"),
    ] {
        assert!(!entry(&drifted, backend, object).acknowledged, "{object}");
    }
    let summary = drifted.summary(&acks);
    assert_eq!(
        (
            summary.missing,
            summary.changed,
            summary.unexpected,
            summary.acknowledged
        ),
        (1, 1, 1, 0)
    );
}

/// With 50 unacknowledged and 10 acknowledged drifted objects, 4 of them acknowledged without a
/// fingerprint, the summary holds the four counts, `unfingerprinted: 4`, the first 20
/// unacknowledged objects with their fingerprints, and `more: 30`, never an acknowledged object
#[test]
fn the_summary_caps_the_listed_objects_and_counts_the_rest() {
    // 60 unexpected tables, `t00` to `t59`
    let name = |index: usize| format!("t{index:02}");
    let objects = (0..60)
        .map(|index| {
            let label = format!("fp{index}");
            drift(
                SchemaBackend::Scylla,
                &name(index),
                DriftKind::Unexpected,
                Some(&label),
            )
        })
        .collect();
    // acknowledge the last 10: four without a fingerprint, six with the observed one
    let acks = (50..60)
        .map(|index| {
            let label = format!("fp{index}");
            let fingerprint = (index >= 54).then_some(label.as_str());
            ack(SchemaBackend::Scylla, &name(index), fingerprint)
        })
        .collect::<Vec<_>>();
    let mut many = report(objects);
    many.acknowledge(&acks);
    let summary = many.summary(&acks);
    // the counts, with the listed objects the first 20 unacknowledged in report order
    assert_eq!(
        (
            summary.missing,
            summary.changed,
            summary.unexpected,
            summary.acknowledged,
            summary.unfingerprinted,
            summary.more
        ),
        (0, 0, 50, 10, 4, 30)
    );
    assert_eq!(summary.objects.len(), 20);
    for (index, listed) in summary.objects.iter().enumerate() {
        assert_eq!(listed.object, name(index));
        assert_eq!(listed.kind, DriftKind::Unexpected);
        assert_eq!(
            listed.fingerprint.as_deref(),
            Some(fingerprint(&format!("fp{index}")).as_str())
        );
        assert!(!listed.lapsed);
    }
    assert!(summary.objects.iter().all(|listed| {
        let at = listed.object[1..].parse::<usize>().unwrap();
        at < 50
    }));
}

/// A backend whose only drift lies beyond the cap still reads `drift`, and a backend without
/// drift reads `ok`
#[test]
fn a_backend_beyond_the_cap_still_reads_drift() {
    // 20 Scylla objects fill the cap, and one Elastic object lies beyond it
    let mut objects = (0..20)
        .map(|index| {
            drift(
                SchemaBackend::Scylla,
                &format!("t{index:02}"),
                DriftKind::Missing,
                None,
            )
        })
        .collect::<Vec<_>>();
    objects.push(drift(
        SchemaBackend::Elastic,
        "SampleTags",
        DriftKind::Changed,
        Some("a"),
    ));
    let mut drifted = report(objects);
    // a backend that matches is part of the report too
    drifted.backends.insert(
        SchemaBackend::Redis,
        BackendDrift {
            status: BackendSchemaStatus::Ok,
            objects: Vec::new(),
        },
    );
    let summary = drifted.summary(&[]);
    assert_eq!((summary.objects.len(), summary.more), (20, 1));
    assert_eq!(
        summary.backends,
        BTreeMap::from([
            (SchemaBackend::Scylla, BackendSchemaStatus::Drift),
            (SchemaBackend::Elastic, BackendSchemaStatus::Drift),
            (SchemaBackend::Redis, BackendSchemaStatus::Ok),
        ])
    );
}

/// The condition's message counts each kind and names the drifting backends
#[test]
fn the_condition_message_counts_each_kind() {
    // one object of each kind on two backends
    let drifted = report(vec![
        drift(SchemaBackend::Scylla, "nodes", DriftKind::Missing, None),
        drift(
            SchemaBackend::Scylla,
            "tools",
            DriftKind::Unexpected,
            Some("a"),
        ),
        drift(
            SchemaBackend::Elastic,
            "SampleTags",
            DriftKind::Changed,
            Some("b"),
        ),
    ]);
    let message = drifted.summary(&[]).describe();
    // it counts every kind, names the backends, and points at the full report
    for part in [
        "1 missing",
        "1 changed",
        "1 unexpected",
        "scylla",
        "elastic",
        "thoradm schema diff",
    ] {
        assert!(message.contains(part), "{part} is missing from {message}");
    }
    assert!(!message.contains("redis"), "{message}");
}

/// The targets of a step that overlap a drifted object block it with an explanation naming the
/// object, and unrelated targets don't
#[test]
fn a_step_blocks_on_drift_in_the_objects_it_targets() {
    // a changed table, an unexpected column, a missing table, and a changed Elastic index
    let drifted = report(vec![
        drift(
            SchemaBackend::Scylla,
            "comments_by_user",
            DriftKind::Changed,
            Some("a"),
        ),
        drift(
            SchemaBackend::Scylla,
            "samples_list.preview",
            DriftKind::Unexpected,
            Some("b"),
        ),
        drift(SchemaBackend::Scylla, "nodes", DriftKind::Missing, None),
        drift(
            SchemaBackend::Elastic,
            "SampleTags",
            DriftKind::Changed,
            Some("c"),
        ),
    ])
    .summary(&[]);
    let target = |backend, object, field| SchemaTarget {
        backend,
        object,
        field,
    };
    // a step targeting the changed table blocks, naming it
    let whole = [target(SchemaBackend::Scylla, "comments_by_user", None)];
    let message = drifted
        .block_message("add-comments", &whole)
        .expect("the changed table blocks the step");
    assert!(message.contains("comments_by_user"), "{message}");
    assert!(message.contains("add-comments"), "{message}");
    // a step targeting the object an unexpected column was added to blocks, and so does one
    // targeting that column
    let samples = [target(SchemaBackend::Scylla, "samples_list", None)];
    assert_eq!(drifted.blocking(&samples).len(), 1);
    let column = [target(
        SchemaBackend::Scylla,
        "samples_list",
        Some("preview"),
    )];
    assert_eq!(drifted.blocking(&column).len(), 1);
    // a step targeting another column of that table doesn't
    let other = [target(
        SchemaBackend::Scylla,
        "samples_list",
        Some("origin"),
    )];
    assert_eq!(
        drifted.blocking(&other),
        Vec::<&crate::models::upgrades::DriftEntry>::new()
    );
    // a step with unrelated targets, or none, runs
    let unrelated = [target(SchemaBackend::Scylla, "tags", None)];
    assert_eq!(drifted.block_message("tags", &unrelated), None);
    assert_eq!(drifted.block_message("config", &[]), None);
    // the same object name on another backend is another object
    let elsewhere = [target(SchemaBackend::Redis, "comments_by_user", None)];
    assert_eq!(drifted.block_message("redis", &elsewhere), None);
    // a missing table doesn't block: the converge step creates it, and a changed Elastic index is
    // resolved by its rebuild
    let missing = [target(SchemaBackend::Scylla, "nodes", None)];
    assert_eq!(drifted.block_message("nodes", &missing), None);
    let index = [target(SchemaBackend::Elastic, "SampleTags", None)];
    assert_eq!(drifted.block_message("rebuild", &index), None);
}

/// Acknowledgements in the `drift_acknowledged` form parse, with the fingerprint optional
#[test]
fn acknowledgements_parse_from_the_spec_form() {
    // the form the chart renders into the spec
    let parsed: Vec<Acknowledgement> = serde_json::from_str(
        r#"[{"backend": "scylla", "object": "samples_list.preview", "fingerprint": "81aa"},
            {"backend": "elastic", "object": "SampleTags"}]"#,
    )
    .unwrap();
    assert_eq!(
        parsed,
        [
            Acknowledgement {
                backend: SchemaBackend::Scylla,
                object: "samples_list.preview".to_owned(),
                fingerprint: Some("81aa".to_owned()),
            },
            Acknowledgement {
                backend: SchemaBackend::Elastic,
                object: "SampleTags".to_owned(),
                fingerprint: None,
            },
        ]
    );
}

/// An Elastic field mapped `text` where the snapshot says `keyword` is `changed`, with the
/// expected and observed mappings
#[test]
fn an_elastic_text_field_is_a_changed_mapping() {
    use serde_json::json;
    // observe the first declared index with its `group` field mapped as text
    let declared = SnapshotObject::declared()
        .iter()
        .find(|object| {
            object.backend == SchemaBackend::Elastic
                && object.canonical.to_string().contains("\"group\"")
        })
        .expect("an index maps a group field")
        .clone();
    let mut form = declared.canonical.clone();
    form["body"]["mappings"]["properties"]["group"] = json!({ "type": "text" });
    let observed = SnapshotObject {
        backend: SchemaBackend::Elastic,
        name: declared.name.clone(),
        fingerprint: Fingerprint::of(&form),
        canonical: form,
    };
    // compare only Elastic
    let mut objects = SnapshotObject::declared()
        .iter()
        .filter(|object| object.backend == SchemaBackend::Elastic && object.name != declared.name)
        .cloned()
        .collect::<Vec<_>>();
    objects.push(observed);
    let observed = super::ObservedSchema::new(objects, &[SchemaBackend::Elastic]);
    let snapshot = crate::models::schema::RevisionSnapshot::declared(
        RevisionId::try_from("2026-10-v02".to_owned()).unwrap(),
    );
    let report = DriftReport::compare(&snapshot, &observed, &|_, _: &str| false);
    // the one field is changed, naming both mappings
    let entries = &report.backends[&SchemaBackend::Elastic].objects;
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0].object, format!("{}.group", declared.name));
    assert_eq!(entries[0].kind, DriftKind::Changed);
    assert_eq!(
        entries[0].fields[0].observed.as_deref(),
        Some("{\"type\":\"text\"}")
    );
    assert!(
        entries[0].fields[0]
            .expected
            .as_deref()
            .is_some_and(|expected| expected.contains("keyword")),
        "{:?}",
        entries[0].fields[0]
    );
}

/// Objects beyond the listed cap that block a step are carried as blockers, and ones that never
/// block are not
#[test]
fn unlisted_blocking_objects_are_carried_as_blockers() {
    // 20 listed objects, then one blocking Scylla table and one missing table beyond the cap
    let mut objects = (0..20)
        .map(|index| {
            drift(
                SchemaBackend::Scylla,
                &format!("t{index:02}"),
                DriftKind::Missing,
                None,
            )
        })
        .collect::<Vec<_>>();
    objects.push(drift(
        SchemaBackend::Scylla,
        "late",
        DriftKind::Changed,
        Some("a"),
    ));
    objects.push(drift(
        SchemaBackend::Scylla,
        "gone",
        DriftKind::Missing,
        None,
    ));
    let summary = report(objects).summary(&[]);
    assert_eq!((summary.objects.len(), summary.more), (20, 2));
    // only the blocking object is carried, and it blocks a step targeting it
    let carried = summary
        .blockers
        .iter()
        .map(|entry| entry.object.as_str())
        .collect::<Vec<_>>();
    assert_eq!(carried, ["late"]);
    let targets = [SchemaTarget {
        backend: SchemaBackend::Scylla,
        object: "late",
        field: None,
    }];
    assert_eq!(summary.blocking(&targets).len(), 1);
}

/// The most a serialized drift summary may take, far below what the operator's state `ConfigMap` holds
const SUMMARY_BYTES: usize = 64 * 1024;

/// However many objects drift, the serialized summary stays small and says it was cut short, so
/// it always fits in the operator's state
#[test]
fn a_summary_of_many_unexpected_objects_stays_small() {
    // 10,000 unexpected Scylla tables, each with a long name and a fingerprint
    let objects = (0..10_000)
        .map(|index| {
            drift(
                SchemaBackend::Scylla,
                &format!("an_unexpected_table_with_a_rather_long_name_{index:05}"),
                DriftKind::Unexpected,
                Some("a"),
            )
        })
        .collect::<Vec<_>>();
    let summary = report(objects).summary(&[]);
    // every object is still counted
    assert_eq!((summary.unexpected, summary.objects.len()), (10_000, 20));
    // the serialized summary is small and flags that the carried blockers are not all of them
    let json = serde_json::to_value(&summary).expect("serialize");
    let size = serde_json::to_string(&summary).expect("serialize").len();
    assert!(size < SUMMARY_BYTES, "{size} bytes");
    assert_eq!(json["blockers_truncated"], true, "{json}");
}

/// A summary whose blockers were cut short blocks every step that targets anything, since the
/// object it targets may be among those not carried
#[test]
fn a_summary_with_cut_blockers_blocks_every_targeting_step() {
    // a summary that names one blocker and says there were more
    let summary: DriftSummary = serde_json::from_value(serde_json::json!({
        "missing": 0, "changed": 0, "unexpected": 300, "acknowledged": 0,
        "blockers": [{"backend": "scylla", "object": "one", "kind": "unexpected"}],
        "blockers_truncated": true,
    }))
    .expect("parse");
    // a step targeting a table that isn't named is blocked anyway
    let targets = [SchemaTarget {
        backend: SchemaBackend::Scylla,
        object: "elsewhere",
        field: None,
    }];
    assert!(summary.block_message("step", &targets).is_some());
    // a step that targets nothing is never blocked
    assert!(summary.block_message("step", &[]).is_none());
}

/// An object whose own name holds a dot still blocks a step that targets it
#[test]
fn dotted_object_names_still_block() {
    // an unexpected table named with a dot, and a step that targets exactly that table
    let summary = report(vec![drift(
        SchemaBackend::Scylla,
        "files.v2",
        DriftKind::Unexpected,
        Some("a"),
    )])
    .summary(&[]);
    let targets = [SchemaTarget {
        backend: SchemaBackend::Scylla,
        object: "files.v2",
        field: None,
    }];
    assert_eq!(summary.blocking(&targets).len(), 1);
    // a step targeting a table that merely begins like it is not blocked
    let other = [SchemaTarget {
        backend: SchemaBackend::Scylla,
        object: "files",
        field: Some("other"),
    }];
    assert_eq!(summary.blocking(&other).len(), 0);
}
