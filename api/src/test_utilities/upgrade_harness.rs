//! The upgrade scenario harness: scripts of upgrade actions and checks that drive a deployment
//! through upgrades and downgrades, supply approvals, stop at gates, check data between steps,
//! and inject failures
//!
//! A [`Scenario`] is built with a builder, validated against the driver that will run it, and
//! run action by action by a [`ScenarioDriver`]. The [`DirectDriver`] runs the planner and the
//! migration runner in-process against real backing services; the [`ClusterDriver`] drives a
//! minithor dev cluster through its operator.

mod chain;
mod cluster;
mod data;
mod direct;
mod gate;
mod types;
mod variants;

#[cfg(test)]
mod tests;

use async_trait::async_trait;
use std::collections::BTreeMap;
use std::fmt;

use crate::models::upgrades::{BlockReason, ConfirmRefusal, Family, UpgradeApproval};

pub use chain::ChainRunner;
pub use cluster::ClusterDriver;
pub use direct::DirectDriver;
pub use gate::{GateDecision, confirmation_code};
pub use types::{
    BackupRef, Check, ConfirmSource, CorruptFn, CorruptionKind, FamilyFixture, FieldShape,
    FieldValues, FixtureSeed, FixtureVerifier, HistoryExpectation, Injection, Interrupt, Layout,
    ModifyFn, PassCountCheck, RecordsCheck, RecordsFn, RestartTarget, VariantSet, Variation,
    loss_approval,
};

/// Why a scenario couldn't run or a check failed
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessError {
    /// The scenario holds an action only the cluster driver can run
    ClusterOnly {
        /// The scenario's name
        scenario: String,
        /// The action, rendered
        action: String,
    },
    /// The scenario holds an action only the direct driver can run
    DirectOnly {
        /// The scenario's name
        scenario: String,
        /// The action, rendered
        action: String,
    },
    /// This driver doesn't run the action yet, and why
    Unsupported(String),
    /// A check, expectation, or approval didn't hold
    Failed(String),
    /// A backend, the cluster, or a command failed
    Backend(String),
}

impl fmt::Display for HarnessError {
    /// Write the error for the person reading the test output
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // each variant says what to change in the scenario
        match self {
            HarnessError::ClusterOnly { scenario, action } => write!(
                f,
                "scenario {scenario}: {action} is a cluster-only action, which the direct driver \
                 can't run; run the scenario with the cluster driver or leave the action out"
            ),
            HarnessError::DirectOnly { scenario, action } => write!(
                f,
                "scenario {scenario}: {action} runs under the direct driver only, since the \
                 operator has no stop-before-step setting; check at gates and after completion \
                 instead"
            ),
            HarnessError::Unsupported(what) => write!(f, "not supported by this driver: {what}"),
            HarnessError::Failed(what) => write!(f, "scenario check failed: {what}"),
            HarnessError::Backend(what) => write!(f, "backend failure: {what}"),
        }
    }
}

impl std::error::Error for HarnessError {}

impl From<HarnessError> for crate::Error {
    /// Carry the harness error's text into the crate's error
    ///
    /// # Arguments
    ///
    /// * `error` - The harness error
    fn from(error: HarnessError) -> Self {
        // keep the message as it reads
        crate::Error::new(error.to_string())
    }
}

