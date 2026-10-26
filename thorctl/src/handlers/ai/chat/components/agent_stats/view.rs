//! The full-screen agent stats overlay: bars, swarm grouping, and drill-down
//!
//! Opens on a horizontal bar list of agents by cost (falling back to tool
//! calls when the model is unpriceable); the plotted metric cycles through
//! cost, tool calls, input/output tokens, time lived, and turns. Grouping by
//! swarm sections the bars under aggregate headers, and selecting a bar (or a
//! swarm header) drills into just that agent's (or that swarm's interleaved)
//! chat log. A side panel breaks down where the context's tokens are going by
//! payload category and by tool.

use std::collections::{BTreeSet, HashSet};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use uuid::Uuid;

use crate::handlers::ai::chat::{ChatContext, ChatEntry};
use crate::handlers::ai::pricing::CostModel;

use super::super::message_log::MessageLog;
use super::super::status_bar::with_thousands;
use super::super::theme::Theme;
use super::super::traversal::ViewOutcome;
use super::log::AgentStatsLog;
use super::state::{AgentStats, Metric, Row, RowId, StatsState, build_rows, now_ms};

/// The columns the side panel takes when the terminal is wide enough
const SIDE_PANEL_WIDTH: u16 = 34;

/// The narrowest terminal that fits the side panel beside the bars
const SIDE_PANEL_MIN_WIDTH: u16 = 72;

/// The columns an agent/swarm label may occupy in the bar list
const LABEL_WIDTH: usize = 26;

/// The columns the right-aligned value column occupies in the bar list
const VALUE_WIDTH: usize = 12;

/// The chat log of one selected agent or swarm, rendered below the bars
struct DrillDown {
    /// The display title of what is being inspected
    title: String,
    /// The agent display names whose chat entries are shown
    members: BTreeSet<String>,
    /// The tool-call ids owned by members, correlating logs and results
    owned_calls: HashSet<Uuid>,
    /// The filtered, append-only copy of the shared context the log folds
    entries: Vec<ChatEntry>,
    /// How far into the shared context the filter has scanned
    scanned: usize,
    /// A private message log rendering the filtered entries
    log: MessageLog,
}

/// The full-screen agent stats overlay
pub struct AgentStatsView {
    /// Whether the overlay is currently shown
    pub open: bool,
    /// The shared agent stats log this view renders
    log: AgentStatsLog,
    /// The shared chat history the drill-down filters
    context: ChatContext,
    /// The cost estimator for the session's model, when it is priceable
    cost: Option<CostModel>,
    /// The metric the bars currently plot
    metric: Metric,
    /// Whether the bars are grouped into swarm sections
    group_by_swarm: bool,
    /// The selected bar or swarm header, when one is selected
    selected: Option<RowId>,
    /// The selectable rows in visual order, rebuilt every frame
    row_order: Vec<RowId>,
    /// The display rows of the last frame, for selection-aware scrolling
    last_rows: Vec<RowId>,
    /// The bar list's scroll offset in rows
    scroll: usize,
    /// Whether the next render should scroll the selection into view
    follow_selection: bool,
    /// The open drill-down, when a selection was entered
    drill: Option<DrillDown>,
    /// The theme supplying the view's styles
    theme: Theme,
}

impl AgentStatsView {
    /// Creates a view over a shared agent stats log
    ///
    /// # Arguments
    ///
    /// * `log` - The shared agent stats log to render
    /// * `context` - The shared chat history the drill-down filters
    /// * `cost` - The session's cost estimator, when its model is priceable
    pub fn new(log: AgentStatsLog, context: ChatContext, cost: Option<CostModel>) -> Self {
        AgentStatsView {
            open: false,
            log,
            context,
            cost,
            // open on cost when it is estimable, otherwise on tool calls
            metric: if cost.is_some() {
                Metric::Cost
            } else {
                Metric::ToolCalls
            },
            group_by_swarm: false,
            selected: None,
            row_order: Vec::new(),
            last_rows: Vec::new(),
            scroll: 0,
            follow_selection: false,
            drill: None,
            theme: Theme::default(),
        }
    }

