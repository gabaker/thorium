//! The consistency checks every catalog must pass, run by the catalog tests over the production
//! catalog and over every test fixture catalog

#[cfg(feature = "scylla-utils")]
use std::collections::BTreeMap;
use std::collections::BTreeSet;

#[cfg(feature = "scylla-utils")]
use super::Family;
use super::{
    Catalog, CatalogEntry, MigrationPhase, Reversibility, RevisionId, SchemaTarget, StepDef,
    StepKind, SupportFloor,
};

/// The largest issue group limit a step may set
pub const MAX_GROUP_MAX: u64 = 20;

/// The fewest revisions before the release that raised a floor that must stay at or above it
pub const MIN_SUPPORTED_REVISIONS: usize = 4;

/// The fewest months between a floor's minimum and the release that raised it, so every revision
/// of the twelve months before that release stays supported
pub const MIN_SUPPORTED_MONTHS: u32 = 12;

impl Catalog {
    /// Check that this catalog is consistent, returning every problem found
    ///
    /// Every id parses, edges are unique by their two ends, step ids are unique, steps that need
    /// an approval (manual steps among them) have a manual procedure that isn't blank, issue
    /// group limits are at most [`MAX_GROUP_MAX`], every pre-release edge (stacked ones included,
    /// and one leading from a pre-release to a later release) has a revert edge, every downward
    /// edge undoes a forward edge and reverses at least one of its steps when it has any (which
    /// also catches a stacked pre-release that sorts before the one it stacks on, since the order
    /// of the ids decides which edge of a pair goes forward), a revert step names a step of the
    /// edge it undoes, and every transform on a pre-release edge can be undone (or its step is
    /// declared `Irreversible` with a justification). The floors are ordered oldest first, the
    /// last one's minimum is a baseline no edge starts below, and each floor keeps the support
    /// window. No family's transforms read a family whose transforms read it back. An
    /// `Irreversible` step gives a justification; on an edge with an edge back, every transform
    /// that isn't irreversible has a step reversing it there (and on a release edge declares how
    /// it is undone), and no step of an edge leading down has a transform of its own. No contract
    /// step drops an object or field an additive step of a later edge adds back, which the
    /// converge placed first would add before the drop.
    pub fn check(&self) -> Result<(), Vec<String>> {
        // gather the problems of each kind of check
        let mut problems = Vec::new();
        self.check_ids(&mut problems);
        self.check_steps(&mut problems);
        self.check_prereleases(&mut problems);
        self.check_reverse_edges(&mut problems);
        self.check_contracts(&mut problems);
        self.check_floors(&mut problems);
        #[cfg(feature = "scylla-utils")]
        self.check_reads(&mut problems);
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems)
        }
    }

    /// Check that every id parses, that edges are unique, and that there is a newest revision
    ///
    /// # Arguments
    ///
    /// * `problems` - The problems found so far
    fn check_ids(&self, problems: &mut Vec<String>) {
        // every revision an edge names must be a valid id
        for edge in self.edges {
            for id in std::iter::once(edge.id).chain(edge.from) {
                if id.parse::<RevisionId>().is_err() {
                    problems.push(format!(
                        "edge {} names the malformed revision {id:?}",
                        label(edge)
                    ));
                }
            }
        }
        // no two edges may join the same two revisions
        let mut ends = BTreeSet::new();
        for edge in self.edges {
            if !ends.insert((edge.from, edge.id)) {
                problems.push(format!("edge {} is declared twice", label(edge)));
            }
        }
        // a catalog needs a baseline and a forward edge to plan anything
        if !self.edges.iter().any(|edge| edge.from.is_none()) {
            problems.push("the catalog has no baseline (an edge with no `from`)".to_owned());
        }
    }

    /// Check every step: unique ids, manual procedures that aren't blank, group limits, and revert
    /// targets
    ///
    /// # Arguments
    ///
    /// * `problems` - The problems found so far
    fn check_steps(&self, problems: &mut Vec<String>) {
        // every step id is unique across the catalog
        let mut ids = BTreeSet::new();
        for (edge, step) in self.steps() {
            if !ids.insert(step.id) {
                problems.push(format!(
                    "step {} of edge {} is declared twice",
                    step.id,
                    label(edge)
                ));
            }
            // steps that need approval say how to do them by hand, and the operator shows a
            // manual step's procedure to the admin while it waits for their work
            let has_procedure = step
                .manual_procedure
                .is_some_and(|procedure| !procedure.trim().is_empty());
            if step.kind == StepKind::Manual && !has_procedure {
                problems.push(format!(
                    "manual step {} has no manual procedure telling the admin what to do by hand",
                    step.id
                ));
            } else if step.kind.needs_approval() && !has_procedure {
                problems.push(format!(
                    "step {} needs an approval but has no manual procedure",
                    step.id
                ));
            }
            // an issue group limit above the cap would hide too many bad objects
            if step.group_max > MAX_GROUP_MAX {
                problems.push(format!(
                    "step {} allows issue groups of {} objects, more than {MAX_GROUP_MAX}",
                    step.id, step.group_max
                ));
            }
            // a step that reverses another must sit on a downward edge and name a step of the
            // edge it undoes
            if let Some(forward) = step.reverses {
                let undone = self.revert_edge(edge).filter(|_| edge.is_downward());
                if !undone.is_some_and(|undone| undone.steps.iter().any(|s| s.id == forward)) {
                    problems.push(format!(
                        "step {} reverses {forward}, which isn't a step of the edge {} undoes",
                        step.id,
                        label(edge)
                    ));
                }
            }
        }
    }

    /// Check the pre-release edges: their ids, their revert edges, and their transforms
    ///
    /// # Arguments
    ///
    /// * `problems` - The problems found so far
    fn check_prereleases(&self, problems: &mut Vec<String>) {
        // judge each edge's pre-release marking, revert edge, and transforms on its own
        for edge in self.edges {
            // only edges with a valid start and end can be judged
            let (Some(from), Ok(to)) = (edge.from, edge.id.parse::<RevisionId>()) else {
                continue;
            };
            let Ok(from) = from.parse::<RevisionId>() else {
                continue;
            };
            // a pre-release id may only appear on an edge marked as a pre-release
            let touches_prerelease = from.is_prerelease() || to.is_prerelease();
            if touches_prerelease != edge.prerelease {
                problems.push(format!(
                    "edge {} must be marked prerelease exactly when it joins a pre-release id",
                    label(edge)
                ));
                continue;
            }
            // a downward edge must undo a forward edge, reversing its steps
            if edge.is_downward() {
                match self.revert_edge(edge) {
                    None => problems.push(format!(
                        "downward edge {} undoes no forward edge",
                        label(edge)
                    )),
                    Some(forward)
                        if !forward.steps.is_empty()
                            && !edge.steps.iter().any(|step| step.reverses.is_some()) =>
                    {
                        problems.push(format!(
                            "downward edge {} reverses none of the steps of {} (a stacked \
                             pre-release must sort after the one it stacks on)",
                            label(edge),
                            label(forward)
                        ));
                    }
                    Some(_) => (),
                }
                continue;
            }
            // the rest of the checks are for pre-release forward edges
            if !edge.prerelease {
                continue;
            }
            // a pre-release builds on its own base, from the base itself or a pre-release of it,
            // except that a branch's converting edge may lead from one of its pre-releases to a
            // release after that base
            let converts = from.is_prerelease() && !to.is_prerelease() && to > from;
            if to.base() != from.base() && !converts {
                problems.push(format!(
                    "pre-release edge {} must start from {} or one of its pre-releases, or lead \
                     from a pre-release to a later release",
                    label(edge),
                    to.base()
                ));
            }
            // every pre-release forward edge needs an edge back, marked as a pre-release too
            if !self.revert_edge(edge).is_some_and(|back| back.prerelease) {
                problems.push(format!(
                    "pre-release edge {} has no revert edge",
                    label(edge)
                ));
            }
            // every transform on it can be undone, or its step says why it can't be
            for step in edge.steps {
                if !undoable(step) {
                    problems.push(format!(
                        "step {} of pre-release edge {} has a transform with no `down` and isn't \
                         declared Irreversible with a justification",
                        step.id,
                        label(edge)
                    ));
                }
            }
        }
    }
}

