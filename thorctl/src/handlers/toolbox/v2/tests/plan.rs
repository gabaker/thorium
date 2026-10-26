//! Tests for the plan types

use super::super::plan_types::{
    ConflictKind, GroupPlan, GroupState, ItemKind, ItemPlan, ItemRef, ItemState, Plan, PolicyPlan,
    PolicyState, VersionClass, label_with_hash,
};

/// The label a conflicting version is published under carries a prefix of its spec hash
#[test]
fn conflicting_labels_get_a_hash_suffix() {
    // only the first eight characters of the hash are used
    assert_eq!(
        label_with_hash("1.0.0", "0123456789abcdef"),
        "1.0.0+01234567"
    );
    assert_eq!(label_with_hash("latest", "abc"), "latest+abc");
}

/// A plan counts its items, finds its conflicts, and knows whether it would write anything
#[test]
fn plans_summarize_and_find_conflicts() {
    // a plan that leaves everything as is
    let item = |kind, name: &str, state| ItemPlan {
        item: ItemRef::new(kind, name),
        group: "g".to_owned(),
        state,
        versions: Vec::new(),
        diff: None,
        conflict: None,
    };
    let mut plan = Plan {
        groups: vec![GroupPlan {
            name: "g".to_owned(),
            state: GroupState::Exists,
            description: None,
        }],
        policies: vec![PolicyPlan {
            name: "p".to_owned(),
            state: PolicyState::Reuse,
        }],
        items: vec![
            item(ItemKind::Tool, "a", ItemState::Unchanged),
            item(ItemKind::Pipeline, "b", ItemState::Skipped),
        ],
        notes: Vec::new(),
    };
    assert!(!plan.has_changes());
    assert_eq!(plan.conflicts().len(), 0);
    // a tool with a label conflict, once it is to be updated, is a change and a conflict
    let mut conflicted = item(ItemKind::Tool, "c", ItemState::Update);
    conflicted.conflict = Some(super::super::plan_types::Conflict {
        item: conflicted.item.clone(),
        group: "g".to_owned(),
        label: Some("1".to_owned()),
        kind: ConflictKind::LabelConflict,
        detail: "label 1 exists with another spec".to_owned(),
    });
    conflicted
        .versions
        .push(super::super::plan_types::VersionPlan {
            label: "1".to_owned(),
            class: VersionClass::LabelConflict,
            spec_hash: Some("abc".to_owned()),
            publish_label: Some(label_with_hash("1", "abc")),
        });
    plan.items.push(conflicted);
    assert!(plan.has_changes());
    assert_eq!(plan.conflicts().len(), 1);
    let summary = plan.summary();
    assert_eq!(
        (
            summary.update,
            summary.unchanged,
            summary.skipped,
            summary.create
        ),
        (1, 1, 1, 0)
    );
    // the plan is a document that reads back the same
    let json = serde_json::to_string(&plan).expect("serialize");
    assert_eq!(serde_json::from_str::<Plan>(&json).expect("parse"), plan);
    // a missing group is a change on its own
    let empty = Plan {
        groups: vec![GroupPlan {
            name: "g".to_owned(),
            state: GroupState::Missing,
            description: None,
        }],
        ..Plan::default()
    };
    assert!(empty.has_changes());
}
