//! The per-model breakdown: token mix, cost share, and cache efficiency
//!
//! One row per model id seen across the store, costliest first. The cache hit
//! rate is `cached / (cached + input)` -- exact under Anthropic/Bedrock cache
//! semantics (where `input` is the uncached remainder) and a slight
//! underestimate under the OpenAI protocol (whose reported input already
//! includes cache reads).

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::handlers::ai::chat::{fmt_cost, with_thousands};

use super::super::model::{GlobalStats, ModelRollup, token_sum};
use super::clip;

/// The table's columns: label plus display width
const COLUMNS: [(&str, usize); 8] = [
    ("model", 32),
    ("sessions", 10),
    ("input", 14),
    ("output", 12),
    ("cached", 14),
    ("cost", 11),
    ("$/MTok", 9),
    ("hit%", 6),
];

/// The per-model breakdown's view state
#[derive(Debug, Default)]
pub struct ModelsView {
    /// The first visible row's offset
    scroll: usize,
}

impl ModelsView {
    /// Handles a key press while the breakdown is showing
    ///
    /// # Arguments
    ///
    /// * `key` - The key event to handle
    /// * `stats` - The cross-session aggregates
    pub fn handle_key(&mut self, key: KeyEvent, stats: &GlobalStats) {
        match key.code {
            // scroll through the model rows
            KeyCode::Char('j') | KeyCode::Down => {
                self.scroll = (self.scroll + 1).min(stats.models.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
            _ => {}
        }
    }

    /// Routes a wheel scroll through the rows
    ///
    /// # Arguments
    ///
    /// * `up` - Whether the wheel scrolled up
    pub fn handle_scroll(&mut self, up: bool) {
        if up {
            self.scroll = self.scroll.saturating_sub(1);
        } else {
            self.scroll = self.scroll.saturating_add(1);
        }
    }

    /// The effective dollars per million tokens a model actually cost
    ///
    /// # Arguments
    ///
    /// * `rollup` - The model rollup to rate
    fn effective_rate(rollup: &ModelRollup) -> Option<f64> {
        // only a priced rollup with real traffic has a meaningful rate; the
        // provider-total-first sum keeps inclusive-input providers honest
        let tokens = token_sum(&rollup.totals);
        if rollup.priced == 0 || tokens == 0 {
            return None;
        }
        Some(rollup.cost / tokens as f64 * 1_000_000.0)
    }

    /// Renders the breakdown table
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The area the table owns
    /// * `stats` - The cross-session aggregates
    pub fn render(&mut self, frame: &mut Frame, area: Rect, stats: &GlobalStats) {
        let dim = Style::default().fg(Color::DarkGray);
        // costliest models first, then the busiest
        let mut models: Vec<_> = stats.models.iter().collect();
        models.sort_by(|a, b| {
            b.1.cost
                .total_cmp(&a.1.cost)
                .then(b.1.sessions.cmp(&a.1.sessions))
                .then(a.0.cmp(b.0))
        });
        let mut lines = vec![Line::from(Span::styled(
            format!(" MODELS • {} seen • by cost", models.len()),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))];
        // the column header row
        let header: String = COLUMNS
            .iter()
            .map(|(label, width)| clip(label, *width))
            .collect();
        lines.push(Line::from(Span::styled(format!(" {header}"), dim)));
        // one row per model, scrolled to the area
        let visible = (area.height as usize).saturating_sub(3);
        self.scroll = self.scroll.min(models.len().saturating_sub(1));
        for (model, rollup) in models.iter().skip(self.scroll).take(visible.max(1)) {
            // unpriced rollups thin their dollar columns instead of lying
            let cost = if rollup.priced > 0 {
                fmt_cost(rollup.cost)
            } else {
                "—".to_owned()
            };
            let rate = Self::effective_rate(rollup)
                .map(|rate| format!("{rate:.2}"))
                .unwrap_or_else(|| "—".to_owned());
            // the cache hit rate of this model's input traffic
            let reads = rollup.totals.cached_input_tokens;
            let hit = if reads > 0 || rollup.totals.input_tokens > 0 {
                format!(
                    "{:.0}%",
                    reads as f64 / (reads + rollup.totals.input_tokens).max(1) as f64 * 100.0,
                )
            } else {
                "—".to_owned()
            };
            lines.push(Line::from(format!(
                " {}{}{}{}{}{}{}{}",
                clip(model, COLUMNS[0].1),
                clip(&rollup.sessions.to_string(), COLUMNS[1].1),
                clip(&with_thousands(rollup.totals.input_tokens), COLUMNS[2].1),
                clip(&with_thousands(rollup.totals.output_tokens), COLUMNS[3].1),
                clip(
                    &with_thousands(rollup.totals.cached_input_tokens),
                    COLUMNS[4].1
                ),
                clip(&cost, COLUMNS[5].1),
                clip(&rate, COLUMNS[6].1),
                clip(&hit, COLUMNS[7].1),
            )));
        }
        // the savings footer puts the cache columns in dollars
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            format!(
                "   cache savings {} across priced sessions",
                fmt_cost(stats.cache_savings)
            ),
            dim,
        )));
        // an empty store says why the table is bare
        if models.is_empty() {
            lines.push(Line::from(Span::styled("   no sessions scanned yet", dim)));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }
}
