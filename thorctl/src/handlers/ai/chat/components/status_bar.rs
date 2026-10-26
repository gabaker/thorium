//! The status bar component for thorctl analyze chat
//!
//! This module provides a status bar at the bottom of the screen that
//! displays whether the prompt box has the keyboard and other status
//! information.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

use super::ActivitySnapshot;
use crate::handlers::ai::pricing::CostModel;

/// Render the status bar to the frame
///
/// Displays whether the prompt box has the keyboard (INSERT) or keys navigate
/// (NORMAL) in the bottom left corner and, when a run has
/// reported any token usage, a live token readout on the right: the latest
/// turn's context size, the cumulative input/output/total token counts, and --
/// when the session's model is priceable -- a running estimated cost. When a
/// session-wide snapshot is supplied and its totals differ from this page's
/// (side conversations are spending alongside it), a `Σ` grand-total segment
/// is appended so the whole session's spend stays visible from every tab.
///
/// # Arguments
///
/// * `frame` - The frame to render to
/// * `area` - The area to render the status bar in
/// * `prompt_focused` - Whether the page's prompt box currently has the keyboard
/// * `activity` - This page's run-activity snapshot for the token-usage readout
/// * `session` - The session-wide snapshot, or `None` to never show a `Σ` segment
/// * `cost` - The session's cost estimator, or `None` to omit the cost segments
pub fn render(
    frame: &mut Frame,
    area: Rect,
    prompt_focused: bool,
    activity: &ActivitySnapshot,
    session: Option<&ActivitySnapshot>,
    cost: Option<&CostModel>,
) {
    // determine the mode text and color from where the keyboard currently goes
    let (mode_text, mode_color) = if prompt_focused {
        ("INSERT", Color::Green)
    } else {
        ("NORMAL", Color::Blue)
    };
    // build the mode indicator span with styling
    let mode_span = Span::styled(
        format!(" {} ", mode_text),
        Style::default()
            .fg(Color::Black)
            .bg(mode_color)
            .add_modifier(Modifier::BOLD),
    );
    // build the status line with the left-aligned mode indicator and render it
    // inside a block whose top border separates the bar from the prompt above
    let status = Paragraph::new(Line::from(vec![mode_span])).block(
        Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    // render the status bar
    frame.render_widget(status, area);
    // overlay a right-aligned token readout once a run has reported any usage; a
    // run that has emitted nothing yet leaves the totals at zero and is skipped
    if activity.total_tokens > 0 || session.is_some_and(|s| s.total_tokens > 0) {
        // estimate the run's dollar cost when the session's model is priceable
        let dollars = cost.map(|cost| {
            cost.cost(
                activity.total_input,
                activity.total_output,
                activity.total_cached,
                activity.total_cache_creation,
            )
        });
        // append the session-wide segment only when side conversations have
        // made it differ from this page's own totals
        let session_segment = session
            .filter(|session| session.total_tokens != activity.total_tokens)
            .map(|session| {
                // estimate the whole session's cost from its own counters
                let session_dollars = cost.map(|cost| {
                    cost.cost(
                        session.total_input,
                        session.total_output,
                        session.total_cached,
                        session.total_cache_creation,
                    )
                });
                session_readout(session, session_dollars)
            });
        // the readout shares the content row with the mode indicator, so leave
        // room for it (and one separating column) when sizing the readout
        let max_width = (area.width as usize).saturating_sub(9);
        // build the widest readout that fits the available columns
        let usage = Line::from(Span::styled(
            readout(activity, dollars, session_segment.as_deref(), max_width),
            Style::default().fg(Color::DarkGray),
        ));
        // render it on the content row (below the top border) aligned to the right
        let inner = Rect {
            x: area.x,
            y: area.y + 1,
            width: area.width,
            height: area.height.saturating_sub(1),
        };
        frame.render_widget(Paragraph::new(usage).right_aligned(), inner);
    }
}

/// Render the session-wide `Σ` grand-total segment
///
/// # Arguments
///
/// * `session` - The session-wide activity snapshot the counts come from
/// * `dollars` - The estimated session cost, or `None` to omit it
fn session_readout(session: &ActivitySnapshot, dollars: Option<f64>) -> String {
    // render the session's cost, switching to cents precision for large runs
    let cost = dollars.map(|dollars| {
        if dollars < 100.0 {
            format!(" · ${dollars:.4}")
        } else {
            format!(" · ${dollars:.2}")
        }
    });
    let cost = cost.as_deref().unwrap_or("");
    // the grand total plus its cost, prefixed with the sum glyph
    format!(" · Σ: {}{cost}", with_thousands(session.total_tokens))
}

/// Build the widest token/cost readout that fits in `max_width` columns
///
/// The full readout is the latest turn's context size, the cumulative
/// input/output/total counts, the estimated cost when one is known, and the
/// session-wide `Σ` segment when side conversations make it differ:
/// `ctx: 48,120 · in: 1,204,567 · out: 48,320 · total: 1,252,887 · $1.2345 `.
/// On narrow terminals the least essential segments are dropped in order --
/// first the `Σ` grand total, then `ctx:` (a live context-pressure hint), then
/// `total:` (derivable from in + out) -- so the input/output/cost core
/// survives the squeeze.
///
/// # Arguments
///
/// * `activity` - The run-activity snapshot the counts come from
/// * `dollars` - The estimated run cost, or `None` to omit the cost segment
/// * `session` - The rendered session-wide segment, or `None` to omit it
/// * `max_width` - The columns available for the readout
fn readout(
    activity: &ActivitySnapshot,
    dollars: Option<f64>,
    session: Option<&str>,
    max_width: usize,
) -> String {
    // the cumulative input is derived as total minus output, which is correct
    // on every provider protocol regardless of its cache-count semantics
    let input = activity.total_tokens.saturating_sub(activity.total_output);
    // render the cost segment, switching to cents precision for large runs
    let cost = dollars.map(|dollars| {
        if dollars < 100.0 {
            format!(" · ${dollars:.4}")
        } else {
            format!(" · ${dollars:.2}")
        }
    });
    let cost = cost.as_deref().unwrap_or("");
    // the session-wide segment is the first to go under a squeeze
    let sum = session.unwrap_or("");
    // pre-render the shared segments the candidates assemble from
    let ctx = with_thousands(activity.last_input);
    let input = with_thousands(input);
    let output = with_thousands(activity.total_output);
    let total = with_thousands(activity.total_tokens);
    // try the candidates widest-first, dropping Σ, ctx, and then total to fit
    let candidates = [
        format!("ctx: {ctx} · in: {input} · out: {output} · total: {total}{cost}{sum} "),
        format!("ctx: {ctx} · in: {input} · out: {output} · total: {total}{cost} "),
        format!("in: {input} · out: {output} · total: {total}{cost} "),
        format!("in: {input} · out: {output}{cost} "),
    ];
    for candidate in &candidates {
        // keep the first candidate that fits the available columns
        if candidate.chars().count() <= max_width {
            return candidate.clone();
        }
    }
    // nothing fits outright: hand back the narrowest and let the right-aligned
    // paragraph clip its left edge gracefully
    candidates.into_iter().next_back().unwrap_or_default()
}

/// Format an unsigned integer with `,` grouping every three digits
///
/// Mirrors ThorPal's own token-count formatting (e.g. 48120 -> "48,120") without
/// taking a cross-crate dependency just for a readout.
///
/// # Arguments
///
/// * `value` - The token count to render
pub(crate) fn with_thousands(value: u64) -> String {
    // render the raw digits so we can walk them and place separators
    let digits = value.to_string();
    // total digit count, used to detect where each group of three begins
    let len = digits.len();
    // preallocate room for the digits plus the commas between groups
    let mut out = String::with_capacity(len + len / 3);
    // walk the digits left-to-right, inserting a comma ahead of every fresh group
    for (idx, ch) in digits.chars().enumerate() {
        // a separator precedes any non-leading digit that starts a group of three
        if idx != 0 && (len - idx).is_multiple_of(3) {
            out.push(',');
        }
        // emit this digit after any separator that belonged before it
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A snapshot with every counter populated for the readout tests.
    fn snapshot() -> ActivitySnapshot {
        ActivitySnapshot {
            busy: false,
            agent: None,
            detail: None,
            retries: Vec::new(),
            last_input: 48_120,
            last_output: 512,
            total_tokens: 1_252_887,
            total_input: 1_100_000,
            total_output: 48_320,
            total_cached: 90_000,
            total_cache_creation: 14_567,
        }
    }

    /// A wide terminal gets the full ctx/in/out/total/cost readout.
    #[test]
    fn readout_shows_everything_when_it_fits() {
        let text = readout(&snapshot(), Some(1.2345), None, 200);
        assert_eq!(
            text,
            "ctx: 48,120 · in: 1,204,567 · out: 48,320 · total: 1,252,887 · $1.2345 "
        );
    }

    /// Without a cost estimator the dollar segment is omitted entirely.
    #[test]
    fn readout_hides_cost_without_an_estimator() {
        let text = readout(&snapshot(), None, None, 200);
        assert!(!text.contains('$'));
        assert!(text.contains("total: 1,252,887"));
    }

    /// A squeezed terminal drops ctx first, then the derivable total.
    #[test]
    fn readout_degrades_on_narrow_terminals() {
        // just too narrow for the full readout: ctx goes first
        let text = readout(&snapshot(), Some(1.2345), None, 65);
        assert!(!text.contains("ctx:"));
        assert!(text.contains("total:"));
        // narrower still: the derivable total goes too
        let text = readout(&snapshot(), Some(1.2345), None, 40);
        assert!(!text.contains("total:"));
        assert!(text.contains("in: 1,204,567"));
        assert!(text.contains("out: 48,320"));
    }

    /// A large run's cost switches from tenth-of-a-cent to cent precision.
    #[test]
    fn readout_widens_cost_precision_for_large_runs() {
        let text = readout(&snapshot(), Some(123.456789), None, 200);
        assert!(text.ends_with("$123.46 "));
    }

    /// The session-wide Σ segment is appended when it fits and dropped first.
    #[test]
    fn readout_appends_session_segment_when_it_fits() {
        // a session snapshot whose totals exceed this page's own
        let mut session = snapshot();
        session.total_tokens = 2_000_000;
        let segment = session_readout(&session, Some(2.5));
        assert_eq!(segment, " · Σ: 2,000,000 · $2.5000");
        // a wide terminal keeps the Σ segment at the end
        let text = readout(&snapshot(), Some(1.2345), Some(&segment), 200);
        assert!(text.contains("Σ: 2,000,000"));
        assert!(text.contains("total: 1,252,887"));
        // under a squeeze the Σ segment is the first to go
        let text = readout(&snapshot(), Some(1.2345), Some(&segment), 75);
        assert!(!text.contains('Σ'));
        assert!(text.contains("ctx:"));
    }
}
