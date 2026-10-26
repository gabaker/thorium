//! The catalog of revisions: its edges, the steps on each edge, and the support floors
//!
//! Each edge lives in its own file under `catalog/`, and [`CATALOG`] lists them oldest first; a
//! new edge is a new file appended to that list, so edges never share a step list.

use std::collections::{BTreeSet, VecDeque};

use super::{RevisionId, StepDef, UpgradeError};

mod v01;
mod v02;
mod v03;
mod v03_reverse;
mod v04;
mod v04_reverse;

pub use v02::REINDEX_PROCEDURE;
pub use v03::{SCHEMA_APPLY_STEP, USERS_SETTINGS_THEME_REVERSE_STEP, USERS_SETTINGS_THEME_STEP};

#[cfg(test)]
pub(super) use v02::HELM_STEPS;

/// An edge of the revision graph: the steps that move a deployment from one revision to another
#[derive(Debug, Clone, Copy)]
pub struct CatalogEntry {
    /// The revision this edge starts from (`None` for a baseline every install starts at)
    pub from: Option<&'static str>,
    /// The revision this edge reaches
    pub id: &'static str,
    /// The Thorium version this revision was released with (advisory)
    pub released_with: &'static str,
    /// What this revision changes
    pub description: &'static str,
    /// The deployment fingerprint of `id`'s committed schema snapshot, once one is committed
    pub schema: Option<&'static str>,
    /// Whether this edge belongs to a feature branch: a pre-release forward edge (from the base or
    /// stacked on another pre-release) or its revert edge, taken only on an `auto_target_dev`
    /// cluster or for an explicit target
    pub prerelease: bool,
    /// Whether this edge is a sibling path to a depth the primary chain already reaches;
    /// `auto_target_dev` never targets a sibling's revision
    pub sibling: bool,
    /// The steps that bring a cluster from the previous revision to this one, in order
    pub steps: &'static [StepDef],
}

impl CatalogEntry {
    /// Get this entry's revision id
    ///
    /// # Panics
    ///
    /// Panics if the catalog holds a malformed id, which the catalog tests rule out.
    #[must_use]
    pub fn revision(&self) -> RevisionId {
        // parse the static id, which the catalog tests check is well formed
        self.id.parse().expect("catalog revision ids are valid")
    }

    /// Whether this edge moves a deployment down: a revert edge of a pre-release or a release's
    /// reverse edge, whose `id` sorts before its `from`
    #[must_use]
    pub fn is_downward(&self) -> bool {
        // a baseline starts nowhere, so only an edge with a start can lead down
        self.from.is_some_and(|from| self.id < from)
    }
}

/// A revision graph the planner works on: the production catalog, or a test-only fixture catalog
#[derive(Debug, Clone, Copy)]
pub struct Catalog {
    /// Every edge, release edges oldest first
    pub edges: &'static [CatalogEntry],
    /// Every time the minimum supported revision was raised, oldest first; the last one is this
    /// build's floor
    pub floors: &'static [SupportFloor],
}

impl Catalog {
    /// The catalog this build deploys: every edge in [`CATALOG`] and every floor in [`FLOORS`]
    pub const PRODUCTION: Catalog = Catalog {
        edges: CATALOG,
        floors: FLOORS,
    };

    /// Check that a revision isn't below this catalog's minimum supported revision
    ///
    /// A revision below the last floor's minimum is refused, naming that minimum and the
    /// intermediate release of the oldest floor the revision is below: the one release whose
    /// catalog still plans from it, which the admin installs first.
    ///
    /// # Arguments
    ///
    /// * `current` - The revision a deployment is at
    pub fn check_floor(&self, current: &RevisionId) -> Result<(), UpgradeError> {
        // without a floor every revision is supported
        let Some(last) = self.floors.last() else {
            return Ok(());
        };
        // a revision at or above the floor is supported
        let Ok(minimum) = last.minimum.parse::<RevisionId>() else {
            return Ok(());
        };
        if *current >= minimum {
            return Ok(());
        }
        // name the intermediate of the oldest floor the revision is below
        let intermediate = self
            .floors
            .iter()
            .find(|floor| current.as_str() < floor.minimum)
            .unwrap_or(last)
            .intermediate
            .to_owned();
        Err(UpgradeError::BelowMinimum {
            current: current.clone(),
            minimum,
            intermediate,
        })
    }

