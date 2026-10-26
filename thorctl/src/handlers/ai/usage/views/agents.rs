//! The global usage bars: the chat Usage view's cross-session sibling
//!
//! One bar per session (or, grouped, per agent role across every session),
//! with the same metric-cycling grammar the chat view uses: `m`/`←`/`→` cycle
//! the metric and `g` regroups. Costs are per-session priced, so the bars stay
//! honest across mixed-model stores.

use std::collections::BTreeMap;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::handlers::ai::chat::{fmt_cost, with_thousands};

use super::super::model::{GlobalStats, SessionSummary, UsageMetric, token_sum};
use super::{clip, fill_bar};

/// The label column's width in the bar list
const LABEL_WIDTH: usize = 28;

/// The right-aligned value column's width in the bar list
const VALUE_WIDTH: usize = 12;

/// How wide the side panel is when the terminal affords one
const SIDE_WIDTH: u16 = 34;

/// One renderable bar row
struct Row {
    /// The row's display label
    label: String,
    /// The metric's value, when it is knowable for this row
    value: Option<f64>,
    /// Whether the row's session is live (sessions mode only)
    live: bool,
}

/// The global usage bars' view state
#[derive(Debug)]
pub struct AgentsView {
    /// The metric the bars plot
    metric: UsageMetric,
    /// Whether rows group by agent role instead of by session
    by_role: bool,
    /// The selected row's label
    ///
    /// Identity-keyed on purpose: the scanner re-sorts the bars live, so a
    /// positional selection would drift onto a different row between a
    /// keypress and the next frame.
    selected: Option<String>,
    /// The first visible row's index
    scroll: usize,
}

impl Default for AgentsView {
    /// Open on cost, by session
    fn default() -> Self {
        AgentsView {
            metric: UsageMetric::Cost,
            by_role: false,
            selected: None,
            scroll: 0,
        }
    }
}

impl AgentsView {
    /// Builds the rows for the current grouping, sorted by value descending
    ///
    /// # Arguments
    ///
    /// * `sessions` - The scanned per-session summaries
    /// * `stats` - The cross-session aggregates
    fn rows(&self, sessions: &BTreeMap<String, SessionSummary>, stats: &GlobalStats) -> Vec<Row> {
        let mut rows: Vec<Row> = if self.by_role {
            // grouped: one bar per agent role, from the global rollups
            stats
                .roles
                .iter()
                .map(|(role, rollup)| Row {
                    label: role.clone(),
                    value: match self.metric {
                        // cost only means something once a window priced
                        UsageMetric::Cost => (rollup.priced > 0).then_some(rollup.cost),
                        // prefer provider totals so inclusive-input providers
                        // never double-count their cache reads
                        UsageMetric::TotalTokens => Some(token_sum(&rollup.totals) as f64),
                        UsageMetric::InputTokens => Some(rollup.totals.input_tokens as f64),
                        UsageMetric::OutputTokens => Some(rollup.totals.output_tokens as f64),
                        UsageMetric::ToolCalls => Some(rollup.tool_calls as f64),
                        // lifetimes and turns are per-session facts, not roles'
                        UsageMetric::ActiveTime | UsageMetric::Turns => None,
                    },
                    live: false,
                })
                .collect()
        } else {
            // ungrouped: one bar per session
            sessions
                .values()
                .map(|session| Row {
                    label: session.name.clone(),
                    value: self.metric.value(session),
                    live: session.live,
                })
                .collect()
        };
        // biggest first; unknown values sink to the bottom alphabetically
        rows.sort_by(|a, b| {
            b.value
                .unwrap_or(f64::NEG_INFINITY)
                .total_cmp(&a.value.unwrap_or(f64::NEG_INFINITY))
                .then_with(|| a.label.cmp(&b.label))
        });
        rows
    }

    /// Handles a key press, returning a session name to drill into
    ///
    /// # Arguments
    ///
    /// * `key` - The key event to handle
    /// * `sessions` - The scanned per-session summaries
    /// * `stats` - The cross-session aggregates
    /// The selected row's current index in a row list
    ///
    /// Falls back to the top row when nothing is selected yet or the selected
    /// row vanished from the data.
    ///
    /// # Arguments
    ///
    /// * `rows` - The freshly built rows in display order
    fn selected_index(&self, rows: &[Row]) -> usize {
        // locate the selected label in the fresh rows, defaulting to the top
        self.selected
            .as_deref()
            .and_then(|label| rows.iter().position(|row| row.label == label))
            .unwrap_or(0)
    }

    /// Moves the selection by rows, pinning it to the landed row's label
    ///
    /// # Arguments
    ///
    /// * `rows` - The freshly built rows in display order
    /// * `delta` - How many rows to move, negative for up
    fn step(&mut self, rows: &[Row], delta: isize) {
        // an empty list has nothing to select
        if rows.is_empty() {
            self.selected = None;
            return;
        }
        // step from the current position, clamped into the list
        let index = self
            .selected_index(rows)
            .saturating_add_signed(delta)
            .min(rows.len() - 1);
        self.selected = Some(rows[index].label.clone());
    }

