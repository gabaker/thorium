//! The full-screen gantt timeline overlay: spans, expansion, and call detail
//!
//! Renders the reduced timeline as a Jaeger-style trace: one row per swarm,
//! agent, or tool call, each with a horizontal bar sized by duration over a
//! pannable, zoomable time window that auto-fits the whole run until the user
//! moves it. Rows expand hierarchically (a swarm into its member agents, an
//! agent into its tool calls) via Enter or a mouse click, and each row carries
//! a preview of counts, goals or arguments, tokens, and estimated cost.
//! Selecting a tool call opens a bottom detail panel showing its full argument
//! and result payloads. Per-call token and cost figures are chars/4 estimates
//! and render `~`-prefixed; per-agent figures are exact.
//!
//! The window lives on the gap-compressed axis of a per-frame [`TimeMap`]:
//! idle downtime collapses to a narrow break marker, the axis ticks count
//! cumulative worked time, and the right edge only tracks the wall clock
//! while work is genuinely open — so a resumed session reviewed later stays
//! frozen in time with its real proportions.

use std::collections::BTreeSet;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use crate::handlers::ai::pricing::CostModel;

use super::super::agent_stats::{fmt_cost, fmt_duration, now_ms};
use super::super::status_bar::with_thousands;
use super::super::theme::Theme;
use super::super::traversal::ViewOutcome;
use super::log::GanttLog;
use super::state::{AgentSpan, GanttRow, GanttState, RowKey, ToolSpan, build_rows};
use super::timemap::TimeMap;

/// The columns the row labels (indent, glyphs, and name) occupy
const LABEL_WIDTH: usize = 28;

/// The columns a preview needs before it is worth drawing at all
const MIN_PREVIEW_WIDTH: usize = 16;

/// The shortest time window the view will zoom into, in milliseconds
const MIN_SPAN_MS: u64 = 1_000;

/// The glyph a collapsed idle gap renders as, on the axis and through bars
///
/// Must be a single-column character; if `⫽` proves ambiguous-width in some
/// terminal font, swap it for the box-drawing `┊`, which is guaranteed narrow.
const BREAK_GLYPH: char = '⫽';

/// The open detail panel for one selected tool call
struct Detail {
    /// The call id of the tool call being inspected
    call_id: String,
    /// How many lines the panel's text is scrolled down
    scroll: u16,
}

/// The full-screen gantt timeline overlay
pub struct GanttView {
    /// Whether the overlay is currently shown
    pub open: bool,
    /// The shared gantt log this view renders
    log: GanttLog,
    /// The cost estimator for the session's model, when it is priceable
    cost: Option<CostModel>,
    /// The keys of the rows that are currently expanded
    expanded: BTreeSet<RowKey>,
    /// The selected row, when one is selected
    selected: Option<RowKey>,
    /// The open tool-call detail panel, when a call was entered
    detail: Option<Detail>,
    /// The row list's scroll offset in rows
    scroll: usize,
    /// Whether the next render should scroll the selection into view
    follow_selection: bool,
    /// The left edge of the time window, in milliseconds since the epoch
    view_start: u64,
    /// The right edge of the time window, in milliseconds since the epoch
    view_end: u64,
    /// Whether the window re-fits the whole run every frame
    auto_fit: bool,
    /// The rows of the last frame, for click mapping and selection cycling
    last_rows: Vec<GanttRow>,
    /// The row list's screen area of the last frame, for click mapping
    body_area: Rect,
    /// The theme supplying the view's styles
    theme: Theme,
}

impl GanttView {
    /// Creates a view over a shared gantt log
    ///
    /// # Arguments
    ///
    /// * `log` - The shared gantt log to render
    /// * `cost` - The session's cost estimator, when its model is priceable
    pub fn new(log: GanttLog, cost: Option<CostModel>) -> Self {
        GanttView {
            open: false,
            log,
            cost,
            expanded: BTreeSet::new(),
            selected: None,
            detail: None,
            scroll: 0,
            follow_selection: false,
            view_start: 0,
            view_end: 0,
            auto_fit: true,
            last_rows: Vec::new(),
            body_area: Rect::default(),
            theme: Theme::default(),
        }
    }

    /// Opens or closes the overlay
    pub fn toggle(&mut self) {
        self.open = !self.open;
        // a closed view drops its selection, detail, and scroll, and the
        // window re-fits the whole run on the next open
        if !self.open {
            self.selected = None;
            self.detail = None;
            self.scroll = 0;
            self.auto_fit = true;
        }
    }