/// One action of a scenario
#[derive(Debug, Clone)]
pub enum ScenarioAction {
    /// Write fixture objects in a layout
    Seed(FixtureSeed),
    /// Move the deployment toward a revision (up or down)
    Target(String),
    /// Assert the deployment reached a revision
    ExpectRevision(String),
    /// Assert the plan is blocked on a step, for a reason, optionally with a message
    ExpectBlocked {
        /// The step the plan must be blocked on
        step: String,
        /// Why it must be blocked; a confirmation code inside the reason isn't compared, only
        /// that one is shown
        reason: BlockReason,
        /// Text the block message must contain
        message: Option<String>,
    },
    /// Assert the plan is refused before anything runs, naming an irreversible step
    ExpectRefused {
        /// The irreversible step the refusal must name
        step: String,
    },
    /// Assert a lossy approval was refused on a step for this reason, the step is still blocked,
    /// and the status shows a current code different from the refused one
    ExpectConfirmRefused {
        /// The lossy step the approval named
        step: String,
        /// Why the approval's code must be refused
        refusal: ConfirmRefusal,
    },
    /// Give one approval `{step, backup}` for a step that isn't lossy
    Approve(UpgradeApproval),
    /// Give one lossy approval `{step, backup, accept_loss: true, confirm}`, taking the code from
    /// `confirm`
    ApproveLoss {
        /// The lossy step to approve
        step: String,
        /// The backup reference the approval names, `None` to approve without a backup
        backup: Option<BackupRef>,
        /// Where the approval's confirmation code comes from
        confirm: ConfirmSource,
    },
    /// Approve every gated step in the plan that isn't lossy with one backup reference; lossy
    /// steps can't be approved ahead
    ApproveAll(BackupRef),
    /// Set `dangerous_downgrade_lossy` on the deployment
    SetDevLossy(bool),
    /// Run exactly one step, even one that needs no approval; direct driver only
    Step,
    /// Run a check against the data, state, and history
    Check(Check),
    /// Record the deployment's data under a label, for a later `Check::SameDataAs`
    Capture(String),
    /// Inject a failure or drift
    Inject(Injection),
    /// Deploy a Thorium image (cluster driver only)
    Deploy(String),
    /// Restart the operator or the running Job's pod (cluster driver only)
    Restart(RestartTarget),
}

impl ScenarioAction {
    /// Whether only the cluster driver can run this action
    #[must_use]
    pub fn cluster_only(&self) -> bool {
        // an image, a pod, or a killed process exists only on a cluster
        matches!(
            self,
            ScenarioAction::Deploy(_)
                | ScenarioAction::Restart(_)
                | ScenarioAction::Inject(Injection::InterruptAtBatch {
                    signal: Interrupt::Kill,
                    ..
                })
        )
    }

    /// Whether only the direct driver can run this action
    #[must_use]
    pub fn direct_only(&self) -> bool {
        // the operator has no stop-before-step setting
        matches!(self, ScenarioAction::Step)
    }

    /// Render the action for logs and error messages
    #[must_use]
    pub fn render(&self) -> String {
        // name the action and its main argument, never an approval's confirmation code
        match self {
            ScenarioAction::Seed(seed) => format!(
                "seed {} objects of {} in layout {}",
                seed.count, seed.family, seed.layout.0
            ),
            ScenarioAction::Target(revision) => format!("target {revision}"),
            ScenarioAction::ExpectRevision(revision) => format!("expect revision {revision}"),
            ScenarioAction::ExpectBlocked { step, reason, .. } => {
                format!("expect {step} blocked: {}", types::render_reason(reason))
            }
            ScenarioAction::ExpectRefused { step } => format!("expect a refusal naming {step}"),
            ScenarioAction::ExpectConfirmRefused { step, refusal } => {
                format!("expect {step} confirmation refused: {refusal:?}")
            }
            ScenarioAction::Approve(approval) => format!("approve {}", approval.step),
            ScenarioAction::ApproveLoss { step, confirm, .. } => {
                let source = match confirm {
                    ConfirmSource::Current => "the current code",
                    ConfirmSource::Previous => "the previous code",
                    ConfirmSource::Literal(_) => "a literal code",
                };
                format!("approve the loss of {step} with {source}")
            }
            ScenarioAction::ApproveAll(_) => "approve every gated step".to_owned(),
            ScenarioAction::SetDevLossy(on) => format!("set dangerous downgrade lossy to {on}"),
            ScenarioAction::Step => "step".to_owned(),
            ScenarioAction::Check(check) => format!("check {}", render_check(check)),
            ScenarioAction::Capture(label) => format!("capture {label}"),
            ScenarioAction::Inject(injection) => format!("inject {}", render_injection(injection)),
            ScenarioAction::Deploy(image) => format!("deploy {image}"),
            ScenarioAction::Restart(target) => format!("restart {target:?}"),
        }
    }
}