    /// Whether a revision is one this catalog's edges start from or reach
    ///
    /// # Arguments
    ///
    /// * `id` - The revision to look for
    #[must_use]
    pub fn knows(&self, id: &RevisionId) -> bool {
        // a revision is known when any edge starts from it or reaches it
        self.edges
            .iter()
            .any(|edge| edge.id == id.as_str() || edge.from == Some(id.as_str()))
    }

    /// Get the newest revision this catalog's primary edges reach: the newest release, or on a
    /// feature branch the tip of its pre-release stack (never a sibling's revision)
    ///
    /// # Panics
    ///
    /// Panics if the catalog has no forward edge, which the catalog tests rule out.
    #[must_use]
    pub fn latest(&self) -> RevisionId {
        // take the newest revision any forward edge that isn't a sibling reaches
        self.edges
            .iter()
            .filter(|edge| !edge.is_downward() && !edge.sibling)
            .map(CatalogEntry::revision)
            .max()
            .expect("the catalog has at least one revision")
    }

    /// Get the edge that brings a deployment from one revision to another, if the catalog has one
    ///
    /// # Arguments
    ///
    /// * `from` - The revision the edge starts from
    /// * `to` - The revision the edge reaches
    #[must_use]
    pub fn edge(&self, from: &str, to: &str) -> Option<&'static CatalogEntry> {
        // edges are unique by their two ends, which the catalog tests check
        self.edges
            .iter()
            .find(|edge| edge.from == Some(from) && edge.id == to)
    }

    /// Get the revert edge that undoes a pre-release forward edge (or the reverse edge of a
    /// release edge), if the catalog has one
    ///
    /// For a downward edge this is the forward edge it undoes.
    ///
    /// # Arguments
    ///
    /// * `forward` - The edge to undo
    #[must_use]
    pub fn revert_edge(&self, forward: &CatalogEntry) -> Option<&'static CatalogEntry> {
        // the edge back runs between the same two revisions the other way round
        forward.from.and_then(|from| self.edge(forward.id, from))
    }

    /// Get the step that undoes a forward step, with the edge holding it
    ///
    /// # Arguments
    ///
    /// * `forward` - The id of the forward step to undo
    #[must_use]
    pub fn reverting_step(
        &self,
        forward: &str,
    ) -> Option<(&'static CatalogEntry, &'static StepDef)> {
        // look for a step on a downward edge that names the forward step as the one it reverses
        self.steps()
            .find(|(edge, step)| edge.is_downward() && step.reverses == Some(forward))
    }

    /// Find a step anywhere in this catalog by its id, with the edge holding it
    ///
    /// # Arguments
    ///
    /// * `id` - The id of the step to find
    #[must_use]
    pub fn step(&self, id: &str) -> Option<(&'static CatalogEntry, &'static StepDef)> {
        // search every edge's steps
        self.steps().find(|(_, step)| step.id == id)
    }

    /// Iterate over every step of every edge, with the edge holding it
    pub fn steps(&self) -> impl Iterator<Item = (&'static CatalogEntry, &'static StepDef)> {
        // pair each step with its edge, edges in catalog order
        self.edges
            .iter()
            .flat_map(|edge| edge.steps.iter().map(move |step| (edge, step)))
    }

    /// Find the shortest path of edges from one revision to another
    ///
    /// A path with fewer edges wins, so a skip edge is taken over the chain of releases it skips;
    /// between paths of the same length the one using earlier catalog edges wins. A path to the
    /// starting revision itself is empty.
    ///
    /// # Arguments
    ///
    /// * `from` - The revision the deployment is at
    /// * `to` - The revision to reach
    pub fn path(
        &self,
        from: &RevisionId,
        to: &RevisionId,
    ) -> Result<Vec<&'static CatalogEntry>, UpgradeError> {
        // search breadth first from the starting revision, remembering for each revision reached
        // the edge that reached it and the revision that edge started from
        let mut reached: Vec<(&str, Option<(&'static CatalogEntry, usize)>)> =
            vec![(from.as_str(), None)];
        let mut queue = VecDeque::from([0]);
        while let Some(index) = queue.pop_front() {
            // stop once the target is reached
            let revision = reached[index].0;
            if revision == to.as_str() {
                // walk the remembered edges back to the start
                let mut path = Vec::new();
                let mut cursor = index;
                while let Some((edge, parent)) = reached[cursor].1 {
                    path.push(edge);
                    cursor = parent;
                }
                path.reverse();
                return Ok(path);
            }
            // follow every edge out of this revision to a revision not yet reached
            for edge in self.edges {
                if edge.from == Some(revision) && reached.iter().all(|(seen, _)| *seen != edge.id) {
                    reached.push((edge.id, Some((edge, index))));
                    queue.push_back(reached.len() - 1);
                }
            }
        }
        // the target can't be reached from the starting revision
        Err(UpgradeError::NoPath {
            from: from.clone(),
            to: to.clone(),
        })
    }

    /// Get every revision a deployment can reach from a revision through forward edges that
    /// aren't siblings: the primary chain of releases and stacked pre-releases, the start included
    ///
    /// # Arguments
    ///
    /// * `from` - The revision to start from
    #[must_use]
    pub fn primary_chain(&self, from: &RevisionId) -> BTreeSet<RevisionId> {
        // walk forward edges out of every revision reached so far
        let mut reached = BTreeSet::from([from.clone()]);
        let mut queue = VecDeque::from([from.clone()]);
        while let Some(revision) = queue.pop_front() {
            // follow each forward edge out of this revision that isn't a sibling
            for edge in self.edges.iter().filter(|edge| {
                edge.from == Some(revision.as_str()) && !edge.is_downward() && !edge.sibling
            }) {
                // queue each revision the first time it is reached
                if reached.insert(edge.revision()) {
                    queue.push_back(edge.revision());
                }
            }
        }
        reached
    }

    /// Whether a revision is at or below another on some forward path: the same revision, or one
    /// from which a deployment can reach the other through forward edges, siblings included
    ///
    /// Revisions this catalog doesn't know are compared by their ids.
    ///
    /// # Arguments
    ///
    /// * `below` - The revision that may come first
    /// * `above` - The revision that may come after it
    #[must_use]
    pub fn at_or_below(&self, below: &RevisionId, above: &RevisionId) -> bool {
        // without both revisions in the graph only their ids can order them
        if below == above || !self.knows(below) || !self.knows(above) {
            return below <= above;
        }
        // walk forward edges from the lower revision looking for the higher one
        let mut reached = BTreeSet::from([below.as_str()]);
        let mut queue = VecDeque::from([below.as_str()]);
        while let Some(revision) = queue.pop_front() {
            for edge in self
                .edges
                .iter()
                .filter(|edge| edge.from == Some(revision) && !edge.is_downward())
            {
                // the higher revision is reachable, so it comes after the lower one
                if edge.id == above.as_str() {
                    return true;
                }
                // keep walking from revisions not yet seen
                if reached.insert(edge.id) {
                    queue.push_back(edge.id);
                }
            }
        }
        false
    }
}