impl Catalog {
    /// Check how steps are undone: justifications of irreversible steps and, with the transforms,
    /// the reverse steps of every edge with an edge back
    ///
    /// # Arguments
    ///
    /// * `problems` - The problems found so far
    fn check_reverse_edges(&self, problems: &mut Vec<String>) {
        // an irreversible step says why no fill can be valid
        for (_, step) in self.steps() {
            if matches!(step.reversibility, Some(Reversibility::Irreversible { justification }) if justification.trim().is_empty())
            {
                problems.push(format!(
                    "step {} is declared Irreversible without a justification",
                    step.id
                ));
            }
        }
        // the transforms decide which steps need a reverse
        #[cfg(feature = "scylla-utils")]
        self.check_reverse_transforms(problems);
    }

    /// Check that no contract step drops an object or field that an additive step of a later edge
    /// adds back: the plan converges first and contracts last, so it would add the object before
    /// dropping it
    ///
    /// # Arguments
    ///
    /// * `problems` - The problems found so far
    fn check_contracts(&self, problems: &mut Vec<String>) {
        // pair each forward contract step with each additive step of an edge after its own
        let forward = || self.steps().filter(|(edge, _)| !edge.is_downward());
        for (edge, contract) in
            forward().filter(|(_, step)| step.phase == Some(MigrationPhase::Contract))
        {
            for (later, additive) in forward().filter(|(_, step)| additive(step)) {
                // only an edge starting at or after the contract's revision comes after it
                let after = later
                    .from
                    .and_then(|from| from.parse::<RevisionId>().ok())
                    .zip(edge.id.parse::<RevisionId>().ok())
                    .is_some_and(|(from, dropped_at)| self.at_or_below(&dropped_at, &from));
                if after && overlaps(contract.targets, additive.targets) {
                    problems.push(format!(
                        "contract step {} drops what additive step {} of the later edge {} adds \
                         back, which a plan across both would add before dropping",
                        contract.id,
                        additive.id,
                        label(later)
                    ));
                }
            }
        }
    }
}

