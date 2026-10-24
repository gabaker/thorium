//! The gate on lossy downgrade steps: the confirmation code a blocked step shows, the check of an
//! approval against it, the dev setting that waives both, and the codes a history has consumed
//!
//! The operator, `thoradm migrate revert`, and the upgrade harness's direct driver all decide
//! with this one module, so a code or a verdict never differs between them.

use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

use super::{
    AppliedStep, BlockReason, ConfirmRefusal, StepLoss, StepState, UpgradeApproval, UpgradeState,
};

/// The alphabet confirmation codes are written in: Crockford base32, which leaves out the letters
/// that read as digits
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// How many characters a confirmation code has
pub const CODE_LEN: usize = 8;

/// The words a migration Job's failure message holds when it refused to run because its
/// confirmation code was already consumed, which the operator looks for in the Job's summary
pub const CONSUMED_REFUSAL: &str = "was already consumed by an earlier run of the step";

/// Compute a lossy step's confirmation code
///
/// The code is 8 Crockford base32 characters of a sha256 over the step id, the recorded revision,
/// the loss report's revision, step, description, and field names (not their object counts or
/// backup coverage, so live writes don't change the code between showing and approving it), and
/// the length of the applied history, which grows with every finished step so a round trip back
/// to the same step never repeats a code.
///
/// # Arguments
///
/// * `step` - The step id
/// * `revision` - The revision the deployment is recorded at
/// * `loss` - The step's loss report
/// * `history` - How many entries the full applied history holds
#[must_use]
pub fn confirmation_code(step: &str, revision: &str, loss: &StepLoss, history: usize) -> String {
    // hash each part after its length so parts can't run into each other
    let mut hasher = Sha256::new();
    let mut add = |part: &str| {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    };
    add(step);
    add(revision);
    add(&loss.revision);
    add(&loss.step);
    add(&loss.description);
    for field in &loss.fields {
        add(&field.field);
    }
    add(&history.to_string());
    let digest = hasher.finalize();
    // write the first 40 bits as 8 characters of 5 bits each
    let bits = digest[..5]
        .iter()
        .fold(0_u64, |bits, byte| (bits << 8) | u64::from(*byte));
    (0..CODE_LEN)
        .rev()
        .map(|index| CROCKFORD[((bits >> (index * 5)) & 0x1f) as usize] as char)
        .collect()
}

/// What `dangerous_downgrade_lossy` comes to on a cluster
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LossySetting {
    /// The setting isn't set, so every lossy step needs its own approval
    Off,
    /// The setting is set on a dev cluster, so lossy steps run without confirmation or a backup
    Honored,
    /// The setting is set on a cluster that isn't dev, so it is ignored and reported
    Refused,
}

impl LossySetting {
    /// Decide what the setting comes to
    ///
    /// # Arguments
    ///
    /// * `set` - Whether `dangerous_downgrade_lossy` is set
    /// * `dev` - Whether the cluster is a dev cluster: `auto_target_dev` and the Namespace label
    #[must_use]
    pub fn of(set: bool, dev: bool) -> Self {
        // only a dev cluster honors the setting
        match (set, dev) {
            (false, _) => LossySetting::Off,
            (true, true) => LossySetting::Honored,
            (true, false) => LossySetting::Refused,
        }
    }
}

/// What the gate decided about a lossy step
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LossVerdict {
    /// The step may run
    Run {
        /// Whether the dev setting waived the approval, so the run has no confirmation code or
        /// backup to record
        blanket: bool,
    },
    /// The step waits, and what the status says about it
    Blocked {
        /// What the step waits for
        reason: BlockReason,
        /// What an admin should do
        message: String,
    },
}

/// Everything the gate weighs for one lossy step
#[derive(Debug, Clone, Copy)]
pub struct LossGate<'a> {
    /// What the dev setting comes to
    pub setting: LossySetting,
    /// The code the step shows, recorded when it blocked
    pub code: &'a str,
    /// The step's loss report, with counts and backup coverage from its check
    pub loss: &'a StepLoss,
    /// The approval given for the step, if any
    pub approval: Option<&'a UpgradeApproval>,
    /// The codes already consumed, which never match again
    pub consumed: &'a BTreeSet<String>,
}

