//! Static detectors ported from the `ghidra-flag` proof of concept.
//!
//! This subtree runs a catalog of static analysis passes ("detectors") over an
//! imported binary via `ghidra-cli` and produces structured [`DetectionHit`]s,
//! each tied to the function it lands in. ThorPal runs these once, before the
//! agent loop, to pre-seed the forum with points of interest (see
//! [`crate::thorpal::seed`]).
//!
//! [`DetectionHit`]: crate::thorpal::detect::detections::DetectionHit

pub mod artifact_gates;
pub mod authenticode;
pub mod confidence;
pub mod context;
pub mod decode;
pub mod detections;
pub mod emulation;
pub mod fileinfo;
pub mod indicators;
pub mod provenance_types;
pub mod script_detections;
pub mod toolchain;
pub mod validate;
pub mod yara;

use ghidra_cli::models::Instruction;

/// Renders an instruction's operands as a single, comma-joined string
///
/// ThorPal's `ghidra-cli` models `Instruction::operands` as a `Vec<String>`,
/// whereas the detectors were written against a single `Option<String>`. This is the
/// one adaptation seam between the two: every operand read in the ported code goes
/// through here so the detectors keep seeing the flat string they expect.
///
/// # Arguments
///
/// * `insn` - The instruction whose operands to render
pub fn op_str(insn: &Instruction) -> String {
    // join the operand list back into the single lowercase-agnostic string the
    // detectors were written to scan; a missing operand list renders as empty
    insn.operands.join(", ")
}
