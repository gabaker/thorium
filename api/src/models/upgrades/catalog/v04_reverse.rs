//! The reverse edge of revision `2026-10-v04`: the way back to the revision that applies the
//! schema first

use super::v04::FINALIZE_TOOL_ONLY_STEP;
use crate::models::upgrades::{CatalogEntry, StepDef, StepKind};

/// The id of the step of the reverse edge that undoes the record step
pub const FINALIZE_TOOL_ONLY_REVERSE_STEP: &str = "finalize-tool-only-reverse";

/// The steps of the reverse edge, in the order they run
///
/// This list is append only: each step that undoes a step of `2026-10-v04` is appended here, naming
/// the step it reverses. The three conversions are irreversible, so the planner refuses a revert
/// across a finished one, naming it, and the only step here undoes the record step, which the
/// catalog needs so an edge back reverses a step of the edge it undoes. A deployment that can take
/// this edge hasn't recorded the revision's end, since the record step runs after the conversions,
/// so the step is never planned. The schema that revision declares is not undone: `2026-10-v03`
/// ignores what it doesn't read.
pub(super) const STEPS: &[StepDef] = &[StepDef {
    id: FINALIZE_TOOL_ONLY_REVERSE_STEP,
    kind: StepKind::Config,
    description: "Undo the record of the end of the revision that converts images, pipelines, \
                  and provenance; a revert that gets this far has no conversion to undo",
    reverses: Some(FINALIZE_TOOL_ONLY_STEP),
    ..StepDef::BASE
}];

/// The edge from the revision that converts tools back to the revision that applies the schema
pub(super) const ENTRY: CatalogEntry = CatalogEntry {
    from: Some("2026-10-v04"),
    id: "2026-10-v03",
    released_with: "1.8.1",
    description: "Revert the revision that converts images, pipelines, and provenance",
    schema: Some("86722e9607083db0e1f7d2b527ce3c4937c78b34fdc5cdb3ef5691eb70fbbdfd"),
    prerelease: false,
    sibling: false,
    steps: STEPS,
};
