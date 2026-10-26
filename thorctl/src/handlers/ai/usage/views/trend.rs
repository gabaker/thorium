//! The daily trend: per-day bars with a running cumulative spend column

use std::collections::BTreeMap;

use chrono::{Datelike, NaiveDate};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::handlers::ai::chat::fmt_cost;

use super::super::model::{GlobalStats, HeatMetric};
use super::fill_bar;

/// The weekday labels, Monday first
const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// The daily trend's view state
#[derive(Debug)]
pub struct TrendView {
    /// The metric the bars plot
    metric: HeatMetric,
    /// The first visible day's offset from the newest
    scroll: usize,
}

impl Default for TrendView {
    /// Open on cost
    fn default() -> Self {
        TrendView {
            metric: HeatMetric::Cost,
            scroll: 0,
        }
    }
}

impl TrendView {
    /// Handles a key press while the trend is showing
    ///
    /// # Arguments
    ///
    /// * `key` - The key event to handle
    /// * `stats` - The cross-session aggregates
    pub fn handle_key(&mut self, key: KeyEvent, stats: &GlobalStats) {
        match key.code {
            // cycle the plotted metric
            KeyCode::Char('m') | KeyCode::Right => self.metric = self.metric.next(),
            KeyCode::Left => self.metric = self.metric.prev(),
            // scroll through the days (newest first)
            KeyCode::Char('j') | KeyCode::Down => {
                self.scroll = (self.scroll + 1).min(stats.days.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
            _ => {}
        }
    }

    /// Routes a wheel scroll through the days
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

    /// Renders the trend rows
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The area the trend owns
    /// * `stats` - The cross-session aggregates
    pub fn render(&mut self, frame: &mut Frame, area: Rect, stats: &GlobalStats) {
        let dim = Style::default().fg(Color::DarkGray);
        // the running spend sums oldest-to-newest so every row can show its Σ
        let mut cumulative: BTreeMap<NaiveDate, f64> = BTreeMap::new();
        let mut running = 0.0;
        for (day, bucket) in &stats.days {
            running += bucket.cost;
            cumulative.insert(*day, running);
        }
        // normalize the bars against the biggest day of the metric
        let max = stats
            .days
            .values()
            .map(|bucket| self.metric.value(bucket))
            .fold(0.0_f64, f64::max);
        let estimated = if stats.estimated {
            " · ~ estimates"
        } else {
            ""
        };
        let mut lines = vec![Line::from(Span::styled(
            format!(
                " TREND • metric: {}{estimated} (m metric · j/k days)",
                self.metric.label(),
            ),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))];
        // lay the columns out: date label, bar lane, value, cumulative spend
        let value_width = 14;
        let cum_width = 14;
        let bar_width = (area.width as usize)
            .saturating_sub(10 + value_width + cum_width + 6)
            .max(8);
        // newest day first, scrolled and clipped to the area
        let visible = (area.height as usize).saturating_sub(2);
        self.scroll = self.scroll.min(stats.days.len().saturating_sub(1));
        for (day, bucket) in stats.days.iter().rev().skip(self.scroll).take(visible) {
            let value = self.metric.value(bucket);
            let frac = if max > 0.0 { value / max } else { 0.0 };
            let weekday = WEEKDAYS[day.weekday().num_days_from_monday() as usize];
            lines.push(Line::from(vec![
                Span::styled(format!(" {} {weekday} ", day.format("%m-%d")), dim),
                Span::styled(fill_bar(bar_width, frac), Style::default().fg(Color::Cyan)),
                Span::raw(format!(" {:>value_width$}", self.metric.format(value),)),
                Span::styled(
                    format!(
                        " {:>cum_width$}",
                        format!(
                            "Σ {}",
                            fmt_cost(cumulative.get(day).copied().unwrap_or(0.0))
                        ),
                    ),
                    dim,
                ),
            ]));
        }
        // an empty store says why there is nothing to plot
        if stats.days.is_empty() {
            lines.push(Line::from(Span::styled(
                "   no stamped activity to bucket yet",
                dim,
            )));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }
}