/// Render a check for logs
///
/// # Arguments
///
/// * `check` - The check
pub(crate) fn render_check(check: &Check) -> String {
    // name the kind of check and what it covers
    match check {
        Check::Fixtures(verifier) => {
            format!("{} in layout {}", verifier.family, verifier.layout.0)
        }
        Check::Records(check) => format!("{} of {}", check.name, check.family),
        Check::Issues { migration, .. } => format!("issues of {migration}"),
        Check::Drift(_) => "drift".to_owned(),
        Check::History(entries) => format!("history of {} steps", entries.len()),
        Check::Passes(counts) => format!("pass counts of {}", counts.family),
        Check::SameDataAs(label) => format!("same data as {label}"),
        Check::Condition { kind, present } => format!("condition {kind:?} present {present}"),
    }
}

/// Render an injection for logs
///
/// # Arguments
///
/// * `injection` - The injection
fn render_injection(injection: &Injection) -> String {
    // name the kind of failure and where it lands
    match injection {
        Injection::CorruptObjects {
            family,
            objects,
            kind,
            ..
        } => format!("{} {kind:?} corruptions in {family}", objects.len()),
        Injection::SystematicBreak { family, field } => {
            format!("a systematic break of {} in {family}", field.0)
        }
        Injection::InterruptAtBatch { batch, signal } => {
            format!("a {signal:?} after batch {batch}")
        }
        Injection::BackendOutage { backend, .. } => format!("an outage of {backend:?}"),
        Injection::Modify { family, keys, .. } => match keys {
            Some(keys) => format!("a rewrite of {} objects of {family}", keys.len()),
            None => format!("a rewrite of every object of {family}"),
        },
        Injection::Drift { kind, object } => format!("{kind:?} drift on {object}"),
    }
}

/// Which driver runs a scenario
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverKind {
    /// Real backing services through `test-utilities`, with the planner and the migration runner
    /// in-process
    Direct,
    /// A minithor dev cluster, through the operator and its Jobs
    Cluster,
}

/// One action as a driver ran it, for comparing the two drivers' action logs
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionRecord {
    /// The action, rendered
    pub action: String,
    /// What happened, rendered (the plan's state, the check's result)
    pub outcome: String,
}

/// Runs scenario actions against a deployment
#[async_trait]
pub trait ScenarioDriver: Send {
    /// Which driver this is, so cluster-only actions are rejected under the direct driver
    fn kind(&self) -> DriverKind;

    /// Run one action and record what it did
    ///
    /// # Arguments
    ///
    /// * `action` - The action to run
    /// * `scenario` - The scenario the action belongs to, for the families and fixtures it
    ///   registered
    async fn run(
        &mut self,
        action: &ScenarioAction,
        scenario: &Scenario,
    ) -> Result<ActionRecord, HarnessError>;
}

/// A script of upgrade actions and checks, run step by step by either driver
#[derive(Debug, Clone)]
pub struct Scenario {
    /// The scenario's name, used in test output
    name: String,
    /// The catalog the direct driver plans from: the test-only fixture catalog for the pattern
    /// matrix; the cluster driver uses the deployed images'
    catalog: Option<crate::models::upgrades::Catalog>,
    /// How each family the scenario seeds or verifies builds and reads its objects
    families: BTreeMap<Family, FamilyFixture>,
    /// Whether the deployment counts as dev, which the direct driver passes to the gate in place
    /// of reading a Namespace label it doesn't have
    dev: bool,
    /// The actions to run, in order
    actions: Vec<ScenarioAction>,
}

impl Scenario {
    /// Start a scenario with no actions
    ///
    /// # Arguments
    ///
    /// * `name` - The scenario's name, used in test output
    #[must_use]
    pub fn new<T: Into<String>>(name: T) -> Self {
        // an empty script on the production catalog
        Scenario {
            name: name.into(),
            catalog: None,
            families: BTreeMap::new(),
            dev: false,
            actions: Vec::new(),
        }
    }

    /// Mark the deployment as dev, the way a labeled Namespace of a dev cluster does
    ///
    /// # Arguments
    ///
    /// * `dev` - Whether the deployment counts as dev
    #[must_use]
    pub fn dev(mut self, dev: bool) -> Self {
        // keep the marking for the driver
        self.dev = dev;
        self
    }

