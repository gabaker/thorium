//! The approval gate of the direct driver: which approvals let a planned item run, the
//! confirmation codes lossy steps ask for, and the dev setting
//!
//! The direct driver has no operator, so this module holds what the operator keeps in memory: an
//! approval names a step and a backup, a lossy step also needs its current confirmation code, and a
//! code is used up by the step it let run. The code and the verdict on a lossy step's approval are
//! the shared gate logic the operator and `thoradm migrate revert` decide with.

use std::collections::{BTreeMap, BTreeSet};

use crate::models::upgrades::{
    BlockReason, LossGate, LossVerdict, LossySetting, PlannedStep, UpgradeApproval,
};

pub use crate::models::upgrades::confirmation_code;

/// What the gate decided about the next item of a plan
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateDecision {
    /// Every gated step of the item is approved: run it
    Run,
    /// A step waits, so the item doesn't run
    Blocked {
        /// The step the plan is blocked on
        step: String,
        /// What it waits for
        reason: BlockReason,
        /// What the status says about it
        message: String,
    },
}

/// The approvals given, the codes shown and used, and the dev setting
#[derive(Debug, Clone, Default)]
pub struct Gate {
    /// The approval given for each step, the latest replacing earlier ones
    approvals: BTreeMap<String, UpgradeApproval>,
    /// The code each blocked lossy step currently shows
    codes: BTreeMap<String, String>,
    /// The codes lossy steps ran with, which never match again
    consumed: BTreeSet<String>,
    /// The code the scenario last gave for each lossy step
    given: BTreeMap<String, String>,
    /// The code each lossy step last ran with
    used: BTreeMap<String, String>,
    /// Whether the deployment counts as dev
    dev: bool,
    /// Whether `dangerous_downgrade_lossy` is set
    dev_lossy: bool,
}

impl Gate {
    /// A gate with no approvals
    ///
    /// # Arguments
    ///
    /// * `dev` - Whether the deployment counts as dev
    #[must_use]
    pub fn new(dev: bool) -> Self {
        // nothing approved, shown, or used yet
        Gate {
            dev,
            ..Gate::default()
        }
    }

    /// Give an approval, replacing the one already given for its step
    ///
    /// # Arguments
    ///
    /// * `approval` - The approval
    pub fn approve(&mut self, approval: UpgradeApproval) {
        // one approval per step
        self.approvals.insert(approval.step.clone(), approval);
    }

    /// Set `dangerous_downgrade_lossy`
    ///
    /// # Arguments
    ///
    /// * `on` - Whether the setting is on
    pub fn set_dev_lossy(&mut self, on: bool) {
        // keep the setting
        self.dev_lossy = on;
    }

    /// Whether the setting is on but refused because the deployment isn't dev
    #[must_use]
    pub fn setting_refused(&self) -> bool {
        // the setting only counts on a dev deployment
        self.dev_lossy && !self.dev
    }

    /// Whether the setting is on and honored, so lossy steps run without confirmation
    #[must_use]
    pub fn setting_honored(&self) -> bool {
        // dev and set
        self.dev_lossy && self.dev
    }

    /// The code a blocked lossy step shows right now
    ///
    /// # Arguments
    ///
    /// * `step` - The step id
    #[must_use]
    pub fn current_code(&self, step: &str) -> Option<&str> {
        // only a step that blocked has a code
        self.codes.get(step).map(String::as_str)
    }

    /// The code a lossy step last ran with, or failing that the code the scenario last gave for it
    ///
    /// # Arguments
    ///
    /// * `step` - The step id
    #[must_use]
    pub fn previous_code(&self, step: &str) -> Option<&str> {
        // the code a run used is the one that is consumed
        self.used
            .get(step)
            .or_else(|| self.given.get(step))
            .map(String::as_str)
    }