    /// Handles a key press while the overlay is open
    ///
    /// # Arguments
    ///
    /// * `key` - The key event to handle
    pub fn handle_key(&mut self, key: KeyEvent) -> ViewOutcome {
        // an open detail panel owns the keys until Esc backs out of it
        if let Some(detail) = &mut self.detail {
            match key.code {
                // back out to the timeline, keeping the selection
                KeyCode::Esc => self.detail = None,
                // close the whole overlay
                KeyCode::Char('q') => return ViewOutcome::Close,
                // scroll the detail text
                KeyCode::Up | KeyCode::Char('k') => detail.scroll = detail.scroll.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    detail.scroll = detail.scroll.saturating_add(1);
                }
                KeyCode::PageUp => detail.scroll = detail.scroll.saturating_sub(10),
                KeyCode::PageDown => detail.scroll = detail.scroll.saturating_add(10),
                _ => (),
            }
            return ViewOutcome::None;
        }
        match key.code {
            // Esc clears a selection first; with none it closes like q
            KeyCode::Esc => {
                if self.selected.is_some() {
                    self.selected = None;
                } else {
                    return ViewOutcome::Close;
                }
            }
            // close the overlay
            KeyCode::Char('q') => return ViewOutcome::Close,
            // cycle the selection through the visual row order
            KeyCode::Tab | KeyCode::Down | KeyCode::Char('j') => self.cycle_selection(1),
            KeyCode::BackTab | KeyCode::Up | KeyCode::Char('k') => self.cycle_selection(-1),
            // expand/collapse the selected row, or open a call's detail
            KeyCode::Enter => self.enter_selection(),
            // pan the time window sideways
            KeyCode::Left | KeyCode::Char('h') => self.pan(true),
            KeyCode::Right | KeyCode::Char('l') => self.pan(false),
            // zoom the time window around its center
            KeyCode::Char('+') | KeyCode::Char('=') => self.zoom(true),
            KeyCode::Char('-') | KeyCode::Char('_') => self.zoom(false),
            // re-fit the window to the whole run
            KeyCode::Char('0') => self.auto_fit = true,
            // page the row list
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(10),
            _ => (),
        }
        ViewOutcome::None
    }

    /// Scrolls the view from a mouse wheel event
    ///
    /// # Arguments
    ///
    /// * `up` - Whether the wheel scrolled up
    pub fn handle_scroll(&mut self, up: bool) {
        // the wheel scrolls whichever pane is showing
        match &mut self.detail {
            Some(detail) => {
                if up {
                    detail.scroll = detail.scroll.saturating_sub(3);
                } else {
                    detail.scroll = detail.scroll.saturating_add(3);
                }
            }
            None => {
                // manual scrolling moves the window without touching selection
                if up {
                    self.scroll = self.scroll.saturating_sub(3);
                } else {
                    self.scroll = self.scroll.saturating_add(3);
                }
            }
        }
    }

    /// Pans the time window from a horizontal mouse wheel event
    ///
    /// # Arguments
    ///
    /// * `left` - Whether the wheel scrolled left
    pub fn handle_pan(&mut self, left: bool) {
        self.pan(left);
    }

    /// Handles a mouse click while the overlay is open
    ///
    /// A click selects its row; an expandable row also toggles open or
    /// closed, and a tool-call row also opens its detail panel.
    ///
    /// # Arguments
    ///
    /// * `x` - The column position of the click
    /// * `y` - The row position of the click
    pub fn handle_click(&mut self, x: u16, y: u16) {
        // an open detail panel ignores clicks (Esc backs out of it)
        if self.detail.is_some() {
            return;
        }
        // only clicks inside the row list land on a row
        if !self.body_area.contains((x, y).into()) {
            return;
        }
        // map the click's screen row onto the scrolled row list
        let index = self.scroll + usize::from(y - self.body_area.y);
        let Some(row) = self.last_rows.get(index) else {
            return;
        };
        // select the clicked row, then act on what it is
        self.selected = Some(row.key.clone());
        if row.expandable {
            // an expandable row toggles its subtree
            self.toggle_expanded(&row.key.clone());
        } else if let RowKey::Call(call_id) = &row.key {
            // a tool call opens its detail panel
            self.detail = Some(Detail {
                call_id: call_id.clone(),
                scroll: 0,
            });
        }
    }

    /// Moves the selection one step through the visual row order
    ///
    /// # Arguments
    ///
    /// * `step` - The direction to move: positive forward, negative backward
    fn cycle_selection(&mut self, step: isize) {
        // an empty list has nothing to select
        if self.last_rows.is_empty() {
            return;
        }
        // find where the current selection sits in the visual order
        let current = self
            .selected
            .as_ref()
            .and_then(|key| self.last_rows.iter().position(|row| &row.key == key));
        // step from the current slot, or start at an end when none is selected
        let next = match current {
            Some(index) => {
                let len = self.last_rows.len() as isize;
                (index as isize + step).rem_euclid(len) as usize
            }
            None if step < 0 => self.last_rows.len() - 1,
            None => 0,
        };
        self.selected = Some(self.last_rows[next].key.clone());
        // the next render scrolls the fresh selection into view
        self.follow_selection = true;
    }

    /// Acts on the selected row: toggle its subtree or open a call's detail
    fn enter_selection(&mut self) {
        // Enter with no selection is a no-op
        let Some(selected) = self.selected.clone() else {
            return;
        };
        match &selected {
            // swarms and agents expand or collapse their subtree
            RowKey::Swarm(_) | RowKey::Agent(_) => self.toggle_expanded(&selected),
            // a tool call opens its detail panel
            RowKey::Call(call_id) => {
                self.detail = Some(Detail {
                    call_id: call_id.clone(),
                    scroll: 0,
                });
            }
        }
    }

    /// Toggles a row's membership in the expanded set
    ///
    /// # Arguments
    ///
    /// * `key` - The key of the row to expand or collapse
    fn toggle_expanded(&mut self, key: &RowKey) {
        // flip the key in or out of the expanded set
        if !self.expanded.remove(key) {
            self.expanded.insert(key.clone());
        }
    }

    /// Pans the time window sideways by 15% of its span
    ///
    /// # Arguments
    ///
    /// * `left` - Whether to pan toward earlier times
    fn pan(&mut self, left: bool) {
        // panning takes manual control of the window
        self.auto_fit = false;
        let span = self
            .view_end
            .saturating_sub(self.view_start)
            .max(MIN_SPAN_MS);
        let shift = (span / 100).saturating_mul(15).max(1);
        if left {
            self.view_start = self.view_start.saturating_sub(shift);
            self.view_end = self.view_end.saturating_sub(shift);
        } else {
            self.view_start = self.view_start.saturating_add(shift);
            self.view_end = self.view_end.saturating_add(shift);
        }
    }

    /// Zooms the time window around its center
    ///
    /// # Arguments
    ///
    /// * `in_` - Whether to zoom in (shrink the span)
    fn zoom(&mut self, in_: bool) {
        // zooming takes manual control of the window
        self.auto_fit = false;
        let span = self
            .view_end
            .saturating_sub(self.view_start)
            .max(MIN_SPAN_MS);
        // shrink to 4/5 or grow to 5/4 of the current span
        let new_span = if in_ {
            ((span / 5).saturating_mul(4)).max(MIN_SPAN_MS)
        } else {
            (span / 4).saturating_mul(5).max(span.saturating_add(1))
        };
        // keep the same center while the span changes
        let center = self.view_start.saturating_add(span / 2);
        self.view_start = center.saturating_sub(new_span / 2);
        self.view_end = self.view_start.saturating_add(new_span);
    }

    /// Renders the overlay over the whole given area
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render to
    /// * `area` - The full area the overlay owns
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        // the overlay owns the whole screen while it is open
        frame.render_widget(Clear, area);
        // snapshot the live timeline once per frame
        let state = self.log.snapshot();
        let now = now_ms();
        // build the frame's gap-compressed time axis from the recorded activity
        let map = TimeMap::build(&state, now);
        // rebuild the visible rows for the current expansion state
        self.last_rows = build_rows(&state, &self.expanded, now);
        // drop a selection the current layout no longer contains
        if let Some(selected) = &self.selected
            && !self.last_rows.iter().any(|row| &row.key == selected)
        {
            self.selected = None;
            self.detail = None;
        }
        // a detail panel for a call the layout dropped closes with it
        if let Some(detail) = &self.detail
            && !state
                .calls
                .iter()
                .any(|call| call.call_id == detail.call_id)
        {
            self.detail = None;
        }
        // while auto-fitting, the window spans the whole mapped run; the right
        // edge only tracks the clock while work is open, so a resumed session
        // with nothing running stays frozen at its last stamp
        if self.auto_fit
            && let (Some(min), Some(max)) = (state.min_ts, state.max_ts)
        {
            self.view_start = map.compress(min);
            self.view_end = map.end().unwrap_or_else(|| map.compress(max));
        }
        // guard the window against a degenerate or inverted span
        if self.view_end <= self.view_start {
            self.view_end = self.view_start.saturating_add(MIN_SPAN_MS);
        }
        // split off the header, time axis, detail panel, and footer
        let detail_height = if self.detail.is_some() {
            // the detail takes two fifths of the screen, within sane bounds;
            // a tiny terminal lowers the floor so the clamp cannot invert
            let cap = area.height.saturating_sub(4);
            (area.height * 2 / 5).clamp(6.min(cap), cap)
        } else {
            0
        };
        let [header, axis, body, detail, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(detail_height),
            Constraint::Length(1),
        ])
        .areas(area);
        // remember the row list's area so clicks can map back onto rows
        self.body_area = body;
        // scroll a fresh selection into view, then let the user roam freely
        let height = body.height as usize;
        if self.follow_selection
            && let Some(selected) = &self.selected
            && let Some(index) = self.last_rows.iter().position(|row| &row.key == selected)
        {
            if index < self.scroll {
                self.scroll = index;
            } else if height > 0 && index >= self.scroll + height {
                self.scroll = index + 1 - height;
            }
            self.follow_selection = false;
        }
        // clamp the window to the rows that exist
        self.scroll = self.scroll.min(self.last_rows.len().saturating_sub(1));
        // place the collapsed gaps once per frame: the vertical split keeps
        // every pane's width, so the axis and all the rows share these columns
        let bar_width = (area.width as usize).saturating_sub(LABEL_WIDTH);
        let gap_cells = gap_cols(&map, self.view_start, self.view_end, bar_width);
        self.render_header(frame, header, &state, &map);
        self.render_axis(frame, axis, &map, &gap_cells);
        self.render_rows(frame, body, &state, now, &map, &gap_cells);
        if detail_height > 0 {
            self.render_detail(frame, detail, &state, now);
        }
        // the footer's hints follow whichever pane owns the keyboard
        let hints = if self.detail.is_some() {
            " j/k scroll  PgUp/PgDn page  Esc back  q close"
        } else {
            " Tab/j/k select  Enter expand/detail  h/l pan  +/- zoom  0 fit  Esc/q close  (~ = chars/4 est)"
        };
        let line = Line::from(Span::styled(
            hints.to_owned(),
            self.theme.graph.node_discovered,
        ));
        frame.render_widget(Paragraph::new(line), footer);
    }

    /// Renders the one-line status header above the timeline
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render to
    /// * `area` - The one-row header strip
    /// * `state` - The reduced timeline state being shown
    /// * `map` - The frame's gap-compressed time axis
    fn render_header(&self, frame: &mut Frame, area: Rect, state: &GanttState, map: &TimeMap) {
        // describe the population and the run's wall-clock span
        let mut status = format!(" TIMELINE • {} agents", state.agents.len());
        if !state.swarms.is_empty() {
            status.push_str(&format!(" • {} swarms", state.swarms.len()));
        }
        match (state.min_ts, state.max_ts) {
            // a stamped run shows how long it has been on screen
            (Some(min), Some(max)) => {
                status.push_str(&format!(
                    " • span {}",
                    fmt_duration(max.saturating_sub(min))
                ));
                // hidden downtime surfaces the active total so the wall-clock
                // span cannot be misread as time actually worked
                let gaps = map.gaps().len();
                if gaps > 0 {
                    status.push_str(&format!(
                        " • active {} • {gaps} gap{} hidden",
                        fmt_duration(map.total_active()),
                        if gaps == 1 { "" } else { "s" },
                    ));
                }
            }
            // a legacy transcript with no stamps cannot draw bars at all
            _ => status.push_str(" • no timestamps — bars unavailable"),
        }
        let line = Line::from(Span::styled(
            status,
            Style::default().add_modifier(Modifier::BOLD),
        ));
        frame.render_widget(Paragraph::new(line), area);
    }

    /// Renders the time axis: tick labels as worked time from the window start
    ///
    /// Ticks count cumulative active time, so the collapsed gaps never inflate
    /// them; each gap paints break glyphs over the ticks with its real length
    /// trailing, so the downtime stays visible without dominating.
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render to
    /// * `area` - The one-row axis strip
    /// * `map` - The frame's gap-compressed time axis
    /// * `gap_cells` - The collapsed gaps' column ranges within the bar area
    fn render_axis(
        &self,
        frame: &mut Frame,
        area: Rect,
        map: &TimeMap,
        gap_cells: &[(usize, usize, u64)],
    ) {
        // the axis spans the bar columns to the right of the label gutter
        let bar_width = (area.width as usize).saturating_sub(LABEL_WIDTH);
        if bar_width == 0 {
            return;
        }
        let span = self.view_end.saturating_sub(self.view_start).max(1);
        // lay the ticks out roughly every sixth of the bar area
        let tick_step = (bar_width / 6).max(12);
        let mut cells: Vec<char> = vec![' '; bar_width];
        let mut col = 0;
        while col < bar_width {
            // place the tick on the compressed axis, then label it with the
            // active time worked between the window start and the tick
            let tick = self
                .view_start
                .saturating_add(span.saturating_mul(col as u64) / bar_width as u64);
            let offset = if map.is_empty() {
                // a stampless run keeps the raw window offsets
                tick.saturating_sub(self.view_start)
            } else {
                map.active_between(self.view_start, tick)
            };
            let label = format!("|{}", fmt_duration(offset));
            // write the label unless it would spill past the axis edge
            if col + label.chars().count() > bar_width {
                break;
            }
            for (i, c) in label.chars().enumerate() {
                cells[col + i] = c;
            }
            col += tick_step;
        }
        // paint every collapsed gap over the ticks: break glyphs with the
        // gap's real length trailing, outranking any tick label they cover
        for &(x0, x1, real_ms) in gap_cells {
            for cell in &mut cells[x0..x1] {
                *cell = BREAK_GLYPH;
            }
            for (i, c) in fmt_duration(real_ms).chars().enumerate() {
                if x1 + i >= bar_width {
                    break;
                }
                cells[x1 + i] = c;
            }
        }
        let axis: String = cells.into_iter().collect();
        let line = Line::from(vec![
            Span::raw(" ".repeat(LABEL_WIDTH)),
            Span::styled(axis, self.theme.graph.node_discovered),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    /// Renders the visible window of timeline rows
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render to
    /// * `area` - The pane the rows fill
    /// * `state` - The reduced timeline state being shown
    /// * `now` - The current wall clock in milliseconds since the epoch
    /// * `map` - The frame's gap-compressed time axis
    /// * `gap_cells` - The collapsed gaps' column ranges within the bar area
    fn render_rows(
        &self,
        frame: &mut Frame,
        area: Rect,
        state: &GanttState,
        now: Option<u64>,
        map: &TimeMap,
        gap_cells: &[(usize, usize, u64)],
    ) {
        // an empty session shows a hint instead of a blank pane
        if self.last_rows.is_empty() {
            let hint = Paragraph::new(Line::from(
                "No activity yet — the timeline grows as the run works",
            ))
            .style(self.theme.graph.node_discovered)
            .centered();
            frame.render_widget(hint, area);
            return;
        }
        // draw the visible window of rows against the frame's shared gap columns
        let height = area.height as usize;
        let bar_width = (area.width as usize).saturating_sub(LABEL_WIDTH);
        let mut lines = Vec::with_capacity(height);
        for row in self.last_rows.iter().skip(self.scroll).take(height) {
            let selected = self.selected.as_ref() == Some(&row.key);
            lines.push(self.row_line(state, row, now, bar_width, selected, map, gap_cells));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    /// Builds one timeline row's line: label, bar, and preview
    ///
    /// # Arguments
    ///
    /// * `state` - The reduced timeline state being shown
    /// * `row` - The laid-out row to draw
    /// * `now` - The current wall clock in milliseconds since the epoch
    /// * `bar_width` - The columns the bar area occupies
    /// * `selected` - Whether this row is the current selection
    /// * `map` - The frame's gap-compressed time axis
    /// * `gap_cells` - The collapsed gaps' column ranges within the bar area
    fn row_line(
        &self,
        state: &GanttState,
        row: &GanttRow,
        now: Option<u64>,
        bar_width: usize,
        selected: bool,
        map: &TimeMap,
        gap_cells: &[(usize, usize, u64)],
    ) -> Line<'static> {
        // resolve the row's name, color, and preview from its identity
        let palette = &self.theme.graph.agent_palette;
        let (name, color, preview) = match &row.key {
            RowKey::Swarm(name) => {
                let preview = state
                    .swarms
                    .get(name)
                    .map(|swarm| self.swarm_preview(state, swarm))
                    .unwrap_or_default();
                (name.clone(), None, preview)
            }
            RowKey::Agent(name) => {
                let span = state.agents.get(name);
                let color = span.map(|span| palette[span.color_slot % palette.len()]);
                let preview = span
                    .map(|span| self.agent_preview(span))
                    .unwrap_or_default();
                (name.clone(), color, preview)
            }
            RowKey::Call(call_id) => {
                let call = state.calls.iter().find(|call| &call.call_id == call_id);
                // a call's bar borrows its owning agent's palette color
                let color = call
                    .and_then(|call| state.agents.get(&call.agent))
                    .map(|span| palette[span.color_slot % palette.len()]);
                let name = call.map(|call| call.tool.clone()).unwrap_or_default();
                let preview = call
                    .map(|call| self.call_preview(call, now))
                    .unwrap_or_default();
                (name, color, preview)
            }
        };
        // the label gutter: indent, expansion glyph, swatch, and name
        let indent = usize::from(row.depth) * 2;
        let glyph = if row.expandable {
            if row.expanded { "▾ " } else { "▸ " }
        } else {
            "  "
        };
        // tool names render in the dim routine-tool style; agents get their
        // palette color and swarms stay bold in the default color
        let name_style = match (&row.key, color) {
            (RowKey::Call(_), _) => self.theme.messages.tool,
            (_, Some(color)) => Style::default().fg(color),
            (RowKey::Swarm(_), None) => Style::default().add_modifier(Modifier::BOLD),
            (_, None) => Style::default(),
        };
        // the glyph and swatch each take two columns out of the label gutter
        let budget = LABEL_WIDTH.saturating_sub(indent + 4);
        let label = format!("{:<budget$}", truncate_label(&name, budget));
        let mut label_style = name_style;
        if selected {
            label_style = label_style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
        }
        // the bar area: a duration-scaled bar plus the preview around it
        let bar_color = color.unwrap_or(Color::White);
        let mut bar_style = Style::default().fg(bar_color);
        if selected {
            bar_style = bar_style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
        }
        let (pre, bar, post) = self.bar_cells(row, now, bar_width, &preview, map, gap_cells);
        Line::from(vec![
            Span::raw(" ".repeat(indent)),
            Span::styled(glyph.to_owned(), name_style),
            Span::styled("● ", name_style),
            Span::styled(label, label_style),
            Span::styled(pre, self.theme.graph.node_discovered),
            Span::styled(bar, bar_style),
            Span::styled(post, self.theme.graph.node_discovered),
        ])
    }

    /// Lays one row's bar area out as pre-text, bar, and post-text cells
    ///
    /// The bar is placed by mapping the row's span through the gap-compressed
    /// axis onto the time window, with the break glyph showing through any
    /// collapsed gap the bar spans; the preview lands after the bar when there
    /// is room, before it otherwise, and spans with no timestamps show a dash
    /// with the preview beside it.
    ///
    /// # Arguments
    ///
    /// * `row` - The laid-out row being drawn
    /// * `now` - The current wall clock in milliseconds since the epoch
    /// * `bar_width` - The columns the bar area occupies
    /// * `preview` - The row's preview text, truncated here to what fits
    /// * `map` - The frame's gap-compressed time axis
    /// * `gap_cells` - The collapsed gaps' column ranges within the bar area
    fn bar_cells(
        &self,
        row: &GanttRow,
        now: Option<u64>,
        bar_width: usize,
        preview: &str,
        map: &TimeMap,
        gap_cells: &[(usize, usize, u64)],
    ) -> (String, String, String) {
        // nothing fits in a zero-width bar area
        if bar_width == 0 {
            return (String::new(), String::new(), String::new());
        }
        // a span with no stamps renders a dash with the preview beside it
        let Some(start) = row.start else {
            let text = truncate_label(preview, bar_width.saturating_sub(2));
            return (String::new(), "—".to_owned(), format!(" {text}"));
        };
        // an open-ended span (in flight) runs up to now
        let end = row.end.or(now).unwrap_or(start).max(start);
        // place both endpoints on the gap-compressed axis
        let start = map.compress(start);
        let end = map.compress(end).max(start);
        // a span entirely outside the window draws no bar, only the preview
        if end < self.view_start || start > self.view_end {
            let text = truncate_label(preview, bar_width.saturating_sub(1));
            return (format!(" {text}"), String::new(), String::new());
        }
        // map the clamped span onto the window's columns through the shared
        // clamp, so the bar lands exactly where the gap glyphs expect it
        let (x0, x1) = clamp_to_bar(start, end, self.view_start, self.view_end, bar_width);
        // the shared clamp already widened a visible span to at least one cell
        let len = x1.saturating_sub(x0);
        // paint the bar cell by cell, letting the break glyph show through any
        // collapsed gap the bar spans; an in-flight bar gains a growth tail
        let tail = usize::from(row.end.is_none());
        let mut bar = String::with_capacity(len.saturating_mul(3));
        for col in x0..x0 + len.saturating_sub(tail) {
            let in_gap = gap_cells
                .iter()
                .any(|(g0, g1, _)| (*g0..*g1).contains(&col));
            bar.push(if in_gap { BREAK_GLYPH } else { '█' });
        }
        if row.end.is_none() {
            bar.push('▶');
        }
        // place the preview in the roomier side of the bar
        let after = bar_width.saturating_sub(x0 + len);
        if after >= MIN_PREVIEW_WIDTH || after >= x0 {
            // after the bar: pad to the bar start, then trail the preview
            let text = truncate_label(preview, after.saturating_sub(1));
            (" ".repeat(x0), bar, format!(" {text}"))
        } else {
            // before the bar: right-align the preview against the bar start
            let text = truncate_label(preview, x0.saturating_sub(1));
            let pad = x0.saturating_sub(text.chars().count() + 1);
            (format!("{}{text} ", " ".repeat(pad)), bar, String::new())
        }
    }

    /// Composes a swarm row's preview: members, outcome, tokens, and cost
    ///
    /// # Arguments
    ///
    /// * `state` - The reduced timeline state being shown
    /// * `swarm` - The swarm span to preview
    fn swarm_preview(&self, state: &GanttState, swarm: &super::state::SwarmSpan) -> String {
        // sum the member agents' exact token buckets and cost
        let members: Vec<&AgentSpan> = swarm
            .members
            .iter()
            .filter_map(|member| state.agents.get(member))
            .collect();
        let input: u64 = members.iter().map(|span| span.input).sum();
        let output: u64 = members.iter().map(|span| span.output).sum();
        let total: u64 = members.iter().map(|span| span.total_tokens()).sum();
        // the member count leads, with the outcome once it is known
        let mut head = format!("{} agents", swarm.members.len());
        if let Some((succeeded, failed)) = swarm.outcome {
            head.push_str(&format!(" ✓{succeeded} ✗{failed}"));
        }
        let mut parts = vec![head];
        // cost sums each member through the session's estimator
        if let Some(cost) = &self.cost {
            let dollars: f64 = members
                .iter()
                .map(|span| cost.cost(span.input, span.output, span.cached, span.cache_creation))
                .sum();
            parts.push(fmt_cost(dollars));
        }
        parts.push(format!("tot {}", with_thousands(total)));
        parts.push(format!(
            "in {} out {}",
            with_thousands(input),
            with_thousands(output)
        ));
        compose_preview(&parts, Some(&swarm.goal))
    }

    /// Composes an agent row's preview: tokens, cost, and its goal
    ///
    /// # Arguments
    ///
    /// * `span` - The agent span to preview
    fn agent_preview(&self, span: &AgentSpan) -> String {
        let mut parts = Vec::new();
        // cost runs the exact buckets through the session's estimator
        if let Some(cost) = &self.cost {
            parts.push(fmt_cost(cost.cost(
                span.input,
                span.output,
                span.cached,
                span.cache_creation,
            )));
        }
        parts.push(format!("tot {}", with_thousands(span.total_tokens())));
        parts.push(format!(
            "in {} out {}",
            with_thousands(span.input),
            with_thousands(span.output)
        ));
        compose_preview(&parts, span.goal.as_deref())
    }

    /// Composes a tool call row's preview: outcome, duration, estimates, args
    ///
    /// Per-call tokens and cost are chars/4 context-size estimates (there is
    /// no per-call usage telemetry), so every figure is `~`-prefixed.
    ///
    /// # Arguments
    ///
    /// * `call` - The tool call span to preview
    /// * `now` - The current wall clock in milliseconds since the epoch
    fn call_preview(&self, call: &ToolSpan, now: Option<u64>) -> String {
        // the outcome glyph and duration lead the preview; a call its session
        // died around is interrupted, not failed and not still running
        let glyph = match (call.success, call.interrupted) {
            (Some(true), _) => "✓",
            (Some(false), _) => "✗",
            (None, true) => "⚠",
            (None, false) => "…",
        };
        let mut head = glyph.to_owned();
        if let Some(duration) = call_duration(call, now) {
            head.push_str(&format!(" {}", fmt_duration(duration)));
        }
        let mut parts = vec![head];
        // every per-call figure is an estimate, so flag each with a tilde
        let tokens = call.args_est.saturating_add(call.result_est);
        if let Some(cost) = &self.cost {
            parts.push(format!(
                "~{}",
                fmt_cost(cost.cost(call.args_est, call.result_est, 0, 0))
            ));
        }
        parts.push(format!("~{} tok", with_thousands(tokens)));
        compose_preview(&parts, Some(&one_line(&call.args)))
    }

    /// Renders the bottom detail panel for the selected tool call
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render to
    /// * `area` - The pane the panel fills
    /// * `state` - The reduced timeline state being shown
    /// * `now` - The current wall clock in milliseconds since the epoch
    fn render_detail(&self, frame: &mut Frame, area: Rect, state: &GanttState, now: Option<u64>) {
        let Some(detail) = &self.detail else {
            return;
        };
        // the render loop already closed details for vanished calls
        let Some(call) = state
            .calls
            .iter()
            .find(|call| call.call_id == detail.call_id)
        else {
            return;
        };
        // frame the panel with the call's tool and owning agent
        let block = Block::bordered().title(format!(" {} — {} ", call.tool, call.agent));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        // the status line: outcome, duration, and the estimate figures; a call
        // its session died around is interrupted, not failed and not running
        let glyph = match (call.success, call.interrupted) {
            (Some(true), _) => "✓ ok",
            (Some(false), _) => "✗ failed",
            (None, true) => "⚠ interrupted",
            (None, false) => "… running",
        };
        let mut status = glyph.to_owned();
        if let Some(duration) = call_duration(call, now) {
            status.push_str(&format!(" • {}", fmt_duration(duration)));
        }
        status.push_str(&format!(
            " • ~args {} tok • ~result {} tok",
            with_thousands(call.args_est),
            with_thousands(call.result_est),
        ));
        if let Some(cost) = &self.cost {
            status.push_str(&format!(
                " • ~{}",
                fmt_cost(cost.cost(call.args_est, call.result_est, 0, 0))
            ));
        }
        let mut lines = vec![
            Line::from(Span::styled(
                status,
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::default(),
            Line::from(Span::styled("args:".to_owned(), self.theme.messages.agent)),
        ];
        // pretty-print the arguments when they parse as JSON
        for text in pretty_args(&call.args).lines() {
            lines.push(Line::from(text.to_owned()));
        }
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "result:".to_owned(),
            self.theme.messages.agent,
        )));
        // the full result text, or a placeholder while the call runs
        match &call.result {
            Some(result) => {
                for text in result.lines() {
                    lines.push(Line::from(text.to_owned()));
                }
            }
            None => lines.push(Line::from(Span::styled(
                if call.interrupted {
                    "(interrupted — the session ended before a result was recorded)".to_owned()
                } else {
                    "(still running)".to_owned()
                },
                self.theme.graph.node_discovered,
            ))),
        }
        // wrap and scroll the panel's text
        let paragraph = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((detail.scroll, 0));
        frame.render_widget(paragraph, inner);
    }
}

/// Places a compressed span's endpoints onto a window's bar columns
///
/// This single clamp is the alignment contract between the bars and the gap
/// break glyphs: both map their endpoints through it, so a bar's cells and
/// the break columns painted over it line up by construction. The returned
/// pair is `(first column, past-the-end column)`: the first column stays
/// inside the bar area even at the window's right edge, and a visible span
/// always covers at least one column.
///
/// # Arguments
///
/// * `start` - The span's left edge on the compressed axis
/// * `end` - The span's right edge on the compressed axis
/// * `view_start` - The window's left edge on the compressed axis
/// * `view_end` - The window's right edge on the compressed axis
/// * `bar_width` - The columns the bar area occupies
fn clamp_to_bar(
    start: u64,
    end: u64,
    view_start: u64,
    view_end: u64,
    bar_width: usize,
) -> (usize, usize) {
    // guard the divisor against a degenerate or inverted window
    let span = view_end.saturating_sub(view_start).max(1);
    // pin a stamp inside the window, then scale it onto the columns
    let place = |ts: u64| -> usize {
        let ts = ts.clamp(view_start, view_end);
        (ts.saturating_sub(view_start)
            .saturating_mul(bar_width as u64)
            / span) as usize
    };
    // the first column stays inside the bar area even at the right edge
    let x0 = place(start).min(bar_width.saturating_sub(1));
    // the past-the-end column caps at the bar edge and covers at least a cell
    let x1 = place(end).min(bar_width).max(x0 + 1);
    (x0, x1)
}

/// Maps the collapsed gaps onto a window's bar columns
///
/// Each visible gap becomes a `(first column, past-the-end column, real
/// milliseconds)` triple, placed through the shared [`clamp_to_bar`] helper
/// the bars also map their endpoints through, so the axis break glyphs and the
/// glyphs showing through bars line up exactly by construction.
/// Break runs whose glyphs-plus-label would collide on screen coalesce into
/// one run carrying the summed downtime, so adjacent gaps (e.g. around a lone
/// resume-and-ask stamp) never overwrite each other's labels.
///
/// # Arguments
///
/// * `map` - The frame's gap-compressed time axis
/// * `view_start` - The window's left edge on the compressed axis
/// * `view_end` - The window's right edge on the compressed axis
/// * `bar_width` - The columns the bar area occupies
fn gap_cols(
    map: &TimeMap,
    view_start: u64,
    view_end: u64,
    bar_width: usize,
) -> Vec<(usize, usize, u64)> {
    let mut runs: Vec<(usize, usize, u64)> = Vec::new();
    for gap in map.gaps() {
        // a gap outside the window paints nothing
        if gap.c1 < view_start || gap.c0 > view_end || bar_width == 0 {
            continue;
        }
        // map the gap's compressed edges onto columns through the shared clamp
        let (x0, x1) = clamp_to_bar(gap.c0, gap.c1, view_start, view_end, bar_width);
        // coalesce with the previous run when this one would start inside its
        // glyphs or its trailing duration label
        if let Some((_, last_x1, last_ms)) = runs.last_mut() {
            let label_end = last_x1.saturating_add(fmt_duration(*last_ms).chars().count());
            if x0 <= label_end {
                *last_x1 = (*last_x1).max(x1);
                *last_ms = last_ms.saturating_add(gap.real_ms);
                continue;
            }
        }
        runs.push((x0, x1, gap.real_ms));
    }
    runs
}

/// How long a tool call ran (or has been running), in milliseconds
///
/// # Arguments
///
/// * `call` - The tool call span to measure
/// * `now` - The current wall clock in milliseconds since the epoch
fn call_duration(call: &ToolSpan, now: Option<u64>) -> Option<u64> {
    // a call with no start stamp has no measurable duration
    let start = call.start_ts?;
    // an in-flight call measures up to now when it is known
    let end = call.end_ts.or_else(|| now.filter(|_| call.in_flight()))?;
    Some(end.saturating_sub(start))
}

/// Joins stat segments and trailing free text into one preview string
///
/// The segments are given most-important-first, so a tight row keeps the
/// leading stats; the free text (a goal or an argument preview) trails and
/// absorbs any truncation when the row is finally drawn.
///
/// # Arguments
///
/// * `parts` - The stat segments, most important first
/// * `text` - The trailing free text, when there is any
fn compose_preview(parts: &[String], text: Option<&str>) -> String {
    // the stats lead the preview in importance order
    let mut preview = parts.join(" • ");
    // the free text trails so bar-side truncation eats it first
    if let Some(text) = text
        && !text.is_empty()
    {
        if !preview.is_empty() {
            preview.push_str(" • ");
        }
        preview.push_str(&one_line(text));
    }
    preview
}

/// Collapses a text payload onto one line for a row preview
///
/// # Arguments
///
/// * `text` - The payload text to collapse
fn one_line(text: &str) -> String {
    // newlines and repeated spaces flatten into single spaces
    text.split_whitespace().collect::<Vec<&str>>().join(" ")
}

/// Pretty-prints a tool call's raw JSON arguments when they parse
///
/// # Arguments
///
/// * `args` - The raw JSON argument string the tool was called with
fn pretty_args(args: &str) -> String {
    // anything that is not valid JSON renders raw rather than failing
    match serde_json::from_str::<serde_json::Value>(args) {
        Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_else(|_| args.to_owned()),
        Err(_) => args.to_owned(),
    }
}

/// Truncates a label to a column budget, appending an ellipsis
///
/// # Arguments
///
/// * `label` - The label to truncate
/// * `max` - The most characters the label may occupy
fn truncate_label(label: &str, max: usize) -> String {
    // short labels pass through untouched
    if label.chars().count() <= max {
        return label.to_owned();
    }
    // longer labels keep their head and gain an ellipsis
    let mut truncated: String = label.chars().take(max.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::super::state::GanttEvent;

    /// Builds a key press event for a key code
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// Builds a gantt log with the main agent, a swarm, and one member
    fn seeded_log() -> GanttLog {
        let log = GanttLog::default();
        // the main agent works first
        log.push(GanttEvent::ToolCall {
            agent: "main".to_owned(),
            call_id: "call-1".to_owned(),
            tool: "list_funcs".to_owned(),
            args: "{}".to_owned(),
            ts: Some(1_000),
        });
        log.push(GanttEvent::ToolResult {
            agent: "main".to_owned(),
            call_id: "call-1".to_owned(),
            tool: "list_funcs".to_owned(),
            result: "entry".to_owned(),
            success: true,
            ts: Some(2_000),
        });
        // then a swarm spawns one member who makes a call of its own
        log.push(GanttEvent::SwarmStarted {
            swarm: "Swarm(Io)".to_owned(),
            goal: "answer questions".to_owned(),
            ts: Some(3_000),
        });
        log.push(GanttEvent::AgentStarted {
            agent: "ReverseEngineer(Europa)".to_owned(),
            role: Some("ReverseEngineer".to_owned()),
            goal: Some("find the key".to_owned()),
            swarm: Some("Swarm(Io)".to_owned()),
            ts: Some(4_000),
        });
        log.push(GanttEvent::ToolCall {
            agent: "ReverseEngineer(Europa)".to_owned(),
            call_id: "call-2".to_owned(),
            tool: "decomp_function".to_owned(),
            args: "{\"name\": \"entry\"}".to_owned(),
            ts: Some(5_000),
        });
        log.push(GanttEvent::ToolResult {
            agent: "ReverseEngineer(Europa)".to_owned(),
            call_id: "call-2".to_owned(),
            tool: "decomp_function".to_owned(),
            result: "void entry(void) {}".to_owned(),
            success: true,
            ts: Some(6_000),
        });
        log.push(GanttEvent::SwarmFinished {
            swarm: "Swarm(Io)".to_owned(),
            succeeded: 1,
            failed: 0,
            ts: Some(7_000),
        });
        // a usage checkpoint gives the member exact token buckets
        log.push(GanttEvent::Usage {
            agent: "ReverseEngineer(Europa)".to_owned(),
            input: 1_000,
            output: 100,
            cached: 0,
            cache_creation: 0,
            ts: Some(6_500),
        });
        log
    }

    /// Draws one frame and returns the buffer's visible text
    fn draw(view: &mut GanttView, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .expect("frame");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    /// The view renders collapsed, expanded, selected, and tiny frames
    #[test]
    fn view_renders_all_modes() {
        let mut view = GanttView::new(seeded_log(), None);
        view.toggle();
        assert!(view.open);
        // the collapsed frame shows the roots with bars and previews
        let content = draw(&mut view, 110, 30);
        assert!(content.contains("main"));
        assert!(content.contains("Swarm(Io)"));
        assert!(content.contains("█"));
        assert!(content.contains("1 agents"));
        // the member only appears once its swarm expands
        assert!(!content.contains("ReverseEngineer"));
        // select the swarm (second row) and expand it with Enter
        assert!(matches!(
            view.handle_key(key(KeyCode::Tab)),
            ViewOutcome::None
        ));
        assert!(matches!(
            view.handle_key(key(KeyCode::Tab)),
            ViewOutcome::None
        ));
        assert_eq!(view.selected, Some(RowKey::Swarm("Swarm(Io)".to_owned())));
        assert!(matches!(
            view.handle_key(key(KeyCode::Enter)),
            ViewOutcome::None
        ));
        // the member's label is ellipsis-truncated to the label gutter; a
        // wide frame leaves room for its goal preview beside the real bar
        let content = draw(&mut view, 140, 30);
        assert!(content.contains("ReverseEngineer(Europ…"));
        assert!(content.contains("find the key"));
        // expand the member to reveal its tool call row
        assert!(matches!(
            view.handle_key(key(KeyCode::Tab)),
            ViewOutcome::None
        ));
        assert_eq!(
            view.selected,
            Some(RowKey::Agent("ReverseEngineer(Europa)".to_owned()))
        );
        assert!(matches!(
            view.handle_key(key(KeyCode::Enter)),
            ViewOutcome::None
        ));
        let content = draw(&mut view, 110, 30);
        assert!(content.contains("decomp_function"));
        // a tiny terminal renders without panicking
        draw(&mut view, 40, 8);
    }

    /// Entering a tool call opens its detail panel; Esc backs out in steps
    #[test]
    fn call_detail_opens_and_esc_backs_out() {
        let mut view = GanttView::new(seeded_log(), None);
        view.toggle();
        // expand down to the member's tool call and select it
        view.expanded.insert(RowKey::Swarm("Swarm(Io)".to_owned()));
        view.expanded
            .insert(RowKey::Agent("ReverseEngineer(Europa)".to_owned()));
        draw(&mut view, 110, 30);
        view.selected = Some(RowKey::Call("call-2".to_owned()));
        assert!(matches!(
            view.handle_key(key(KeyCode::Enter)),
            ViewOutcome::None
        ));
        // the detail panel shows the call's args and result
        let content = draw(&mut view, 110, 30);
        assert!(content.contains("decomp_function — ReverseEngineer(Europa)"));
        assert!(content.contains("void entry(void) {}"));
        assert!(content.contains("~args"));
        // Esc closes the detail, then clears the selection, then closes
        assert!(matches!(
            view.handle_key(key(KeyCode::Esc)),
            ViewOutcome::None
        ));
        assert!(view.detail.is_none());
        assert!(view.selected.is_some());
        assert!(matches!(
            view.handle_key(key(KeyCode::Esc)),
            ViewOutcome::None
        ));
        assert!(view.selected.is_none());
        assert!(matches!(
            view.handle_key(key(KeyCode::Esc)),
            ViewOutcome::Close
        ));
    }

    /// Clicking a row selects it, toggling expansion or opening detail
    #[test]
    fn clicks_map_onto_rows() {
        let mut view = GanttView::new(seeded_log(), None);
        view.toggle();
        // render once so the body area and row list are known
        draw(&mut view, 110, 30);
        // the swarm sits on the second body row; clicking it expands it
        let y = view.body_area.y + 1;
        view.handle_click(5, y);
        assert_eq!(view.selected, Some(RowKey::Swarm("Swarm(Io)".to_owned())));
        assert!(
            view.expanded
                .contains(&RowKey::Swarm("Swarm(Io)".to_owned()))
        );
        // after a redraw the member row sits below the swarm; expand it too
        draw(&mut view, 110, 30);
        view.handle_click(5, y + 1);
        assert!(
            view.expanded
                .contains(&RowKey::Agent("ReverseEngineer(Europa)".to_owned()))
        );
        // after another redraw the call row follows; clicking it opens detail
        draw(&mut view, 110, 30);
        view.handle_click(5, y + 2);
        assert_eq!(view.selected, Some(RowKey::Call("call-2".to_owned())));
        assert!(view.detail.is_some());
        // a click outside the body area is ignored
        view.detail = None;
        view.handle_click(5, 0);
        assert_eq!(view.selected, Some(RowKey::Call("call-2".to_owned())));
    }

    /// Zoom and pan take manual control of the window; `0` re-fits it
    #[test]
    fn window_zooms_pans_and_refits() {
        let mut view = GanttView::new(seeded_log(), None);
        view.toggle();
        // the first render auto-fits the window over the whole run; nothing
        // is open, so the right edge freezes at the run's last stamp
        draw(&mut view, 110, 30);
        assert_eq!(view.view_start, 1_000);
        assert_eq!(view.view_end, 7_000);
        // zooming in shrinks the span and pins the window
        let span = view.view_end - view.view_start;
        assert!(matches!(
            view.handle_key(key(KeyCode::Char('+'))),
            ViewOutcome::None
        ));
        assert!(!view.auto_fit);
        assert!(view.view_end - view.view_start < span);
        // panning shifts both edges the same way
        let start = view.view_start;
        assert!(matches!(
            view.handle_key(key(KeyCode::Char('l'))),
            ViewOutcome::None
        ));
        assert!(view.view_start > start);
        // `0` hands the window back to auto-fit on the next render
        assert!(matches!(
            view.handle_key(key(KeyCode::Char('0'))),
            ViewOutcome::None
        ));
        assert!(view.auto_fit);
        draw(&mut view, 110, 30);
        assert_eq!(view.view_start, 1_000);
        assert_eq!(view.view_end, 7_000);
    }

    /// A started tool call event for an agent at a stamp
    fn timed_call(agent: &str, call_id: &str, tool: &str, ts: u64) -> GanttEvent {
        GanttEvent::ToolCall {
            agent: agent.to_owned(),
            call_id: call_id.to_owned(),
            tool: tool.to_owned(),
            args: "{}".to_owned(),
            ts: Some(ts),
        }
    }

    /// A finished tool call event for an agent at a stamp
    fn timed_result(agent: &str, call_id: &str, tool: &str, ts: u64) -> GanttEvent {
        GanttEvent::ToolResult {
            agent: agent.to_owned(),
            call_id: call_id.to_owned(),
            tool: tool.to_owned(),
            result: "ok".to_owned(),
            success: true,
            ts: Some(ts),
        }
    }

    /// Splits a drawn frame's text back into its screen rows
    fn screen_rows(content: &str, width: usize) -> Vec<String> {
        content
            .chars()
            .collect::<Vec<char>>()
            .chunks(width)
            .map(|chunk| chunk.iter().collect())
            .collect()
    }

    /// Counts the bar cells on the first screen row naming a label
    fn bar_len(content: &str, width: usize, label: &str) -> usize {
        let rows = screen_rows(content, width);
        let row = rows
            .iter()
            .find(|row| row.contains(label))
            .unwrap_or_else(|| panic!("no row labeled {label}"));
        row.chars().filter(|c| *c == '█').count()
    }

    /// A resumed, finished session freezes at its own window with real bars
    #[test]
    fn resumed_cold_session_freezes_and_keeps_proportions() {
        let now = now_ms().expect("a wall clock");
        // a day-old run: two closed calls of 60s and 120s back to back
        let base = now - 86_400_000;
        let log = GanttLog::default();
        log.push(timed_call("main", "call-1", "list_funcs", base));
        log.push(timed_result("main", "call-1", "list_funcs", base + 60_000));
        log.push(timed_call(
            "main",
            "call-2",
            "decomp_function",
            base + 60_000,
        ));
        log.push(timed_result(
            "main",
            "call-2",
            "decomp_function",
            base + 180_000,
        ));
        let mut view = GanttView::new(log, None);
        view.toggle();
        // reveal the call rows under the main agent
        view.expanded.insert(RowKey::Agent("main".to_owned()));
        let content = draw(&mut view, 110, 30);
        // the window froze at the historical run, not the current clock
        assert_eq!(view.view_start, base);
        assert_eq!(view.view_end, base + 180_000);
        // nothing is in flight and nothing was compressed away
        assert!(!content.contains('▶'));
        assert!(!content.contains(BREAK_GLYPH));
        // the second call's bar is about twice the first's
        let first = bar_len(&content, 110, "list_funcs");
        let second = bar_len(&content, 110, "decomp_function");
        assert!(first > 10, "the first bar is a sliver: {first} cells");
        let ratio = second as f64 / first as f64;
        assert!(
            (1.6..=2.4).contains(&ratio),
            "bars lost their proportions: {first} vs {second} cells"
        );
    }

    /// A continued session compresses its downtime to a labeled break
    #[test]
    fn gap_renders_break_marker_and_active_ticks() {
        let now = now_ms().expect("a wall clock");
        // two one-minute runs of work separated by a day of downtime
        let base = now - 90_000_000;
        let log = GanttLog::default();
        log.push(timed_call("alpha", "call-1", "list_funcs", base));
        log.push(timed_result("alpha", "call-1", "list_funcs", base + 60_000));
        log.push(timed_call(
            "beta",
            "call-2",
            "decomp_function",
            base + 86_400_000,
        ));
        log.push(timed_result(
            "beta",
            "call-2",
            "decomp_function",
            base + 86_400_000 + 60_000,
        ));
        let mut view = GanttView::new(log, None);
        view.toggle();
        let content = draw(&mut view, 110, 30);
        // frozen (nothing open), with the downtime called out in the header
        assert!(!content.contains('▶'));
        assert!(content.contains("active 2m 00s"));
        assert!(content.contains("1 gap hidden"));
        // the break marker shows with the gap's real length beside it
        assert!(content.contains(BREAK_GLYPH));
        assert!(content.contains("23h 59m"));
        // both runs keep readable bars instead of one-cell slivers
        assert!(bar_len(&content, 110, "alpha") > 20);
        assert!(bar_len(&content, 110, "beta") > 20);
    }

    /// Live in-flight work keeps the right edge tracking the clock
    #[test]
    fn live_in_flight_tracks_now() {
        let now = now_ms().expect("a wall clock");
        // a closed day-old run, then a call still in flight right now
        let base = now - 86_400_000;
        let log = GanttLog::default();
        log.push(timed_call("alpha", "call-1", "list_funcs", base));
        log.push(timed_result("alpha", "call-1", "list_funcs", base + 60_000));
        log.push(timed_call("beta", "call-2", "decomp_function", now - 2_000));
        let mut view = GanttView::new(log, None);
        view.toggle();
        let content = draw(&mut view, 110, 30);
        // the in-flight bar grows and the old run sits behind a break
        assert!(content.contains('▶'));
        assert!(content.contains(BREAK_GLYPH));
        // the compressed window covers the active minute plus the live tail,
        // not the day of downtime between them
        let span = view.view_end - view.view_start;
        assert!(span >= 62_000, "the live tail is missing: {span}ms");
        assert!(span <= 120_000, "the downtime dominates: {span}ms");
    }

    /// A detail panel renders without panicking on tiny terminals
    #[test]
    fn detail_panel_survives_tiny_terminals() {
        let mut view = GanttView::new(seeded_log(), None);
        view.toggle();
        // open the member's call detail the usual way
        view.expanded.insert(RowKey::Swarm("Swarm(Io)".to_owned()));
        view.expanded
            .insert(RowKey::Agent("ReverseEngineer(Europa)".to_owned()));
        draw(&mut view, 110, 30);
        view.selected = Some(RowKey::Call("call-2".to_owned()));
        assert!(matches!(
            view.handle_key(key(KeyCode::Enter)),
            ViewOutcome::None
        ));
        // formerly clamp(6, height - 4) inverted at nine rows and panicked
        draw(&mut view, 110, 9);
        draw(&mut view, 40, 4);
    }

    /// Adjacent collapsed gaps coalesce into one truthfully labeled break
    #[test]
    fn adjacent_gaps_coalesce_with_summed_label() {
        let now = now_ms().expect("a wall clock");
        // a run, a lone stamp a day later, and another run a day after that
        let base = now - 200_000_000;
        let log = GanttLog::default();
        log.push(timed_call("alpha", "call-1", "list_funcs", base));
        log.push(timed_result("alpha", "call-1", "list_funcs", base + 60_000));
        log.push(GanttEvent::Message {
            agent: "main".to_owned(),
            ts: Some(base + 86_400_000),
        });
        log.push(timed_call(
            "beta",
            "call-2",
            "decomp_function",
            base + 2 * 86_400_000,
        ));
        log.push(timed_result(
            "beta",
            "call-2",
            "decomp_function",
            base + 2 * 86_400_000 + 60_000,
        ));
        let mut view = GanttView::new(log, None);
        view.toggle();
        let content = draw(&mut view, 110, 30);
        // both gaps hide behind one break run labeled with their sum, instead
        // of the second gap's glyphs clobbering the first gap's label
        assert!(content.contains("2 gaps hidden"));
        assert!(content.contains("47h 59m"));
        assert!(!content.contains("23h 59m"));
    }

    /// A torn transcript settles at replay end and freezes the window
    #[test]
    fn torn_transcript_freezes_after_finalize() {
        let now = now_ms().expect("a wall clock");
        // a day-old run: alpha's call never got its result (last seen at its
        // 30s message), while beta worked on to the run's real end
        let base = now - 86_400_000;
        let log = GanttLog::default();
        log.push(timed_call("alpha", "call-1", "list_funcs", base));
        log.push(GanttEvent::Message {
            agent: "alpha".to_owned(),
            ts: Some(base + 30_000),
        });
        log.push(timed_call(
            "beta",
            "call-2",
            "decomp_function",
            base + 30_000,
        ));
        log.push(timed_result(
            "beta",
            "call-2",
            "decomp_function",
            base + 120_000,
        ));
        log.push(GanttEvent::ReplayEnded);
        let mut view = GanttView::new(log, None);
        view.toggle();
        view.expanded.insert(RowKey::Agent("alpha".to_owned()));
        let content = draw(&mut view, 110, 30);
        // the window froze at the run's last stamp, not the current clock
        assert_eq!(view.view_start, base);
        assert_eq!(view.view_end, base + 120_000);
        // the dangling call reads as interrupted, not still running
        assert!(!content.contains('▶'));
        assert!(content.contains('⚠'));
    }
}
