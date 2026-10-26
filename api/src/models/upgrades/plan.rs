//! Planning the steps that bring a cluster to a target revision, and the plan types the planner
//! composes

use std::collections::BTreeSet;

use super::{
    AppliedStep, Catalog, CatalogEntry, Family, FieldLoss, LossReport, PlannedStep, Reversibility,
    RevisionId, StepDef, StepLoss, StepState, UpgradeComponent, UpgradeError, UpgradeState,
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

impl Catalog {
    /// Check that a deployment's recorded revision is one this catalog plans from
    ///
    /// An unknown pre-release id is refused first, naming its base, so a branch's revision gets
    /// the revert instruction; then a revision below the catalog's floor, naming the release to
    /// install first; then a revision newer than the catalog, and then any other unknown revision.
    ///
    /// # Arguments
    ///
    /// * `current` - The revision the deployment is at
    pub fn check_current(&self, current: &RevisionId) -> Result<(), UpgradeError> {
        // a revision this catalog knows can be planned from
        if self.knows(current) {
            return Ok(());
        }
        // a branch's revision this build doesn't carry must be reverted by that branch's image
        if current.is_prerelease() {
            return Err(UpgradeError::UnknownPrerelease {
                current: current.clone(),
                base: current.base(),
            });
        }
        // a revision squashed into the baseline needs an intermediate release first
        self.check_floor(current)?;
        // a revision past the end of the catalog came from a newer build
        let latest = self.latest();
        if *current > latest {
            return Err(UpgradeError::AheadOfCatalog {
                current: current.clone(),
                latest,
            });
        }
        Err(UpgradeError::UnknownRevision(current.to_string()))
    }

    /// Check that a target is a revision this catalog knows
    ///
    /// # Arguments
    ///
    /// * `target` - The revision to reach
    pub fn check_target(&self, target: &RevisionId) -> Result<(), UpgradeError> {
        // a target must be a revision some edge starts from or reaches
        if self.knows(target) {
            Ok(())
        } else {
            Err(UpgradeError::UnknownRevision(target.to_string()))
        }
    }

    /// Check that a path down crosses no irreversible step the deployment finished, and undoes
    /// every finished step it leaves
    ///
    /// Each edge leading down undoes a forward edge; a step of that forward edge declared
    /// `Irreversible` refuses the whole path once the history counts it as finished, before
    /// anything runs, naming the step and its revision. A step the deployment never finished, as
    /// after a partial upgrade, has nothing to undo.
    ///
    /// The forward edges the deployment took need not be the ones the path's edges undo: it may
    /// have come up through a chain of edges while the path goes down a skip edge, the other way
    /// round, or the path itself may go up a skip edge and back down a reverse edge. So for each
    /// run of edges leading down, every forward step the history counts as finished, or that an
    /// earlier forward edge of the path runs, whose revision the run leaves (at or below the
    /// revision the run starts from and not at or below the one it ends at) is checked too: an
    /// irreversible one refuses the path as irreversible, and one with a transform that no step
    /// of the run reverses refuses it as unreversed, since the run would lower the revision while
    /// the data stays in the newer layout.
    ///
    /// # Arguments
    ///
    /// * `state` - The deployment's recorded upgrade state
    /// * `path` - The edges of the path, in order
    pub fn check_reversible(
        &self,
        state: &UpgradeState,
        path: &[&'static CatalogEntry],
    ) -> Result<(), UpgradeError> {
        // look at the forward edge each edge leading down undoes
        for edge in path.iter().filter(|edge| edge.is_downward()) {
            let Some(forward) = self.revert_edge(edge) else {
                continue;
            };
            // a finished irreversible step can't be undone
            let irreversible = forward.steps.iter().find(|step| {
                matches!(step.reversibility, Some(Reversibility::Irreversible { .. }))
                    && self.finished(state, forward.id, step.id)
            });
            if let Some(step) = irreversible {
                return Err(UpgradeError::Irreversible {
                    revision: forward.revision(),
                    step: step.id.to_owned(),
                });
            }
        }
        // the steps an unfinished upgrade ran above the deployment's revision stay in place too
        self.check_above(state, path)?;
        // walk the runs of edges leading the same way, from the deployment's revision
        let mut top = state.revision.clone();
        let mut ran: Vec<(&'static CatalogEntry, &'static StepDef)> = Vec::new();
        for run in path.chunk_by(|first, second| first.is_downward() == second.is_downward()) {
            let Some(last) = run.last() else {
                continue;
            };
            let bottom = last.revision();
            // a forward run finishes its steps before any later run leads down
            if !last.is_downward() {
                ran.extend(
                    run.iter()
                        .flat_map(|edge| edge.steps.iter().map(move |step| (*edge, step))),
                );
                top = bottom;
                continue;
            }
            // a run leading down must undo the finished forward steps of the revisions it leaves
            self.check_left(state, run, &ran, &top, &bottom)?;
            top = bottom;
        }
        Ok(())
    }

    /// Check that a path leading down undoes every forward step an unfinished upgrade ran above
    /// the deployment's revision
    ///
    /// An upgrade that stopped partway along an edge (its target cleared, lowered, or set to the
    /// deployment's revision before the edge finished) leaves the revision recorded below the
    /// edge while steps of the edge finished or started and changed data. A path down from the
    /// recorded revision never takes that edge's reverse, so such a step stays in place unless a
    /// step of the path reverses it: one that is irreversible refuses the path as irreversible,
    /// and one with a transform that no step of the path reverses refuses it as unreversed. A path
    /// that goes up through the edge itself runs its steps, which the runs of the path check.
    ///
    /// # Arguments
    ///
    /// * `state` - The deployment's recorded upgrade state
    /// * `path` - The edges of the path, in order
    fn check_above(
        &self,
        state: &UpgradeState,
        path: &[&'static CatalogEntry],
    ) -> Result<(), UpgradeError> {
        // only a path that leads down leaves anything behind
        let Some(end) = path
            .iter()
            .any(|edge| edge.is_downward())
            .then(|| path.last().map(|edge| edge.revision()))
            .flatten()
        else {
            return Ok(());
        };
        // the forward steps above the deployment's revision that ran, on edges the path doesn't
        // take, of revisions the path ends below
        let above = self
            .steps()
            .filter(|(edge, step)| {
                let own = edge.revision();
                !edge.is_downward()
                    && !self.at_or_below(&own, &state.revision)
                    && !self.at_or_below(&own, &end)
                    && !path.iter().any(|taken| std::ptr::eq(*taken, *edge))
                    && self.touched(state, edge.id, step.id)
            })
            .collect::<Vec<_>>();
        // any of them that is irreversible refuses the path
        let irreversible = above.iter().find(|(_, step)| {
            matches!(step.reversibility, Some(Reversibility::Irreversible { .. }))
        });
        if let Some((edge, step)) = irreversible {
            return Err(UpgradeError::Irreversible {
                revision: edge.revision(),
                step: step.id.to_owned(),
            });
        }
        // and every one with a transform needs a step of the path undoing it
        let reversed = path
            .iter()
            .filter(|edge| edge.is_downward())
            .flat_map(|edge| edge.steps.iter().filter_map(|step| step.reverses))
            .collect::<BTreeSet<_>>();
        let unreversed = above
            .iter()
            .find(|(_, step)| has_transform(step) && !reversed.contains(step.id));
        if let Some((edge, step)) = unreversed {
            return Err(UpgradeError::Unreversed {
                revision: edge.revision(),
                step: step.id.to_owned(),
            });
        }
        Ok(())
    }

    /// Check that a run of edges leading down undoes every finished forward step of the revisions
    /// it leaves: none is irreversible, and every one with a transform has a step of the run
    /// reversing it
    ///
    /// # Arguments
    ///
    /// * `state` - The deployment's recorded upgrade state
    /// * `run` - The edges of the run, in order
    /// * `ran` - The forward steps earlier edges of the path run
    /// * `top` - The revision the run starts from
    /// * `bottom` - The revision the run ends at
    fn check_left(
        &self,
        state: &UpgradeState,
        run: &[&'static CatalogEntry],
        ran: &[(&'static CatalogEntry, &'static StepDef)],
        top: &RevisionId,
        bottom: &RevisionId,
    ) -> Result<(), UpgradeError> {
        // find the forward steps finished by the history or by the path, of the revisions left
        let finished = self
            .steps()
            .filter(|(edge, step)| !edge.is_downward() && self.finished(state, edge.id, step.id));
        let left = finished
            .chain(ran.iter().copied())
            .filter(|(edge, _)| {
                let own = edge.revision();
                self.at_or_below(&own, top) && !self.at_or_below(&own, bottom)
            })
            .collect::<Vec<_>>();
        // any of them that is irreversible refuses the path, whichever edges it took
        let irreversible = left.iter().find(|(_, step)| {
            matches!(step.reversibility, Some(Reversibility::Irreversible { .. }))
        });
        if let Some((edge, step)) = irreversible {
            return Err(UpgradeError::Irreversible {
                revision: edge.revision(),
                step: step.id.to_owned(),
            });
        }
        // and every one with a transform needs a step of the run undoing it
        let reversed = run
            .iter()
            .flat_map(|edge| edge.steps.iter().filter_map(|step| step.reverses))
            .collect::<BTreeSet<_>>();
        let unreversed = left
            .iter()
            .find(|(_, step)| has_transform(step) && !reversed.contains(step.id));
        if let Some((edge, step)) = unreversed {
            return Err(UpgradeError::Unreversed {
                revision: edge.revision(),
                step: step.id.to_owned(),
            });
        }
        Ok(())
    }

    /// Describe a catalog step as a pending step of a plan, naming what a lossy reverse loses
    ///
    /// # Arguments
    ///
    /// * `edge` - The edge holding the step
    /// * `step` - The step to plan
    fn describe(&self, edge: &CatalogEntry, step: &StepDef) -> PlannedStep {
        // a step of an edge leading down may reverse a lossy forward step
        let loss = step
            .reverses
            .filter(|_| edge.is_downward())
            .and_then(|forward| self.step(forward))
            .and_then(|(_, forward)| loss_of(edge, step, forward));
        PlannedStep {
            loss,
            ..planned(edge, step)
        }
    }

    /// Name what a step of an edge leading down loses, when it reverses a `LossyRevert` step
    ///
    /// The loss names the fields and leaves their counts and backup coverage to the step's check.
    ///
    /// # Arguments
    ///
    /// * `step` - The id of the step of the edge leading down
    #[must_use]
    pub fn planned_loss(&self, step: &str) -> Option<StepLoss> {
        // the step must undo a forward step the catalog holds
        let (edge, reverse) = self.step(step)?;
        if !edge.is_downward() {
            return None;
        }
        let (_, forward) = self.step(reverse.reverses?)?;
        loss_of(edge, reverse, forward)
    }

    /// Plan the steps that bring a deployment from its revision to a target revision
    ///
    /// The steps of every edge on the shortest path are planned in order, leaving out the steps
    /// the history counts as finished ([`Catalog::finished`]), so a plan resumes where an
    /// interrupted upgrade stopped. A path down across a finished irreversible step is refused
    /// ([`Catalog::check_reversible`]).
    ///
    /// On an upward edge, a step that needs components running ([`StepDef::needs_running`]) is
    /// planned before the first earlier step that needs none and pauses one of them
    /// (`lead_first`). Steps on a path down keep their place.
    ///
    /// # Arguments
    ///
    /// * `state` - The deployment's recorded upgrade state
    /// * `target` - The revision to reach
    pub fn plan(
        &self,
        state: &UpgradeState,
        target: &RevisionId,
    ) -> Result<Vec<PlannedStep>, UpgradeError> {
        // a state in a newer layout may record things this build would misread or drop
        state.check_schema()?;
        // both revisions must be ones this catalog plans between
        self.check_current(&state.revision)?;
        self.check_target(target)?;
        // find the edges between them and the work left on each
        let path = self.path(&state.revision, target)?;
        // a path down may not cross a finished irreversible step
        self.check_reversible(state, &path)?;
        let steps = lead_first(
            path.into_iter()
                .flat_map(|edge| {
                    self.edge_steps(state, edge)
                        .into_iter()
                        .map(move |step| (edge, step))
                })
                .collect(),
            |(edge, step)| {
                if edge.is_downward() {
                    &[]
                } else {
                    step.needs_running
                }
            },
            |(edge, step)| if edge.is_downward() { &[] } else { step.pause },
        )
        .into_iter()
        .map(|(edge, step)| self.describe(edge, step))
        .collect();
        Ok(steps)
    }

    /// Build the state of a deployment that reached a revision through this catalog: at that
    /// revision, with every step of the shortest path from a baseline recorded as done
    ///
    /// A revision no baseline reaches gets a state with no history.
    ///
    /// # Arguments
    ///
    /// * `revision` - The revision the deployment reached
    /// * `version` - The Thorium version recording the state
    #[must_use]
    pub fn reached(&self, revision: &RevisionId, version: &str) -> UpgradeState {
        // start at the revision with an empty history
        let mut state = UpgradeState::new(revision.clone(), version);
        // find the first baseline with a path to the revision
        let path = self
            .edges
            .iter()
            .filter(|edge| edge.from.is_none())
            .find_map(|baseline| self.path(&baseline.revision(), revision).ok())
            .unwrap_or_default();
        // record every step along it as done, edge by edge
        for edge in path {
            for step in self.edge_steps(&state, edge) {
                state.record(AppliedStep {
                    revision: edge.id.to_owned(),
                    step: step.id.to_owned(),
                    outcome: StepState::Done,
                    ..AppliedStep::default()
                });
            }
        }
        state
    }

    /// Pick the revision a deployment should move to
    ///
    /// An explicit target always wins and may be any revision this catalog knows, pre-releases
    /// and siblings included. Without one, `auto_target_dev` targets the newest revision on the
    /// primary chain from the recorded revision (releases and stacked pre-releases, never a
    /// sibling), and otherwise there is no target, so the deployment never plans a pre-release
    /// edge on its own.
    ///
    /// # Arguments
    ///
    /// * `target` - The target revision set on the deployment, if any
    /// * `auto_target_dev` - Whether to follow the primary chain when no target is set
    /// * `current` - The revision the deployment is at
    pub fn resolve_target(
        &self,
        target: Option<&str>,
        auto_target_dev: bool,
        current: &RevisionId,
    ) -> Result<Option<RevisionId>, UpgradeError> {
        match target {
            // an explicit target must be a revision we know
            Some(raw) => {
                let target: RevisionId = raw.parse()?;
                self.check_target(&target)?;
                Ok(Some(target))
            }
            // dev clusters follow the newest revision on their primary chain
            None if auto_target_dev => Ok(self.primary_chain(current).into_iter().max()),
            // anything else waits for an admin to pick a target
            None => Ok(None),
        }
    }
}

/// Whether a step runs an object transform
///
/// # Arguments
///
/// * `step` - The step
#[cfg(feature = "scylla-utils")]
pub(super) fn has_transform(step: &StepDef) -> bool {
    // the step names the transform it runs
    step.transform.is_some()
}

/// Whether a step runs an object transform: never in a build without transforms
///
/// # Arguments
///
/// * `_step` - The step
#[cfg(not(feature = "scylla-utils"))]
pub(super) fn has_transform(_step: &StepDef) -> bool {
    // a build without transforms has no step running one
    false
}

/// Describe a catalog step as a pending step of a plan
///
/// # Arguments
///
/// * `edge` - The edge holding the step
/// * `step` - The step to plan
pub(super) fn planned(edge: &CatalogEntry, step: &StepDef) -> PlannedStep {
    // copy what the status shows about the step and mark it pending
    PlannedStep {
        revision: edge.id.to_owned(),
        step: step.id.to_owned(),
        kind: step.kind,
        description: step.description.to_owned(),
        pause: step.pause.to_vec(),
        state: StepState::Pending,
        message: None,
        phase: step.phase,
        prerelease: edge.prerelease,
        reverse: edge.is_downward(),
        pass: None,
        blocked: None,
        loss: None,
    }
}

/// Describe what the reverse of a lossy forward step loses: the fields it fills, each named
/// without counts, since the step's check estimates how many objects each affects and whether
/// backups cover it
///
/// Returns `None` unless the forward step is a `LossyRevert`.
///
/// # Arguments
///
/// * `edge` - The edge leading down that holds the reverse step
/// * `step` - The reverse step
/// * `forward` - The forward step it reverses
pub(super) fn loss_of(edge: &CatalogEntry, step: &StepDef, forward: &StepDef) -> Option<StepLoss> {
    // only a lossy reverse loses anything
    let Some(Reversibility::LossyRevert {
        lost_fields,
        description,
        ..
    }) = forward.reversibility
    else {
        return None;
    };
    // name each lost field, leaving its count and backup coverage to the step's check
    let fields = lost_fields
        .iter()
        .map(|field| FieldLoss {
            field: (*field).to_owned(),
            objects: 0,
            backed_up: false,
        })
        .collect();
    Some(StepLoss {
        revision: edge.id.to_owned(),
        step: step.id.to_owned(),
        description: description.to_owned(),
        fields,
    })
}

/// Plan the steps that bring a cluster from its revision to a target revision in this build's
/// catalog
///
/// See [`Catalog::plan`].
///
/// # Arguments
///
/// * `state` - The cluster's recorded upgrade state
/// * `target` - The revision to upgrade to
pub fn plan(state: &UpgradeState, target: &RevisionId) -> Result<Vec<PlannedStep>, UpgradeError> {
    // plan over the catalog this build deploys
    Catalog::PRODUCTION.plan(state, target)
}

/// Move each step that needs components running ahead of the first earlier step that pauses one
///
/// A step that lists [`StepDef::needs_running`] components can't come after an earlier edge's step
/// that paused one of them, so across several edges it moves to just before the first step that
/// pauses any of them. A step with no such step before it stays where it is, and the steps that
/// move keep their order.
///
/// # Arguments
///
/// * `steps` - The steps of a path, in path order
/// * `needs` - The components a step needs running
/// * `pauses` - The components a step pauses
pub(super) fn lead_first<T>(
    steps: Vec<T>,
    needs: impl Fn(&T) -> &'static [UpgradeComponent],
    pauses: impl Fn(&T) -> &'static [UpgradeComponent],
) -> Vec<T> {
    // find the place each step sorts at: a step needing components takes the place of the first
    // earlier step that needs none and pauses one of them, and sorts ahead of it; every other step
    // keeps its own place
    let keys = steps
        .iter()
        .enumerate()
        .map(|(index, step)| {
            let needed = needs(step);
            let blocker = steps[..index].iter().position(|earlier| {
                needs(earlier).is_empty() && pauses(earlier).iter().any(|c| needed.contains(c))
            });
            if needed.is_empty() {
                (index, 1, index)
            } else {
                (blocker.unwrap_or(index), 0, index)
            }
        })
        .collect::<Vec<_>>();
    // sort the steps by their keys, which are all different
    let mut keyed = keys.into_iter().zip(steps).collect::<Vec<_>>();
    keyed.sort_by_key(|(key, _)| *key);
    keyed.into_iter().map(|(_, step)| step).collect()
}

/// Pick the revision a cluster should move to in this build's catalog
///
/// An explicit target always wins. Without one, `auto_target_dev` targets this build's latest
/// revision, and otherwise there is no target and the cluster waits for one. The production
/// catalog holds no pre-release or sibling edges, so its latest revision is the end of every
/// primary chain; see [`Catalog::resolve_target`] for a catalog that has them.
///
/// # Arguments
///
/// * `target` - The target revision set on the cluster, if any
/// * `auto_target_dev` - Whether to target the latest revision when no target is set
pub fn resolve_target(
    target: Option<&str>,
    auto_target_dev: bool,
) -> Result<Option<RevisionId>, UpgradeError> {
    // resolve over the catalog this build deploys, whose primary chain ends at its latest revision
    let catalog = Catalog::PRODUCTION;
    catalog.resolve_target(target, auto_target_dev, &catalog.latest())
}
