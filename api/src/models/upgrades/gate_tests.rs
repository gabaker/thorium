//! Tests of the lossy-step gate: confirmation codes, the approval check, the dev setting, the code
//! a blocked step recorded, and the codes a history consumed

use std::collections::BTreeSet;

use super::gate::{
    LossGate, LossVerdict, LossySetting, confirmation_code, consumed_codes, recorded_code,
};
use super::{
    AppliedStep, BlockReason, ConfirmRefusal, FieldLoss, RevisionId, StepLoss, StepState,
    UpgradeApproval, UpgradeState,
};

/// The step every test judges
const STEP: &str = "users-v2-reverse";

/// Build a loss report
///
/// # Arguments
///
/// * `fields` - The fields it loses as `(name, objects, backed up)`
fn loss(fields: &[(&str, u64, bool)]) -> StepLoss {
    // a lossy reverse of a step in the second revision
    StepLoss {
        revision: "2026-10-v01".to_owned(),
        step: STEP.to_owned(),
        description: "Users keep one contact".to_owned(),
        fields: fields
            .iter()
            .map(|(field, objects, backed_up)| FieldLoss {
                field: (*field).to_owned(),
                objects: *objects,
                backed_up: *backed_up,
            })
            .collect(),
    }
}

/// Build an approval for the step
///
/// # Arguments
///
/// * `backup` - The backup it names
/// * `accept_loss` - Whether it accepts the loss
/// * `confirm` - The code it repeats
/// * `gaps` - Whether it accepts backup gaps
fn approval(backup: &str, accept_loss: bool, confirm: Option<&str>, gaps: bool) -> UpgradeApproval {
    // an approval of the step
    UpgradeApproval {
        step: STEP.to_owned(),
        backup: backup.to_owned(),
        accept_loss,
        confirm: confirm.map(ToOwned::to_owned),
        accept_backup_gaps: gaps,
        ..UpgradeApproval::default()
    }
}

/// Judge a step against an approval
///
/// # Arguments
///
/// * `setting` - What the dev setting comes to
/// * `loss` - The step's loss report
/// * `approval` - The approval, if any
/// * `consumed` - The codes already consumed
fn judge(
    setting: LossySetting,
    loss: &StepLoss,
    approval: Option<&UpgradeApproval>,
    consumed: &BTreeSet<String>,
) -> LossVerdict {
    // the code the step shows is the one its report and history compute
    let code = confirmation_code(STEP, "2026-10-v02", loss, 12);
    LossGate {
        setting,
        code: &code,
        loss,
        approval,
        consumed,
    }
    .judge()
}

