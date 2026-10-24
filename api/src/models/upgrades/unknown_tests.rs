//! Tests that every enum stored in a status or an upgrade state reads a value it doesn't know as its
//! `Unknown` catch-all, so an operator meeting a newer operator's status keeps reconciling

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::fmt::Debug;

use super::{
    BackendSchemaStatus, BlockReason, ConfirmRefusal, DriftKind, MigrationPhase, SchemaBackend,
    StepKind, StepState, UpgradeComponent, UpgradeConditionType, UpgradeState, UpgradeStatus,
};

/// Check that a value no build knows reads as the enum's catch-all
///
/// # Arguments
///
/// * `unknown` - The catch-all variant
fn reads_unknown<T: DeserializeOwned + Debug + PartialEq>(unknown: &T) {
    // a name from a newer build
    let read = serde_json::from_value::<T>(json!("FromANewerBuild"))
        .or_else(|_| serde_json::from_value::<T>(json!("fromanewerbuild")));
    assert_eq!(read.as_ref().ok(), Some(unknown), "{read:?}");
}

/// Every unit-only status and state enum reads an unknown value as `Unknown`
#[test]
fn every_status_and_state_enum_reads_an_unknown_value_as_unknown() {
    // the enums the status and the state store
    reads_unknown(&StepState::Unknown);
    reads_unknown(&StepKind::Unknown);
    reads_unknown(&UpgradeComponent::Unknown);
    reads_unknown(&MigrationPhase::Unknown);
    reads_unknown(&ConfirmRefusal::Unknown);
    reads_unknown(&UpgradeConditionType::Unknown);
    reads_unknown(&DriftKind::Unknown);
    reads_unknown(&SchemaBackend::Unknown);
    reads_unknown(&BackendSchemaStatus::Unknown);
}

/// A block reason carrying data reads an unknown variant, with or without data, as `Unknown`
#[test]
fn a_block_reason_with_data_reads_an_unknown_variant_as_unknown() {
    // a unit variant and a data variant from a newer build
    for unknown in [
        json!("Quarantine"),
        json!({"Quarantine": {"until": "tomorrow", "by": 3}}),
    ] {
        assert_eq!(
            serde_json::from_value::<BlockReason>(unknown).expect("reads"),
            BlockReason::Unknown
        );
    }
}

/// A whole status holding a value of every enum that the build doesn't know parses
#[test]
fn a_whole_status_holding_unknown_values_parses() {
    // what a newer operator might write into every enum the status holds
    let raw = json!({
        "current": "2026-10-v02",
        "target": "2026-10-v03",
        "latest": "2026-10-v04",
        "steps": [{
            "revision": "2026-10-v03",
            "step": "later-step",
            "kind": "Hologram",
            "description": "A step of a newer build",
            "pause": ["Api", "Teleporter"],
            "state": "Quantum",
            "phase": "Sideways",
            "blocked": {"Quarantine": {"until": "tomorrow"}},
        }],
        "schema": {"tape": "wobbly", "scylla": "ok"},
        "drift": {
            "missing": 0, "changed": 0, "unexpected": 1, "acknowledged": 0,
            "objects": [{"backend": "tape", "object": "reel", "kind": "vanished"}],
            "backends": {"tape": "wobbly"},
        },
        "conditions": [{"type": "Gravity", "status": "True", "message": "heavy"}],
    });
    let status = serde_json::from_value::<UpgradeStatus>(raw).expect("the status parses");
    // each unknown value reads as its catch-all and the known ones are kept
    let step = &status.steps[0];
    assert_eq!(step.kind, StepKind::Unknown);
    assert_eq!(step.state, StepState::Unknown);
    assert_eq!(step.phase, Some(MigrationPhase::Unknown));
    assert_eq!(
        step.pause,
        [UpgradeComponent::Api, UpgradeComponent::Unknown]
    );
    assert_eq!(step.blocked, Some(BlockReason::Unknown));
    assert_eq!(status.conditions[0].kind, UpgradeConditionType::Unknown);
    assert_eq!(
        status.schema.get(&SchemaBackend::Scylla),
        Some(&BackendSchemaStatus::Ok)
    );
    assert_eq!(
        status.schema.get(&SchemaBackend::Unknown),
        Some(&BackendSchemaStatus::Unknown)
    );
    let drift = status.drift.expect("drift");
    assert_eq!(drift.objects[0].backend, SchemaBackend::Unknown);
    assert_eq!(drift.objects[0].kind, DriftKind::Unknown);
}

/// A whole upgrade state holding an outcome the build doesn't know parses, and the step isn't
/// counted as finished
#[test]
fn a_whole_state_holding_an_unknown_outcome_parses_and_finishes_nothing() {
    // a history entry a newer operator wrote
    let raw: Value = json!({
        "schema_version": 1,
        "revision": "2026-10-v02",
        "applied": [{"revision": "2026-10-v02", "step": "schema-apply", "outcome": "Quantum"}],
        "schema": {"record": "scylla"},
    });
    let state = serde_json::from_value::<UpgradeState>(raw).expect("the state parses");
    assert_eq!(state.applied[0].outcome, StepState::Unknown);
    assert!(!state.finished("2026-10-v02", "schema-apply"));
}