/// A minimum supported revision and the release that bridges deployments below it
///
/// When old revisions are squashed into a baseline, their edges, executors, and migrations are
/// deleted; a deployment below `minimum` installs `intermediate` first. The admin docs' table of
/// valid upgrade paths is rendered from these entries
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupportFloor {
    /// The oldest revision builds from `since_version` on plan from (the baseline's id)
    pub minimum: &'static str,
    /// The last release whose catalog still plans from below `minimum`
    pub intermediate: &'static str,
    /// The Thorium version that raised the floor to `minimum`
    pub since_version: &'static str,
}

/// Every edge this build knows about, oldest first; a new edge is appended in its own file
pub const CATALOG: &[CatalogEntry] = &[
    v01::ENTRY,
    v02::ENTRY,
    v03::ENTRY,
    v03_reverse::ENTRY,
    v04::ENTRY,
    v04_reverse::ENTRY,
];

/// The artifacts this build's reversible steps register: round-trip and new-layout fixtures,
/// compatibility tests, the changes of lossy steps, and the security-sensitive fields; a step
/// that ships a reverse appends its own
#[cfg(feature = "scylla-utils")]
pub const ARTIFACTS: super::ReverseArtifacts = super::ReverseArtifacts {
    round_trips: &[super::StepFixtures {
        step: USERS_SETTINGS_THEME_STEP,
        fixtures: super::users_theme_round_trip_fixtures,
    }],
    native: &[super::StepFixtures {
        step: USERS_SETTINGS_THEME_STEP,
        fixtures: super::users_theme_native_fixtures,
    }],
    ..super::ReverseArtifacts::EMPTY
};

