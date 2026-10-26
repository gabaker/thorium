//! The single-session drill-down: the chat TUI's own Usage and Timeline views
//!
//! Enter on a session rebuilds that one session's [`AgentStatsView`] and
//! [`GanttView`] -- the exact components the chat TUI renders -- from its
//! snapshot and transcript, the same way a resume does: replay the transcript
//! through the shared folds, then seed only the ledger remainder the replayed
//! usage lines did not cover. One session is loaded at a time and dropped on
//! exit, so drilling into a huge session never accumulates.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::handlers::ai::chat::{
    AgentStatsLog, AgentStatsView, ChatContext, GanttLog, GanttView, SideRegistry, TraversalLog,
    ViewOutcome, replay_transcript, replayed_usage, usage_remainder,
};

use super::scan::{PricingCtx, read_lean_snapshot, read_transcript_from, session_cost_model};

/// Which of the drill's two inner views is showing
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrillTab {
    /// The per-agent usage stats view
    Stats,
    /// The gantt timeline view
    Gantt,
}

/// One session's loaded drill-down state
pub struct SessionDrill {
    /// The session this drill inspects
    pub name: String,
    /// Which inner view is showing
    pub which: DrillTab,
    /// The chat TUI's per-agent usage view over this session
    pub stats: AgentStatsView,
    /// The chat TUI's gantt timeline view over this session
    pub gantt: GanttView,
}

impl SessionDrill {
    /// Handles a key press while the drill is open
    ///
    /// `u`/`t` hop between the two inner views; everything else routes to the
    /// showing view, whose own close (q/Esc) closes the whole drill.
    ///
    /// # Arguments
    ///
    /// * `key` - The key event to handle
    pub fn handle_key(&mut self, key: KeyEvent) -> ViewOutcome {
        match key.code {
            // hop to the usage stats view
            KeyCode::Char('u') => {
                self.which = DrillTab::Stats;
                ViewOutcome::None
            }
            // hop to the gantt timeline view
            KeyCode::Char('t') => {
                self.which = DrillTab::Gantt;
                ViewOutcome::None
            }
            // everything else belongs to the showing view
            _ => match self.which {
                DrillTab::Stats => self.stats.handle_key(key),
                DrillTab::Gantt => self.gantt.handle_key(key),
            },
        }
    }

    /// Routes a scroll event to the showing view
    ///
    /// # Arguments
    ///
    /// * `up` - Whether the wheel scrolled up
    pub fn handle_scroll(&mut self, up: bool) {
        match self.which {
            DrillTab::Stats => self.stats.handle_scroll(up),
            DrillTab::Gantt => self.gantt.handle_scroll(up),
        }
    }

    /// Routes a click to the showing view
    ///
    /// # Arguments
    ///
    /// * `x` - The click's column
    /// * `y` - The click's row
    pub fn handle_click(&mut self, x: u16, y: u16) {
        match self.which {
            DrillTab::Stats => self.stats.handle_click(x, y),
            DrillTab::Gantt => self.gantt.handle_click(x, y),
        }
    }

    /// Renders the drill: a one-line banner plus the showing inner view
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The area the drill owns
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        // split a banner line off the top of the drill's area
        let [banner, body] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
        // name the session and the two hop keys, highlighting the active one
        let active = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let idle = Style::default().fg(Color::DarkGray);
        let (stats_style, gantt_style) = match self.which {
            DrillTab::Stats => (active, idle),
            DrillTab::Gantt => (idle, active),
        };
        let line = Line::from(vec![
            Span::styled(
                format!(" session {} ", self.name),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled("• [u]sage ", stats_style),
            Span::styled("• [t]imeline ", gantt_style),
            Span::styled("• esc back", idle),
        ]);
        frame.render_widget(Paragraph::new(line), banner);
        // the showing view owns the rest of the area
        match self.which {
            DrillTab::Stats => self.stats.render(frame, body),
            DrillTab::Gantt => self.gantt.render(frame, body),
        }
    }
}

/// Loads one session's drill-down off disk, read-only
///
/// Mirrors the chat TUI's resume path exactly: lean snapshot + full lenient
/// transcript read, replay through the shared folds (which also rebuilds the
/// chat context the stats view's Enter drill filters), then seed only the
/// ledger remainder so replayed usage lines never double-count.
///
/// # Arguments
///
/// * `root` - The sessions root the session lives under
/// * `name` - The session to load
/// * `pricing` - The config's best-effort pricing context
#[tracing::instrument(name = "usage::load_drill", skip_all, err(Debug))]
pub async fn load_drill(
    root: PathBuf,
    name: String,
    pricing: PricingCtx,
) -> Result<SessionDrill, String> {
    let dir = root.join(&name);
    // read the lean snapshot (ledger + provider) and the whole transcript
    let lean = read_lean_snapshot(&dir.join("session.json")).await;
    let (transcript, _) = read_transcript_from(&dir.join("transcript.jsonl"), 0)
        .await
        .map_err(|err| format!("failed to read the transcript: {err}"))?;
    // a session with neither a snapshot nor any transcript has nothing to show
    if lean.is_none() && transcript.is_empty() {
        return Err("the session has no snapshot or transcript yet".to_owned());
    }
    // sum the replayed usage lines before the replay consumes the transcript
    let replayed = replayed_usage(&transcript);
    // fresh shared state for the inner views; the context and side registry
    // let the stats view's Enter drill show the real conversation
    let context = ChatContext::default();
    let sides = SideRegistry::default();
    let traversal = TraversalLog::default();
    let stats_log = AgentStatsLog::default();
    let gantt_log = GanttLog::default();
    // replay the transcript through the same folds a resume runs; the drill
    // has no live session, so no delegation set (families (a)/(b) of the
    // resume-superseded debris still hide) and the open-call carry is unused
    let _ = replay_transcript(
        transcript,
        &context,
        &sides,
        &traversal,
        &stats_log,
        &gantt_log,
        &std::collections::BTreeSet::new(),
    );
    // seed only what the replayed lines did not cover
    let ledger = lean
        .as_ref()
        .map(|lean| lean.usage.clone())
        .unwrap_or_default();
    let remainder = usage_remainder(&ledger, &replayed);
    stats_log.seed_usage(&remainder);
    gantt_log.seed_usage(&remainder);
    // price with this session's own model, exactly like a resume would
    let cost = session_cost_model(
        lean.as_ref().and_then(|lean| lean.provider.as_ref()),
        &pricing,
    );
    // build both chat views opened, starting on the usage stats
    let mut stats = AgentStatsView::new(stats_log, context, cost);
    stats.toggle();
    let mut gantt = GanttView::new(gantt_log, cost);
    gantt.toggle();
    Ok(SessionDrill {
        name,
        which: DrillTab::Stats,
        stats,
        gantt,
    })
}