    /// Whether the deployment counts as dev
    #[must_use]
    pub fn is_dev(&self) -> bool {
        // the marking as given
        self.dev
    }

    /// Plan from a test-only catalog instead of the production one
    ///
    /// # Arguments
    ///
    /// * `catalog` - The catalog the direct driver plans from
    #[must_use]
    pub fn catalog(mut self, catalog: crate::models::upgrades::Catalog) -> Self {
        // keep the catalog for the driver
        self.catalog = Some(catalog);
        self
    }

    /// Register how a family's fixture objects are built and read
    ///
    /// # Arguments
    ///
    /// * `family` - The family
    /// * `fixture` - How its objects are built and read
    #[must_use]
    pub fn family(mut self, family: Family, fixture: FamilyFixture) -> Self {
        // later registrations of one family replace earlier ones
        self.families.insert(family, fixture);
        self
    }

    /// The scenario's name
    #[must_use]
    pub fn name(&self) -> &str {
        // the name as given
        &self.name
    }

    /// The catalog the scenario plans from, if it set one
    #[must_use]
    pub fn planned_catalog(&self) -> Option<crate::models::upgrades::Catalog> {
        // a scenario that set none plans from the production catalog
        self.catalog
    }

    /// The actions in the order they run
    #[must_use]
    pub fn actions(&self) -> &[ScenarioAction] {
        // the script as built
        &self.actions
    }

    /// How a family's fixture objects are built and read, if the scenario registered it
    ///
    /// # Arguments
    ///
    /// * `family` - The family
    #[must_use]
    pub fn fixture(&self, family: &Family) -> Option<&FamilyFixture> {
        // look the family up
        self.families.get(family)
    }

    /// The families the scenario registered fixtures for
    pub fn families(&self) -> impl Iterator<Item = &Family> {
        // in family order
        self.families.keys()
    }

    /// Add an action
    ///
    /// # Arguments
    ///
    /// * `action` - The action to run next
    #[must_use]
    pub fn action(mut self, action: ScenarioAction) -> Self {
        // push to the end of the script
        self.actions.push(action);
        self
    }

    /// Write fixture objects before the next action
    ///
    /// # Arguments
    ///
    /// * `seed` - What to write
    #[must_use]
    pub fn seed(self, seed: FixtureSeed) -> Self {
        // one seed action
        self.action(ScenarioAction::Seed(seed))
    }

    /// Move the deployment toward a revision
    ///
    /// # Arguments
    ///
    /// * `revision` - The revision to reach
    #[must_use]
    pub fn target<T: Into<String>>(self, revision: T) -> Self {
        // one target action
        self.action(ScenarioAction::Target(revision.into()))
    }

    /// Assert the deployment reached a revision
    ///
    /// # Arguments
    ///
    /// * `revision` - The revision expected
    #[must_use]
    pub fn expect_revision<T: Into<String>>(self, revision: T) -> Self {
        // one expectation
        self.action(ScenarioAction::ExpectRevision(revision.into()))
    }

    /// Assert the plan is blocked on a step that waits for an approval
    ///
    /// # Arguments
    ///
    /// * `step` - The step the plan must be blocked on
    #[must_use]
    pub fn expect_blocked<T: Into<String>>(self, step: T) -> Self {
        // a gated step that isn't lossy waits for an approval
        self.action(ScenarioAction::ExpectBlocked {
            step: step.into(),
            reason: BlockReason::Approval,
            message: None,
        })
    }

    /// Assert the plan is blocked on a lossy step that waits for its loss to be accepted
    ///
    /// # Arguments
    ///
    /// * `step` - The step the plan must be blocked on
    #[must_use]
    pub fn expect_blocked_lossy<T: Into<String>>(self, step: T) -> Self {
        // a lossy step waits for an approval with its confirmation code
        self.action(ScenarioAction::ExpectBlocked {
            step: step.into(),
            reason: BlockReason::LossAcceptance {
                confirm: String::new(),
            },
            message: None,
        })
    }