/// Every time this build's minimum supported revision was raised, oldest first; empty while no
/// revision has been squashed into a baseline
pub const FLOORS: &[SupportFloor] = &[];

/// Get the newest revision this build knows about
///
/// # Panics
///
/// Panics if the catalog is empty, which the catalog tests rule out.
#[must_use]
pub fn latest() -> RevisionId {
    // the production catalog's primary chain ends at its newest release
    Catalog::PRODUCTION.latest()
}

/// Get the forward catalog edge that reaches a revision
///
/// # Arguments
///
/// * `id` - The revision to find
#[must_use]
pub fn entry(id: &RevisionId) -> Option<&'static CatalogEntry> {
    // look the revision up by its id, ignoring edges that lead down to it
    CATALOG
        .iter()
        .find(|entry| entry.id == id.as_str() && !entry.is_downward())
}

/// Find a step anywhere in the catalog by its id
///
/// # Arguments
///
/// * `id` - The id of the step to find
#[must_use]
pub fn step(id: &str) -> Option<(&'static CatalogEntry, &'static StepDef)> {
    // search every edge's steps, keeping the edge each belongs to
    Catalog::PRODUCTION.step(id)
}

#[cfg(test)]
mod tests {
    use super::{CATALOG, Catalog, CatalogEntry, RevisionId, SCHEMA_APPLY_STEP, v02, v03};
    #[cfg(feature = "scylla-utils")]
    use crate::models::schema::RevisionSnapshot;
    #[cfg(feature = "scylla-utils")]
    use crate::models::upgrades::LAYOUTS;
    #[cfg(feature = "scylla-utils")]
    use crate::models::upgrades::{ARTIFACTS, Reversibility};
    use crate::models::upgrades::{
        AppliedStep, StepKind, StepState, UpgradeComponent, UpgradeState,
    };

    /// The revision that holds the steps converting images, pipelines, and provenance to tools
    const V04: &str = "2026-10-v04";

    /// The steps of revision `2026-10-v04`, in the order they run
    const V04_STEPS: [&str; 5] = [
        "drain-reactions",
        "images-to-tools",
        "pipelines-to-tool-stages",
        "provenance-to-legacy-tools",
        "finalize-tool-only",
    ];

    /// The steps of revision `2026-10-v03`, in the order they run
    const V03_STEPS: [&str; 2] = [SCHEMA_APPLY_STEP, "users-settings-theme"];

