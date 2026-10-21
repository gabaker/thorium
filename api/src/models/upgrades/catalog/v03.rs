//! Revision `2026-10-v03`: the next revision after `2026-10-v02`, declared but not yet released

use crate::models::upgrades::{CatalogEntry, StepDef};

/// The steps of revision `2026-10-v03`, in the order they run
///
/// This list is append only: each change that needs work on existing deployments in this revision
/// appends its steps here, after the steps already listed, and never reorders or removes them.
pub(super) const STEPS: &[StepDef] = &[];

/// The edge from the first Helm revision to the next revision
///
/// This edge is declared but not yet released into [`super::CATALOG`], so it changes neither
/// [`super::latest`], the planner, nor the CRD. The first change that appends a step to [`STEPS`]
/// also appends this entry to [`super::CATALOG`] in the same change, together with what releasing
/// a revision requires:
///
/// * `released_with` set to the Thorium version the revision ships in (the workspace version that
///   `.github/scripts/versions.py check` keeps in agreement across Cargo, the UI, the charts,
///   minithor, megathor, and the docs)
/// * an executor for every new step in `EXECUTORS` (`operator/src/upgrades/steps.rs`)
/// * every data migration a step names registered in thoradm's `MIGRATIONS`
/// * a review of how a namespace without recorded state is seeded
///   (`operator/src/upgrades/detect.rs` and the operator's `FIRST_HELM_REVISION`)
/// * the catalog test that pins [`super::latest`], and the operator and thoradm tests and probes
///   that name the newest revision
/// * the revision and its steps documented in the admin guide's "Upgrading Thorium"
///   (`api/docs/src/admins/deploy/upgrades.md`, section "Revisions"), and any chart, megathor, or
///   docs example that names the newest revision as a `targetRevision`
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "declared ahead of its release; appended to CATALOG with its first step"
    )
)]
pub(super) const ENTRY: CatalogEntry = CatalogEntry {
    from: Some("2026-10-v02"),
    id: "2026-10-v03",
    released_with: "1.8.1",
    description: "The revision after the first Helm revision (not yet released)",
    schema: None,
    prerelease: false,
    sibling: false,
    steps: STEPS,
};
