//! The day/hour heatmap: when the spend actually happens
//!
//! A grid of local calendar days (newest first) by hour of day, each cell
//! shaded by the selected metric -- active time, tokens, or cost -- on a
//! square-root ramp so a few huge hours cannot flatten everything else into
//! the lowest shade. `g` collapses the dates into a weekday-by-hour aggregate
//! to expose weekly rhythm.

use chrono::{Datelike, NaiveDate};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::handlers::ai::chat::Theme;

use super::super::model::{GlobalStats, HeatMetric, HourBucket};
use super::clip;

/// The row-label column's width
const LABEL_WIDTH: usize = 11;

/// The weekday labels, Monday first, matching the aggregate grid's rows
const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// The day/hour heatmap's view state
#[derive(Debug)]
pub struct HeatmapView {
    /// The metric the cells shade by
    metric: HeatMetric,
    /// Whether the dates collapse into a weekday aggregate
    weekday: bool,
    /// The first visible date row's offset (dates mode only)
    scroll: usize,
}

impl Default for HeatmapView {
    /// Open on tokens, by date
    fn default() -> Self {
        HeatmapView {
            metric: HeatMetric::Tokens,
            weekday: false,
            scroll: 0,
        }
    }
}

impl HeatmapView {
    /// Handles a key press while the heatmap is showing
    ///
    /// # Arguments
    ///
    /// * `key` - The key event to handle
    /// * `stats` - The cross-session aggregates
    pub fn handle_key(&mut self, key: KeyEvent, stats: &GlobalStats) {
        match key.code {
            // cycle the shaded metric, matching the chat view's keys
            KeyCode::Char('m') | KeyCode::Right => self.metric = self.metric.next(),
            KeyCode::Left => self.metric = self.metric.prev(),
            // toggle the weekday aggregate
            KeyCode::Char('g') => self.weekday = !self.weekday,
            // scroll through the dates (newest first)
            KeyCode::Char('j') | KeyCode::Down => {
                self.scroll = (self.scroll + 1).min(stats.days.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
            _ => {}
        }
    }

    /// Routes a wheel scroll through the dates
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

    /// Renders the heatmap grid
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The area the heatmap owns
    /// * `stats` - The cross-session aggregates
    pub fn render(&mut self, frame: &mut Frame, area: Rect, stats: &GlobalStats) {
        let theme = Theme::default();
        let dim = Style::default().fg(Color::DarkGray);
        // cells are 2 chars wide, with a spacer column on roomy terminals
        let slot = if area.width as usize >= LABEL_WIDTH + 24 * 3 + 4 {
            3
        } else {
            2
        };
        // the dates newest-first (or the fixed weekday rows)
        let dates: Vec<NaiveDate> = stats.days.keys().rev().copied().collect();
        // gather the visible rows' buckets so normalization tracks the screen
        let header_rows = 3usize;
        let visible = (area.height as usize).saturating_sub(header_rows + 1);
        let rows: Vec<(String, [HourBucket; 24])> = if self.weekday {
            // the aggregate's seven fixed rows, Monday first
            WEEKDAYS
                .iter()
                .enumerate()
                .map(|(day, label)| ((*label).to_owned(), stats.weekdays[day]))
                .collect()
        } else {
            // one row per date with any recorded spend, newest first
            self.scroll = self.scroll.min(dates.len().saturating_sub(1));
            dates
                .iter()
                .skip(self.scroll)
                .take(visible.max(1))
                .map(|date| {
                    // pull the date's 24 cells out of the sparse grid
                    let mut cells = [HourBucket::default(); 24];
                    for (hour, cell) in cells.iter_mut().enumerate() {
                        if let Some(bucket) = stats.grid.get(&(*date, hour as u8)) {
                            *cell = *bucket;
                        }
                    }
                    (
                        format!(
                            "{} {}",
                            date.format("%m-%d"),
                            WEEKDAYS[date.weekday().num_days_from_monday() as usize]
                        ),
                        cells,
                    )
                })
                .collect()
        };
        // normalize against the biggest visible cell
        let max = rows
            .iter()
            .flat_map(|(_, cells)| cells.iter())
            .map(|bucket| self.metric.value(bucket))
            .fold(0.0_f64, f64::max);
        // the header carries the metric, the mode, and the estimate marker
        let mode = if self.weekday {
            "by weekday"
        } else {
            "by date"
        };
        let estimated = if stats.estimated {
            " · ~ estimates"
        } else {
            ""
        };
        let mut lines = vec![Line::from(Span::styled(
            format!(
                " HEATMAP • metric: {} • {mode}{estimated} (m metric · g weekday · j/k days)",
                self.metric.label(),
            ),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))];
        // the hour axis labels every third hour, aligned to the cell slots
        let mut axis = " ".repeat(LABEL_WIDTH + 1);
        for hour in 0..24 {
            if hour % 3 == 0 {
                axis.push_str(&format!(
                    "{:<width$}",
                    format!("{hour:02}"),
                    width = slot * 3
                ));
            }
        }
        lines.push(Line::from(Span::styled(axis, dim)));
        // one line per row: the label plus its 24 shaded cells
        for (label, cells) in &rows {
            let mut spans = vec![Span::styled(clip(label, LABEL_WIDTH), dim), Span::raw(" ")];
            let mut row_total = 0.0;
            for cell in cells {
                let value = self.metric.value(cell);
                row_total += value;
                // a square-root ramp keeps mid-size hours distinguishable
                let (glyph, style) = if value <= 0.0 || max <= 0.0 {
                    ("··", dim)
                } else {
                    let intensity = (value / max).sqrt();
                    let step = ((intensity * 5.0).ceil() as usize).clamp(1, 5);
                    ("▇▇", theme.graph.heat_ramp[step - 1])
                };
                spans.push(Span::styled(glyph, style));
                if slot == 3 {
                    spans.push(Span::raw(" "));
                }
            }
            // the row total makes a day comparable at a glance
            spans.push(Span::styled(
                format!("  Σ {}", self.metric.format(row_total)),
                dim,
            ));
            lines.push(Line::from(spans));
        }
        // an empty grid says why instead of rendering a bare axis
        if rows.is_empty() {
            lines.push(Line::from(Span::styled(
                "   no stamped activity to bucket yet",
                dim,
            )));
        }
        // the legend anchors the ramp to the visible maximum
        let mut legend = vec![Span::styled(
            format!(" {} ·· zero  ", clip("", LABEL_WIDTH)),
            dim,
        )];
        for step in 0..5 {
            legend.push(Span::styled("▇▇", theme.graph.heat_ramp[step]));
            legend.push(Span::raw(" "));
        }
        legend.push(Span::styled(
            format!(" sqrt ramp · max {}", self.metric.format(max)),
            dim,
        ));
        lines.push(Line::from(legend));
        frame.render_widget(Paragraph::new(lines), area);
    }
}