    /// Get the release edge to the revision that holds the tool conversions
    fn v04() -> &'static CatalogEntry {
        // the edge starts at the revision that applies the schema first
        Catalog::PRODUCTION
            .edge(v03::ENTRY.id, V04)
            .expect("the edge to 2026-10-v04")
    }

    /// Plan the steps from one revision to another over a state that reached the first
    ///
    /// # Arguments
    ///
    /// * `from` - The revision the deployment reached
    /// * `to` - The revision to plan to
    fn planned(from: &str, to: &str) -> Vec<String> {
        // plan over the production catalog, naming each step
        let catalog = Catalog::PRODUCTION;
        let state = catalog.reached(&from.parse().expect("revision"), "0.0.0");
        catalog
            .plan(&state, &to.parse().expect("revision"))
            .expect("plan")
            .into_iter()
            .map(|step| step.step)
            .collect()
    }

    /// Record a revision's steps as done in a state
    ///
    /// # Arguments
    ///
    /// * `state` - The state to record into
    /// * `revision` - The revision the steps belong to
    /// * `steps` - The ids of the steps
    fn finish(state: &mut UpgradeState, revision: &str, steps: &[&str]) {
        // record each step as done
        for step in steps {
            state.record(AppliedStep {
                revision: revision.to_owned(),
                step: (*step).to_owned(),
                outcome: StepState::Done,
                ..AppliedStep::default()
            });
        }
    }

    /// The revision after the first Helm revision is released: its first step applies the schema,
    /// it holds only the schema and users steps, and a deployment at the first Helm revision plans
    /// exactly those steps
    #[test]
    fn v03_is_released_with_the_schema_apply_step_first() {
        // the id parses as a revision newer than the edge it starts from
        let id = v03::ENTRY.id.parse::<RevisionId>().expect("valid revision");
        assert!(id > v02::ENTRY.revision());
        assert_eq!(v03::ENTRY.from, Some(v02::ENTRY.id));
        // the edge is in the catalog the planner and the operator use
        assert!(CATALOG.iter().any(|entry| entry.id == v03::ENTRY.id));
        // its first step applies the schema, and it needs no approval
        let first = v03::ENTRY.steps.first().expect("a step");
        assert_eq!(first.id, SCHEMA_APPLY_STEP);
        assert_eq!(first.kind, StepKind::Infra);
        assert!(first.thoradm_migration.is_none());
        // the revision holds no step that converts tools, so an image that only reached it
        // still has every conversion to run
        let ids = v03::ENTRY
            .steps
            .iter()
            .map(|step| step.id)
            .collect::<Vec<_>>();
        assert_eq!(ids, V03_STEPS);
        assert!(
            v03::ENTRY
                .steps
                .iter()
                .all(|step| step.needs_running.is_empty())
        );
        // the catalog's own checks pass
        assert_eq!(Catalog::PRODUCTION.check(), Ok(()));
        // a deployment at the first Helm revision plans exactly its steps
        assert_eq!(planned(v02::ENTRY.id, v03::ENTRY.id), V03_STEPS);
    }

    /// The revision after it is the newest, holds only the conversion steps in order, starts from
    /// the revision that applies the schema first, and is documented as released with this version
    #[test]
    fn v04_is_the_newest_revision_with_the_conversion_steps() {
        // the edge starts at the schema revision, reaches a newer id, and is the newest
        let edge = v04();
        let id = edge.id.parse::<RevisionId>().expect("valid revision");
        assert!(id > v03::ENTRY.revision());
        assert_eq!(edge.from, Some(v03::ENTRY.id));
        assert!(!edge.prerelease && !edge.sibling && !edge.is_downward());
        assert_eq!(Catalog::PRODUCTION.latest(), id);
        assert_eq!(edge.released_with, v03::ENTRY.released_with);
        // its steps are the drain, the three conversions, and the record, in order
        let ids = edge.steps.iter().map(|step| step.id).collect::<Vec<_>>();
        assert_eq!(ids, V04_STEPS);
        // the catalog's checks pass
        assert_eq!(Catalog::PRODUCTION.check(), Ok(()));
    }

    /// The drain waits first and pauses nothing, every step that pauses comes after it, and every
    /// data step after it reads after it, so no check precedes the wait
    #[test]
    fn v04_drains_before_any_pause_and_its_data_steps_read_after_the_drain() {
        let steps = v04().steps;
        // the drain leads the edge, needs the API running, and pauses nothing
        let drain = steps.first().expect("a step");
        assert_eq!(drain.id, "drain-reactions");
        assert_eq!(drain.pause, []);
        assert!(drain.needs_running.contains(&UpgradeComponent::Api));
        // the steps that pause are the three conversions, all after the drain
        let pausing = steps
            .iter()
            .filter(|step| !step.pause.is_empty())
            .map(|step| step.id)
            .collect::<Vec<_>>();
        assert_eq!(
            pausing,
            [
                "images-to-tools",
                "pipelines-to-tool-stages",
                "provenance-to-legacy-tools"
            ]
        );
        // each data step after the drain names it as a step its check reads after
        for step in steps[1..].iter().filter(|step| step.kind == StepKind::Data) {
            assert!(
                step.reads_after.contains(&"drain-reactions"),
                "{} doesn't read after the drain",
                step.id
            );
        }
        // the record step is last and pauses nothing
        let last = steps.last().expect("a step");
        assert_eq!(
            (last.id, last.kind),
            ("finalize-tool-only", StepKind::Config)
        );
        assert_eq!(last.pause, []);
    }

    /// The three conversion steps can't be reverted, so the edge back refuses them naming the step,
    /// while the drain and the record are not marked
    #[test]
    fn v04s_conversions_are_irreversible() {
        // each conversion declares why a revert is refused
        for step in v04().steps {
            let irreversible = matches!(
                step.reversibility,
                Some(super::super::Reversibility::Irreversible { .. })
            );
            let converts = matches!(
                step.id,
                "images-to-tools" | "pipelines-to-tool-stages" | "provenance-to-legacy-tools"
            );
            assert_eq!(irreversible, converts, "{}", step.id);
        }
    }

    /// The released revisions' committed snapshots have the fingerprints their entries record
    #[test]
    #[cfg(feature = "scylla-utils")]
    fn the_revisions_record_their_snapshots_fingerprints() {
        // the committed snapshot's deployment fingerprint is the one in each entry
        for entry in [&v03::ENTRY, v04()] {
            let snapshot = RevisionSnapshot::committed(entry.id)
                .expect("a committed snapshot")
                .expect("a valid snapshot");
            assert_eq!(
                entry.schema,
                Some(snapshot.deployment.as_str()),
                "write the snapshot of {} with the schema_report example",
                entry.id
            );
        }
        // the two revisions declare different schemas: pipelines are stored in a second layout
        // from the second one on
        assert_ne!(v03::ENTRY.schema, v04().schema);
    }

    /// The users example's step is a `Data` step on the release edge to the revision that applies
    /// the schema, naming its thoradm migration, pausing the API, and holding any issue group; its
    /// reverse is a step of the reverse edge, and neither edge names a pre-release id
    #[test]
    #[cfg(feature = "scylla-utils")]
    fn the_users_example_step_and_its_reverse_are_declared() {
        let catalog = Catalog::PRODUCTION;
        // the step sits on the release edge from the first Helm revision
        let (edge, step) = catalog.step("users-settings-theme").expect("the step");
        assert_eq!((edge.from, edge.id), (Some(v02::ENTRY.id), v03::ENTRY.id));
        assert!(!edge.prerelease && !edge.is_downward());
        // it migrates data with the API paused and no tolerance for unreadable users
        assert_eq!(step.kind, StepKind::Data);
        assert_eq!(step.thoradm_migration, Some("users-settings-theme"));
        assert_eq!(step.pause, &[UpgradeComponent::Api]);
        assert_eq!(step.group_max, 0);
        assert!(
            step.transform
                .is_some_and(|transform| transform.down.is_some())
        );
        assert!(matches!(step.reversibility, Some(Reversibility::Backfill)));
        // the schema is applied before the users are converted, and nothing else is on the edge
        let ids = edge.steps.iter().map(|step| step.id).collect::<Vec<_>>();
        assert_eq!(ids, V03_STEPS);
        // the reverse edge holds the step that runs the transform's `down`
        let (back, reverse) = catalog
            .reverting_step("users-settings-theme")
            .expect("the reverse step");
        assert_eq!((back.from, back.id), (Some(v03::ENTRY.id), v02::ENTRY.id));
        assert!(!back.prerelease && back.is_downward());
        assert_eq!(reverse.id, "users-settings-theme-reverse");
        assert_eq!(
            reverse.thoradm_migration,
            Some("users-settings-theme-reverse")
        );
        assert_eq!(reverse.pause, &[UpgradeComponent::Api]);
        assert!(reverse.transform.is_none());
        // neither release edge holds a pre-release id
        for id in [edge.from, Some(edge.id), back.from, Some(back.id)]
            .into_iter()
            .flatten()
        {
            let revision = id.parse::<RevisionId>().expect("valid revision");
            assert!(!revision.is_prerelease(), "{id}");
        }
        // the catalog's checks and the reversible steps' artifacts pass
        assert_eq!(catalog.check(), Ok(()));
        assert_eq!(catalog.check_artifacts(&ARTIFACTS), Ok(()));
    }

    /// No pre-release revision is in the production catalog: pre-releases belong to feature
    /// branches and the fixture catalog, so a released build never plans through one
    #[test]
    fn the_production_catalog_holds_no_pre_release() {
        // no edge starts from or reaches a pre-release id
        for edge in CATALOG {
            assert!(!edge.prerelease, "{} is a pre-release edge", edge.id);
            for id in edge.from.into_iter().chain([edge.id]) {
                assert!(!id.contains("-pre."), "{id} is a pre-release id");
            }
        }
    }

    /// A deployment at the first Helm revision plans the steps of each release after it: only the
    /// schema and users steps for the first, then the conversions on top of them for the second
    #[test]
    fn the_first_helm_revision_plans_the_steps_of_the_releases_after_it() {
        // to the revision that applies the schema first: only its own steps
        assert_eq!(planned(v02::ENTRY.id, v03::ENTRY.id), V03_STEPS);
        // from there to the newest: only the conversions, the drain first
        assert_eq!(planned(v03::ENTRY.id, V04), V04_STEPS);
        // straight to the newest: the drain moves ahead of the users step that pauses the API, so
        // it waits with Thorium running, and the schema is still applied first
        assert_eq!(
            planned(v02::ENTRY.id, V04),
            [
                "schema-apply",
                "drain-reactions",
                "users-settings-theme",
                "images-to-tools",
                "pipelines-to-tool-stages",
                "provenance-to-legacy-tools",
                "finalize-tool-only",
            ]
        );
        // the same steps in two stops, the first stop's steps recorded between them
        let mut state = Catalog::PRODUCTION.reached(&v02::ENTRY.revision(), "0.0.0");
        finish(&mut state, v03::ENTRY.id, &V03_STEPS);
        state.revision = v03::ENTRY.revision();
        let next = Catalog::PRODUCTION
            .plan(&state, &V04.parse().expect("revision"))
            .expect("plan")
            .into_iter()
            .map(|step| step.step)
            .collect::<Vec<_>>();
        assert_eq!(next, V04_STEPS);
    }

    /// This build and every release snapshot keep users layout 2, and pipelines are stored in
    /// layout 2 only from the revision that converts them
    #[test]
    #[cfg(feature = "scylla-utils")]
    fn the_releases_keep_users_layout_2() {
        // the registry declares the layout this build writes
        let users = LAYOUTS
            .iter()
            .find(|layout| layout.family == "users")
            .expect("users layout");
        assert_eq!((users.version, users.reads_from), (2, 2));
        // each snapshot holds users layout 2 and the pipelines layout its release reads
        for (revision, pipelines) in [(v02::ENTRY.id, 1), (v03::ENTRY.id, 1), (V04, 2)] {
            let snapshot = RevisionSnapshot::committed(revision)
                .expect("committed snapshot")
                .expect("valid snapshot");
            for (name, version) in [("users", 2), ("pipelines", pipelines)] {
                let object = snapshot
                    .objects
                    .iter()
                    .find(|object| object.name == name && object.canonical.get(name).is_some())
                    .expect("the object");
                assert_eq!(object.canonical[name], version, "{revision} {name}");
            }
        }
    }

    /// A deployment at the revision that applies the schema first plans the users reverse step for
    /// the first Helm revision, since no conversion step belongs to it, while one that ran a
    /// conversion step of the newest revision can't go back to either
    #[test]
    fn a_cluster_at_v03_plans_the_users_reverse() {
        // a deployment that finished the steps of the revision
        let released = Catalog::PRODUCTION.reached(&v03::ENTRY.revision(), "0.0.0");
        // the only step planned for the first Helm revision is the users reverse
        let planned = Catalog::PRODUCTION
            .plan(&released, &v02::ENTRY.revision())
            .expect("plan")
            .into_iter()
            .map(|step| step.step)
            .collect::<Vec<_>>();
        assert_eq!(planned, ["users-settings-theme-reverse"], "{planned:?}");
    }

    /// A deployment at the newest revision refuses to go back, naming the first conversion step, once
    /// that step finished or was skipped; one that only drained goes back with nothing to undo
    #[test]
    fn a_cluster_at_v04_is_refused_naming_the_first_conversion() {
        let catalog = Catalog::PRODUCTION;
        let v04_revision = V04.parse::<RevisionId>().expect("revision");
        // a deployment that finished every step is refused on the way to either older revision
        let released = catalog.reached(&v04_revision, "0.0.0");
        for target in [v03::ENTRY.revision(), v02::ENTRY.revision()] {
            let error = catalog.plan(&released, &target).expect_err("refused");
            assert!(error.to_string().contains("images-to-tools"), "{error}");
        }
        // a step that finished as skipped counts as finished, so the refusal stands
        let mut skipped = catalog.reached(&v03::ENTRY.revision(), "0.0.0");
        for step in ["drain-reactions", "images-to-tools"] {
            skipped.record(AppliedStep {
                revision: V04.to_owned(),
                step: step.to_owned(),
                outcome: StepState::Skipped,
                ..AppliedStep::default()
            });
        }
        skipped.revision = v04_revision.clone();
        let error = catalog
            .plan(&skipped, &v03::ENTRY.revision())
            .expect_err("refused");
        assert!(error.to_string().contains("images-to-tools"), "{error}");
        // a deployment that only drained has nothing to undo going back to v03
        let mut drained = catalog.reached(&v03::ENTRY.revision(), "0.0.0");
        finish(&mut drained, V04, &["drain-reactions"]);
        drained.revision = v04_revision;
        let back = catalog
            .plan(&drained, &v03::ENTRY.revision())
            .expect("plan");
        assert!(back.is_empty(), "{back:?}");
        // and going back to the first Helm revision from there plans just the users reverse
        let down = catalog
            .plan(&drained, &v02::ENTRY.revision())
            .expect("plan")
            .into_iter()
            .map(|step| step.step)
            .collect::<Vec<_>>();
        assert_eq!(down, ["users-settings-theme-reverse"], "{down:?}");
    }

    /// The reverse edge of the newest revision starts there, reaches the revision that applies the
    /// schema, and holds only the step that undoes the record: the conversions are irreversible
    #[test]
    fn v04_has_a_reverse_edge_that_undoes_only_the_record() {
        // the edge back exists and leads down
        let forward = v04();
        let back = Catalog::PRODUCTION
            .revert_edge(forward)
            .expect("the edge back");
        assert_eq!((back.from, back.id), (Some(V04), v03::ENTRY.id));
        assert!(back.is_downward() && !back.prerelease);
        // its one step reverses the record step, and none reverses a conversion
        let steps = back
            .steps
            .iter()
            .map(|step| (step.id, step.reverses))
            .collect::<Vec<_>>();
        assert_eq!(
            steps,
            [("finalize-tool-only-reverse", Some("finalize-tool-only"))]
        );
        // it records the schema fingerprint of the revision it reaches
        assert_eq!(back.schema, v03::ENTRY.schema);
    }
}
