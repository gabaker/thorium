//! The overview dashboard: windowed totals, cache savings, and top lists

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::handlers::ai::chat::{fmt_cost, fmt_duration, with_thousands};

use super::super::model::GlobalStats;
use super::clip;

/// How many entries each top list shows
const TOP_N: usize = 5;

/// The overview dashboard's view state
#[derive(Debug, Default)]
pub struct OverviewView {
    /// The vertical scroll offset in lines
    scroll: usize,
}

impl OverviewView {
    /// Handles a key press while the overview is showing
    ///
    /// # Arguments
    ///
    /// * `key` - The key event to handle
    pub fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            // scroll the dashboard a line at a time
            KeyCode::Char('j') | KeyCode::Down => self.scroll = self.scroll.saturating_add(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
            _ => {}
        }
    }

    /// Routes a wheel scroll
    ///
    /// # Arguments
    ///
    /// * `up` - Whether the wheel scrolled up
    pub fn handle_scroll(&mut self, up: bool) {
        if up {
            self.scroll = self.scroll.saturating_sub(2);
        } else {
            self.scroll = self.scroll.saturating_add(2);
        }
    }

    /// Renders the dashboard
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The area the dashboard owns
    /// * `stats` - The cross-session aggregates to render
    pub fn render(&mut self, frame: &mut Frame, area: Rect, stats: &GlobalStats) {
        // shared styles: section headers pop, figures stay plain
        let header = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let dim = Style::default().fg(Color::DarkGray);
        let mut lines: Vec<Line> = Vec::new();
        // the windowed totals table leads the dashboard
        lines.push(Line::from(Span::styled(" TOTALS", header)));
        lines.push(Line::from(Span::styled(
            format!(
                "   {}{}{}{}{}",
                clip("window", 10),
                clip("cost", 12),
                clip("tokens", 16),
                clip("active", 12),
                clip("sessions", 10),
            ),
            dim,
        )));
        for window in &stats.windows {
            // each window sums its days' buckets; tokens round for display
            lines.push(Line::from(format!(
                "   {}{}{}{}{}",
                clip(window.label, 10),
                clip(&fmt_cost(window.bucket.cost), 12),
                clip(&with_thousands(window.bucket.tokens().round() as u64), 16),
                clip(&fmt_duration(window.bucket.active_ms), 12),
                clip(&window.sessions.to_string(), 10),
            )));
        }
        lines.push(Line::default());
        // the cache line: savings across priced sessions plus the unpriced count
        lines.push(Line::from(Span::styled(" CACHE", header)));
        let unpriced = if stats.unpriced > 0 {
            format!(" · {} sessions unpriced", stats.unpriced)
        } else {
            String::new()
        };
        lines.push(Line::from(format!(
            "   estimated savings {} across priced sessions{unpriced}",
            fmt_cost(stats.cache_savings),
        )));
        lines.push(Line::default());
        // the three top lists, stacked so narrow terminals still read
        lines.push(Line::from(Span::styled(" TOP MODELS by cost", header)));
        let mut models: Vec<_> = stats.models.iter().collect();
        models.sort_by(|a, b| {
            b.1.cost
                .total_cmp(&a.1.cost)
                .then(b.1.sessions.cmp(&a.1.sessions))
        });
        for (model, rollup) in models.iter().take(TOP_N) {
            lines.push(Line::from(format!(
                "   {} {}  ({} sessions)",
                clip(model, 36),
                clip(&fmt_cost(rollup.cost), 10),
                rollup.sessions,
            )));
        }
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(" TOP AGENTS by cost", header)));
        let mut roles: Vec<_> = stats.roles.iter().collect();
        roles.sort_by(|a, b| {
            b.1.cost
                .total_cmp(&a.1.cost)
                .then(b.1.count.cmp(&a.1.count))
        });
        for (role, rollup) in roles.iter().take(TOP_N) {
            lines.push(Line::from(format!(
                "   {} {}  ({} agents, {} tool calls)",
                clip(role, 24),
                clip(&fmt_cost(rollup.cost), 10),
                rollup.count,
                with_thousands(rollup.tool_calls),
            )));
        }
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(" TOP TOOLS by calls", header)));
        let mut tools: Vec<_> = stats.tools.iter().collect();
        tools.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        for (tool, calls) in tools.iter().take(TOP_N) {
            lines.push(Line::from(format!(
                "   {} {}",
                clip(tool, 28),
                with_thousands(**calls),
            )));
        }
        // the estimate marker explains any ~ figures elsewhere
        if stats.estimated {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "   ~ some sessions predate per-turn usage lines; their hour and day \
                 figures are estimates that sum to exact totals",
                dim,
            )));
        }
        // clamp the scroll to the content and render
        let max_scroll = lines.len().saturating_sub(area.height as usize);
        self.scroll = self.scroll.min(max_scroll);
        let visible: Vec<Line> = lines.into_iter().skip(self.scroll).collect();
        frame.render_widget(Paragraph::new(visible), area);
    }
}
