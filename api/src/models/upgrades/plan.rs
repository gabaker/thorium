//! Planning the steps that bring a cluster to a target revision, and the plan types the planner
//! composes

use super::{
    CATALOG, Family, LossReport, PlannedStep, RevisionId, StepState, UpgradeError, UpgradeState,
    entry, latest,
};

/// The work that moves a deployment from its revision to a target, composed by the planner
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradePlan {
    /// The revision the deployment is at
    pub from: RevisionId,
    /// The target revision
    pub to: RevisionId,
    /// Whether the path goes forward or back
    pub direction: PlanDirection,
    /// The revisions the path visits, in order, ending at `to`
    pub path: Vec<RevisionId>,
    /// The work to run, in order
    pub items: Vec<PlanItem>,
    /// The loss across every lossy step on the path, when there is any
    pub loss: Option<LossReport>,
}

/// Which way a plan moves
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanDirection {
    /// Release, skip, and pre-release forward edges
    Upgrade,
    /// Revert and reverse edges
    Downgrade,
}

/// One unit of planned work
///
/// Kept unboxed: a plan holds a handful of items, and most of them are single steps
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanItem {
    /// One converge to the target snapshot, replacing every additive step on the path
    Converge {
        /// The snapshot converged to
        target: RevisionId,
        /// The additive steps this converge replaces, for the status and history
        steps: Vec<PlannedStep>,
    },
    /// One migration Job running these passes; passes in one item touch different families and
    /// run concurrently
    Passes(Vec<PlannedPass>),
    /// One step run as is: a config or check step, a rebuild, a contract step, or a manual step
    Step(PlannedStep),
}

/// One read-transform-write sweep over a family, possibly fusing several transforms
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedPass {
    /// The family the pass rewrites
    pub family: Family,
    /// The transforms applied in memory to each object, in order
    pub chain: Vec<ChainLink>,
    /// Whether the history holds a reverse of a chain step newer than its last forward `Done`, so
    /// the pass treats the old form as authoritative and recomputes the new form for every object
    /// that has one, instead of skipping objects whose new form is present
    pub after_reverse: bool,
    /// The catalog steps the pass runs, for the status and history
    pub steps: Vec<PlannedStep>,
}

/// One transform in a pass's chain
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainLink {
    /// The revision the step belongs to
    pub revision: RevisionId,
    /// The step id
    pub step: String,
    /// The layout this link reads
    pub from: u32,
    /// The layout this link writes
    pub to: u32,
}

/// Check that a revision is in the catalog and not newer than it
///
/// # Arguments
///
/// * `id` - The revision to check
fn known(id: &RevisionId) -> Result<(), UpgradeError> {
    // a revision past the end of the catalog came from a newer build
    let latest = latest();
    if *id > latest {
        return Err(UpgradeError::AheadOfCatalog {
            current: id.clone(),
            latest,
        });
    }
    // anything else must be a revision we know
    match entry(id) {
        Some(_) => Ok(()),
        None => Err(UpgradeError::UnknownRevision(id.to_string())),
    }
}

/// Plan the steps that bring a cluster from its revision to a target revision
///
/// Steps the state already records as done or skipped are left out, so a plan resumes where
/// an interrupted upgrade stopped.
///
/// # Arguments
///
/// * `state` - The cluster's recorded upgrade state
/// * `target` - The revision to upgrade to
pub fn plan(state: &UpgradeState, target: &RevisionId) -> Result<Vec<PlannedStep>, UpgradeError> {
    // a state in a newer layout may record things this build would misread or drop
    state.check_schema()?;
    // both revisions must be ones we know
    known(&state.revision)?;
    if *target > latest() {
        return Err(UpgradeError::UnknownRevision(target.to_string()));
    }
    known(target)?;
    // never move backwards
    if *target < state.revision {
        return Err(UpgradeError::Backwards {
            current: state.revision.clone(),
            target: target.clone(),
        });
    }
    // collect the unfinished steps of every revision after the current one up to the target
    let steps = CATALOG
        .iter()
        .filter(|entry| entry.id > state.revision.as_str() && entry.id <= target.as_str())
        .flat_map(|entry| entry.steps.iter().map(move |step| (entry, step)))
        .filter(|(entry, step)| !state.finished(entry.id, step.id))
        .map(|(entry, step)| PlannedStep {
            revision: entry.id.to_owned(),
            step: step.id.to_owned(),
            kind: step.kind,
            description: step.description.to_owned(),
            pause: step.pause.to_vec(),
            state: StepState::Pending,
            message: None,
            phase: step.phase,
            prerelease: entry.prerelease,
            reverse: false,
            pass: None,
            blocked: None,
            loss: None,
        })
        .collect();
    Ok(steps)
}

/// Pick the revision a cluster should upgrade to
///
/// An explicit target always wins. Without one, `auto_target_dev` targets this build's latest
/// revision, and otherwise there is no target and the cluster waits for one.
///
/// # Arguments
///
/// * `target` - The target revision set on the cluster, if any
/// * `auto_target_dev` - Whether to target the latest revision when no target is set
pub fn resolve_target(
    target: Option<&str>,
    auto_target_dev: bool,
) -> Result<Option<RevisionId>, UpgradeError> {
    match target {
        // an explicit target must be a revision we know
        Some(raw) => {
            let target: RevisionId = raw.parse()?;
            if target > latest() {
                return Err(UpgradeError::UnknownRevision(target.to_string()));
            }
            known(&target)?;
            Ok(Some(target))
        }
        // dev clusters follow the latest revision
        None if auto_target_dev => Ok(Some(latest())),
        // anything else waits for an admin to pick a target
        None => Ok(None),
    }
}