    /// Remember the code the scenario gave for a step
    ///
    /// # Arguments
    ///
    /// * `step` - The step id
    /// * `code` - The code
    pub fn remember(&mut self, step: &str, code: &str) {
        // kept so a later approval can reuse it
        self.given.insert(step.to_owned(), code.to_owned());
    }

    /// The approval given for a step
    ///
    /// # Arguments
    ///
    /// * `step` - The step id
    #[must_use]
    pub fn approval(&self, step: &str) -> Option<&UpgradeApproval> {
        // the latest one
        self.approvals.get(step)
    }

    /// Decide whether the steps of one planned item may run
    ///
    /// The first step that waits decides: a gated step needs an approval naming a backup, and a
    /// lossy step needs one with `accept_loss` and its current code too, unless the dev setting
    /// is honored. A lossy step shows its code from the moment it blocks until something runs.
    ///
    /// # Arguments
    ///
    /// * `steps` - The item's planned steps, in order
    /// * `revision` - The revision the deployment is recorded at
    /// * `history` - How many steps the applied history holds
    pub fn evaluate(
        &mut self,
        steps: &[&PlannedStep],
        revision: &str,
        history: usize,
    ) -> GateDecision {
        // look at each step in order, the first that waits blocks the item
        for step in steps {
            if let Some(loss) = &step.loss {
                if self.setting_honored() {
                    continue;
                }
                // the step shows one code for as long as it stays blocked
                let code = self
                    .codes
                    .entry(step.step.clone())
                    .or_insert_with(|| confirmation_code(&step.step, revision, loss, history))
                    .clone();
                if let Some(blocked) = self.lossy_block(step, &code) {
                    return blocked;
                }
            } else if step.kind.needs_approval()
                && self
                    .approval(&step.step)
                    .is_none_or(|approval| approval.backup.is_empty())
            {
                return GateDecision::Blocked {
                    step: step.step.clone(),
                    reason: BlockReason::Approval,
                    message: format!("{} waits for an approval that names a backup", step.step),
                };
            }
        }
        GateDecision::Run
    }

    /// Judge a lossy step's approval against the code it shows
    ///
    /// Returns `None` when the approval lets the step run.
    ///
    /// # Arguments
    ///
    /// * `step` - The lossy step
    /// * `code` - The code it shows now
    fn lossy_block(&self, step: &PlannedStep, code: &str) -> Option<GateDecision> {
        // the direct driver has no backup coverage to consult, so every field counts as covered
        let mut covered = step.loss.clone()?;
        for field in &mut covered.fields {
            field.backed_up = true;
        }
        // the shared gate decides, the dev setting having been handled by the caller
        let verdict = LossGate {
            setting: LossySetting::Off,
            code,
            loss: &covered,
            approval: self.approval(&step.step),
            consumed: &self.consumed,
        }
        .judge();
        match verdict {
            LossVerdict::Run { .. } => None,
            LossVerdict::Blocked { reason, message } => Some(GateDecision::Blocked {
                step: step.step.clone(),
                reason,
                message,
            }),
        }
    }

    /// Use up the code a lossy step ran with and forget the codes of steps still blocked
    ///
    /// The approval that carried the code is spent, so the step needs a new one the next time it
    /// blocks.
    ///
    /// # Arguments
    ///
    /// * `step` - The lossy step that ran, `None` when the item that ran had none
    pub fn ran(&mut self, step: Option<&str>) -> Option<String> {
        // a lossy step's code is consumed with its approval
        let code = step.and_then(|step| {
            let code = self.codes.get(step).cloned();
            if let Some(code) = &code {
                self.used.insert(step.to_owned(), code.clone());
                self.consumed.insert(code.clone());
                self.approvals.remove(step);
            }
            code
        });
        // anything that runs changes the history, so a step blocked later shows a new code; the
        // codes of the other lossy steps of one item stay until the whole item has been recorded
        if step.is_none() {
            self.codes.clear();
        }
        code
    }
}