/// The code is 8 Crockford base32 characters, doesn't change with the object counts, and changes
/// with the step, the revision, the fields, and the history length
#[test]
fn the_code_is_eight_crockford_characters_and_ignores_counts() {
    // eight characters of the alphabet that leaves out I, L, O, and U
    let base = loss(&[("contact", 4, false)]);
    let code = confirmation_code(STEP, "2026-10-v02", &base, 12);
    assert_eq!(code.len(), 8, "{code}");
    assert!(
        code.chars()
            .all(|c| "0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(c)),
        "{code}"
    );
    // live writes change counts and backup coverage, never the code
    let counted = loss(&[("contact", 4_000, true)]);
    assert_eq!(code, confirmation_code(STEP, "2026-10-v02", &counted, 12));
    // everything else that identifies the block changes it
    assert_ne!(code, confirmation_code("other", "2026-10-v02", &base, 12));
    assert_ne!(code, confirmation_code(STEP, "2026-10-v03", &base, 12));
    assert_ne!(code, confirmation_code(STEP, "2026-10-v02", &base, 13));
    let wider = loss(&[("contact", 4, false), ("phone", 1, false)]);
    assert_ne!(code, confirmation_code(STEP, "2026-10-v02", &wider, 12));
}

/// The code is pinned, so the operator, thoradm, and the harness can never drift apart
#[test]
fn the_code_for_known_inputs_is_pinned() {
    // a known input gives a known code
    let base = loss(&[("contact", 4, false)]);
    assert_eq!(
        confirmation_code(STEP, "2026-10-v02", &base, 12),
        confirmation_code(STEP, "2026-10-v02", &base, 12)
    );
    assert_eq!(
        confirmation_code(STEP, "2026-10-v02", &base, 12),
        "P6F569KY"
    );
}

/// The dev setting is honored only on a dev cluster, refused anywhere else, and ignored when off
#[test]
fn the_setting_is_honored_only_on_a_dev_cluster() {
    // set and dev
    assert_eq!(LossySetting::of(true, true), LossySetting::Honored);
    // set and not dev
    assert_eq!(LossySetting::of(true, false), LossySetting::Refused);
    // not set, whatever the cluster is
    assert_eq!(LossySetting::of(false, true), LossySetting::Off);
    assert_eq!(LossySetting::of(false, false), LossySetting::Off);
}

/// A lossy step blocks for its loss to be accepted with its code, and runs only with an approval
/// that has a backup, `accept_loss`, and the code
#[test]
fn a_lossy_step_runs_only_with_a_complete_approval() {
    let report = loss(&[("contact", 4, true)]);
    let code = confirmation_code(STEP, "2026-10-v02", &report, 12);
    let none = BTreeSet::new();
    // no approval, and one that doesn't accept the loss, wait for acceptance and show the code
    for given in [None, Some(approval("snap-1", false, Some(&code), false))] {
        match judge(LossySetting::Off, &report, given.as_ref(), &none) {
            LossVerdict::Blocked {
                reason: BlockReason::LossAcceptance { confirm },
                message,
            } => {
                assert_eq!(confirm, code);
                assert!(message.contains("contact"), "{message}");
                assert!(message.contains(&code), "{message}");
            }
            other => panic!("expected a block for acceptance: {other:?}"),
        }
    }
    // no code is refused as missing
    let missing = approval("snap-1", true, None, false);
    assert!(matches!(
        judge(LossySetting::Off, &report, Some(&missing), &none),
        LossVerdict::Blocked {
            reason: BlockReason::ConfirmationRefused {
                refusal: ConfirmRefusal::Missing,
                ..
            },
            ..
        }
    ));
    // a wrong code is refused as a mismatch, and the status shows the current one
    let wrong = approval("snap-1", true, Some("AAAAAAAA"), false);
    match judge(LossySetting::Off, &report, Some(&wrong), &none) {
        LossVerdict::Blocked {
            reason: BlockReason::ConfirmationRefused { confirm, refusal },
            ..
        } => {
            assert_eq!(confirm, code);
            assert_eq!(refusal, ConfirmRefusal::Mismatch);
        }
        other => panic!("expected a mismatch: {other:?}"),
    }
    // the right code without a backup still blocks
    let no_backup = approval("  ", true, Some(&code), false);
    assert!(matches!(
        judge(LossySetting::Off, &report, Some(&no_backup), &none),
        LossVerdict::Blocked {
            reason: BlockReason::LossAcceptance { .. },
            ..
        }
    ));
    // all of them run it
    let whole = approval("snap-1", true, Some(&code), false);
    assert_eq!(
        judge(LossySetting::Off, &report, Some(&whole), &none),
        LossVerdict::Run { blanket: false }
    );
}

/// A code in the consumed set is refused as consumed, before it is compared with the current one
#[test]
fn a_consumed_code_never_matches_again() {
    let report = loss(&[("contact", 4, true)]);
    let code = confirmation_code(STEP, "2026-10-v02", &report, 12);
    // the same code is consumed, as after a round trip that came back to the same history length
    let consumed = BTreeSet::from([code.clone()]);
    let whole = approval("snap-1", true, Some(&code), false);
    assert!(matches!(
        judge(LossySetting::Off, &report, Some(&whole), &consumed),
        LossVerdict::Blocked {
            reason: BlockReason::ConfirmationRefused {
                refusal: ConfirmRefusal::Consumed,
                ..
            },
            ..
        }
    ));
    // a consumed code that isn't the current one is consumed too, not merely stale
    let old = BTreeSet::from(["AAAAAAAA".to_owned()]);
    let leftover = approval("snap-1", true, Some("AAAAAAAA"), false);
    assert!(matches!(
        judge(LossySetting::Off, &report, Some(&leftover), &old),
        LossVerdict::Blocked {
            reason: BlockReason::ConfirmationRefused {
                refusal: ConfirmRefusal::Consumed,
                ..
            },
            ..
        }
    ));
}

/// A field no backup covers needs `accept_backup_gaps`, named in the refusal
#[test]
fn an_uncovered_field_needs_the_gap_accepted() {
    let report = loss(&[("contact", 4, true), ("phone", 9, false)]);
    let code = confirmation_code(STEP, "2026-10-v02", &report, 12);
    let none = BTreeSet::new();
    // the right code and a backup still block, naming the field
    let covered = approval("snap-1", true, Some(&code), false);
    match judge(LossySetting::Off, &report, Some(&covered), &none) {
        LossVerdict::Blocked {
            reason:
                BlockReason::ConfirmationRefused {
                    refusal: ConfirmRefusal::BackupGap,
                    confirm,
                },
            message,
        } => {
            assert_eq!(confirm, code);
            assert!(message.contains("phone"), "{message}");
            assert!(!message.contains("contact,"), "{message}");
        }
        other => panic!("expected a backup gap: {other:?}"),
    }
    // accepting the gap runs it
    let accepted = approval("snap-1", true, Some(&code), true);
    assert_eq!(
        judge(LossySetting::Off, &report, Some(&accepted), &none),
        LossVerdict::Run { blanket: false }
    );
}

/// An honored dev setting runs the step with no approval; a refused one changes nothing
#[test]
fn an_honored_setting_runs_without_an_approval_and_a_refused_one_does_not() {
    let report = loss(&[("contact", 4, false)]);
    let none = BTreeSet::new();
    // dev with the setting: no approval, no backup, no coverage check
    assert_eq!(
        judge(LossySetting::Honored, &report, None, &none),
        LossVerdict::Run { blanket: true }
    );
    // refused: gated as usual
    assert!(matches!(
        judge(LossySetting::Refused, &report, None, &none),
        LossVerdict::Blocked {
            reason: BlockReason::LossAcceptance { .. },
            ..
        }
    ));
}

/// Build a history entry
///
/// # Arguments
///
/// * `step` - The step
/// * `outcome` - Its outcome
/// * `confirm` - The code it records
fn entry(step: &str, outcome: StepState, confirm: Option<&str>) -> AppliedStep {
    // an entry of the second revision's reverse
    AppliedStep {
        revision: "2026-10-v01".to_owned(),
        step: step.to_owned(),
        outcome,
        confirm: confirm.map(ToOwned::to_owned),
        ..AppliedStep::default()
    }
}

/// Build a state with a history
///
/// # Arguments
///
/// * `applied` - The history, oldest first
fn state(applied: Vec<AppliedStep>) -> UpgradeState {
    // a deployment at the second revision
    let mut state = UpgradeState::new(
        "2026-10-v02".parse::<RevisionId>().expect("revision"),
        "1.8.1",
    );
    state.applied = applied;
    state
}

/// The code a step shows is the newest one its `Blocked` entry recorded, which entries written
/// after it for other steps don't change, and which a step finishing before it replaces
#[test]
fn the_recorded_code_survives_unrelated_entries_and_lapses_with_earlier_steps() {
    let blocked = entry(STEP, StepState::Blocked, Some("BLOCKED1"));
    // the note itself and an unrelated skipped step leave the code alone
    let quiet = state(vec![
        blocked.clone(),
        entry(STEP, StepState::Blocked, None),
        entry("other-step", StepState::Skipped, None),
    ]);
    assert_eq!(
        recorded_code(&quiet, STEP, &["earlier"]).as_deref(),
        Some("BLOCKED1")
    );
    // a step before it finishing afterwards makes the code stale
    let moved = state(vec![
        blocked.clone(),
        entry("earlier", StepState::Done, None),
    ]);
    assert_eq!(recorded_code(&moved, STEP, &["earlier"]), None);
    // the step running ends its block, so the next block computes a new code
    let ran = state(vec![
        blocked.clone(),
        entry(STEP, StepState::Running, Some("BLOCKED1")),
    ]);
    assert_eq!(recorded_code(&ran, STEP, &[]), None);
    // a later block replaces an earlier one
    let again = state(vec![
        blocked,
        entry("earlier", StepState::Done, None),
        entry(STEP, StepState::Blocked, Some("BLOCKED2")),
    ]);
    assert_eq!(
        recorded_code(&again, STEP, &["earlier"]).as_deref(),
        Some("BLOCKED2")
    );
}

/// The codes a history consumed are those on the step's entries that aren't `Blocked`
#[test]
fn a_started_step_consumes_the_code_it_repeated() {
    let history = state(vec![
        entry(STEP, StepState::Blocked, Some("SHOWN001")),
        entry(STEP, StepState::Running, Some("USED0001")),
        entry(STEP, StepState::Done, Some("USED0001")),
        entry("other-step", StepState::Done, Some("OTHER001")),
    ]);
    // only the step's own started entries count
    assert_eq!(
        consumed_codes(&history, STEP),
        BTreeSet::from(["USED0001".to_owned()])
    );
}
