//! The finding-context type shared between detections and the forum seeder.
//!
//! This is the small, self-contained slice of the source project's provenance
//! pass that the detectors actually depend on: the [`CallContext`] a hit carries
//! to say what the surrounding code does with its evidence. The heavier
//! call-graph walk that produced these in the source is not ported.

/// What the surrounding code does with a finding's evidence.
///
/// Attached to a detection hit's `context` so consumers (the forum seeder here)
/// see the same judgement rather than re-deriving it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallContext {
    /// The finding sits in C-runtime / compiler-support code, reached by the
    /// recorded caller chain (e.g. `__scrt_fastfail <- __scrt_dllmain_crt_dispatch`).
    Runtime(String),
    /// The evidence's result seeds a PRNG, via the named routine.
    SeedsRng(String),
    /// The evidence sits in a loop that also performs command-and-control I/O, so
    /// it paces that loop rather than stalling an analyst.
    PacesLoop,
    /// The evidence's result is tested and steers a conditional branch.
    GatesBranch,
}

impl CallContext {
    /// A short machine-readable tag for downstream prompts
    pub fn tag(&self) -> String {
        // render each variant as a compact `kind:detail` (or bare kind) token
        match self {
            CallContext::Runtime(chain) => format!("runtime:{chain}"),
            CallContext::SeedsRng(routine) => format!("seeds-rng:{routine}"),
            CallContext::PacesLoop => "paces-loop".to_string(),
            CallContext::GatesBranch => "gates-branch".to_string(),
        }
    }

    /// The analyst-facing sentence appended to the hit's reasoning
    pub fn note(&self) -> String {
        // spell out, in prose, what the surrounding code does with the evidence
        match self {
            CallContext::Runtime(chain) => format!(
                "Context: this is compiler-generated runtime code ({chain}), not the sample's \
                 own logic — the C runtime performs this check in every binary built with it."
            ),
            CallContext::SeedsRng(routine) => format!(
                "Context: the value read here is consumed by {routine} as a PRNG seed, not \
                 compared against an expected elapsed time — this is randomness, not a timing check."
            ),
            CallContext::PacesLoop => "Context: this sits inside a loop that also performs \
                 command-and-control I/O, so it paces that loop (beacon interval) rather than \
                 stalling to outlast an analysis window."
                .to_string(),
            CallContext::GatesBranch => "Context: the result is tested and steers a conditional \
                 branch in the sample's own code, so it is used as a check."
                .to_string(),
        }
    }
}
