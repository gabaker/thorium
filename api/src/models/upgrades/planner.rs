//! Composing the steps of a path into a plan of one converge step, fused passes, and barrier
//! steps, and running a fused pass's chain of transforms over one record in memory
//!
//! Composition follows the expand, migrate, contract order on every run of edges leading the same
//! way: every additive step becomes one converge to the run's last revision, placed first;
//! transforms between two barriers fuse into passes, one per family and chain of layouts; every
//! other step runs as is where it sits, except contract steps, which run last. A step a later step
//! supersedes is dropped. On a run of edges leading down, each reverse step runs the `down` of the
//! forward step it reverses, so reverse transforms fuse into passes the same way.

use std::collections::BTreeSet;
use std::fmt;

use super::plan::{loss_of, planned};
use super::{
    Catalog, CatalogEntry, ChainLink, Family, LossReport, MigrationPhase, PlanDirection, PlanItem,
    PlannedPass, PlannedStep, Record, RecordClass, Reversibility, RevisionId, StepDef, StepKind,
    StepState, TransformDef, TransformIssue, UpgradeError, UpgradePlan, UpgradeState, Work,
};
use crate::models::schema::PassCheckpoint;

/// A step of a path a deployment still has to run, with the edge holding it
#[derive(Debug, Clone, Copy)]
struct Pending {
    /// The edge holding the step
    edge: &'static CatalogEntry,
    /// The step
    step: &'static StepDef,
    /// Whether the history holds a reverse of this forward step newer than its last `Done`
    after_reverse: bool,
    /// For a step of an edge leading down, the forward step it reverses
    forward: Option<&'static StepDef>,
}

impl Pending {
    /// Describe this step as a pending step of a plan, with the loss of a lossy reverse
    fn planned(&self) -> PlannedStep {
        // copy what the status shows about the step, naming what a lossy reverse loses
        PlannedStep {
            loss: self
                .forward
                .and_then(|forward| loss_of(self.edge, self.step, forward)),
            ..planned(self.edge, self.step)
        }
    }

    /// Get the transform this step runs: its own on a forward edge, or on an edge leading down
    /// the transform of the forward step it reverses, whose `down` it runs
    ///
    /// A reverse of a `DualWrite` step runs no pass, since every object still holds its old form,
    /// and an irreversible step has no reverse to run.
    fn transform(&self) -> Option<&'static TransformDef> {
        // a forward step runs its own transform
        if !self.edge.is_downward() {
            return self.step.transform;
        }
        // a reverse step runs the forward step's `down` unless that step needs no pass back
        self.forward
            .and_then(|forward| match forward.reversibility {
                Some(Reversibility::DualWrite { .. } | Reversibility::Irreversible { .. }) => None,
                _ => forward.transform,
            })
    }

    /// Whether this step makes an earlier step redundant when both are in one plan
    ///
    /// A step does when it lists the earlier step in its `supersedes`. On an edge leading down a
    /// reverse step also does when the forward step it reverses is superseded by the forward
    /// step the earlier reverse step reverses: the reverses run in the opposite order, so the
    /// reverse that runs last, the one bringing the target revision's form, is kept.
    ///
    /// # Arguments
    ///
    /// * `earlier` - The earlier step
    fn supersedes(&self, earlier: &Pending) -> bool {
        // a step names the steps it supersedes
        if self.step.supersedes.contains(&earlier.step.id) {
            return true;
        }
        // reverses supersede each other the opposite way to the forward steps they undo
        match (self.forward, earlier.forward) {
            (Some(later), Some(earlier)) => earlier.supersedes.contains(&later.id),
            _ => false,
        }
    }
}

/// The layout a link of a chain reads and the layout it writes, going the chain's way
///
/// # Arguments
///
/// * `transform` - The link's transform
/// * `direction` - Which way the chain converts records
fn layouts(transform: &TransformDef, direction: PlanDirection) -> (u32, u32) {
    // a link going down reads the transform's new layout and writes its old one
    match direction {
        PlanDirection::Upgrade => (transform.from, transform.to),
        PlanDirection::Downgrade => (transform.to, transform.from),
    }
}

