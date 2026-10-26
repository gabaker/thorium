//! The setup wizard's step views
//!
//! Each step is a plain struct (or a render function, for pages with no input
//! state of their own) dispatched by a match in the app, exactly like the
//! usage TUI's views: `handle_key` returns a small outcome enum the app maps
//! onto flow transitions, and `render` draws the step's body area.

pub mod credentials;
pub mod ghidra;
pub mod java;
pub mod login;
pub mod projects;
pub mod provider;
pub mod review;
pub mod verify;
pub mod welcome;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;

/// The braille spinner frames the busy pages animate through
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Pick the spinner frame for the current instant
///
/// Advances one frame roughly every 100ms off the wall clock, so the spinner
/// animates on the app's periodic redraws without threading a tick counter in.
pub fn spinner() -> &'static str {
    // milliseconds since the epoch, or 0 if the clock is somehow before it
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    // advance one frame every 100ms, wrapping over the frame table
    let index = (millis / 100) as usize % SPINNER_FRAMES.len();
    SPINNER_FRAMES[index]
}

/// The style selected list rows render with
pub fn selected_style() -> Style {
    Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

/// The style idle list rows render with
pub fn idle_style() -> Style {
    Style::default().fg(Color::Gray)
}

/// The dim style explanatory text renders with
pub fn dim_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// A span marking a row selected or not with the conventional pointer
///
/// # Arguments
///
/// * `selected` - Whether this row is the selected one
pub fn pointer(selected: bool) -> Span<'static> {
    if selected {
        Span::styled("❯ ", selected_style())
    } else {
        Span::raw("  ")
    }
}

/// Mask a secret down to its last four characters for review displays
///
/// # Arguments
///
/// * `secret` - The secret to mask
pub fn mask_secret(secret: &str) -> String {
    // short secrets mask entirely so the length leaks nothing useful
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() <= 4 {
        return "••••".to_string();
    }
    // longer ones keep the conventional identifying tail
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("••••{tail}")
}
