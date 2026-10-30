//! The telemetry TUIs' shared shell: theme, sidebar sizing, and pop-outs.
//!
//! Both telemetry TUIs (the explorer and the export wizard) compose the same
//! pieces the chat TUI established: a foreground-only theme, centered modal
//! pop-outs rendered over a [`Clear`], and the Space shortcut palette. The
//! pop-out follows the chat overlays' contract -- `handle_key` returns
//! [`ViewOutcome::Close`] when it wants to close, scrolling is internal --
//! so the owning app routes keys to the top-most layer first.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::handlers::ai::chat::{Theme, ViewOutcome, markdown_lines, wrap_words};

/// The width below which the explorer's sidebar hides entirely
const SIDEBAR_MIN_TERMINAL: u16 = 110;

/// A centered rectangle of the given size inside an area
///
/// The same technique the chat TUI's modals use: center by halving the
/// leftover space, clamping to the area so a tiny terminal never overflows.
///
/// # Arguments
///
/// * `area` - The area to center inside
/// * `width` - The rectangle's desired width
/// * `height` - The rectangle's desired height
pub fn centered(area: Rect, width: u16, height: u16) -> Rect {
    // clamp the requested size to what the area can actually hold
    let width = width.min(area.width);
    let height = height.min(area.height);
    // halve the leftover space on each axis
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width, height)
}

/// The explorer sidebar's width for a terminal width
///
/// A quarter of the terminal clamped into [32, 48] columns, and nothing at
/// all below 110 columns -- the fleet table needs roughly 85 columns, so the
/// chat board's two-fifths rule would squeeze it unusable.
///
/// # Arguments
///
/// * `width` - The whole terminal's width in columns
pub fn sidebar_width(width: u16) -> u16 {
    // narrow terminals give the whole width to the main pane
    if width < SIDEBAR_MIN_TERMINAL {
        0
    } else {
        (width / 4).clamp(32, 48)
    }
}

/// The telemetry TUIs' palette, derived from the chat theme's styles
///
/// The chat [`Theme`] is message/graph-specific rather than a general UI
/// palette, so the telemetry shell lifts the handful of styles it needs from
/// it -- one accent, one dim, and the state colors -- keeping both TUIs in
/// the chat TUI's visual family without inventing new colors.
#[derive(Debug, Clone)]
pub struct TelemetryTheme {
    /// Bold titles and section headers
    pub title: Style,
    /// The accent for the active screen, cursor, and highlights
    pub accent: Style,
    /// De-emphasized hints, legends, and separators
    pub dim: Style,
    /// Warnings (mixed recipes, truncated lists)
    pub warn: Style,
    /// Errors and the loop! flag
    pub error: Style,
    /// Positive state (writes, successes)
    pub ok: Style,
}

impl Default for TelemetryTheme {
    /// Lift the shell's styles from the chat theme's message palette
    fn default() -> Self {
        // the chat theme is the single source of the color family
        let theme = Theme::default();
        TelemetryTheme {
            title: Style::default().add_modifier(Modifier::BOLD),
            accent: theme.messages.agent,
            dim: theme.messages.tool,
            warn: theme.messages.forum,
            error: theme.messages.error,
            ok: theme.messages.tool_ok,
        }
    }
}

/// A pop-out's body: pre-built lines, or markdown rendered at display time
#[derive(Debug, Clone)]
pub enum PopupBody {
    /// Already-styled lines, wrapped line by line at render time
    Lines(Vec<Line<'static>>),
    /// Markdown text, rendered through the chat TUI's markdown pipeline
    Markdown(String),
}

/// A centered, scrollable pop-out window over the current screen
///
/// Follows the chat overlays' interface: the owner renders it last, routes
/// keys to it first, and drops it when `handle_key` returns
/// [`ViewOutcome::Close`].
#[derive(Debug, Clone)]
pub struct Popup {
    /// The title rendered on the pop-out's top border
    pub title: String,
    /// The pop-out's body
    pub body: PopupBody,
    /// The first visible body line
    scroll: usize,
}

impl Popup {
    /// Creates a pop-out over pre-built lines
    ///
    /// # Arguments
    ///
    /// * `title` - The title rendered on the top border
    /// * `lines` - The body's lines
    pub fn lines(title: impl Into<String>, lines: Vec<Line<'static>>) -> Self {
        Popup {
            title: title.into(),
            body: PopupBody::Lines(lines),
            scroll: 0,
        }
    }

    /// Creates a pop-out over markdown text
    ///
    /// # Arguments
    ///
    /// * `title` - The title rendered on the top border
    /// * `markdown` - The body's markdown source
    pub fn markdown(title: impl Into<String>, markdown: impl Into<String>) -> Self {
        Popup {
            title: title.into(),
            body: PopupBody::Markdown(markdown.into()),
            scroll: 0,
        }
    }

    /// Handles a key while the pop-out is open
    ///
    /// # Arguments
    ///
    /// * `key` - The key event to handle
    pub fn handle_key(&mut self, key: KeyEvent) -> ViewOutcome {
        match key.code {
            // the pop-out closes itself; the owner drops it on Close
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => ViewOutcome::Close,
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll = self.scroll.saturating_sub(1);
                ViewOutcome::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll = self.scroll.saturating_add(1);
                ViewOutcome::None
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(20);
                ViewOutcome::None
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(20);
                ViewOutcome::None
            }
            KeyCode::Home => {
                self.scroll = 0;
                ViewOutcome::None
            }
            // every other key stays inside the pop-out
            _ => ViewOutcome::None,
        }
    }

    /// Scrolls the pop-out on a wheel event
    ///
    /// # Arguments
    ///
    /// * `up` - Whether the wheel scrolled up
    pub fn handle_scroll(&mut self, up: bool) {
        if up {
            self.scroll = self.scroll.saturating_sub(3);
        } else {
            self.scroll = self.scroll.saturating_add(3);
        }
    }

