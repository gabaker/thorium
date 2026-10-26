//! The sortable sessions table, the drill-down's launch pad

use std::collections::BTreeMap;

use chrono::Local;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::handlers::ai::chat::{fmt_cost, fmt_duration, with_thousands};

use super::super::model::SessionSummary;
use super::{ago, clip};

/// The table's columns: label plus display width
///
/// Ordered so the figures an operator compares most (cost, tokens, activity)
/// survive narrow terminals; the model and binary columns clip first.
const COLUMNS: [(&str, usize); 8] = [
    ("session", 26),
    ("cost", 10),
    ("tokens", 13),
    ("active", 9),
    ("last", 10),
    ("created", 13),
    ("model", 22),
    ("binary", 18),
];

/// The sortable per-session table's view state
#[derive(Debug)]
pub struct SessionsView {
    /// The index of the column rows sort by
    sort: usize,
    /// Whether the sort runs descending
    desc: bool,
    /// The selected session's name
    ///
    /// Identity-keyed on purpose: the scanner re-sorts the table live, so a
    /// positional selection would drift onto a different session between a
    /// keypress and the next frame.
    selected: Option<String>,
    /// The first visible row's index
    scroll: usize,
}

impl Default for SessionsView {
    /// Open sorted by last activity, newest first
    fn default() -> Self {
        SessionsView {
            sort: 4,
            desc: true,
            selected: None,
            scroll: 0,
        }
    }
}

