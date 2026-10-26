//! The inline "thinking" throbber shown above the prompt while a turn runs.
//!
//! When a turn is in flight the chat page reserves a one-row strip just above the
//! prompt box and renders an animated spinner here, along with the current
//! agent's display name and what it is doing (its goal or the tool it is calling).
//! The animation is driven by wall-clock time so the frame advances on the
//! periodic redraws the app already schedules, with no per-frame state to carry.

use std::time::{SystemTime, UNIX_EPOCH};

use chrono::Utc;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use super::activity::{ActivitySnapshot, RetrySnapshot};

/// The braille spinner frames cycled through to animate the throbber.
const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Pick the spinner frame for the current instant
///
/// Advances one frame roughly every 100ms off the wall clock, so the spinner
/// animates on the app's periodic redraws without threading a tick counter in.
fn current_frame() -> &'static str {
    // milliseconds since the epoch, or 0 if the clock is somehow before it
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    // advance one frame every 100ms, wrapping over the frame table
    let index = (millis / 100) as usize % SPINNER_FRAMES.len();
    SPINNER_FRAMES[index]
}

/// Render the thinking throbber into a one-row strip
///
/// # Arguments
///
/// * `frame` - The frame to render to
/// * `area` - The one-row area reserved for the throbber
/// * `activity` - The current run-activity snapshot for the agent/detail context
pub fn render(frame: &mut Frame, area: Rect, activity: &ActivitySnapshot) {
    // pick the animated spinner glyph for this instant
    let spinner = current_frame();
    // when a completion request is backing off, the strip shows that retry's
    // live status instead of the normal thinking line
    if let Some(retry) = activity.retries.first() {
        return render_retry(frame, area, spinner, retry, activity.retries.len());
    }
    // start the line with the spinner and a dim "Thinking…" label
    let mut spans = vec![
        Span::styled(
            format!("{spinner} "),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("Thinking…", Style::default().fg(Color::DarkGray)),
    ];
    // append the current agent's name when one is known, so the user sees who is working
    if let Some(agent) = &activity.agent {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            agent.clone(),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ));
    }
    // append what that agent is doing (its goal or current tool) when known
    if let Some(detail) = &activity.detail {
        spans.push(Span::styled(
            format!(": {detail}"),
            Style::default().fg(Color::Gray),
        ));
    }
    // render the assembled line into the reserved strip, aligned to the right edge
    frame.render_widget(Paragraph::new(Line::from(spans)).right_aligned(), area);
}

/// Render the live backoff line for the oldest in-flight retry episode
///
/// The attempt counter, "last try Xs ago" readout, and the countdown to the
/// next attempt are computed from the episode's timestamps fresh each frame,
/// so the single line ticks in place on the app's periodic redraws -- no new
/// chat entries, no scrolling retry spam.
///
/// # Arguments
///
/// * `frame` - The frame to render to
/// * `area` - The one-row area reserved for the throbber
/// * `spinner` - The animated spinner glyph for this instant
/// * `retry` - The retry episode whose status is shown
/// * `episodes` - How many episodes are backing off concurrently in total
fn render_retry(
    frame: &mut Frame,
    area: Rect,
    spinner: &str,
    retry: &RetrySnapshot,
    episodes: usize,
) {
    // compute the elapsed and remaining readouts from the stored timestamps
    let now = Utc::now();
    let since = (now - retry.last_attempt_at).num_seconds().max(0);
    let until = (retry.next_attempt_at - now).num_seconds();
    // at zero the wait is over and the next attempt is already in flight
    let countdown = if until > 0 {
        format!("next in {until}s")
    } else {
        "retrying…".to_string()
    };
    // assemble the warning-colored status line for this episode
    let mut spans = vec![
        Span::styled(
            format!("{spinner} "),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("⚠ ", Style::default().fg(Color::Yellow)),
        Span::styled(
            retry.agent.clone(),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(": {}", retry.error),
            Style::default().fg(Color::Gray),
        ),
        Span::styled(
            format!(
                " — attempt {}/{} · last try {since}s ago · {countdown}",
                retry.attempt, retry.max_attempts
            ),
            Style::default().fg(Color::DarkGray),
        ),
    ];
    // note how many more episodes are waiting out their own backoff
    if episodes > 1 {
        spans.push(Span::styled(
            format!(" (+{} more backing off)", episodes - 1),
            Style::default().fg(Color::DarkGray),
        ));
    }
    // render the assembled line into the reserved strip, aligned to the right edge
    frame.render_widget(Paragraph::new(Line::from(spans)).right_aligned(), area);
}