    /// Handles a key press, returning a session name to drill into
    ///
    /// # Arguments
    ///
    /// * `key` - The key event to handle
    /// * `sessions` - The scanned per-session summaries
    /// * `stats` - The cross-session aggregates
    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        sessions: &BTreeMap<String, SessionSummary>,
        stats: &GlobalStats,
    ) -> Option<String> {
        match key.code {
            // cycle the plotted metric, matching the chat view's keys
            KeyCode::Char('m') | KeyCode::Right => self.metric = self.metric.next(),
            KeyCode::Left => self.metric = self.metric.prev(),
            // regroup between sessions and roles
            KeyCode::Char('g') => {
                self.by_role = !self.by_role;
                self.selected = None;
                self.scroll = 0;
            }
            // move the selection through the bars
            KeyCode::Char('j') | KeyCode::Down => {
                let rows = self.rows(sessions, stats);
                self.step(&rows, 1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let rows = self.rows(sessions, stats);
                self.step(&rows, -1);
            }
            // enter drills into the highlighted session by name (sessions
            // mode only), so a live re-sort can never swap the target
            KeyCode::Enter => {
                if !self.by_role {
                    let rows = self.rows(sessions, stats);
                    let index = self.selected_index(&rows);
                    return rows.get(index).map(|row| row.label.clone());
                }
            }
            _ => {}
        }
        None
    }

    /// Routes a wheel scroll onto the selection
    ///
    /// # Arguments
    ///
    /// * `up` - Whether the wheel scrolled up
    /// * `sessions` - The scanned per-session summaries
    /// * `stats` - The cross-session aggregates
    pub fn handle_scroll(
        &mut self,
        up: bool,
        sessions: &BTreeMap<String, SessionSummary>,
        stats: &GlobalStats,
    ) {
        // the wheel moves the identity-keyed selection one row at a time
        let rows = self.rows(sessions, stats);
        self.step(&rows, if up { -1 } else { 1 });
    }

    /// Renders the bars and, when the terminal affords it, the side panel
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The area the view owns
    /// * `sessions` - The scanned per-session summaries
    /// * `stats` - The cross-session aggregates
    pub fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        sessions: &BTreeMap<String, SessionSummary>,
        stats: &GlobalStats,
    ) {
        // split a side panel off wide terminals
        let (list_area, side_area) = if area.width >= LABEL_WIDTH as u16 + 30 + SIDE_WIDTH {
            let [list, side] =
                Layout::horizontal([Constraint::Min(0), Constraint::Length(SIDE_WIDTH)])
                    .areas(area);
            (list, Some(side))
        } else {
            (area, None)
        };
        let rows = self.rows(sessions, stats);
        // resolve the selection to a row, re-pinning it when its row vanished
        let selected = self.selected_index(&rows);
        self.selected = rows.get(selected).map(|row| row.label.clone());
        // keep the selection visible inside the scrolled window
        let visible = list_area.height.saturating_sub(1) as usize;
        if selected < self.scroll {
            self.scroll = selected;
        }
        if visible > 0 && selected >= self.scroll + visible {
            self.scroll = selected + 1 - visible;
        }
        // the header mirrors the chat usage view's grammar
        let grouping = if self.by_role {
            "by role"
        } else {
            "by session"
        };
        let mut lines = vec![Line::from(Span::styled(
            format!(
                " USAGE • metric: {} • {} rows • {grouping} (m metric · g group · ↵ open)",
                self.metric.label(),
                rows.len(),
            ),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))];
        // bars normalize against the biggest visible value
        let max = rows
            .iter()
            .filter_map(|row| row.value)
            .fold(0.0_f64, f64::max);
        let bar_width = (list_area.width as usize)
            .saturating_sub(LABEL_WIDTH + VALUE_WIDTH + 4)
            .max(8);
        for (index, row) in rows.iter().enumerate().skip(self.scroll).take(visible) {
            // unknown values render a dash instead of a zero-length bar
            let (bar, value) = match row.value {
                Some(value) => {
                    let frac = if max > 0.0 { value / max } else { 0.0 };
                    (fill_bar(bar_width, frac), self.metric.format(value))
                }
                None => (" ".repeat(bar_width), "—".to_owned()),
            };
            let dot = if row.live { "●" } else { " " };
            let style = if index == selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            lines.push(Line::from(vec![
                Span::styled(dot, Style::default().fg(Color::Green)),
                Span::styled(clip(&row.label, LABEL_WIDTH), style),
                Span::raw(" "),
                Span::styled(bar, Style::default().fg(Color::Cyan)),
                Span::styled(format!(" {value:>VALUE_WIDTH$}"), style),
            ]));
        }
        frame.render_widget(Paragraph::new(lines), list_area);
        // the side panel: global totals plus the top tools
        if let Some(side) = side_area {
            let dim = Style::default().fg(Color::DarkGray);
            let mut side_lines = vec![Line::from(Span::styled(
                " TOTALS (all time)",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ))];
            // the all-time rollup bucket carries every figure the panel needs
            let totals = &stats.totals;
            side_lines.push(Line::from(format!("   cost     {}", fmt_cost(totals.cost))));
            side_lines.push(Line::from(format!(
                "   input    {}",
                with_thousands(totals.input.round() as u64)
            )));
            side_lines.push(Line::from(format!(
                "   output   {}",
                with_thousands(totals.output.round() as u64)
            )));
            side_lines.push(Line::from(format!(
                "   cached   {}",
                with_thousands(totals.cached.round() as u64)
            )));
            side_lines.push(Line::from(format!(
                "   cache-w  {}",
                with_thousands(totals.cache_creation.round() as u64)
            )));
            side_lines.push(Line::from(format!(
                "   savings  {}",
                fmt_cost(stats.cache_savings)
            )));
            side_lines.push(Line::default());
            side_lines.push(Line::from(Span::styled(
                " TOP TOOLS",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
            // the busiest tools across every session
            let mut tools: Vec<_> = stats.tools.iter().collect();
            tools.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            for (tool, calls) in tools.iter().take(10) {
                side_lines.push(Line::from(vec![
                    Span::raw(format!("   {} ", clip(tool, 20))),
                    Span::styled(with_thousands(**calls), dim),
                ]));
            }
            frame.render_widget(Paragraph::new(side_lines), side);
        }
    }
}