impl SessionsView {
    /// The sessions in the table's current sort order
    ///
    /// # Arguments
    ///
    /// * `sessions` - The scanned per-session summaries
    pub fn order<'a>(
        &self,
        sessions: &'a BTreeMap<String, SessionSummary>,
    ) -> Vec<&'a SessionSummary> {
        let mut order: Vec<&SessionSummary> = sessions.values().collect();
        // sort by the active column; unknown values always sink to the bottom
        order.sort_by(|a, b| {
            let ordering = match self.sort {
                0 => a.name.cmp(&b.name),
                1 => a
                    .cost
                    .unwrap_or(f64::NEG_INFINITY)
                    .total_cmp(&b.cost.unwrap_or(f64::NEG_INFINITY)),
                2 => a.tokens().cmp(&b.tokens()),
                3 => a.active_ms.cmp(&b.active_ms),
                4 => a
                    .last_ts
                    .or(a.created_at.map(|c| c.timestamp_millis().max(0) as u64))
                    .cmp(
                        &b.last_ts
                            .or(b.created_at.map(|c| c.timestamp_millis().max(0) as u64)),
                    ),
                5 => a.created_at.cmp(&b.created_at),
                6 => a.model.cmp(&b.model),
                _ => a.binary.cmp(&b.binary),
            };
            // flip for a descending sort, tie-breaking on the name
            if self.desc {
                ordering.reverse().then_with(|| a.name.cmp(&b.name))
            } else {
                ordering.then_with(|| a.name.cmp(&b.name))
            }
        });
        order
    }

    /// The selected session's current index in a sort order
    ///
    /// Falls back to the top row when nothing is selected yet or the selected
    /// session vanished from the store.
    ///
    /// # Arguments
    ///
    /// * `order` - The sessions in the table's current sort order
    fn selected_index(&self, order: &[&SessionSummary]) -> usize {
        // locate the selected name in the fresh order, defaulting to the top
        self.selected
            .as_deref()
            .and_then(|name| order.iter().position(|session| session.name == name))
            .unwrap_or(0)
    }

    /// Moves the selection by rows, pinning it to the landed session's name
    ///
    /// # Arguments
    ///
    /// * `order` - The sessions in the table's current sort order
    /// * `delta` - How many rows to move, negative for up
    fn step(&mut self, order: &[&SessionSummary], delta: isize) {
        // an empty table has nothing to select
        if order.is_empty() {
            self.selected = None;
            return;
        }
        // step from the current position, clamped into the table
        let index = self
            .selected_index(order)
            .saturating_add_signed(delta)
            .min(order.len() - 1);
        self.selected = Some(order[index].name.clone());
    }

    /// Handles a key press, returning a session name to drill into
    ///
    /// # Arguments
    ///
    /// * `key` - The key event to handle
    /// * `sessions` - The scanned per-session summaries
    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        sessions: &BTreeMap<String, SessionSummary>,
    ) -> Option<String> {
        match key.code {
            // move the selection through the sorted rows
            KeyCode::Char('j') | KeyCode::Down => {
                let order = self.order(sessions);
                self.step(&order, 1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                let order = self.order(sessions);
                self.step(&order, -1);
            }
            KeyCode::Home => {
                self.selected = self.order(sessions).first().map(|s| s.name.clone());
            }
            KeyCode::End => {
                self.selected = self.order(sessions).last().map(|s| s.name.clone());
            }
            // move the sort column; h/l stay free for the timeline's pan habit
            KeyCode::Left => self.sort = self.sort.saturating_sub(1),
            KeyCode::Right => self.sort = (self.sort + 1).min(COLUMNS.len() - 1),
            // flip the sort order
            KeyCode::Char('o') => self.desc = !self.desc,
            // enter drills into the highlighted session by name, so a rescan
            // between the keypress and the frame can never swap the target
            KeyCode::Enter => {
                let order = self.order(sessions);
                let index = self.selected_index(&order);
                return order.get(index).map(|session| session.name.clone());
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
    pub fn handle_scroll(&mut self, up: bool, sessions: &BTreeMap<String, SessionSummary>) {
        // the wheel moves the identity-keyed selection one row at a time
        let order = self.order(sessions);
        self.step(&order, if up { -1 } else { 1 });
    }

    /// Renders the table
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The area the table owns
    /// * `sessions` - The scanned per-session summaries
    pub fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        sessions: &BTreeMap<String, SessionSummary>,
    ) {
        let order = self.order(sessions);
        // resolve the selection to a row, re-pinning it when its session
        // vanished so the highlight never points at thin air
        let selected = self.selected_index(&order);
        self.selected = order.get(selected).map(|session| session.name.clone());
        // keep the selection visible inside the scrolled window
        let rows = area.height.saturating_sub(1) as usize;
        if selected < self.scroll {
            self.scroll = selected;
        }
        if rows > 0 && selected >= self.scroll + rows {
            self.scroll = selected + 1 - rows;
        }
        // the header row carries the sort marker on the active column
        let mut header_spans = vec![Span::raw(" ")];
        for (index, (label, width)) in COLUMNS.iter().enumerate() {
            let marker = if index == self.sort {
                if self.desc { "▼" } else { "▲" }
            } else {
                " "
            };
            header_spans.push(Span::styled(
                clip(&format!("{label}{marker}"), *width),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        let mut lines = vec![Line::from(header_spans)];
        // one row per visible session
        for (index, session) in order.iter().enumerate().skip(self.scroll).take(rows) {
            // live sessions get a green dot in front of their name
            let dot = if session.live { "●" } else { " " };
            let created = session
                .created_at
                .map(|created| {
                    created
                        .with_timezone(&Local)
                        .format("%m-%d %H:%M")
                        .to_string()
                })
                .unwrap_or_else(|| "—".to_owned());
            let cost = session.cost.map(fmt_cost).unwrap_or_else(|| "—".to_owned());
            let text = format!(
                "{}{}{}{}{}{}{}{}",
                clip(&session.name, COLUMNS[0].1),
                clip(&cost, COLUMNS[1].1),
                clip(&with_thousands(session.tokens()), COLUMNS[2].1),
                clip(&fmt_duration(session.active_ms), COLUMNS[3].1),
                clip(&ago(session.last_ts), COLUMNS[4].1),
                clip(&created, COLUMNS[5].1),
                clip(session.model.as_deref().unwrap_or("—"), COLUMNS[6].1),
                clip(session.binary.as_deref().unwrap_or("—"), COLUMNS[7].1),
            );
            // the selected row inverts; live dots stay green either way
            let style = if index == selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            lines.push(Line::from(vec![
                Span::styled(dot, Style::default().fg(Color::Green)),
                Span::styled(text, style),
            ]));
        }
        // an empty store says so instead of rendering a bare header
        if order.is_empty() {
            lines.push(Line::from(Span::styled(
                "   no sessions found under the sessions root yet",
                Style::default().fg(Color::DarkGray),
            )));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }
}