    /// Assert the plan is blocked on a step for a reason, with text in the block message
    ///
    /// # Arguments
    ///
    /// * `step` - The step the plan must be blocked on
    /// * `reason` - Why
    /// * `message` - Text the block message must contain
    #[must_use]
    pub fn expect_blocked_with<T: Into<String>, M: Into<String>>(
        self,
        step: T,
        reason: BlockReason,
        message: M,
    ) -> Self {
        // one expectation with a message
        self.action(ScenarioAction::ExpectBlocked {
            step: step.into(),
            reason,
            message: Some(message.into()),
        })
    }

    /// Assert the plan is refused before anything runs, naming an irreversible step
    ///
    /// # Arguments
    ///
    /// * `step` - The step the refusal must name
    #[must_use]
    pub fn expect_refused<T: Into<String>>(self, step: T) -> Self {
        // one expectation
        self.action(ScenarioAction::ExpectRefused { step: step.into() })
    }

    /// Assert a lossy approval was refused for a reason
    ///
    /// # Arguments
    ///
    /// * `step` - The lossy step the approval named
    /// * `refusal` - Why its code must be refused
    #[must_use]
    pub fn expect_confirm_refused<T: Into<String>>(self, step: T, refusal: ConfirmRefusal) -> Self {
        // one expectation
        self.action(ScenarioAction::ExpectConfirmRefused {
            step: step.into(),
            refusal,
        })
    }

    /// Approve a step that isn't lossy, naming a backup
    ///
    /// # Arguments
    ///
    /// * `step` - The step
    /// * `backup` - The backup the approval names
    #[must_use]
    pub fn approve<T: Into<String>>(self, step: T, backup: BackupRef) -> Self {
        // an approval is the step and its backup
        self.action(ScenarioAction::Approve(UpgradeApproval {
            step: step.into(),
            backup: backup.0,
            ..UpgradeApproval::default()
        }))
    }

    /// Approve a step with an approval built by hand, such as one that accepts issue groups
    ///
    /// # Arguments
    ///
    /// * `approval` - The approval
    #[must_use]
    pub fn approve_with(self, approval: UpgradeApproval) -> Self {
        // the approval as given
        self.action(ScenarioAction::Approve(approval))
    }

    /// Approve a lossy step with the confirmation code its blocked status shows, as an admin does
    ///
    /// # Arguments
    ///
    /// * `step` - The lossy step
    /// * `backup` - The backup the approval names
    #[must_use]
    pub fn approve_loss<T: Into<String>>(self, step: T, backup: BackupRef) -> Self {
        // the current code and a backup
        self.action(ScenarioAction::ApproveLoss {
            step: step.into(),
            backup: Some(backup),
            confirm: ConfirmSource::Current,
        })
    }

    /// Approve a lossy step with the current code and no backup, to check that it is refused
    ///
    /// # Arguments
    ///
    /// * `step` - The lossy step
    #[must_use]
    pub fn approve_loss_without_backup<T: Into<String>>(self, step: T) -> Self {
        // the current code but no backup
        self.action(ScenarioAction::ApproveLoss {
            step: step.into(),
            backup: None,
            confirm: ConfirmSource::Current,
        })
    }

    /// Approve a lossy step with the code this scenario last gave for it, to check that reuse is
    /// refused
    ///
    /// # Arguments
    ///
    /// * `step` - The lossy step
    /// * `backup` - The backup the approval names
    #[must_use]
    pub fn approve_loss_reusing<T: Into<String>>(self, step: T, backup: BackupRef) -> Self {
        // the previous code
        self.action(ScenarioAction::ApproveLoss {
            step: step.into(),
            backup: Some(backup),
            confirm: ConfirmSource::Previous,
        })
    }

    /// Approve a lossy step with a literal code, to check that a wrong code is refused
    ///
    /// # Arguments
    ///
    /// * `step` - The lossy step
    /// * `backup` - The backup the approval names
    /// * `code` - The code to give
    #[must_use]
    pub fn approve_loss_with<T: Into<String>, C: Into<String>>(
        self,
        step: T,
        backup: BackupRef,
        code: C,
    ) -> Self {
        // a literal code
        self.action(ScenarioAction::ApproveLoss {
            step: step.into(),
            backup: Some(backup),
            confirm: ConfirmSource::Literal(code.into()),
        })
    }