#[cfg(feature = "scylla-utils")]
impl Catalog {
    /// Check the transforms of every edge with an edge back: each that isn't irreversible has a
    /// step reversing it on the edge back, each on a release edge declares how it is undone, and
    /// no step of an edge leading down carries a transform of its own
    ///
    /// # Arguments
    ///
    /// * `problems` - The problems found so far
    fn check_reverse_transforms(&self, problems: &mut Vec<String>) {
        for edge in self.edges {
            // a step leading down runs the `down` of the forward step it reverses
            if edge.is_downward() {
                for step in edge.steps.iter().filter(|step| step.transform.is_some()) {
                    problems.push(format!(
                        "step {} of the downward edge {} has a transform of its own; a step \
                         leading down runs the `down` of the forward step it reverses",
                        step.id,
                        label(edge)
                    ));
                }
                continue;
            }
            // only forward edges with an edge back have transforms to undo
            let Some(back) = self.revert_edge(edge) else {
                continue;
            };
            for step in edge.steps.iter().filter(|step| step.transform.is_some()) {
                // a release edge that ships a reverse edge says how each transform is undone
                if !edge.prerelease && step.reversibility.is_none() {
                    problems.push(format!(
                        "step {} of the release edge {} has a reverse edge but declares no \
                         reversibility",
                        step.id,
                        label(edge)
                    ));
                }
                // every transform that isn't refused on the way down has a step reversing it
                let irreversible =
                    matches!(step.reversibility, Some(Reversibility::Irreversible { .. }));
                if !irreversible && !back.steps.iter().any(|back| back.reverses == Some(step.id)) {
                    problems.push(format!(
                        "step {} of edge {} has a transform that no step of {} reverses",
                        step.id,
                        label(edge),
                        label(back)
                    ));
                }
            }
        }
    }
}