    /// Renders the pop-out centered over the given area
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The full area the pop-out centers over
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        // 80% of the area, never smaller than a readable floor
        let width = (area.width as u32 * 4 / 5) as u16;
        let height = (area.height as u32 * 4 / 5) as u16;
        let overlay = centered(area, width.max(40), height.max(10));
        // wrap the body to the inner width (borders take two columns)
        let inner = overlay.width.saturating_sub(2) as usize;
        let lines: Vec<Line<'static>> = match &self.body {
            // markdown renders through the chat TUI's pipeline
            PopupBody::Markdown(source) => markdown_lines(source, inner, Style::default()),
            // plain lines word-wrap individually, preserving their styles
            // only on lines that fit (wrapped tails fall back to plain)
            PopupBody::Lines(prebuilt) => prebuilt
                .iter()
                .flat_map(|line| {
                    let width = line.width();
                    if width <= inner {
                        vec![line.clone()]
                    } else {
                        // re-wrap an overlong line as plain text
                        let text: String = line
                            .spans
                            .iter()
                            .map(|span| span.content.as_ref())
                            .collect();
                        wrap_words(&text, inner)
                            .into_iter()
                            .map(Line::from)
                            .collect()
                    }
                })
                .collect(),
        };
        // clamp the scroll to the body and window the visible slice
        let visible = overlay.height.saturating_sub(2) as usize;
        self.scroll = self
            .scroll
            .min(lines.len().saturating_sub(visible.min(lines.len())));
        let windowed: Vec<Line> = lines
            .iter()
            .skip(self.scroll)
            .take(visible)
            .cloned()
            .collect();
        // the same overlay dressing the chat modals use
        let block = Block::default()
            .title(self.title.as_str())
            .title_alignment(Alignment::Center)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Gray))
            .style(Style::default().bg(Color::Rgb(60, 60, 60)));
        let paragraph = Paragraph::new(windowed)
            .block(block)
            .style(Style::default().fg(Color::White).bg(Color::Rgb(60, 60, 60)));
        // clear first so the pop-out floats over the screen behind it
        frame.render_widget(Clear, overlay);
        frame.render_widget(paragraph, overlay);
    }
}

/// Builds the shared signature-detail pop-out both telemetry TUIs show
///
/// The full multi-line signature renders one wrapped source line at a time,
/// under a header carrying the tool, the cluster's weight, the loop flag,
/// and (when known) how many sessions carry it and which recipes produced it.
///
/// # Arguments
///
/// * `tool` - The tool whose failing results normalized to this signature
/// * `sig` - The full normalized signature
/// * `count` - The cluster's count across the viewed scope
/// * `looped` - Whether any agent hit the stuck-tool-loop threshold on it
/// * `sessions` - How many sessions carry the cluster, when known
/// * `recipes` - The signature recipes in play, when known
pub fn signature_popup(
    tool: &str,
    sig: &str,
    count: u64,
    looped: bool,
    sessions: Option<usize>,
    recipes: Option<&std::collections::BTreeSet<u32>>,
) -> Popup {
    let theme = TelemetryTheme::default();
    let mut lines: Vec<Line<'static>> = Vec::new();
    // the header facts, one per line
    lines.push(Line::from(format!("tool      {tool}")));
    let mut weight = format!("count     {count}");
    if let Some(sessions) = sessions {
        weight.push_str(&format!("   across {sessions} session(s)"));
    }
    lines.push(Line::from(weight));
    if looped {
        lines.push(Line::from(ratatui::text::Span::styled(
            "loop!     3+ consecutive identical failures by one agent on one tool",
            theme.error,
        )));
    }
    if let Some(recipes) = recipes {
        let listed: Vec<String> = recipes.iter().map(|recipe| recipe.to_string()).collect();
        lines.push(Line::from(format!("recipe    {}", listed.join(", "))));
    }
    lines.push(Line::from(""));
    // the signature itself, one source line per row (the pop-out wraps
    // overlong rows at render time)
    for source in sig.lines() {
        lines.push(Line::from(source.to_owned()));
    }
    Popup::lines(" failure signature ", lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sidebar hides on narrow terminals and clamps on wide ones
    #[test]
    fn sidebar_width_hides_and_clamps() {
        assert_eq!(sidebar_width(80), 0);
        assert_eq!(sidebar_width(109), 0);
        // a quarter, floored at 32
        assert_eq!(sidebar_width(110), 32);
        assert_eq!(sidebar_width(160), 40);
        // and capped at 48 however wide the terminal
        assert_eq!(sidebar_width(400), 48);
    }

    /// Centered rectangles clamp to the area instead of overflowing
    #[test]
    fn centered_clamps_to_the_area() {
        let area = Rect::new(0, 0, 50, 10);
        let rect = centered(area, 40, 8);
        assert_eq!((rect.x, rect.y, rect.width, rect.height), (5, 1, 40, 8));
        // a request larger than the area fills it exactly
        let rect = centered(area, 200, 200);
        assert_eq!((rect.width, rect.height), (50, 10));
    }

    /// Pop-outs close on escape and keep scroll keys internal
    #[test]
    fn popups_close_and_scroll() {
        use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};
        let mut popup = Popup::markdown(" help ", "line");
        let key = |code| KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        assert!(matches!(
            popup.handle_key(key(KeyCode::Down)),
            ViewOutcome::None
        ));
        assert!(matches!(
            popup.handle_key(key(KeyCode::Esc)),
            ViewOutcome::Close
        ));
        assert!(matches!(
            popup.handle_key(key(KeyCode::Char('q'))),
            ViewOutcome::Close
        ));
    }
}
