//! Revision `2026-10-v05`: the revision that applies the yara scanning schema

use crate::models::upgrades::{CatalogEntry, StepDef, StepKind};

/// The id of the step that records the revision, after which the operator applies the yara
/// scanning schema
pub const FINALIZE_YARA_SCANNING_STEP: &str = "finalize-yara-scanning";

/// The steps of revision `2026-10-v05`, in the order they run
///
/// This list is append only: each change that needs work on existing deployments in this revision
/// appends its steps here, after the steps already listed, and never reorders or removes them.
/// The append only rule applies once a revision is released.
/// The revision has no schema-apply step of its own: its schema differs from the previous
/// revision's only additively (the `scan_stats` table, the `entities_by_name` view Scylla fills
/// from the `entities` table, and the `yara_rulesets` S3 bucket role at layout 1). The upgrade
/// runs before the reconcile that deploys the new image, so the record step runs first, and that
/// reconcile then applies the image's schema before it rolls anything out (see `2026-10-v03`) and
/// creates a missing bucket. No step changes data: the new objects start empty, so the record step
/// is the whole edge.
pub(super) const STEPS: &[StepDef] = &[StepDef {
    id: FINALIZE_YARA_SCANNING_STEP,
    kind: StepKind::Config,
    description: "Record the revision; the operator applies the yara scanning schema after it",
    ..StepDef::BASE
}];

/// The edge from the revision that converts tools to the revision that applies the yara scanning
/// schema
///
/// Releasing a revision requires, besides the entry itself, what the entry of `2026-10-v03`
/// lists: `released_with`, an executor for every new step, the registered migrations, the review
/// of how a namespace without recorded state is seeded, the committed snapshot and its
/// fingerprint, the catalog, operator, and thoradm tests that pin the newest revision, and the
/// documentation of the revision and its steps.
pub(super) const ENTRY: CatalogEntry = CatalogEntry {
    from: Some("2026-10-v04"),
    id: "2026-10-v05",
    released_with: "1.8.1",
    description: "The yara scanning schema: scan stats, exact-name entity reads, and the yara \
                  ruleset cache",
    schema: Some("63b181d952ed2125bbe9c8405a33fb96658ae07d8011196d309465e7fa3d1cac"),
    prerelease: false,
    sibling: false,
    steps: STEPS,
};