    /// Approve every gated step of the plan that isn't lossy with one backup
    ///
    /// # Arguments
    ///
    /// * `backup` - The backup the approvals name
    #[must_use]
    pub fn approve_all(self, backup: BackupRef) -> Self {
        // one bulk approval
        self.action(ScenarioAction::ApproveAll(backup))
    }

    /// Set `dangerous_downgrade_lossy` on the deployment
    ///
    /// # Arguments
    ///
    /// * `on` - Whether the setting is on
    #[must_use]
    pub fn set_dev_lossy(self, on: bool) -> Self {
        // one setting
        self.action(ScenarioAction::SetDevLossy(on))
    }

    /// Run exactly one step
    #[must_use]
    pub fn step(self) -> Self {
        // one step action
        self.action(ScenarioAction::Step)
    }

    /// Run a check
    ///
    /// # Arguments
    ///
    /// * `check` - The check
    #[must_use]
    pub fn check(self, check: Check) -> Self {
        // one check
        self.action(ScenarioAction::Check(check))
    }

    /// Record the deployment's data under a label
    ///
    /// # Arguments
    ///
    /// * `label` - What to call the capture
    #[must_use]
    pub fn capture<T: Into<String>>(self, label: T) -> Self {
        // one capture
        self.action(ScenarioAction::Capture(label.into()))
    }

    /// Inject a failure or drift
    ///
    /// # Arguments
    ///
    /// * `injection` - What to inject
    #[must_use]
    pub fn inject(self, injection: Injection) -> Self {
        // one injection
        self.action(ScenarioAction::Inject(injection))
    }

    /// Deploy an image (cluster driver only)
    ///
    /// # Arguments
    ///
    /// * `image` - The image to deploy
    #[must_use]
    pub fn deploy<T: Into<String>>(self, image: T) -> Self {
        // one deploy action
        self.action(ScenarioAction::Deploy(image.into()))
    }

    /// Restart the operator or the running Job's pod (cluster driver only)
    ///
    /// # Arguments
    ///
    /// * `target` - What to restart
    #[must_use]
    pub fn restart(self, target: RestartTarget) -> Self {
        // one restart action
        self.action(ScenarioAction::Restart(target))
    }

    /// Check that a driver can run every action of this scenario, before any runs
    ///
    /// # Arguments
    ///
    /// * `kind` - The driver that will run the scenario
    pub fn validate(&self, kind: DriverKind) -> Result<(), HarnessError> {
        // check each action against the driver that will run it
        for action in &self.actions {
            match kind {
                DriverKind::Direct if action.cluster_only() => {
                    return Err(HarnessError::ClusterOnly {
                        scenario: self.name.clone(),
                        action: action.render(),
                    });
                }
                DriverKind::Cluster if action.direct_only() => {
                    return Err(HarnessError::DirectOnly {
                        scenario: self.name.clone(),
                        action: action.render(),
                    });
                }
                _ => (),
            }
        }
        Ok(())
    }

    /// Run the scenario with a driver, action by action, returning what each action did
    ///
    /// Every action is checked against the driver first, so a scenario with an action the driver
    /// can't run fails before anything runs.
    ///
    /// # Arguments
    ///
    /// * `driver` - The driver to run the actions with
    pub async fn run_with<D: ScenarioDriver>(
        &self,
        driver: &mut D,
    ) -> Result<Vec<ActionRecord>, HarnessError> {
        // refuse the scenario up front when the driver can't run all of it
        self.validate(driver.kind())?;
        // run each action in order, stopping at the first failure
        let mut log = Vec::with_capacity(self.actions.len());
        for action in &self.actions {
            log.push(driver.run(action, self).await?);
        }
        Ok(log)
    }

    /// Run the scenario under a new direct driver
    pub async fn run(&self) -> Result<Vec<ActionRecord>, HarnessError> {
        // start the driver against the test backends and run the script
        let mut driver = DirectDriver::start(self).await?;
        self.run_with(&mut driver).await
    }
}