/// How the planner places a step
enum Role {
    /// Additive schema (an infra step tagged `Expand` with no migration), folded into the
    /// converge step placed first
    Additive,
    /// An object transform, fused into a pass with the transforms around it
    Transform(&'static TransformDef),
    /// A contract step, run after everything else: a barrier to fusion where it sits, and with a
    /// transform a pass fused with the contract transforms around it
    Contract(Option<&'static TransformDef>),
    /// Any other step (config, check, manual, rebuild, a migration without a transform even when
    /// tagged `Expand`, or the reverse of a `DualWrite` step): a barrier to fusion that runs where
    /// it sits
    Barrier,
}

impl Role {
    /// Decide how the planner places a step
    ///
    /// # Arguments
    ///
    /// * `pending` - The step to place
    fn of(pending: &Pending) -> Self {
        // a transform fuses whatever phase it declares, contract steps run last, and only
        // additive schema converges: a config step or a migration tagged `Expand` still has work
        // the converge wouldn't run
        let step = pending.step;
        match (pending.transform(), step.kind, step.phase) {
            (transform, _, Some(MigrationPhase::Contract)) => Role::Contract(transform),
            (Some(transform), _, _) => Role::Transform(transform),
            (None, StepKind::Infra, Some(MigrationPhase::Expand))
                if step.thoradm_migration.is_none() =>
            {
                Role::Additive
            }
            (None, _, _) => Role::Barrier,
        }
    }
}

/// A pass being built while a group of transforms is fused
struct Building {
    /// The family the pass rewrites
    family: Family,
    /// The transforms of the pass, in chain order
    links: Vec<(Pending, &'static TransformDef)>,
    /// Which of the group's `Passes` items the pass runs in, counting from 0
    level: usize,
    /// The passes (by their index in the group) that must finish before this one starts
    after: BTreeSet<usize>,
}

impl Building {
    /// Turn this pass into a planned pass
    ///
    /// # Arguments
    ///
    /// * `direction` - Which way the pass converts records
    fn finish(self, direction: PlanDirection) -> PlannedPass {
        // name the pass in each step's status by its family
        let family = self.family;
        let name = family.to_string();
        // describe each transform as a link of the chain, in order
        let chain = self
            .links
            .iter()
            .map(|(pending, transform)| {
                let (from, to) = layouts(transform, direction);
                ChainLink {
                    revision: pending.edge.revision(),
                    step: pending.step.id.to_owned(),
                    from,
                    to,
                }
            })
            .collect();
        // the whole pass runs after a reverse when any of its steps was reversed since its `Done`
        let after_reverse = self.links.iter().any(|(pending, _)| pending.after_reverse);
        // list the steps the pass runs, each naming the pass
        let steps = self
            .links
            .iter()
            .map(|(pending, _)| PlannedStep {
                pass: Some(name.clone()),
                ..pending.planned()
            })
            .collect();
        PlannedPass {
            family,
            chain,
            after_reverse,
            steps,
        }
    }
}

/// Whether a later transform must run after an earlier one of another family
///
/// It must when it reads the family the earlier one writes, when the earlier one reads the
/// family it writes (so the earlier one reads it before it changes), or when its check reads the
/// earlier step's output.
///
/// # Arguments
///
/// * `later` - The later step and its transform
/// * `earlier` - The earlier step and its transform
fn depends(later: (&Pending, &TransformDef), earlier: (&Pending, &TransformDef)) -> bool {
    // compare what each reads with what the other writes, and the later step's check inputs
    later.1.reads.contains(&earlier.1.family)
        || earlier.1.reads.contains(&later.1.family)
        || later.0.step.reads_after.contains(&earlier.0.step.id)
}

/// Fuse a group of transforms with no barrier between them into `Passes` items
///
/// Transforms on one family whose layouts chain fuse into one pass, unless a pass placed after
/// that one depends on it or the transform must wait for a pass that isn't done before it.
/// Passes that depend on no pass of the same item run concurrently in it; a pass that depends on
/// another runs in a later item.
///
/// # Arguments
///
/// * `group` - The transforms, in path order
/// * `direction` - Which way the transforms convert records
fn fuse(group: &[(Pending, &'static TransformDef)], direction: PlanDirection) -> Vec<PlanItem> {
    // place each transform in turn, remembering the pass each went into
    let mut passes: Vec<Building> = Vec::new();
    let mut placed: Vec<usize> = Vec::with_capacity(group.len());
    for (index, (pending, transform)) in group.iter().enumerate() {
        // find the passes this transform must come after
        let after: BTreeSet<usize> = group[..index]
            .iter()
            .zip(&placed)
            .filter(|((earlier, earlier_transform), _)| {
                earlier_transform.family != transform.family
                    && depends((pending, transform), (earlier, earlier_transform))
            })
            .map(|(_, pass)| *pass)
            .collect();
        // it can run no earlier than the item after the latest of them
        let lowest = after
            .iter()
            .map(|pass| passes[*pass].level + 1)
            .max()
            .unwrap_or(0);
        // join the newest pass on the same family when its chain ends where this one starts, it
        // runs late enough, and nothing placed after it depends on it
        let newest = passes
            .iter()
            .rposition(|pass| pass.family == transform.family);
        let join = newest.filter(|candidate| {
            let pass = &passes[*candidate];
            let ends = pass
                .links
                .last()
                .map(|(_, last)| layouts(last, direction).1);
            ends == Some(layouts(transform, direction).0)
                && pass.level >= lowest
                && !passes
                    .iter()
                    .any(|other| other.level > pass.level && other.after.contains(candidate))
        });
        match join {
            // extend the chain of the pass it joins
            Some(pass) => {
                passes[pass].links.push((*pending, transform));
                passes[pass].after.extend(after);
                placed.push(pass);
            }
            // start a new pass after every pass it depends on and the newest on its family
            None => {
                let level = newest.map_or(lowest, |pass| lowest.max(passes[pass].level + 1));
                let mut after = after;
                after.extend(newest);
                passes.push(Building {
                    family: transform.family,
                    links: vec![(*pending, transform)],
                    level,
                    after,
                });
                placed.push(passes.len() - 1);
            }
        }
    }
    // gather the passes of each level into one item, levels in order
    let levels = passes.iter().map(|pass| pass.level + 1).max().unwrap_or(0);
    let mut items: Vec<Vec<PlannedPass>> = (0..levels).map(|_| Vec::new()).collect();
    for pass in passes {
        let level = pass.level;
        items[level].push(pass.finish(direction));
    }
    items.into_iter().map(PlanItem::Passes).collect()
}

/// Split the steps of a run of edges into those to plan and those a later step supersedes
///
/// Each dropped step comes with the id of a kept step: when the step superseding it is itself
/// superseded, the step at the end of that chain.
///
/// # Arguments
///
/// * `pending` - The steps still to run, in path order
fn split_superseded(pending: &[Pending]) -> (Vec<Pending>, Vec<(Pending, &'static str)>) {
    // a step is dropped when a later step in the plan supersedes it, and it is superseded by the
    // kept step at the end of that chain of supersession; walking back from the last step
    // resolves each later step before any step it supersedes
    let mut by: Vec<Option<usize>> = vec![None; pending.len()];
    for (index, current) in pending.iter().enumerate().rev() {
        by[index] = pending[index + 1..]
            .iter()
            .rposition(|later| later.supersedes(current))
            .map(|offset| index + 1 + offset)
            .map(|later| by[later].unwrap_or(later));
    }
    // keep each step nothing supersedes, and name the kept step superseding each other one
    let mut kept = Vec::with_capacity(pending.len());
    let mut dropped = Vec::new();
    for (current, by) in pending.iter().zip(by) {
        match by {
            Some(later) => dropped.push((*current, pending[later].step.id)),
            None => kept.push(*current),
        }
    }
    (kept, dropped)
}

/// Compose a group of contract steps, in path order, into plan items: contract transforms fuse
/// into passes between the contract steps that run as is
///
/// # Arguments
///
/// * `contract` - The contract steps and their transforms, in path order
/// * `direction` - Which way the run moves
fn compose_contract(
    contract: Vec<(Pending, Option<&'static TransformDef>)>,
    direction: PlanDirection,
) -> Vec<PlanItem> {
    // fuse the transforms between contract steps that have none
    let mut items = Vec::new();
    let mut group: Vec<(Pending, &'static TransformDef)> = Vec::new();
    for (current, transform) in contract {
        match transform {
            Some(transform) => group.push((current, transform)),
            None => {
                items.extend(fuse(&group, direction));
                group.clear();
                items.push(PlanItem::Step(current.planned()));
            }
        }
    }
    items.extend(fuse(&group, direction));
    items
}

/// Compose the steps of a run of edges leading the same way into plan items
///
/// # Arguments
///
/// * `target` - The revision the run ends at, whose snapshot the converge step applies
/// * `pending` - The steps still to run, in path order, without the superseded ones
/// * `direction` - Which way the run moves
fn compose_run(
    target: RevisionId,
    pending: Vec<Pending>,
    direction: PlanDirection,
) -> Vec<PlanItem> {
    // sort each step by its role, fusing the transforms between barriers
    let mut converge = Vec::new();
    let mut middle = Vec::new();
    let mut contract = Vec::new();
    let mut group: Vec<(Pending, &'static TransformDef)> = Vec::new();
    for current in pending {
        match Role::of(&current) {
            Role::Additive => converge.push(current.planned()),
            Role::Transform(transform) => group.push((current, transform)),
            Role::Contract(transform) => {
                // a contract step still ends the fusion group it sits in
                middle.extend(fuse(&group, direction));
                group.clear();
                contract.push((current, transform));
            }
            Role::Barrier => {
                middle.extend(fuse(&group, direction));
                group.clear();
                middle.push(PlanItem::Step(current.planned()));
            }
        }
    }
    middle.extend(fuse(&group, direction));
    // the converge runs first, then everything in path order, then the contract steps
    let mut items = Vec::with_capacity(middle.len() + contract.len() + 1);
    if !converge.is_empty() {
        items.push(PlanItem::Converge {
            target,
            steps: converge,
        });
    }
    items.extend(middle);
    items.extend(compose_contract(contract, direction));
    items
}

/// Gather the loss of every lossy step a plan runs, in the order the steps run
///
/// # Arguments
///
/// * `items` - The plan's items
fn losses(items: &[PlanItem]) -> Option<LossReport> {
    // take the loss of each step of each item in turn
    let steps = items
        .iter()
        .flat_map(|item| match item {
            PlanItem::Converge { steps, .. } => steps.iter().collect::<Vec<_>>(),
            PlanItem::Passes(passes) => passes.iter().flat_map(|pass| &pass.steps).collect(),
            PlanItem::Step(step) => vec![step],
        })
        .filter_map(|step| step.loss.clone())
        .collect::<Vec<_>>();
    // a plan with no lossy step loses nothing
    (!steps.is_empty()).then_some(LossReport { steps })
}

impl Catalog {
    /// Compose the plan that brings a deployment from its revision to a target revision
    ///
    /// The steps the history counts as finished are left out, as in [`Catalog::plan`], and a path
    /// down across a finished irreversible step is refused before anything is planned. On each
    /// run of edges leading the same way a step a later step supersedes is dropped, every
    /// additive step becomes one converge to the run's last revision placed first, transforms
    /// between barriers fuse into passes, other steps run where they sit, and contract steps run
    /// last. On a run leading down each reverse step runs the `down` of the forward step it
    /// reverses, with the enum mappings and fills of a lossy reverse, and the reverse of a
    /// `DualWrite` step runs no pass. A path with any edge leading down is a downgrade, and the
    /// plan's loss gathers what every lossy reverse on it loses.
    ///
    /// # Arguments
    ///
    /// * `state` - The deployment's recorded upgrade state
    /// * `target` - The revision to reach
    pub fn compose(
        &self,
        state: &UpgradeState,
        target: &RevisionId,
    ) -> Result<UpgradePlan, UpgradeError> {
        // check the state and both revisions, and find the edges between them
        let edges = self.checked_path(state, target)?;
        // the path starts at the deployment's revision and visits each edge's revision
        let path = std::iter::once(state.revision.clone())
            .chain(edges.iter().map(|edge| edge.revision()))
            .collect();
        // a path with an edge leading down moves the deployment back
        let direction = if edges.iter().any(|edge| edge.is_downward()) {
            PlanDirection::Downgrade
        } else {
            PlanDirection::Upgrade
        };
        // compose each run of edges leading the same way on its own
        let mut items = Vec::new();
        for run in edges.chunk_by(|first, second| first.is_downward() == second.is_downward()) {
            let Some(last) = run.last() else {
                continue;
            };
            // the run moves the way its edges lead
            let way = if last.is_downward() {
                PlanDirection::Downgrade
            } else {
                PlanDirection::Upgrade
            };
            // drop the superseded steps and compose the rest
            let (kept, _) = split_superseded(&self.pending(state, run));
            items.extend(compose_run(last.revision(), kept, way));
        }
        // gather what the plan's lossy reverses lose
        let loss = losses(&items);
        Ok(UpgradePlan {
            from: state.revision.clone(),
            to: target.clone(),
            direction,
            path,
            items,
            loss,
        })
    }

    /// Get the steps [`Catalog::compose`] drops because a later step on the path supersedes
    /// them, each with the id of the step that supersedes it
    ///
    /// The step named is always one the plan keeps: when the step listing a dropped step is
    /// itself superseded, the dropped step names the kept step at the end of that chain.
    ///
    /// The deployment records each as skipped when the step superseding it finishes, so the
    /// revision can move past its edge.
    ///
    /// # Arguments
    ///
    /// * `state` - The deployment's recorded upgrade state
    /// * `target` - The revision to reach
    pub fn superseded(
        &self,
        state: &UpgradeState,
        target: &RevisionId,
    ) -> Result<Vec<(PlannedStep, &'static str)>, UpgradeError> {
        // check the state and both revisions, and find the edges between them
        let edges = self.checked_path(state, target)?;
        // collect the dropped steps of every run of edges leading the same way
        let mut dropped = Vec::new();
        for run in edges.chunk_by(|first, second| first.is_downward() == second.is_downward()) {
            let (_, superseded) = split_superseded(&self.pending(state, run));
            dropped.extend(
                superseded
                    .into_iter()
                    .map(|(step, by)| (step.planned(), by)),
            );
        }
        Ok(dropped)
    }

    /// Resolve the transforms of a planned pass from this catalog
    ///
    /// A link naming a forward step runs that step's transform `up`; a link naming a step of an
    /// edge leading down runs the `down` of the forward step it reverses, with that step's
    /// reversibility. Returns `None` when a link names a step this catalog doesn't have, a step
    /// without a transform to run, or when the links don't all lead the same way.
    ///
    /// # Arguments
    ///
    /// * `pass` - The pass to resolve
    #[must_use]
    pub fn chain(&self, pass: &PlannedPass) -> Option<FusedChain> {
        // look each link's step up and take the transform it runs
        let resolved = pass
            .chain
            .iter()
            .map(|link| self.chain_link(&link.step))
            .collect::<Option<Vec<_>>>()?;
        // every link of one pass leads the same way
        let downward = resolved
            .first()
            .is_some_and(|(_, _, _, downward)| *downward);
        if resolved.iter().any(|(_, _, _, link)| *link != downward) {
            return None;
        }
        // the links' edges decide which way the chain converts
        let direction = if downward {
            PlanDirection::Downgrade
        } else {
            PlanDirection::Upgrade
        };
        // a downgrade chain keeps each link's reversibility for its enum mappings and fills
        let reversibility = if downward {
            resolved.iter().map(|(_, _, rev, _)| *rev).collect()
        } else {
            Vec::new()
        };
        // keep each link's step id and transform
        let links = resolved
            .into_iter()
            .map(|(step, transform, _, _)| (step, transform))
            .collect();
        Some(FusedChain {
            links,
            after_reverse: pass.after_reverse,
            direction,
            reversibility,
        })
    }

    /// Resolve one link of a pass: its step id, the transform it runs, the reversibility of a
    /// reversed forward step, and whether the link's edge leads down
    ///
    /// # Arguments
    ///
    /// * `id` - The step id the link names
    fn chain_link(
        &self,
        id: &str,
    ) -> Option<(
        &'static str,
        &'static TransformDef,
        Option<&'static Reversibility>,
        bool,
    )> {
        // find the step and the edge holding it
        let (edge, step) = self.step(id)?;
        // a forward step runs its own transform
        if !edge.is_downward() {
            return step
                .transform
                .map(|transform| (step.id, transform, None, false));
        }
        // a reverse step runs the transform of the forward step it reverses
        let (_, forward) = self.step(step.reverses?)?;
        forward
            .transform
            .map(|transform| (step.id, transform, forward.reversibility.as_ref(), true))
    }

    /// Check a deployment's state and both revisions, and find the edges between them
    ///
    /// # Arguments
    ///
    /// * `state` - The deployment's recorded upgrade state
    /// * `target` - The revision to reach
    fn checked_path(
        &self,
        state: &UpgradeState,
        target: &RevisionId,
    ) -> Result<Vec<&'static CatalogEntry>, UpgradeError> {
        // a state in a newer layout may record things this build would misread or drop
        state.check_schema()?;
        // both revisions must be ones this catalog plans between
        self.check_current(&state.revision)?;
        self.check_target(target)?;
        let path = self.path(&state.revision, target)?;
        // a path down may not cross a finished irreversible step
        self.check_reversible(state, &path)?;
        Ok(path)
    }

    /// Get the steps of a run of edges a deployment still has to run, in path order
    ///
    /// # Arguments
    ///
    /// * `state` - The deployment's recorded upgrade state
    /// * `run` - The edges, in path order
    fn pending(&self, state: &UpgradeState, run: &[&'static CatalogEntry]) -> Vec<Pending> {
        // keep each edge's unfinished steps, noting the forward ones reversed since their `Done`
        run.iter()
            .flat_map(|&edge| {
                self.edge_steps(state, edge)
                    .into_iter()
                    .map(move |step| Pending {
                        edge,
                        step,
                        after_reverse: !edge.is_downward() && self.reversed(state, edge, step),
                        forward: self.forward_of(edge, step),
                    })
            })
            .collect()
    }

    /// Get the forward step a step of an edge leading down reverses, if it reverses one
    ///
    /// # Arguments
    ///
    /// * `edge` - The edge holding the step
    /// * `step` - The step
    fn forward_of(&self, edge: &CatalogEntry, step: &StepDef) -> Option<&'static StepDef> {
        // only a step of an edge leading down reverses a forward step
        if !edge.is_downward() {
            return None;
        }
        step.reverses
            .and_then(|forward| self.step(forward))
            .map(|(_, forward)| forward)
    }

    /// Whether the history holds a finished reverse of a forward step newer than its last
    /// finished entry
    ///
    /// # Arguments
    ///
    /// * `state` - The deployment's recorded upgrade state
    /// * `edge` - The forward edge holding the step
    /// * `step` - The forward step
    fn reversed(&self, state: &UpgradeState, edge: &CatalogEntry, step: &StepDef) -> bool {
        // only entries after the step last finished can have reversed it since
        let finished = |outcome: StepState| outcome.finishes();
        let since = state
            .applied
            .iter()
            .rposition(|applied| {
                applied.revision == edge.id && applied.step == step.id && finished(applied.outcome)
            })
            .map_or(0, |index| index + 1);
        // look for a finished step of a downward edge that reverses this one
        state.applied[since..].iter().any(|applied| {
            finished(applied.outcome)
                && self
                    .step(&applied.step)
                    .is_some_and(|(reverse_edge, reverse)| {
                        reverse_edge.is_downward() && reverse.reverses == Some(step.id)
                    })
        })
    }
}

impl fmt::Display for Family {
    /// Write this family as the status names a pass: `scylla:<table>`, `redis:<family>`, or
    /// `s3:<bucket>/<prefix>`
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // prefix the family's name with its backend
        match self {
            Family::ScyllaTable(table) => write!(f, "scylla:{table}"),
            Family::Redis(family) => write!(f, "redis:{family}"),
            Family::S3Prefix { bucket, prefix } => write!(f, "s3:{bucket}/{prefix}"),
        }
    }
}

impl PlannedPass {
    /// Get the step ids of this pass's chain, in order: what a checkpoint of the pass records
    #[must_use]
    pub fn chain_ids(&self) -> Vec<String> {
        // take each link's step id
        self.chain.iter().map(|link| link.step.clone()).collect()
    }

    /// Get the checkpoint this pass resumes from: the recorded one when it was recorded for this
    /// chain, and otherwise none, so the pass starts from the beginning
    ///
    /// # Arguments
    ///
    /// * `checkpoint` - The checkpoint recorded for the pass, if any
    #[must_use]
    pub fn resume_from<'a>(
        &self,
        checkpoint: Option<&'a PassCheckpoint>,
    ) -> Option<&'a PassCheckpoint> {
        // a checkpoint of a different chain came from a different plan
        checkpoint.filter(|checkpoint| {
            checkpoint
                .chain
                .iter()
                .map(String::as_str)
                .eq(self.chain.iter().map(|link| link.step.as_str()))
        })
    }
}

/// What a deployment does next with its plan, decided right before the work runs
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NextWork {
    /// Run this item
    Run(PlanItem),
    /// Don't run this step: its check found nothing to do, so it is recorded as skipped
    Skip {
        /// The step to skip
        step: PlannedStep,
        /// Why there is nothing to do, from the step's check
        reason: String,
    },
    /// Nothing is left to run
    Finished,
}

impl UpgradePlan {
    /// Decide what to do with this plan's next item, checking its work right before it runs
    ///
    /// A step run as is gets its executor's check first, so a step another actor made satisfied
    /// after planning is skipped. A converge step and a pass check their own work as they run:
    /// the applier creates only what is missing, and a pass classifies every object, writing
    /// none that are current.
    ///
    /// # Arguments
    ///
    /// * `check` - The check of a step's work, the executor registered for its id
    pub fn next_work<F: FnMut(&PlannedStep) -> Work>(&self, mut check: F) -> NextWork {
        // look only at the first item, since later items wait for it
        match self.items.first() {
            None => NextWork::Finished,
            // a single step runs only when its check still finds work
            Some(PlanItem::Step(step)) => match check(step) {
                Work::Nothing(reason) => NextWork::Skip {
                    step: step.clone(),
                    reason,
                },
                Work::Needed(_) => NextWork::Run(PlanItem::Step(step.clone())),
            },
            Some(item) => NextWork::Run(item.clone()),
        }
    }
}

/// The transforms of a fused pass, resolved from the catalog, run over one record at a time in
/// memory
#[derive(Debug, Clone)]
pub struct FusedChain {
    /// Each link's step id and transform, in chain order
    pub links: Vec<(&'static str, &'static TransformDef)>,
    /// Whether the chain runs in its after-a-reverse mode: the first link's old form is
    /// authoritative, so every record that link reads is recomputed from it, even one that
    /// already holds the new form
    ///
    /// The mode only starts the chain at its first link; the recompute is the links' own `up`.
    /// It holds only because every transform's `up` derives the new form from the old form
    /// whenever the old form is present (see [`TransformDef::up`]); an `up` that returns a record
    /// already holding the new form unchanged leaves a stale new form in place.
    pub after_reverse: bool,
    /// Which way the chain converts records: an upgrade chain runs each link's `up`, and a
    /// downgrade chain each link's `down` with the enum mappings and fills of its lossy reverse
    pub direction: PlanDirection,
    /// For a downgrade chain, how the forward step of each link is undone, in link order; empty
    /// for an upgrade chain
    pub reversibility: Vec<Option<&'static Reversibility>>,
}

/// What running a fused chain over one record gave
#[derive(Debug, Clone, PartialEq)]
pub enum ChainOutcome {
    /// The record is already in the chain's last layout, so nothing is written
    Current,
    /// The record converted to the chain's last layout: the pass's one write of it
    Converted(Record),
    /// The record couldn't be converted, so it stays as it was and nothing is written
    Issue {
        /// The step id of the link that failed, which names the record's issue group
        step: String,
        /// Why the link failed
        issue: TransformIssue,
    },
}

impl FusedChain {
    /// Run this chain over one record in memory
    ///
    /// The record's layout is found from the chain's last link back to its first, the links from
    /// that layout on are applied in turn, and the result is written once by the caller. A link
    /// that can't classify or convert the record stops the chain, and the record is left as it
    /// was, with the issue naming that link's step.
    ///
    /// The newest link that recognizes the record's layout decides where the chain starts, so
    /// this equals running each link as its own pass with a write between them only when the
    /// links' classifiers agree: a record an earlier link calls unrecognized but a later link
    /// calls current or migratable is skipped or converted here, where the step-by-step run
    /// would stop with an issue at the earlier link.
    ///
    /// # Arguments
    ///
    /// * `record` - The record as stored
    ///
    /// A downgrade chain runs the other way: see [`FusedChain::apply_down`].
    #[must_use]
    pub fn apply(&self, record: &Record) -> ChainOutcome {
        // a downgrade chain runs each link's reverse instead
        if self.direction == PlanDirection::Downgrade {
            return self.apply_down(record);
        }
        // find the link to start at, if the record needs converting at all
        let start = match self.start(record) {
            Ok(Some(start)) => start,
            Ok(None) => return ChainOutcome::Current,
            Err(outcome) => return outcome,
        };
        // apply each link from there in memory, stopping at the first that fails
        let mut current = record.clone();
        for (step, transform) in &self.links[start..] {
            match (transform.up)(current) {
                Ok(next) => current = next,
                Err(issue) => {
                    return ChainOutcome::Issue {
                        step: (*step).to_owned(),
                        issue,
                    };
                }
            }
        }
        ChainOutcome::Converted(current)
    }

    /// Run this downgrade chain over one record in memory
    ///
    /// The links run from the newest layout down. The record starts at the first link whose
    /// forward transform classifies it as current, so a record never brought to a link's new
    /// layout skips that link; each link from there reverts it (its `down`, then the enum mappings
    /// and fills of a lossy reverse), and the result is written once by the caller. A record no
    /// link classifies as current is already in the chain's last layout. A record a link doesn't
    /// recognize, or can't revert, stops the chain and is left as it was, with the issue naming
    /// that link's step, so this equals running each link as its own pass with a write between
    /// them for every record that converts.
    ///
    /// # Arguments
    ///
    /// * `record` - The record as stored
    #[must_use]
    pub fn apply_down(&self, record: &Record) -> ChainOutcome {
        // find the first link whose new layout the record is in
        let mut tried = Vec::with_capacity(self.links.len());
        let mut start = None;
        for (index, (step, transform)) in self.links.iter().enumerate() {
            tried.push(transform.to);
            match (transform.classify)(record) {
                RecordClass::Current => {
                    start = Some(index);
                    break;
                }
                RecordClass::Migratable { .. } => (),
                RecordClass::Unrecognized { reason } => {
                    return ChainOutcome::Issue {
                        step: (*step).to_owned(),
                        issue: TransformIssue {
                            reason,
                            layouts_tried: tried,
                        },
                    };
                }
            }
        }
        // a record in no link's new layout is already where the chain ends
        let Some(start) = start else {
            return ChainOutcome::Current;
        };
        // revert it through each link from there in memory, stopping at the first that fails
        let mut current = record.clone();
        for (index, (step, transform)) in self.links.iter().enumerate().skip(start) {
            // a link reverts with the reversibility of the forward step it undoes
            let reversibility = self.reversibility.get(index).copied().flatten();
            match transform.revert(current, reversibility) {
                Ok(previous) => current = previous,
                Err(issue) => {
                    return ChainOutcome::Issue {
                        step: (*step).to_owned(),
                        issue,
                    };
                }
            }
        }
        ChainOutcome::Converted(current)
    }

    /// Find the link a record's conversion starts at, `None` when the record is current, or the
    /// issue when no link recognizes it
    ///
    /// # Arguments
    ///
    /// * `record` - The record as stored
    fn start(&self, record: &Record) -> Result<Option<usize>, ChainOutcome> {
        // an empty chain has nothing to convert
        let Some(last) = self.links.len().checked_sub(1) else {
            return Ok(None);
        };
        // after a reverse, every record the first link reads is recomputed from its old form
        if self.after_reverse
            && !matches!(
                (self.links[0].1.classify)(record),
                RecordClass::Unrecognized { .. }
            )
        {
            return Ok(Some(0));
        }
        // look for the record's layout from the newest link back to the second
        let mut tried = vec![self.links[last].1.to];
        for (index, (_, transform)) in self.links.iter().enumerate().skip(1).rev() {
            tried.push(transform.from);
            match (transform.classify)(record) {
                // a record past this link starts at the next one, or is current after the last
                RecordClass::Current => return Ok((index < last).then_some(index + 1)),
                // a record in this link's layout starts here
                RecordClass::Migratable { layout } if layout >= transform.from => {
                    return Ok(Some(index));
                }
                // a record in an older layout, or one this link doesn't recognize, is for an
                // earlier link
                RecordClass::Migratable { .. } | RecordClass::Unrecognized { .. } => (),
            }
        }
        // the first link starts any record it reads, and one it doesn't recognize is its issue
        let (step, first) = self.links[0];
        tried.push(first.from);
        match (first.classify)(record) {
            RecordClass::Current => Ok((last > 0).then_some(1)),
            RecordClass::Migratable { .. } => Ok(Some(0)),
            RecordClass::Unrecognized { reason } => Err(ChainOutcome::Issue {
                step: step.to_owned(),
                issue: TransformIssue {
                    reason,
                    layouts_tried: tried,
                },
            }),
        }
    }
}