impl Catalog {
    /// Check the support floors: their order, the last one's baseline, and the support window
    ///
    /// # Arguments
    ///
    /// * `problems` - The problems found so far
    fn check_floors(&self, problems: &mut Vec<String>) {
        // every floor names a valid release id and an intermediate release
        let mut minimums = Vec::new();
        for floor in self.floors {
            match floor.minimum.parse::<RevisionId>() {
                Ok(minimum) if !minimum.is_prerelease() => minimums.push(minimum),
                _ => problems.push(format!(
                    "floor {:?} doesn't name a release revision",
                    floor.minimum
                )),
            }
            if floor.intermediate.trim().is_empty() {
                problems.push(format!(
                    "floor {} names no intermediate release",
                    floor.minimum
                ));
            }
        }
        // floors are listed oldest first, each raising the minimum
        if minimums.windows(2).any(|pair| pair[0] >= pair[1]) {
            problems.push(
                "the floors must be listed oldest first, each raising the minimum".to_owned(),
            );
        }
        // the last floor's minimum is the baseline the squashed revisions became
        let Some((last, minimum)) = self.floors.last().zip(minimums.last()) else {
            return;
        };
        if !self
            .edges
            .iter()
            .any(|edge| edge.from.is_none() && edge.id == last.minimum)
        {
            problems.push(format!(
                "the last floor's minimum {minimum} isn't a baseline"
            ));
        }
        // no edge may start or end below the floor, since those revisions were squashed
        for edge in self.edges {
            let below = std::iter::once(edge.id)
                .chain(edge.from)
                .filter_map(|id| id.parse::<RevisionId>().ok())
                .any(|id| id.base() < *minimum);
            if below {
                problems.push(format!("edge {} is below the floor {minimum}", label(edge)));
            }
        }
        // each floor keeps the support window
        for (index, floor) in self.floors.iter().enumerate() {
            self.check_window(floor, index + 1 == self.floors.len(), problems);
        }
    }

    /// Check that a floor leaves the previous four revisions and the twelve months before the
    /// release that raised it supported
    ///
    /// The release that raised a floor is the newest release revision released with the floor's
    /// `since_version`. When no revision was released with it, the last floor is measured from
    /// the newest release revision and an older floor is skipped, since its release was squashed
    /// by a later floor.
    ///
    /// # Arguments
    ///
    /// * `floor` - The floor to check
    /// * `last` - Whether this is the catalog's last floor
    /// * `problems` - The problems found so far
    fn check_window(&self, floor: &SupportFloor, last: bool, problems: &mut Vec<String>) {
        // only a floor with a valid minimum can be measured
        let Ok(minimum) = floor.minimum.parse::<RevisionId>() else {
            return;
        };
        // every release revision the catalog still holds, oldest first
        let releases = self
            .edges
            .iter()
            .filter(|edge| !edge.prerelease && !edge.is_downward())
            .filter_map(|edge| edge.id.parse::<RevisionId>().ok())
            .collect::<BTreeSet<_>>();
        // find the release that raised the floor
        let raised_by = self
            .edges
            .iter()
            .filter(|edge| {
                !edge.prerelease && !edge.is_downward() && edge.released_with == floor.since_version
            })
            .filter_map(|edge| edge.id.parse::<RevisionId>().ok())
            .max();
        let Some(raised_by) =
            raised_by.or_else(|| last.then(|| releases.last().cloned()).flatten())
        else {
            return;
        };
        // the previous four revisions must still be at or above the floor
        let supported = releases
            .iter()
            .filter(|release| **release >= minimum && **release < raised_by)
            .count();
        if supported < MIN_SUPPORTED_REVISIONS {
            problems.push(format!(
                "floor {minimum} leaves {supported} of the previous four revisions before \
                 {raised_by} supported"
            ));
        }
        // so must every revision of the twelve months before the release that raised it
        if raised_by.months().saturating_sub(minimum.months()) < MIN_SUPPORTED_MONTHS {
            problems.push(format!(
                "floor {minimum} drops revisions from the twelve months before {raised_by}"
            ));
        }
    }
}

