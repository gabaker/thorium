//! The reverse edge of revision `2026-10-v05`: the way back to the revision that converts tools

use super::v05::FINALIZE_YARA_SCANNING_STEP;
use crate::models::upgrades::{CatalogEntry, StepDef, StepKind};

/// The id of the step of the reverse edge that undoes the record step
pub const FINALIZE_YARA_SCANNING_REVERSE_STEP: &str = "finalize-yara-scanning-reverse";

/// The steps of the reverse edge, in the order they run
///
/// This list is append only: each step that undoes a step of `2026-10-v05` is appended here,
/// naming the step it reverses. The only step undoes the record step, which the catalog needs so
/// an edge back reverses a step of the edge it undoes. The objects the revision's schema adds are
/// not dropped and need no pass back: `2026-10-v04` ignores what it doesn't read, every write to
/// the `scan_stats` telemetry table expires by TTL, Scylla rebuilds the `entities_by_name` view
/// from the `entities` table, and the `yara_rulesets` bucket holds a cache rebuilt from rule
/// entities on demand. No step has a transform, so the reversibility checks need no artifacts.
pub(super) const STEPS: &[StepDef] = &[StepDef {
    id: FINALIZE_YARA_SCANNING_REVERSE_STEP,
    kind: StepKind::Config,
    description: "Undo the record of the end of the revision that applies the yara scanning \
                  schema; the objects it added expire by TTL or are rebuilt, so nothing is \
                  dropped",
    reverses: Some(FINALIZE_YARA_SCANNING_STEP),
    ..StepDef::BASE
}];

/// The edge from the revision that applies the yara scanning schema back to the revision that
/// converts tools
pub(super) const ENTRY: CatalogEntry = CatalogEntry {
    from: Some("2026-10-v05"),
    id: "2026-10-v04",
    released_with: "1.8.1",
    description: "Revert the revision that applies the yara scanning schema",
    schema: Some("f9f605025a1d9dca23c939a5dec4b9699f7e525877b8439e214de03bdd6bd346"),
    prerelease: false,
    sibling: false,
    steps: STEPS,
};