    /// Opens or closes the overlay
    pub fn toggle(&mut self) {
        self.open = !self.open;
        // a closed view drops its selection, drill-down, and scroll
        if !self.open {
            self.selected = None;
            self.drill = None;
            self.scroll = 0;
        }
    }

    /// Handles a key press while the overlay is open
    ///
    /// # Arguments
    ///
    /// * `key` - The key event to handle
    pub fn handle_key(&mut self, key: KeyEvent) -> ViewOutcome {
        // an open drill-down owns the keys until Esc backs out of it
        if let Some(drill) = &mut self.drill {
            match key.code {
                // back out to the bar list, keeping the selection
                KeyCode::Esc => self.drill = None,
                // close the whole overlay
                KeyCode::Char('q') => return ViewOutcome::Close,
                // scroll the chat log
                KeyCode::Up | KeyCode::Char('k') => drill.log.scroll_up(1),
                KeyCode::Down | KeyCode::Char('j') => drill.log.scroll_down(1),
                KeyCode::PageUp => drill.log.page_up(10),
                KeyCode::PageDown => drill.log.page_down(10),
                // jump to the bottom and follow new entries
                KeyCode::Char('G') | KeyCode::End => drill.log.end(),
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
            // cycle the plotted metric
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('m') => {
                self.metric = self.metric.next();
            }
            KeyCode::Left | KeyCode::Char('h') => self.metric = self.metric.prev(),
            // toggle grouping the bars into swarm sections
            KeyCode::Char('g') => {
                self.group_by_swarm = !self.group_by_swarm;
                // a selected swarm header does not exist in flat mode
                if !self.group_by_swarm && matches!(self.selected, Some(RowId::Swarm(_))) {
                    self.selected = None;
                }
            }
            // drill into the selected agent's or swarm's chat
            KeyCode::Enter => self.open_drill(),
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
        match &mut self.drill {
            Some(drill) => {
                if up {
                    drill.log.scroll_up(3);
                } else {
                    drill.log.scroll_down(3);
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

    /// Handles a mouse click while the overlay is open
    ///
    /// # Arguments
    ///
    /// * `x` - The column position of the click
    /// * `y` - The row position of the click
    pub fn handle_click(&mut self, x: u16, y: u16) {
        // only the drill-down's tool bubbles react to clicks
        if let Some(drill) = &mut self.drill {
            drill.log.handle_click(x, y);
        }
    }

    /// Moves the selection one step through the visual row order
    ///
    /// # Arguments
    ///
    /// * `step` - The direction to move: positive forward, negative backward
    fn cycle_selection(&mut self, step: isize) {
        // an empty list has nothing to select
        if self.row_order.is_empty() {
            return;
        }
        // find where the current selection sits in the visual order
        let current = self
            .selected
            .as_ref()
            .and_then(|id| self.row_order.iter().position(|other| other == id));
        // step from the current slot, or start at an end when none is selected
        let next = match current {
            Some(index) => {
                let len = self.row_order.len() as isize;
                (index as isize + step).rem_euclid(len) as usize
            }
            None if step < 0 => self.row_order.len() - 1,
            None => 0,
        };
        self.selected = Some(self.row_order[next].clone());
        // the next render scrolls the fresh selection into view
        self.follow_selection = true;
    }

    /// Opens the drill-down for the current selection, if there is one
    fn open_drill(&mut self) {
        // resolve the selection into a title and a member set
        let state = self.log.snapshot();
        let (title, members) = match &self.selected {
            Some(RowId::Agent(agent)) => (agent.clone(), BTreeSet::from([agent.clone()])),
            Some(RowId::Swarm(swarm)) => {
                // a swarm's chat interleaves every member's entries
                let Some(stats) = state.swarms.get(swarm) else {
                    return;
                };
                let members: BTreeSet<String> = stats.members.iter().cloned().collect();
                // a swarm whose members never came alive has nothing to show
                if members.is_empty() {
                    return;
                }
                (format!("{swarm} — {} members", members.len()), members)
            }
            // Enter with no selection is a no-op
            None => return,
        };
        self.drill = Some(DrillDown {
            title,
            members,
            owned_calls: HashSet::new(),
            entries: Vec::new(),
            scanned: 0,
            log: MessageLog::default(),
        });
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
        // snapshot the live tallies once per frame
        let state = self.log.snapshot();
        let now = now_ms();
        // split off the one-line header and footer
        let [header, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(area);
        // an open drill-down replaces the bar list entirely
        if self.drill.is_some() {
            self.render_drill(frame, header, body, &state, now);
            render_footer(
                frame,
                footer,
                " j/k scroll  PgUp/PgDn page  G bottom  click expand  Esc back  q close",
                &self.theme,
            );
            return;
        }
        // lay the bars out for the current metric and grouping mode
        let rows = build_rows(
            &state,
            self.metric,
            self.group_by_swarm,
            self.cost.as_ref(),
            now,
        );
        // rebuild the selectable order Tab cycles through
        self.row_order = rows
            .iter()
            .filter(|row| row.selectable)
            .map(|row| row.id.clone())
            .collect();
        self.last_rows = rows.iter().map(|row| row.id.clone()).collect();
        // drop a selection the current layout no longer contains
        if let Some(selected) = &self.selected
            && !self.row_order.contains(selected)
        {
            self.selected = None;
        }
        self.render_header(frame, header, &state);
        // split off the side panel when the terminal is wide enough
        let (bars_area, side_area) = if area.width >= SIDE_PANEL_MIN_WIDTH {
            let [bars, side] =
                Layout::horizontal([Constraint::Min(0), Constraint::Length(SIDE_PANEL_WIDTH)])
                    .areas(body);
            (bars, Some(side))
        } else {
            (body, None)
        };
        self.render_bars(frame, bars_area, &state, &rows);
        if let Some(side_area) = side_area {
            self.render_side(frame, side_area, &state);
        }
        render_footer(
            frame,
            footer,
            " Tab/j/k select  ←/→ metric  g group  Enter chat  Esc/q close",
            &self.theme,
        );
    }

    /// Renders the one-line status header above the bar list
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render to
    /// * `area` - The one-row header strip
    /// * `state` - The reduced stats state being shown
    fn render_header(&self, frame: &mut Frame, area: Rect, state: &StatsState) {
        // describe the metric, population, and grouping mode
        let mut status = format!(
            " USAGE • metric: {} • {} agents",
            self.metric.label(),
            state.agents.len(),
        );
        if !state.swarms.is_empty() {
            status.push_str(&format!(" • {} swarms", state.swarms.len()));
        }
        if self.group_by_swarm {
            status.push_str(" • grouped by swarm");
        }
        // flag an unpriceable model while the cost metric is up
        if self.metric == Metric::Cost && self.cost.is_none() {
            status.push_str(" • cost unavailable (model unpriceable)");
        }
        let line = Line::from(Span::styled(
            status,
            Style::default().add_modifier(Modifier::BOLD),
        ));
        frame.render_widget(Paragraph::new(line), area);
    }

    /// Renders the horizontal bar list of agents (and swarm headers)
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render to
    /// * `area` - The pane the bars fill
    /// * `state` - The reduced stats state being shown
    /// * `rows` - The laid-out rows to draw
    fn render_bars(&mut self, frame: &mut Frame, area: Rect, state: &StatsState, rows: &[Row]) {
        // an empty session shows a hint instead of a blank pane
        if rows.is_empty() {
            let hint = Paragraph::new(Line::from(
                "No agent activity yet — stats grow as the run works",
            ))
            .style(self.theme.graph.node_discovered)
            .centered();
            frame.render_widget(hint, area);
            return;
        }
        let height = area.height as usize;
        // scroll a fresh selection into view, then let the user roam freely
        if self.follow_selection
            && let Some(selected) = &self.selected
            && let Some(index) = rows.iter().position(|row| &row.id == selected)
        {
            if index < self.scroll {
                self.scroll = index;
            } else if height > 0 && index >= self.scroll + height {
                self.scroll = index + 1 - height;
            }
            self.follow_selection = false;
        }
        // clamp the window to the rows that exist
        self.scroll = self.scroll.min(rows.len().saturating_sub(1));
        // the widest known value scales every bar in the window
        let max_value = rows
            .iter()
            .filter(|row| matches!(row.id, RowId::Agent(_)))
            .filter_map(|row| row.value)
            .fold(0.0_f64, f64::max);
        // the bar budget is what is left after the label and value columns
        let indent = usize::from(self.group_by_swarm) * 2;
        let bar_width = (area.width as usize)
            .saturating_sub(indent + 2 + LABEL_WIDTH + 1 + VALUE_WIDTH + 2)
            .max(1);
        // draw the visible window of rows
        let mut lines = Vec::with_capacity(height);
        for row in rows.iter().skip(self.scroll).take(height) {
            let selected = self.selected.as_ref() == Some(&row.id);
            let line = match &row.id {
                RowId::Agent(agent) => self.agent_line(
                    state, agent, row.value, max_value, indent, bar_width, selected,
                ),
                RowId::Swarm(swarm) => self.swarm_line(state, swarm, row.value, selected),
            };
            lines.push(line);
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    /// Builds one agent's bar line
    ///
    /// # Arguments
    ///
    /// * `state` - The reduced stats state being shown
    /// * `agent` - The display name of the agent the line plots
    /// * `value` - The agent's metric value, or `None` when unknown
    /// * `max_value` - The widest value in the list, for bar scaling
    /// * `indent` - The columns swarm grouping indents member bars by
    /// * `bar_width` - The columns the bar itself may occupy
    /// * `selected` - Whether this row is the current selection
    #[allow(clippy::too_many_arguments)]
    fn agent_line(
        &self,
        state: &StatsState,
        agent: &str,
        value: Option<f64>,
        max_value: f64,
        indent: usize,
        bar_width: usize,
        selected: bool,
    ) -> Line<'static> {
        // the agent's stable palette color matches the traversal graph
        let palette = &self.theme.graph.agent_palette;
        let color = state
            .agents
            .get(agent)
            .map(|stats| palette[stats.color_slot % palette.len()]);
        let color_style = color.map_or_else(Style::default, |color| Style::default().fg(color));
        // scale the bar against the widest value, keeping tiny bars visible
        let bar_len = match value {
            Some(v) if v > 0.0 && max_value > 0.0 => {
                (((v / max_value) * bar_width as f64).round() as usize).clamp(1, bar_width)
            }
            _ => 0,
        };
        // unknown values show a dash instead of a zero
        let value_str = match value {
            Some(v) => self.metric.format(v),
            None => "—".to_owned(),
        };
        // assemble: indent, swatch, padded label, bar, padding, value
        let label = format!("{:<LABEL_WIDTH$}", truncate_label(agent, LABEL_WIDTH));
        let mut style = color_style;
        if selected {
            style = style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
        }
        Line::from(vec![
            Span::raw(" ".repeat(indent)),
            Span::styled("● ", color_style),
            Span::styled(label, style),
            Span::raw(" "),
            Span::styled("█".repeat(bar_len), color_style),
            Span::raw(" ".repeat(bar_width.saturating_sub(bar_len))),
            Span::styled(
                format!("{value_str:>VALUE_WIDTH$}"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ])
    }

    /// Builds one swarm section's header line
    ///
    /// # Arguments
    ///
    /// * `state` - The reduced stats state being shown
    /// * `swarm` - The display name of the swarm, empty for the ungrouped section
    /// * `value` - The section's aggregate value, or `None` when unknown
    /// * `selected` - Whether this header is the current selection
    fn swarm_line(
        &self,
        state: &StatsState,
        swarm: &str,
        value: Option<f64>,
        selected: bool,
    ) -> Line<'static> {
        // the synthetic ungrouped section is a dim divider, not a swarm
        if swarm.is_empty() {
            return Line::from(Span::styled(
                "▼ (no swarm)".to_owned(),
                self.theme.graph.node_discovered,
            ));
        }
        // describe the swarm: size, aggregate, and outcome once finished
        let mut text = format!("▼ {swarm}");
        if let Some(stats) = state.swarms.get(swarm) {
            text.push_str(&format!(" — {} agents", stats.members.len()));
            if let Some((succeeded, failed)) = stats.outcome {
                text.push_str(&format!(" ✓{succeeded} ✗{failed}"));
            }
        }
        if let Some(value) = value {
            text.push_str(&format!(" — {}", self.metric.format(value)));
        }
        let mut style = Style::default().add_modifier(Modifier::BOLD);
        if selected {
            style = style.add_modifier(Modifier::REVERSED);
        }
        Line::from(Span::styled(text, style))
    }

    /// Renders the context-spend side panel
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render to
    /// * `area` - The side panel's pane
    /// * `state` - The reduced stats state being shown
    fn render_side(&self, frame: &mut Frame, area: Rect, state: &StatsState) {
        let dim = self.theme.graph.node_discovered;
        let mut lines = Vec::new();
        // the category breakdown answers "what is the context made of"
        lines.push(Line::from(Span::styled(
            "Context spend (est tokens)",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        let total = state.spend.total();
        for (label, value) in state.spend.ranked() {
            // scale a small inline bar against the whole spend
            let bar = if total > 0 {
                let cells = ((value as f64 / total as f64) * 10.0).round() as usize;
                "▰".repeat(cells.min(10))
            } else {
                String::new()
            };
            lines.push(Line::from(vec![
                Span::raw(format!(" {label:<14}")),
                Span::styled(format!("{:>8}", with_thousands(value)), Style::default()),
                Span::styled(format!(" {bar}"), dim),
            ]));
        }
        // the per-tool ranking names the heaviest context consumers
        let mut tools: Vec<(&String, &u64)> = state.tool_spend.iter().collect();
        tools.sort_by(|(_, a), (_, b)| b.cmp(a));
        if !tools.is_empty() {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "Top tools (est tokens)",
                Style::default().add_modifier(Modifier::BOLD),
            )));
            // show as many of the busiest tools as the pane has rows left
            let budget = (area.height as usize).saturating_sub(lines.len());
            for (tool, est) in tools.into_iter().take(budget) {
                lines.push(Line::from(vec![
                    Span::raw(format!(" {:<22}", truncate_label(tool, 22))),
                    Span::styled(format!("{:>8}", with_thousands(*est)), dim),
                ]));
            }
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    /// Renders the drill-down: a summary line over the filtered chat log
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render to
    /// * `header` - The one-row header strip
    /// * `body` - The pane the chat log fills
    /// * `state` - The reduced stats state being shown
    /// * `now` - The current wall clock in milliseconds since the epoch
    fn render_drill(
        &mut self,
        frame: &mut Frame,
        header: Rect,
        body: Rect,
        state: &StatsState,
        now: Option<u64>,
    ) {
        let Some(drill) = &mut self.drill else {
            return;
        };
        // summarize the inspected members' tallies for the header line
        let summary = summarize(state, &drill.members, self.cost.as_ref(), now);
        let line = Line::from(Span::styled(
            format!(" {} • {summary}", drill.title),
            Style::default().add_modifier(Modifier::BOLD),
        ));
        frame.render_widget(Paragraph::new(line), header);
        // extend the filtered copy with any fresh shared-context entries; the
        // copy only ever grows at the tail, so the private message log's
        // incremental fold and wrap caches stay valid
        {
            let guard = self.context.access();
            for entry in guard.iter().skip(drill.scanned) {
                if drill_matches(entry, &drill.members, &mut drill.owned_calls) {
                    drill.entries.push(entry.clone());
                }
            }
            drill.scanned = guard.len();
        }
        // render the filtered chat through the private message log
        drill.log.render(frame, body, &drill.entries);
    }
}

/// Renders the one-line key-hint footer
///
/// # Arguments
///
/// * `frame` - The frame to render to
/// * `area` - The one-row footer strip
/// * `hints` - The key hints to show
/// * `theme` - The theme supplying the footer's style
fn render_footer(frame: &mut Frame, area: Rect, hints: &str, theme: &Theme) {
    let line = Line::from(Span::styled(hints.to_owned(), theme.graph.node_discovered));
    frame.render_widget(Paragraph::new(line), area);
}

/// Whether a shared-context entry belongs to a drill-down's member set
///
/// `Ai` and `ToolCall` entries carry their producing agent and filter on it
/// directly; `ToolLog` and `ToolResult` entries carry only their call id, so
/// they correlate through the owned-call set the matching `ToolCall`s fill.
/// User messages and unattributed entries are run-wide, not per-agent.
///
/// # Arguments
///
/// * `entry` - The shared-context entry to test
/// * `members` - The agent display names being inspected
/// * `owned` - The tool-call ids owned by members, updated as calls match
fn drill_matches(entry: &ChatEntry, members: &BTreeSet<String>, owned: &mut HashSet<Uuid>) -> bool {
    match entry {
        // an attributed message belongs to its producing agent
        ChatEntry::Ai {
            agent: Some(agent), ..
        } => members.contains(agent),
        // a member's tool call also claims its id for log/result correlation
        ChatEntry::ToolCall {
            id,
            agent: Some(agent),
            ..
        } => {
            if members.contains(agent) {
                owned.insert(*id);
                true
            } else {
                false
            }
        }
        // logs and results follow the call that owns their id
        ChatEntry::ToolLog { id, .. } | ChatEntry::ToolResult { id, .. } => owned.contains(id),
        // user messages and unattributed entries are run-wide
        _ => false,
    }
}

/// Summarizes a member set's tallies into one header line
///
/// # Arguments
///
/// * `state` - The reduced stats state to read
/// * `members` - The agent display names being inspected
/// * `cost` - The session's cost estimator, when its model is priceable
/// * `now` - The current wall clock in milliseconds since the epoch
fn summarize(
    state: &StatsState,
    members: &BTreeSet<String>,
    cost: Option<&CostModel>,
    now: Option<u64>,
) -> String {
    // fold every member's tallies together
    let stats: Vec<&AgentStats> = members
        .iter()
        .filter_map(|member| state.agents.get(member))
        .collect();
    let calls: u64 = stats.iter().map(|stats| stats.tool_calls).sum();
    let turns: u64 = stats.iter().map(|stats| stats.turns).sum();
    // sum the members' metric values where they are known
    let mut segments = Vec::new();
    if let Some(cost) = cost {
        let dollars: f64 = stats
            .iter()
            .map(|s| cost.cost(s.input, s.output, s.cached, s.cache_creation))
            .sum();
        segments.push(super::state::fmt_cost(dollars));
    }
    segments.push(format!("{} calls", with_thousands(calls)));
    segments.push(format!("{} turns", with_thousands(turns)));
    // the wall-clock window spans the earliest start to the latest end
    let first = stats.iter().filter_map(|stats| stats.first_ts).min();
    let last = stats
        .iter()
        .filter_map(|stats| {
            if stats.busy_calls > 0 {
                now.or(stats.last_ts)
            } else {
                stats.last_ts
            }
        })
        .max();
    if let (Some(first), Some(last)) = (first, last) {
        segments.push(super::state::fmt_duration(last.saturating_sub(first)));
    }
    segments.join(" • ")
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

    use super::super::state::StatsEvent;

    /// Builds a key press event for a key code
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// Builds a key press event for a character
    fn press(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }

    /// Builds a stats log with a swarm, two members, and the main agent
    fn seeded_log() -> AgentStatsLog {
        let log = AgentStatsLog::default();
        log.push(StatsEvent::SwarmStarted {
            swarm: "Swarm(Io)".to_owned(),
            goal: "answer questions".to_owned(),
            ts: Some(10),
        });
        for (member, calls) in [("ReverseEngineer(Europa)", 3), ("Triage(Callisto)", 1)] {
            log.push(StatsEvent::AgentStarted {
                agent: member.to_owned(),
                role: None,
                swarm: Some("Swarm(Io)".to_owned()),
                ts: Some(20),
            });
            for call in 0..calls {
                log.push(StatsEvent::ToolCall {
                    agent: member.to_owned(),
                    tool: "decomp_function".to_owned(),
                    args_est: 4,
                    ts: Some(30 + call),
                });
                log.push(StatsEvent::ToolResult {
                    agent: member.to_owned(),
                    tool: "decomp_function".to_owned(),
                    success: true,
                    result_est: 100,
                    ts: Some(40 + call),
                });
            }
        }
        log.push(StatsEvent::Usage {
            agent: "main".to_owned(),
            input: 1_000,
            output: 100,
            cached: 0,
            cache_creation: 0,
            ts: Some(50),
        });
        log
    }

    /// The view renders flat, grouped, selected, and tiny-terminal frames
    #[test]
    fn view_renders_all_modes() {
        let mut view = AgentStatsView::new(seeded_log(), ChatContext::default(), None);
        view.toggle();
        assert!(view.open);
        // without a cost model the view opens on tool calls
        assert_eq!(view.metric, Metric::ToolCalls);
        // draw the flat view on a headless terminal
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .expect("flat frame");
        // the busiest member leads the bar list
        let content: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(content.contains("ReverseEngineer(Europa)"));
        assert!(content.contains("Context spend"));
        // grouped mode renders swarm headers and the ungrouped divider
        assert_eq!(view.handle_key(press('g')), ViewOutcome::None);
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .expect("grouped frame");
        let content: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(content.contains("Swarm(Io)"));
        assert!(content.contains("(no swarm)"));
        // Tab selects the first selectable row (the swarm header)
        assert_eq!(view.handle_key(key(KeyCode::Tab)), ViewOutcome::None);
        assert_eq!(view.selected, Some(RowId::Swarm("Swarm(Io)".to_owned())));
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .expect("selected frame");
        // a tiny terminal (side panel hidden) renders without panicking
        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).expect("small terminal");
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .expect("small frame");
        // Esc clears the selection first, then closes the overlay
        assert_eq!(view.handle_key(key(KeyCode::Esc)), ViewOutcome::None);
        assert!(view.selected.is_none());
        assert_eq!(view.handle_key(key(KeyCode::Esc)), ViewOutcome::Close);
    }

    /// Metric cycling and the grouping toggle drop stale swarm selections
    #[test]
    fn metric_cycles_and_grouping_drops_swarm_selection() {
        let mut view = AgentStatsView::new(seeded_log(), ChatContext::default(), None);
        view.toggle();
        // the metric cycles forward and backward
        assert_eq!(view.handle_key(press('m')), ViewOutcome::None);
        assert_eq!(view.metric, Metric::InputTokens);
        assert_eq!(view.handle_key(key(KeyCode::Left)), ViewOutcome::None);
        assert_eq!(view.metric, Metric::ToolCalls);
        // select the swarm header in grouped mode
        assert_eq!(view.handle_key(press('g')), ViewOutcome::None);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .expect("grouped frame");
        assert_eq!(view.handle_key(key(KeyCode::Tab)), ViewOutcome::None);
        assert_eq!(view.selected, Some(RowId::Swarm("Swarm(Io)".to_owned())));
        // flattening drops the swarm-header selection, which cannot exist flat
        assert_eq!(view.handle_key(press('g')), ViewOutcome::None);
        assert!(view.selected.is_none());
    }

    /// Entering a selection opens a drill-down filtered to its members
    #[test]
    fn drill_down_filters_the_shared_context() {
        // a shared context interleaving two agents' entries
        let context = ChatContext::default();
        context.add_user("analyze this");
        context.add_ai_from("found it", Some("ReverseEngineer(Europa)".to_owned()));
        let europa_call = Uuid::new_v4();
        context.add_tool_call(
            europa_call,
            Some("ReverseEngineer(Europa)".to_owned()),
            "decomp_function",
            None,
        );
        context.add_tool_log(europa_call, "[Ghidra] decompiling");
        context.add_tool_result(europa_call, "void entry(void) {}", Some(true));
        let main_call = Uuid::new_v4();
        context.add_tool_call(main_call, Some("main".to_owned()), "list_funcs", None);
        context.add_tool_result(main_call, "entry", Some(true));
        let mut view = AgentStatsView::new(seeded_log(), context, None);
        view.toggle();
        // select the busiest agent in the flat view and enter its chat
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .expect("flat frame");
        assert_eq!(view.handle_key(key(KeyCode::Tab)), ViewOutcome::None);
        assert_eq!(
            view.selected,
            Some(RowId::Agent("ReverseEngineer(Europa)".to_owned()))
        );
        assert_eq!(view.handle_key(key(KeyCode::Enter)), ViewOutcome::None);
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .expect("drill frame");
        // only Europa's message, call, log, and result made the filtered copy
        let drill = view.drill.as_ref().expect("open drill-down");
        assert_eq!(drill.entries.len(), 4);
        assert!(
            drill
                .entries
                .iter()
                .all(|entry| !matches!(entry, ChatEntry::User(_)))
        );
        // the main agent's call was filtered out along with its result
        assert!(!drill.owned_calls.contains(&main_call));
        assert!(drill.owned_calls.contains(&europa_call));
        // Esc backs out to the bars, keeping the selection
        assert_eq!(view.handle_key(key(KeyCode::Esc)), ViewOutcome::None);
        assert!(view.drill.is_none());
        assert!(view.selected.is_some());
    }

    /// A swarm selection drills into its members' interleaved chat
    #[test]
    fn swarm_drill_down_interleaves_members() {
        // both members spoke in the shared context
        let context = ChatContext::default();
        context.add_ai_from("europa here", Some("ReverseEngineer(Europa)".to_owned()));
        context.add_ai_from("callisto here", Some("Triage(Callisto)".to_owned()));
        context.add_ai_from("main here", Some("main".to_owned()));
        let mut view = AgentStatsView::new(seeded_log(), context, None);
        view.toggle();
        // select the swarm header in grouped mode and enter it
        assert_eq!(view.handle_key(press('g')), ViewOutcome::None);
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .expect("grouped frame");
        assert_eq!(view.handle_key(key(KeyCode::Tab)), ViewOutcome::None);
        assert_eq!(view.handle_key(key(KeyCode::Enter)), ViewOutcome::None);
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .expect("swarm drill frame");
        // both members' messages interleave; the main agent's is excluded
        let drill = view.drill.as_ref().expect("open drill-down");
        assert_eq!(drill.entries.len(), 2);
        assert_eq!(drill.members.len(), 2);
    }
}
