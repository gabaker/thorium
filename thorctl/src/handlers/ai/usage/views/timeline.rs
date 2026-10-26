//! The cross-session timeline: every session's busy stretches on one axis
//!
//! The chat Timeline view's global sibling: one lane per session (expandable
//! to its agents' windows), laid over the same gap-compressed axis the gantt
//! uses -- idle days between sessions collapse to a narrow `⫽` break, so
//! months of occasional work keep readable proportions. Pan and zoom mirror
//! the gantt's keys: `h`/`l` pan, `+`/`-` zoom, `0` re-fits the whole run.

use std::collections::{BTreeMap, BTreeSet};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::handlers::ai::chat::{Theme, TimeMap, fmt_duration, now_ms};

use super::super::model::{GlobalStats, SessionSummary};
use super::clip;

/// The row-label column's width
const LABEL_WIDTH: usize = 24;

/// One row's identity in the visual order
struct RowRef {
    /// The session the row belongs to
    session: String,
    /// The agent the row shows, or `None` for the session's own lane
    agent: Option<String>,
}

/// The cross-session timeline's view state
#[derive(Debug)]
pub struct TimelineView {
    /// The sessions whose agent rows are expanded
    expanded: BTreeSet<String>,
    /// The selected row's index into the visual order
    selected: usize,
    /// The first visible row's index
    scroll: usize,
    /// The window's left edge on the compressed axis
    view_start: u64,
    /// The window's right edge on the compressed axis
    view_end: u64,
    /// Whether the window re-fits the whole run every frame
    auto_fit: bool,
}

impl Default for TimelineView {
    /// Open auto-fit over the whole run
    fn default() -> Self {
        TimelineView {
            expanded: BTreeSet::new(),
            selected: 0,
            scroll: 0,
            view_start: 0,
            view_end: 0,
            auto_fit: true,
        }
    }
}

impl TimelineView {
    /// Builds the visual row order: sessions by first stamp, agents inside
    ///
    /// # Arguments
    ///
    /// * `sessions` - The scanned per-session summaries
    fn rows(&self, sessions: &BTreeMap<String, SessionSummary>) -> Vec<RowRef> {
        // sessions order chronologically; stamp-less ones sink to the bottom
        let mut order: Vec<&SessionSummary> = sessions.values().collect();
        order.sort_by(|a, b| {
            a.first_ts
                .is_none()
                .cmp(&b.first_ts.is_none())
                .then(a.first_ts.cmp(&b.first_ts))
                .then_with(|| a.name.cmp(&b.name))
        });
        let mut rows = Vec::new();
        for session in order {
            rows.push(RowRef {
                session: session.name.clone(),
                agent: None,
            });
            // an expanded session reveals its agents' windows chronologically
            if self.expanded.contains(&session.name) {
                let mut agents: Vec<(&String, &super::super::model::AgentWindow)> =
                    session.agents.iter().collect();
                agents.sort_by(|a, b| {
                    a.1.first_ts
                        .is_none()
                        .cmp(&b.1.first_ts.is_none())
                        .then(a.1.first_ts.cmp(&b.1.first_ts))
                        .then_with(|| a.0.cmp(b.0))
                });
                for (agent, _) in agents {
                    rows.push(RowRef {
                        session: session.name.clone(),
                        agent: Some(agent.clone()),
                    });
                }
            }
        }
        rows
    }

