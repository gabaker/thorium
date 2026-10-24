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

pub use v02::{FIRST_HELM_REVISION, REINDEX_PROCEDURE};
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
pub const CATALOG: &[CatalogEntry] = &[v01::ENTRY, v02::ENTRY, v03::ENTRY, v03_reverse::ENTRY];

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
    use super::{CATALOG, Catalog, RevisionId, SCHEMA_APPLY_STEP, v02, v03};
    #[cfg(feature = "scylla-utils")]
    use crate::models::schema::RevisionSnapshot;
    #[cfg(feature = "scylla-utils")]
    use crate::models::upgrades::LAYOUTS;
    #[cfg(feature = "scylla-utils")]
    use crate::models::upgrades::{ARTIFACTS, Reversibility};
    use crate::models::upgrades::{StepKind, UpgradeComponent, UpgradeState};

    /// The revision after the first Helm revision is released: its first step applies the schema,
    /// it is the newest revision, and a deployment at the first Helm revision plans just that step
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
        // it is the newest revision, and the catalog's own checks pass
        let catalog = Catalog::PRODUCTION;
        assert_eq!(catalog.latest(), id);
        assert_eq!(catalog.check(), Ok(()));
        // a deployment at the first Helm revision plans the schema-apply step first
        let state = UpgradeState::new(v02::ENTRY.revision(), "0.0.0");
        let planned = catalog.plan(&state, &id).expect("plan");
        let steps = planned
            .iter()
            .map(|step| step.step.as_str())
            .collect::<Vec<_>>();
        assert_eq!(steps.first().copied(), Some(SCHEMA_APPLY_STEP), "{steps:?}");
    }

    /// The released revision's committed snapshot has the fingerprint its entry records
    #[test]
    #[cfg(feature = "scylla-utils")]
    fn v03_records_its_snapshots_fingerprint() {
        // the committed snapshot's deployment fingerprint is the one in the entry
        let snapshot = RevisionSnapshot::committed(v03::ENTRY.id)
            .expect("a committed snapshot")
            .expect("a valid snapshot");
        assert_eq!(
            v03::ENTRY.schema,
            Some(snapshot.deployment.as_str()),
            "write the snapshot with the schema_report example"
        );
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
        // the schema is applied before the users are converted
        let ids = edge.steps.iter().map(|step| step.id).collect::<Vec<_>>();
        assert_eq!(ids, [SCHEMA_APPLY_STEP, "users-settings-theme"]);
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

    /// A deployment at the first Helm revision plans exactly the steps of the next release
    #[test]
    fn the_first_helm_revision_plans_exactly_the_next_releases_steps() {
        // plan from the first Helm revision to the newest one
        let state = UpgradeState::new(v02::ENTRY.revision(), "0.0.0");
        let planned = Catalog::PRODUCTION
            .plan(&state, &v03::ENTRY.revision())
            .expect("plan");
        let steps = planned
            .iter()
            .map(|step| step.step.as_str())
            .collect::<Vec<_>>();
        // only the steps of the release edge are planned
        let expected = v03::ENTRY
            .steps
            .iter()
            .map(|step| step.id)
            .collect::<Vec<_>>();
        assert_eq!(steps, expected, "{steps:?}");
    }

    /// This build and every release snapshot keep users layout 2, and a deployment at the newest
    /// release plans the reverse step for the first Helm revision
    #[test]
    #[cfg(feature = "scylla-utils")]
    fn the_releases_keep_users_layout_2() {
        // the registry declares the layout this build writes
        let users = LAYOUTS
            .iter()
            .find(|layout| layout.family == "users")
            .expect("users layout");
        assert_eq!((users.version, users.reads_from), (2, 2));
        // the release snapshots hold layout 2
        for (revision, version) in [(v02::ENTRY.id, 2), (v03::ENTRY.id, 2)] {
            let snapshot = RevisionSnapshot::committed(revision)
                .expect("committed snapshot")
                .expect("valid snapshot");
            let object = snapshot
                .objects
                .iter()
                .find(|object| object.name == "users" && object.canonical.get("users").is_some())
                .expect("users object");
            assert_eq!(object.canonical["users"], version, "{revision}");
        }
    }

    /// A deployment at the newest release plans the reverse step for the first Helm revision
    #[test]
    fn a_cluster_at_the_release_plans_the_reverse() {
        // the release's reverse step is the only step planned for the first Helm revision
        let released = Catalog::PRODUCTION.reached(&v03::ENTRY.revision(), "0.0.0");
        let planned = Catalog::PRODUCTION
            .plan(&released, &v02::ENTRY.revision())
            .expect("plan");
        let steps = planned
            .iter()
            .map(|step| step.step.as_str())
            .collect::<Vec<_>>();
        assert_eq!(steps, ["users-settings-theme-reverse"], "{steps:?}");
    }
}
