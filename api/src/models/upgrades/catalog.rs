//! The catalog of revisions: its edges, the steps on each edge, and the support floors
//!
//! Each edge lives in its own file under `catalog/`, and [`CATALOG`] lists them oldest first; a
//! new edge is a new file appended to that list, so edges never share a step list.

use super::{RevisionId, StepDef};

mod v01;
mod v02;
mod v03;

pub use v02::REINDEX_PROCEDURE;

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
pub const CATALOG: &[CatalogEntry] = &[v01::ENTRY, v02::ENTRY];

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
    // the catalog is oldest first, so its last entry is the newest
    CATALOG
        .last()
        .expect("the catalog has at least one revision")
        .revision()
}

/// Get the catalog entry for a revision
///
/// # Arguments
///
/// * `id` - The revision to find
#[must_use]
pub fn entry(id: &RevisionId) -> Option<&'static CatalogEntry> {
    // look the revision up by its id
    CATALOG.iter().find(|entry| entry.id == id.as_str())
}

/// Find a step anywhere in the catalog by its id
///
/// # Arguments
///
/// * `id` - The id of the step to find
#[must_use]
pub fn step(id: &str) -> Option<(&'static CatalogEntry, &'static StepDef)> {
    // search every revision's steps, keeping the revision each belongs to
    CATALOG
        .iter()
        .flat_map(|entry| entry.steps.iter().map(move |step| (entry, step)))
        .find(|(_, step)| step.id == id)
}

#[cfg(test)]
mod tests {
    use super::{CATALOG, RevisionId, v02, v03};

    /// The declared next revision is a well formed edge from `2026-10-v02` that isn't released yet
    #[test]
    fn next_revision_is_declared_but_not_released() {
        // the declared id parses as a revision newer than the edge it starts from
        let id = v03::ENTRY.id.parse::<RevisionId>().expect("valid revision");
        assert!(id > v02::ENTRY.revision());
        // the declared edge starts from the first Helm revision
        assert_eq!(v03::ENTRY.from, Some(v02::ENTRY.id));
        // the declared edge isn't in the catalog the planner and the operator use
        assert!(CATALOG.iter().all(|entry| entry.id != v03::ENTRY.id));
    }
}