#[cfg(feature = "scylla-utils")]
impl Catalog {
    /// Check that no family's transforms read, directly or through other families, a family
    /// that reads it back: a pass reading a family runs after the passes writing it, so such a
    /// cycle has no order
    ///
    /// # Arguments
    ///
    /// * `problems` - The problems found so far
    fn check_reads(&self, problems: &mut Vec<String>) {
        // map each family a transform writes to the other families its transforms read
        let mut reads: BTreeMap<Family, BTreeSet<Family>> = BTreeMap::new();
        for (_, step) in self.steps() {
            if let Some(transform) = step.transform {
                let read = transform
                    .reads
                    .iter()
                    .copied()
                    .filter(|family| *family != transform.family);
                reads.entry(transform.family).or_default().extend(read);
            }
        }
        // a family that reaches itself through what the families read is in a cycle
        for family in reads.keys() {
            let mut seen = BTreeSet::new();
            let mut stack: Vec<Family> = reads[family].iter().copied().collect();
            while let Some(next) = stack.pop() {
                if next == *family {
                    problems.push(format!(
                        "transforms of {family:?} read families that read it back, a reads cycle \
                         no pass order satisfies"
                    ));
                    break;
                }
                if seen.insert(next) {
                    stack.extend(reads.get(&next).into_iter().flatten().copied());
                }
            }
        }
    }
}

/// Whether a pre-release step's transform can be undone, or the step is declared irreversible
/// with a justification
///
/// # Arguments
///
/// * `step` - The step to check
fn undoable(step: &StepDef) -> bool {
    // a step declared irreversible with a reason is allowed, since its revert edge refuses it
    if matches!(step.reversibility, Some(Reversibility::Irreversible { justification }) if !justification.trim().is_empty())
    {
        return true;
    }
    // otherwise a transform must provide its `down`
    #[cfg(feature = "scylla-utils")]
    if step
        .transform
        .is_some_and(|transform| transform.down.is_none())
    {
        return false;
    }
    true
}

/// Whether a step is additive schema the planner folds into the converge step: an infra step
/// tagged `Expand` with no migration
///
/// # Arguments
///
/// * `step` - The step to check
fn additive(step: &StepDef) -> bool {
    // the planner converges exactly these steps
    step.kind == StepKind::Infra
        && step.phase == Some(MigrationPhase::Expand)
        && step.thoradm_migration.is_none()
}

/// Whether two lists of schema targets name a common object or field: the same object of the same
/// backend, with the same field or either one naming the whole object
///
/// # Arguments
///
/// * `first` - The first targets
/// * `second` - The second targets
fn overlaps(first: &[SchemaTarget], second: &[SchemaTarget]) -> bool {
    // compare every pair of targets
    first.iter().any(|one| {
        second.iter().any(|other| {
            one.backend == other.backend
                && one.object == other.object
                && (one.field.is_none() || other.field.is_none() || one.field == other.field)
        })
    })
}

/// Name an edge by its two ends for a problem report
///
/// # Arguments
///
/// * `edge` - The edge to name
fn label(edge: &CatalogEntry) -> String {
    // show the start (or that it is a baseline) and the end
    format!("{} -> {}", edge.from.unwrap_or("(baseline)"), edge.id)
}