    /// Handles a key press while the timeline is showing
    ///
    /// # Arguments
    ///
    /// * `key` - The key event to handle
    /// * `sessions` - The scanned per-session summaries
    pub fn handle_key(&mut self, key: KeyEvent, sessions: &BTreeMap<String, SessionSummary>) {
        // the window math works on the current span
        let span = self.view_end.saturating_sub(self.view_start).max(1);
        match key.code {
            // move the selection through the rows
            KeyCode::Char('j') | KeyCode::Down => {
                let count = self.rows(sessions).len();
                self.selected = (self.selected + 1).min(count.saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            // enter expands or collapses the selected session's agents
            KeyCode::Enter => {
                if let Some(row) = self.rows(sessions).get(self.selected)
                    && row.agent.is_none()
                {
                    // toggle membership in the expanded set
                    if !self.expanded.remove(&row.session) {
                        self.expanded.insert(row.session.clone());
                    }
                }
            }
            // pan the window by a tenth of its span, pinning auto-fit off
            KeyCode::Char('h') => {
                self.auto_fit = false;
                self.view_start = self.view_start.saturating_sub(span / 10);
                self.view_end = self.view_end.saturating_sub(span / 10);
            }
            KeyCode::Char('l') => {
                self.auto_fit = false;
                self.view_start = self.view_start.saturating_add(span / 10);
                self.view_end = self.view_end.saturating_add(span / 10);
            }
            // zoom around the window's center
            KeyCode::Char('+') | KeyCode::Char('=') => {
                self.auto_fit = false;
                let center = self.view_start + span / 2;
                let half = (span / 4).max(500);
                self.view_start = center.saturating_sub(half);
                self.view_end = center.saturating_add(half);
            }
            KeyCode::Char('-') => {
                self.auto_fit = false;
                let center = self.view_start + span / 2;
                self.view_start = center.saturating_sub(span);
                self.view_end = center.saturating_add(span);
            }
            // re-fit the whole run
            KeyCode::Char('0') => self.auto_fit = true,
            _ => {}
        }
    }

    /// Routes a wheel scroll onto the selection
    ///
    /// # Arguments
    ///
    /// * `up` - Whether the wheel scrolled up
    pub fn handle_scroll(&mut self, up: bool) {
        if up {
            self.selected = self.selected.saturating_sub(1);
        } else {
            self.selected = self.selected.saturating_add(1);
        }
    }

    /// Maps a compressed stamp onto a lane column inside the window
    ///
    /// # Arguments
    ///
    /// * `compressed` - The compressed-axis position to place
    /// * `lane` - The lane's width in columns
    fn col(&self, compressed: u64, lane: usize) -> Option<usize> {
        // positions outside the window simply have no column
        if compressed < self.view_start || compressed > self.view_end {
            return None;
        }
        let span = self.view_end.saturating_sub(self.view_start).max(1);
        let offset = compressed - self.view_start;
        Some(((offset as f64 / span as f64) * lane.saturating_sub(1) as f64).round() as usize)
    }

    /// Renders the timeline
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The area the timeline owns
    /// * `sessions` - The scanned per-session summaries
    /// * `stats` - The cross-session aggregates
    pub fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        sessions: &BTreeMap<String, SessionSummary>,
        stats: &GlobalStats,
    ) {
        let theme = Theme::default();
        let dim = Style::default().fg(Color::DarkGray);
        // the axis compresses the union of every session's busy stretches; it
        // only grows toward now while something is actually live
        let extend = if stats.live_sessions > 0 {
            now_ms()
        } else {
            None
        };
        let map = TimeMap::from_segments(stats.union_activity.clone(), extend);
        let mut lines = vec![Line::from(Span::styled(
            format!(
                " TIMELINE • {} sessions • {} active (↵ expand · h/l pan · +/- zoom · 0 fit)",
                stats.total_sessions,
                fmt_duration(map.total_active()),
            ),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))];
        // nothing stamped yet means nothing to lay out
        if map.is_empty() {
            lines.push(Line::from(Span::styled(
                "   no stamped activity to lay out yet",
                dim,
            )));
            frame.render_widget(Paragraph::new(lines), area);
            return;
        }
        // fit (or keep) the window on the compressed axis
        let run_start = stats
            .union_activity
            .first()
            .map(|(start, _)| map.compress(*start))
            .unwrap_or_default();
        let run_end = map.end().unwrap_or(run_start + 1).max(run_start + 1);
        if self.auto_fit || self.view_end <= self.view_start {
            self.view_start = run_start;
            self.view_end = run_end;
        }
        let lane = (area.width as usize).saturating_sub(LABEL_WIDTH + 2);
        if lane < 8 {
            // a terminal this narrow cannot hold a lane worth drawing
            lines.push(Line::from(Span::styled("   terminal too narrow", dim)));
            frame.render_widget(Paragraph::new(lines), area);
            return;
        }
        // the axis row: collapsed-gap break markers over a baseline
        let mut axis: Vec<char> = vec!['─'; lane];
        for gap in map.gaps() {
            if let Some(col) = self.col(gap.c0, lane) {
                axis[col] = '⫽';
            }
        }
        lines.push(Line::from(vec![
            Span::styled(clip("", LABEL_WIDTH), dim),
            Span::raw(" "),
            Span::styled(axis.into_iter().collect::<String>(), dim),
        ]));
        // the tick row labels cumulative active time from the window's left
        let mut ticks = " ".repeat(LABEL_WIDTH + 1);
        for step in 0..4 {
            let frac = step as f64 / 4.0;
            let span = self.view_end - self.view_start;
            let at = self.view_start + (span as f64 * frac) as u64;
            let label = format!("+{}", fmt_duration(map.active_between(self.view_start, at)));
            // place each label at its fraction of the lane
            let col = LABEL_WIDTH + 1 + (lane as f64 * frac) as usize;
            if col + label.len() < LABEL_WIDTH + 1 + lane {
                while ticks.chars().count() < col {
                    ticks.push(' ');
                }
                ticks.push_str(&label);
            }
        }
        lines.push(Line::from(Span::styled(ticks, dim)));
        // build the rows and keep the selection visible
        let rows = self.rows(sessions);
        self.selected = self.selected.min(rows.len().saturating_sub(1));
        let visible = (area.height as usize).saturating_sub(3);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        }
        if visible > 0 && self.selected >= self.scroll + visible {
            self.scroll = self.selected + 1 - visible;
        }
        // draw each visible row's lane
        for (index, row) in rows
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(visible.max(1))
        {
            let Some(session) = sessions.get(&row.session) else {
                continue;
            };
            // a session lane draws its busy intervals; an agent lane its window
            let (label, intervals, style) = match &row.agent {
                None => {
                    let marker = if self.expanded.contains(&row.session) {
                        "▾ "
                    } else if session.agents.len() > 1 {
                        "▸ "
                    } else {
                        "  "
                    };
                    let dot = if session.live { "●" } else { " " };
                    (
                        format!("{dot}{marker}{}", row.session),
                        session.activity.clone(),
                        // a stable palette slot per session row keeps lanes apart
                        Style::default()
                            .fg(theme.graph.agent_palette[index % theme.graph.agent_palette.len()]),
                    )
                }
                Some(agent) => {
                    let window = session.agents.get(agent);
                    let intervals = window
                        .and_then(|window| window.first_ts.zip(window.last_ts))
                        .map(|(first, last)| vec![(first, last.max(first))])
                        .unwrap_or_default();
                    (format!("    {agent}"), intervals, dim)
                }
            };
            // paint the lane buffer with the row's compressed intervals
            let mut lane_chars: Vec<char> = vec![' '; lane];
            for (start, end) in &intervals {
                let c0 = map.compress(*start);
                let c1 = map.compress(*end).max(c0);
                // skip intervals entirely outside the window instead of
                // letting the clamps smear them onto its edges
                if c1 < self.view_start || c0 > self.view_end {
                    continue;
                }
                // clamp the span into the window before mapping to columns
                let c0 = c0.clamp(self.view_start, self.view_end);
                let c1 = c1.clamp(self.view_start, self.view_end);
                if let (Some(x0), Some(x1)) = (self.col(c0, lane), self.col(c1, lane)) {
                    for cell in lane_chars.iter_mut().take(x1 + 1).skip(x0) {
                        *cell = '█';
                    }
                }
            }
            // the selected row inverts its label for the keyboard's focus
            let label_style = if index == self.selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            lines.push(Line::from(vec![
                Span::styled(clip(&label, LABEL_WIDTH), label_style),
                Span::raw(" "),
                Span::styled(lane_chars.into_iter().collect::<String>(), style),
            ]));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }
}
