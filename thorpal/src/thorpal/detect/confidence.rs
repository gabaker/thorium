//! The confidence scale a detection hit is reported on.

use serde::Serialize;

/// How confident/reliable a detection hit is
///
/// The variant order mirrors the scale it was ported from (`Fact` is the most
/// certain, `Untrusted` the least). Ordering between two values is expressed
/// through [`confidence_rank`](crate::thorpal::detect::detections::confidence_rank)
/// rather than a derived `Ord`, so the declaration order here is not load-bearing.
#[derive(Debug, Clone, Copy, Serialize)]
pub enum Confidence {
    /// This is known to be a fact
    Fact,
    /// This is more than likely true
    Likely,
    /// This may or may not be true (50/50 odds)
    Unsure,
    /// This is unlikely to be true and should be validated
    Untrusted,
}