impl LossGate<'_> {
    /// Describe what a step loses and how to accept it, for the status
    fn needs(&self) -> String {
        // the fields with their counts, then the approval to add
        let fields = self
            .loss
            .fields
            .iter()
            .map(|field| {
                format!(
                    "{} ({} objects, {})",
                    field.field,
                    field.objects,
                    if field.backed_up {
                        "covered by thoradm backup"
                    } else {
                        "not covered by thoradm backup"
                    }
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "Step {} loses data: {}. It loses {fields}. To accept the loss, take a backup and add \
             the approval {{step: {}, backup: <where your backup is>, acceptLoss: true, confirm: \
             {}}} (Helm value operator.cluster.upgrade.approvals[]); the code is for this block \
             of this step only",
            self.loss.step, self.loss.description, self.loss.step, self.code
        )
    }

    /// Refuse the approval, showing the current code
    ///
    /// # Arguments
    ///
    /// * `refusal` - Why it was refused
    /// * `text` - What is wrong with it
    fn refused(&self, refusal: ConfirmRefusal, text: &str) -> LossVerdict {
        // the refusal and the code the approval must repeat
        LossVerdict::Blocked {
            reason: BlockReason::ConfirmationRefused {
                confirm: self.code.to_owned(),
                refusal,
            },
            message: format!(
                "The approval for {} was refused: {text}. Repeat the current code {} as confirm",
                self.loss.step, self.code
            ),
        }
    }

    /// Judge the step: whether it runs, or what it waits for
    #[must_use]
    pub fn judge(&self) -> LossVerdict {
        // a dev cluster with the setting waives everything
        if self.setting == LossySetting::Honored {
            return LossVerdict::Run { blanket: true };
        }
        // the loss must be accepted at all
        let Some(approval) = self.approval.filter(|approval| approval.accept_loss) else {
            return LossVerdict::Blocked {
                reason: BlockReason::LossAcceptance {
                    confirm: self.code.to_owned(),
                },
                message: self.needs(),
            };
        };
        // the code must be given, unused, and this block's
        match approval.confirm.as_deref().map(str::trim) {
            None | Some("") => {
                return self.refused(ConfirmRefusal::Missing, "it repeats no confirmation code");
            }
            Some(given) if self.consumed.contains(given) => {
                return self.refused(
                    ConfirmRefusal::Consumed,
                    "that code was already used by a run of this step",
                );
            }
            Some(given) if given != self.code => {
                return self.refused(
                    ConfirmRefusal::Mismatch,
                    "that code isn't the current one: it is wrong, or was shown for an earlier \
                     block of the step",
                );
            }
            Some(_) => (),
        }
        // a lossy step also needs a backup
        if approval.backup.trim().is_empty() {
            return LossVerdict::Blocked {
                reason: BlockReason::LossAcceptance {
                    confirm: self.code.to_owned(),
                },
                message: format!(
                    "The approval for {} names no backup: take one with `thoradm backup new` and \
                     set backup to where it is",
                    self.loss.step
                ),
            };
        }
        // a field no backup covers needs the gap accepted too
        let gaps = self
            .loss
            .fields
            .iter()
            .filter(|field| !field.backed_up)
            .map(|field| field.field.as_str())
            .collect::<Vec<_>>();
        if !gaps.is_empty() && !approval.accept_backup_gaps {
            return self.refused(
                ConfirmRefusal::BackupGap,
                &format!(
                    "{} isn't covered by thoradm backup, so a backup can't restore it; add \
                     acceptBackupGaps: true to accept that",
                    gaps.join(", ")
                ),
            );
        }
        LossVerdict::Run { blanket: false }
    }
}

/// Whether an entry ends a step's block: the step started, finished, or failed
///
/// # Arguments
///
/// * `entry` - The history entry
fn ends_block(entry: &AppliedStep) -> bool {
    // a block isn't over until the step does something else
    matches!(
        entry.outcome,
        StepState::Running | StepState::Done | StepState::Skipped | StepState::Failed
    )
}

/// Get the code a blocked lossy step shows: the one its newest `Blocked` entry recorded
///
/// The code is computed once, when the step blocks, so entries written after the block (its own
/// `Blocked` notes, unrelated steps, copied operator entries) don't change what an admin was
/// shown. It lapses when the step itself started, finished, or failed since, or when a step before
/// it finished since, because the next block of the step then records a new code.
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
/// * `step` - The lossy step
/// * `preceding` - The ids of the steps that run before it on the path
#[must_use]
pub fn recorded_code(state: &UpgradeState, step: &str, preceding: &[&str]) -> Option<String> {
    // find the newest block that recorded a code
    let block = state.applied.iter().rposition(|entry| {
        entry.step == step && entry.outcome == StepState::Blocked && entry.confirm.is_some()
    })?;
    // anything that moved the step on since makes the code lapse
    let lapsed = state.applied[block + 1..].iter().any(|later| {
        (later.step == step && ends_block(later))
            || (preceding.contains(&later.step.as_str())
                && matches!(later.outcome, StepState::Done | StepState::Skipped))
    });
    if lapsed {
        None
    } else {
        state.applied[block].confirm.clone()
    }
}

/// Get the confirmation codes a step's runs consumed
///
/// A run consumes the code its approval repeated when it starts, so every entry of the step other
/// than a `Blocked` one that records a code counts.
///
/// # Arguments
///
/// * `state` - The recorded upgrade state
/// * `step` - The lossy step
#[must_use]
pub fn consumed_codes(state: &UpgradeState, step: &str) -> BTreeSet<String> {
    // the codes on the step's entries that ended a block
    state
        .applied
        .iter()
        .filter(|entry| entry.step == step && ends_block(entry))
        .filter_map(|entry| entry.confirm.clone())
        .collect()
}
