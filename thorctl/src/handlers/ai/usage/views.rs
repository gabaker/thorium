//! The usage TUI's screens and their shared rendering helpers

pub mod agents;
pub mod heatmap;
pub mod models;
pub mod overview;
pub mod sessions;
pub mod timeline;
pub mod trend;

use chrono::Utc;

/// Builds a horizontal bar of a given width filled to a fraction
///
/// Any nonzero fraction fills at least one cell, so tiny values never
/// disappear entirely next to a huge maximum.
///
/// # Arguments
///
/// * `width` - The bar's total width in cells
/// * `frac` - The filled fraction, clamped into `0.0..=1.0`
pub(crate) fn fill_bar(width: usize, frac: f64) -> String {
    // clamp the fraction so bad math can never overflow the lane
    let frac = frac.clamp(0.0, 1.0);
    // quantize to whole cells, guaranteeing a sliver for any nonzero value
    let mut filled = (frac * width as f64).round() as usize;
    if frac > 0.0 {
        filled = filled.max(1);
    }
    filled = filled.min(width);
    // fill the lane and pad the rest with spaces
    let mut bar = "█".repeat(filled);
    bar.push_str(&" ".repeat(width - filled));
    bar
}

/// Clips (or pads) a string to an exact display width in characters
///
/// # Arguments
///
/// * `text` - The text to fit
/// * `width` - The exact character width to fit it into
pub(crate) fn clip(text: &str, width: usize) -> String {
    // count characters, not bytes, so unicode names stay aligned
    let count = text.chars().count();
    if count > width {
        // leave room for the ellipsis marker when clipping
        let kept: String = text.chars().take(width.saturating_sub(1)).collect();
        format!("{kept}…")
    } else {
        // pad short text out to the exact width
        format!("{text}{}", " ".repeat(width - count))
    }
}

/// Formats how long ago an epoch stamp was, coarsely
///
/// # Arguments
///
/// * `ts` - Milliseconds since the Unix epoch, when known
pub(crate) fn ago(ts: Option<u64>) -> String {
    // no stamp means nothing sensible to say
    let Some(ts) = ts else {
        return "—".to_owned();
    };
    // a future stamp (clock skew) reads as right now
    let now = Utc::now().timestamp_millis().max(0) as u64;
    let age_ms = now.saturating_sub(ts);
    // climb the units coarsely; precision past days helps nobody here
    let minutes = age_ms / 60_000;
    if minutes < 1 {
        "now".to_owned()
    } else if minutes < 60 {
        format!("{minutes}m ago")
    } else if minutes < 60 * 24 {
        format!("{}h ago", minutes / 60)
    } else {
        format!("{}d ago", minutes / (60 * 24))
    }
}
