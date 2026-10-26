//! The full-screen traversal graph overlay
//!
//! Toggled from the space-command menu, this takes the whole terminal and
//! renders the traversal graph on a pannable, zoomable canvas: functions as
//! labeled nodes, call edges as braille lines, per-agent cursors in stable
//! palette colors, and a right-hand side strip listing the agents and the
//! recent non-function-scoped ("global") tool calls. The view renders either
//! the live graph or a replay-from-the-start driven by the app's refresh
//! ticker.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::canvas::{Canvas, Line as CanvasLine};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use super::super::theme::Theme;
use super::layout::Layout as GraphLayout;
use super::log::TraversalLog;
use super::replay::ReplayController;
use super::state::{FnNode, GraphState, NodeId, ToolStats};

/// The narrowest terminal that fits the graph beside the side strip
const SIDE_STRIP_MIN_WIDTH: u16 = 70;

/// The side strip's width in columns
const SIDE_STRIP_WIDTH: u16 = 32;

/// The world width a node label occupies before the next layer starts
const LABEL_WORLD_WIDTH: f64 = 16.0;

/// The visit count at which a node's label grows a `xN` heat badge
const HEAT_BADGE_MIN_VISITS: u32 = 3;

/// What a key press on the open view asks the owner to do
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewOutcome {
    /// The view handled the key itself
    None,
    /// The view wants to be closed
    Close,
}

/// Whether the view renders the live graph or a replay of it
enum ViewMode {
    /// Render the log's materialized live graph
    Live,
    /// Render a replay folded up to the controller's cursor
    Replay(ReplayController),
}

/// The pannable, zoomable camera over the graph's world coordinates
#[derive(Debug, Clone, Copy)]
struct Viewport {
    /// The world point at the center of the view
    center: (f64, f64),
    /// Half the world width currently visible
    half_width: f64,
    /// Whether the camera refits itself to the displayed nodes every frame,
    /// following the newest ones once the graph outgrows the legible zoom
    auto_fit: bool,
}

impl Default for Viewport {
    /// A camera fitted to nothing yet, following nodes as the graph appears
    fn default() -> Self {
        Viewport {
            center: (0.0, 0.0),
            half_width: 40.0,
            auto_fit: true,
        }
    }
}

/// The full-screen traversal graph overlay
pub struct TraversalView {
    /// Whether the overlay is currently shown
    pub open: bool,
    /// The shared traversal log this view renders
    log: TraversalLog,
    /// The incremental layout of the rendered graph
    layout: GraphLayout,
    /// The camera over the graph's world coordinates
    viewport: Viewport,
    /// Whether the view renders live state or a replay
    mode: ViewMode,
    /// Set while a replay is playing, so the app ticker keeps redrawing
    animating: Arc<AtomicBool>,
    /// Whether the right-hand agent/tool strip is shown
    show_side: bool,
    /// Whether node colors encode visit frequency only (heat mode)
    heat_mode: bool,
    /// The node whose stats the side strip shows, when one is selected
    selected: Option<NodeId>,
    /// The visual (top-to-bottom, left-to-right) node order Tab cycles
    /// through, with each node's world position, rebuilt every frame
    node_order: Vec<(NodeId, (f64, f64))>,
    /// The theme supplying the graph's styles
    theme: Theme,
}

impl TraversalView {
    /// Creates a view over a shared traversal log
    ///
    /// # Arguments
    ///
    /// * `log` - The shared traversal log to render
    pub fn new(log: TraversalLog) -> Self {
        TraversalView {
            open: false,
            log,
            layout: GraphLayout::default(),
            viewport: Viewport::default(),
            mode: ViewMode::Live,
            animating: Arc::new(AtomicBool::new(false)),
            show_side: true,
            heat_mode: false,
            selected: None,
            node_order: Vec::new(),
            theme: Theme::default(),
        }
    }

    /// A handle to the flag that is set while a replay animation is playing
    pub fn animating(&self) -> Arc<AtomicBool> {
        self.animating.clone()
    }

    /// Opens or closes the overlay
    pub fn toggle(&mut self) {
        self.open = !self.open;
        // a closed view stops animating and drops any replay and selection
        if !self.open {
            self.mode = ViewMode::Live;
            self.selected = None;
            self.animating.store(false, Ordering::Relaxed);
        }
    }

    /// Handles a key press while the overlay is open
    ///
    /// # Arguments
    ///
    /// * `key` - The key event to handle
    pub fn handle_key(&mut self, key: KeyEvent) -> ViewOutcome {
        // pan distance scales with the current zoom so it feels constant
        let pan = self.viewport.half_width * 0.15;
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
            // cycle the selection through the visual node order
            KeyCode::Tab => self.cycle_selection(1),
            KeyCode::BackTab => self.cycle_selection(-1),
            // pan the camera, pinning it until a refit is asked for
            KeyCode::Char('h') | KeyCode::Left => self.pan(-pan, 0.0),
            KeyCode::Char('l') | KeyCode::Right => self.pan(pan, 0.0),
            KeyCode::Char('k') | KeyCode::Up => self.pan(0.0, pan * 0.5),
            KeyCode::Char('j') | KeyCode::Down => self.pan(0.0, -pan * 0.5),
            // zoom around the center, pinning the camera
            KeyCode::Char('+') | KeyCode::Char('=') => self.zoom(0.8),
            KeyCode::Char('-') => self.zoom(1.25),
            // refit the camera and keep it following the displayed nodes
            KeyCode::Char('0') => self.viewport.auto_fit = true,
            // restart (or start) a replay from the very first event
            KeyCode::Char('r') => {
                let mut replay = ReplayController::new(&self.log);
                replay.restart();
                self.mode = ViewMode::Replay(replay);
                // a replay rebuilds from one node, so follow it from the start
                self.viewport.auto_fit = true;
                // the replay drives frames through the app's refresh ticker
                self.animating.store(true, Ordering::Relaxed);
            }
            // pause or resume a running replay
            KeyCode::Char('p') | KeyCode::Char(' ') => {
                if let ViewMode::Replay(replay) = &mut self.mode {
                    replay.playing = !replay.playing;
                    self.animating.store(replay.playing, Ordering::Relaxed);
                }
            }
            // step a paused replay one event forward
            KeyCode::Char('n') => {
                if let ViewMode::Replay(replay) = &mut self.mode {
                    if !replay.playing {
                        replay.step(&self.log, 1);
                    }
                }
            }
            // halve or double the replay speed
            KeyCode::Char('<') => {
                if let ViewMode::Replay(replay) = &mut self.mode {
                    replay.slower();
                }
            }
            KeyCode::Char('>') => {
                if let ViewMode::Replay(replay) = &mut self.mode {
                    replay.faster();
                }
            }
            // jump back to the live/latest state
            KeyCode::Char('G') | KeyCode::End => {
                self.mode = ViewMode::Live;
                self.animating.store(false, Ordering::Relaxed);
            }
            // toggle heat mode (color by visit frequency only)
            KeyCode::Char('H') => self.heat_mode = !self.heat_mode,
            // toggle the side strip
            KeyCode::Char('t') => self.show_side = !self.show_side,
            _ => (),
        }
        ViewOutcome::None
    }

    /// Moves the selection one step through the visual node order
    ///
    /// # Arguments
    ///
    /// * `step` - The direction to move: positive forward, negative backward
    fn cycle_selection(&mut self, step: isize) {
        // an empty graph has nothing to select
        if self.node_order.is_empty() {
            return;
        }
        // find where the current selection sits in the visual order
        let current = self
            .selected
            .as_ref()
            .and_then(|id| self.node_order.iter().position(|(other, _)| other == id));
        // step from the current slot, or start at an end when none is selected
        let next = match current {
            Some(index) => {
                let len = self.node_order.len() as isize;
                (index as isize + step).rem_euclid(len) as usize
            }
            None if step < 0 => self.node_order.len() - 1,
            None => 0,
        };
        let (id, (x, y)) = self.node_order[next].clone();
        self.selected = Some(id);
        // center the camera on the selection (world y is negated when drawn)
        self.viewport.auto_fit = false;
        self.viewport.center = (x + LABEL_WORLD_WIDTH / 2.0, -y);
    }

    /// Zooms the camera from a mouse wheel event
    ///
    /// # Arguments
    ///
    /// * `up` - Whether the wheel scrolled up (zoom in) or down (zoom out)
    pub fn handle_scroll(&mut self, up: bool) {
        // wheel up zooms in, wheel down zooms out
        self.zoom(if up { 0.8 } else { 1.25 });
    }

    /// Pans the camera by a world-space delta, disabling auto-fit
    ///
    /// # Arguments
    ///
    /// * `dx` - The horizontal world distance to pan
    /// * `dy` - The vertical world distance to pan
    fn pan(&mut self, dx: f64, dy: f64) {
        self.viewport.auto_fit = false;
        self.viewport.center.0 += dx;
        self.viewport.center.1 += dy;
    }

    /// Zooms the camera by a factor, disabling auto-fit
    ///
    /// # Arguments
    ///
    /// * `factor` - The multiplier on the visible world width (< 1 zooms in)
    fn zoom(&mut self, factor: f64) {
        self.viewport.auto_fit = false;
        // clamp the zoom so the camera can neither vanish nor fly away
        self.viewport.half_width = (self.viewport.half_width * factor).clamp(5.0, 10_000.0);
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
        // advance a playing replay by the wall time since the last frame
        if let ViewMode::Replay(replay) = &mut self.mode {
            replay.tick(&self.log, Instant::now());
            // a replay that caught up with the log pauses itself
            if replay.cursor >= self.log.len() && replay.playing {
                replay.playing = false;
                self.animating.store(false, Ordering::Relaxed);
            }
        }
        // pick the graph to draw: the live snapshot or the replay's fold
        let state = match &self.mode {
            ViewMode::Live => self.log.snapshot(),
            ViewMode::Replay(replay) => replay.state.clone(),
        };
        // a name-selected node may have merged into an address node since
        if let Some(selected @ NodeId::Name(name)) = &self.selected
            && !state.nodes.contains_key(selected)
            && let Some(addr) = state.aliases.get(name)
        {
            self.selected = Some(NodeId::Addr(addr.clone()));
        }
        // overlay the enrichment edges that touch nodes this state holds
        let enriched: std::collections::BTreeSet<_> = self
            .log
            .enriched()
            .into_iter()
            .filter(|(from, to)| state.nodes.contains_key(from) && state.nodes.contains_key(to))
            .collect();
        // fold any new nodes/edges into the incremental layout
        self.layout.update(&state, &enriched);
        let positions = self.layout.positions(&state);
        // rebuild the visual order Tab cycles through (top-down, left-right)
        self.node_order = positions
            .iter()
            .map(|(id, pos)| (id.clone(), *pos))
            .collect();
        self.node_order
            .sort_by(|(_, a), (_, b)| a.1.total_cmp(&b.1).then(a.0.total_cmp(&b.0)));
        // split off the one-line footer, then the side strip when it fits
        let [body, footer] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);
        let (graph_area, side_area) = if self.show_side && area.width >= SIDE_STRIP_MIN_WIDTH {
            let [graph, side] =
                Layout::horizontal([Constraint::Min(0), Constraint::Length(SIDE_STRIP_WIDTH)])
                    .areas(body);
            (graph, Some(side))
        } else {
            (body, None)
        };
        // render the three panes
        self.render_graph(frame, graph_area, &state, &enriched, &positions);
        if let Some(side_area) = side_area {
            self.render_side(frame, side_area, &state);
        }
        self.render_footer(frame, footer, &state);
    }

    /// Renders the graph canvas pane
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render to
    /// * `area` - The pane the canvas fills
    /// * `state` - The graph state being drawn
    /// * `enriched` - The enrichment edges to overlay
    /// * `positions` - The world position of every placed node
    fn render_graph(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        state: &GraphState,
        enriched: &std::collections::BTreeSet<(NodeId, NodeId)>,
        positions: &std::collections::HashMap<NodeId, (f64, f64)>,
    ) {
        // an empty graph shows a hint instead of a blank canvas
        if positions.is_empty() {
            let hint = Paragraph::new(Line::from(
                "No function-scoped tool calls yet — the graph grows as agents explore",
            ))
            .style(self.theme.graph.node_discovered)
            .block(Block::default().borders(Borders::ALL).title(" Traversal "))
            .centered();
            frame.render_widget(hint, area);
            return;
        }
        // refit the camera to the displayed nodes while auto-fit is on
        let inner_w = f64::from(area.width.max(1));
        if self.viewport.auto_fit {
            self.fit(state, positions, area);
        }
        // cap zoom-out so labels always stay legible; pan to see the rest
        let max_half_width = inner_w * MAX_UNITS_PER_COL / 2.0;
        self.viewport.half_width = self.viewport.half_width.min(max_half_width);
        // compute the world bounds the camera shows over this pane; terminal
        // cells are about twice as tall as wide, so a row covers twice the
        // world distance a column does, keeping the layout visually square
        let inner_h = f64::from(area.height.max(1));
        let units_per_col = (self.viewport.half_width * 2.0) / inner_w;
        let half_height = (inner_h * units_per_col * 2.0) / 2.0;
        let x_bounds = [
            self.viewport.center.0 - self.viewport.half_width,
            self.viewport.center.0 + self.viewport.half_width,
        ];
        let y_bounds = [
            self.viewport.center.1 - half_height,
            self.viewport.center.1 + half_height,
        ];
        // the hottest node scales the heat ramp
        let max_visits = state.max_visits();
        let graph_theme = &self.theme.graph;
        let canvas = Canvas::default()
            .block(Block::default().borders(Borders::ALL).title(" Traversal "))
            .marker(Marker::Braille)
            .x_bounds(x_bounds)
            .y_bounds(y_bounds)
            .paint(|ctx| {
                // draw the reduced edges, brightening fully-traversed ones
                for (from, to) in &state.edges {
                    let (Some(a), Some(b)) = (positions.get(from), positions.get(to)) else {
                        continue;
                    };
                    // an edge is traversed once both of its ends were visited
                    let traversed = state.nodes.get(from).is_some_and(|n| n.visits > 0)
                        && state.nodes.get(to).is_some_and(|n| n.visits > 0);
                    let color = if traversed {
                        graph_theme.edge_traversed
                    } else {
                        graph_theme.edge
                    };
                    ctx.draw(&CanvasLine {
                        x1: a.0,
                        y1: -a.1,
                        x2: b.0,
                        y2: -b.1,
                        color,
                    });
                }
                // draw the enrichment overlay dimmest, skipping known edges
                for edge in enriched {
                    if state.edges.contains(edge) {
                        continue;
                    }
                    let (Some(a), Some(b)) = (positions.get(&edge.0), positions.get(&edge.1))
                    else {
                        continue;
                    };
                    ctx.draw(&CanvasLine {
                        x1: a.0,
                        y1: -a.1,
                        x2: b.0,
                        y2: -b.1,
                        color: graph_theme.edge,
                    });
                }
                // draw every node as its label, highlighting the selection
                for (id, pos) in positions {
                    let Some(node) = state.nodes.get(id) else {
                        continue;
                    };
                    let selected = self.selected.as_ref() == Some(id);
                    let line = self.node_line(state, node, max_visits, selected);
                    ctx.print(pos.0, -pos.1, line);
                }
            });
        frame.render_widget(canvas, area);
    }

    /// Builds the rendered line for one node, cursors and badges included
    ///
    /// # Arguments
    ///
    /// * `state` - The graph state being drawn
    /// * `node` - The node to render
    /// * `max_visits` - The hottest visit count, scaling the heat ramp
    /// * `selected` - Whether this node is the current selection
    fn node_line(
        &self,
        state: &GraphState,
        node: &FnNode,
        max_visits: u32,
        selected: bool,
    ) -> Line<'static> {
        let graph_theme = &self.theme.graph;
        // pick the node's base style: heat mode colors purely by frequency,
        // normal mode colors by what happened to the function
        let heat = heat_bucket(node.visits, max_visits);
        let mut style = if self.heat_mode {
            graph_theme.heat_ramp[heat]
        } else if node.unresolved {
            graph_theme.node_unresolved
        } else if node.examined {
            graph_theme.node_examined
        } else if node.visits > 0 {
            graph_theme.node_visited
        } else {
            graph_theme.node_discovered
        };
        // the selected node pops out of the graph via reverse video
        if selected {
            style = style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
        }
        // the agents currently standing on this node show as colored cursors
        let mut spans: Vec<Span<'static>> = Vec::new();
        for cursor in state.agents.values() {
            if cursor.at.as_ref() == Some(&node.id) {
                let color =
                    graph_theme.agent_palette[cursor.color_slot % graph_theme.agent_palette.len()];
                spans.push(Span::styled("▶", Style::default().fg(color)));
            }
        }
        // the label itself, truncated to its layer cell
        let mut label = node.label.clone();
        if label.len() > LABEL_WORLD_WIDTH as usize {
            label.truncate(LABEL_WORLD_WIDTH as usize - 1);
            label.push('…');
        }
        spans.push(Span::styled(label, style));
        // annotate recursion, mutations, hidden neighbors, and failures
        if node.recursive {
            spans.push(Span::styled("↺", style));
        }
        if node.mutated && !self.heat_mode {
            spans.push(Span::styled("*", graph_theme.node_mutated));
        }
        if node.unresolved && !self.heat_mode {
            spans.push(Span::styled("?", graph_theme.node_unresolved));
        }
        if node.hidden_neighbors > 0 {
            spans.push(Span::styled(
                format!("+{}", node.hidden_neighbors),
                graph_theme.node_discovered,
            ));
        }
        // hot nodes grow a visit-count badge styled by the heat ramp
        if node.visits >= HEAT_BADGE_MIN_VISITS {
            spans.push(Span::styled(
                format!(" ×{}", node.visits),
                graph_theme.heat_ramp[heat],
            ));
        }
        Line::from(spans)
    }

    /// Renders the right-hand side strip: the agent legend, then either the
    /// selected node's stats or the global tool counts and recent tool feed
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render to
    /// * `area` - The side strip's area
    /// * `state` - The graph state being drawn
    fn render_side(&self, frame: &mut Frame, area: Rect, state: &GraphState) {
        let graph_theme = &self.theme.graph;
        // a selected node (present in this state) owns the lower strip
        let selected = self.selected.as_ref().and_then(|id| state.nodes.get(id));
        // the legend gets the top; the rest goes to the selected node's stats,
        // or to the global tool counts above the recent tool feed
        let agents_height = (state.agents.len() as u16 + 2).min(12);
        let (agents_area, counts_area, tools_area) = if let Some(node) = selected {
            // the selected panel replaces the counts and the feed entirely
            let [agents_area, selected_area] =
                Layout::vertical([Constraint::Length(agents_height), Constraint::Min(0)])
                    .areas(area);
            self.render_selected(frame, selected_area, state, node);
            (agents_area, None, None)
        } else {
            let [agents_area, counts_area, tools_area] = Layout::vertical([
                Constraint::Length(agents_height),
                Constraint::Length((state.tool_counts.len() as u16).min(10) + 2),
                Constraint::Min(0),
            ])
            .areas(area);
            (agents_area, Some(counts_area), Some(tools_area))
        };
        // the global per-tool call counts, busiest first
        if let Some(counts_area) = counts_area {
            self.render_tool_counts(frame, counts_area, state);
        }
        // one line per agent: swatch, name, then its current tool and function
        let mut agent_lines: Vec<Line> = Vec::new();
        for cursor in state.agents.values() {
            let color =
                graph_theme.agent_palette[cursor.color_slot % graph_theme.agent_palette.len()];
            let mut spans = vec![
                Span::styled("● ", Style::default().fg(color)),
                Span::styled(cursor.agent.clone(), Style::default().fg(color)),
            ];
            // show what the agent is doing and where
            if let Some(tool) = &cursor.tool {
                spans.push(Span::styled(
                    format!(" {tool}"),
                    graph_theme.node_discovered,
                ));
            }
            if let Some(at) = &cursor.at {
                if let Some(node) = state.nodes.get(at) {
                    spans.push(Span::styled(
                        format!(" @ {}", node.label),
                        graph_theme.node_visited,
                    ));
                }
            }
            // flag the last result, or that a call is still in flight
            match (cursor.busy, cursor.last_ok) {
                (true, _) => spans.push(Span::styled(" …", graph_theme.node_discovered)),
                (false, Some(true)) => spans.push(Span::styled(" ✓", self.theme.messages.tool_ok)),
                (false, Some(false)) => {
                    spans.push(Span::styled(" ✗", self.theme.messages.tool_err));
                }
                (false, None) => (),
            }
            agent_lines.push(Line::from(spans));
        }
        let agents = Paragraph::new(agent_lines)
            .block(Block::default().borders(Borders::ALL).title(" Agents "));
        frame.render_widget(agents, agents_area);
        // the global tool feed, newest first (hidden while a node is selected)
        if let Some(tools_area) = tools_area {
            let mut tool_lines: Vec<Line> = Vec::new();
            for call in state.global_tools.iter().rev() {
                let color = state
                    .agents
                    .get(&call.agent)
                    .map(|cursor| {
                        graph_theme.agent_palette
                            [cursor.color_slot % graph_theme.agent_palette.len()]
                    })
                    .unwrap_or(ratatui::style::Color::DarkGray);
                let status = match call.success {
                    Some(true) => Span::styled(" ✓", self.theme.messages.tool_ok),
                    Some(false) => Span::styled(" ✗", self.theme.messages.tool_err),
                    None => Span::styled(" …", graph_theme.node_discovered),
                };
                tool_lines.push(Line::from(vec![
                    Span::styled("● ", Style::default().fg(color)),
                    Span::styled(call.tool.clone(), self.theme.messages.tool),
                    status,
                ]));
            }
            let tools = Paragraph::new(tool_lines)
                .block(Block::default().borders(Borders::ALL).title(" Tools "));
            frame.render_widget(tools, tools_area);
        }
    }

    /// Renders the selected node's stats panel in the side strip
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render to
    /// * `area` - The panel's area
    /// * `state` - The graph state being drawn
    /// * `node` - The selected node to describe
    fn render_selected(&self, frame: &mut Frame, area: Rect, state: &GraphState, node: &FnNode) {
        let graph_theme = &self.theme.graph;
        let mut lines: Vec<Line> = Vec::new();
        // the header shows the visit count and the node's status flags
        let mut header = vec![Span::styled(
            format!("visits ×{}", node.visits),
            graph_theme.node_visited,
        )];
        if node.examined {
            header.push(Span::styled(" examined", graph_theme.node_examined));
        }
        if node.mutated {
            header.push(Span::styled(" mutated", graph_theme.node_mutated));
        }
        if node.unresolved {
            header.push(Span::styled(" unresolved", graph_theme.node_unresolved));
        }
        if node.recursive {
            header.push(Span::styled(" ↺", graph_theme.node_visited));
        }
        lines.push(Line::from(header));
        // one line per tool called on this function, busiest first
        let mut counts: Vec<(&String, &u32)> = node.tool_counts.iter().collect();
        counts.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        for (tool, count) in counts {
            lines.push(Line::from(vec![
                Span::styled(truncate_label(tool, 24), self.theme.messages.tool),
                Span::styled(format!(" ×{count}"), graph_theme.node_visited),
            ]));
        }
        // the agents that touched this node, in their palette colors
        for (agent, tools) in &node.agents {
            let color = state
                .agents
                .get(agent)
                .map(|cursor| {
                    graph_theme.agent_palette[cursor.color_slot % graph_theme.agent_palette.len()]
                })
                .unwrap_or(ratatui::style::Color::DarkGray);
            let joined = tools.iter().cloned().collect::<Vec<_>>().join(", ");
            lines.push(Line::from(vec![
                Span::styled("● ", Style::default().fg(color)),
                Span::styled(agent.clone(), Style::default().fg(color)),
                Span::styled(format!(" {joined}"), graph_theme.node_discovered),
            ]));
        }
        // the panel is titled with the selected node's label
        let title = format!(" {} ", truncate_label(&node.label, 26));
        let panel =
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title));
        frame.render_widget(panel, area);
    }

    /// Renders the global per-tool call-count panel in the side strip
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render to
    /// * `area` - The panel's area
    /// * `state` - The graph state being drawn
    fn render_tool_counts(&self, frame: &mut Frame, area: Rect, state: &GraphState) {
        // sort the tools busiest first, then alphabetically
        let mut counts: Vec<(&String, &ToolStats)> = state.tool_counts.iter().collect();
        counts.sort_by(|a, b| b.1.calls.cmp(&a.1.calls).then(a.0.cmp(b.0)));
        let mut lines: Vec<Line> = Vec::new();
        for (tool, stats) in counts {
            let mut spans = vec![
                Span::styled(truncate_label(tool, 18), self.theme.messages.tool),
                Span::styled(format!(" ×{}", stats.calls), self.theme.graph.node_visited),
            ];
            // show how the finished calls split, omitting empty sides
            if stats.ok > 0 {
                spans.push(Span::styled(
                    format!(" ✓{}", stats.ok),
                    self.theme.messages.tool_ok,
                ));
            }
            if stats.err > 0 {
                spans.push(Span::styled(
                    format!(" ✗{}", stats.err),
                    self.theme.messages.tool_err,
                ));
            }
            lines.push(Line::from(spans));
        }
        let panel = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Tool Counts "),
        );
        frame.render_widget(panel, area);
    }

    /// Renders the one-line footer: mode status plus the key hints
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render to
    /// * `area` - The footer's one-row area
    /// * `state` - The graph state being drawn
    fn render_footer(&self, frame: &mut Frame, area: Rect, state: &GraphState) {
        // describe the mode: live counts, or the replay's cursor and speed
        let mut status = match &self.mode {
            ViewMode::Live => format!(
                " LIVE • {} functions • {} edges",
                state.nodes.len(),
                state.edges.len()
            ),
            ViewMode::Replay(replay) => format!(
                " REPLAY {} • event {}/{} • {}",
                replay.speed_label(),
                replay.cursor,
                self.log.len(),
                if replay.playing { "playing" } else { "paused" }
            ),
        };
        // name the selected node, when this state knows it
        if let Some(node) = self.selected.as_ref().and_then(|id| state.nodes.get(id)) {
            status.push_str(&format!(" • sel {}", node.label));
        }
        let hints = " Tab select  r replay  p pause  </> speed  G live  H heat  t tools  hjkl pan  +/- zoom  0 follow  q close";
        let footer = Line::from(vec![
            Span::styled(status, Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(hints, self.theme.graph.node_discovered),
        ]);
        frame.render_widget(Paragraph::new(footer), area);
    }

    /// Fits the camera to the displayed nodes, newest first, up to the zoom cap
    ///
    /// While every displayed node fits at a legible zoom this frames them all.
    /// Once the graph outgrows the zoom-out cap it instead frames the largest
    /// most-recently-discovered suffix of the nodes that still fits, so the
    /// camera follows the frontier where new nodes appear rather than parking
    /// on the (often empty) center of the full bounding box.
    ///
    /// # Arguments
    ///
    /// * `state` - The graph state being drawn, supplying discovery order
    /// * `positions` - The world position of every placed node
    /// * `area` - The pane the camera projects into
    fn fit(
        &mut self,
        state: &GraphState,
        positions: &std::collections::HashMap<NodeId, (f64, f64)>,
        area: Rect,
    ) {
        // order the displayed nodes newest discovery first, breaking ties on
        // the id so the kept suffix is deterministic frame to frame
        let mut newest_first: Vec<(&NodeId, &(f64, f64))> = positions.iter().collect();
        newest_first.sort_by(|a, b| {
            let seq_a = state.nodes.get(a.0).map_or(0, |node| node.seq);
            let seq_b = state.nodes.get(b.0).map_or(0, |node| node.seq);
            seq_b.cmp(&seq_a).then_with(|| a.0.cmp(b.0))
        });
        // the zoom cap the render loop enforces; growing the framed box past
        // it would just push the newest nodes right back off screen
        let inner_w = f64::from(area.width.max(1));
        let inner_h = f64::from(area.height.max(1));
        let max_half_width = inner_w * MAX_UNITS_PER_COL / 2.0;
        // grow the framed bounding box newest first while it stays legible
        let mut min_x = f64::MAX;
        let mut max_x = f64::MIN;
        let mut min_y = f64::MAX;
        let mut max_y = f64::MIN;
        for (_, (x, y)) in newest_first {
            // the box this node would grow the frame to
            let cand_min_x = min_x.min(*x);
            // leave room for the label text that extends right of the node
            let cand_max_x = max_x.max(*x + LABEL_WORLD_WIDTH);
            let cand_min_y = min_y.min(*y);
            let cand_max_y = max_y.max(*y);
            // the half width the candidate box needs, honoring cell aspect: a
            // row covers twice a column's world distance, so the vertical
            // extent converts into the horizontal half-width that shows it
            let half_w = (cand_max_x - cand_min_x) / 2.0 + 2.0;
            let half_h = (cand_max_y - cand_min_y) / 2.0 + 2.0;
            let needed = half_w.max(half_h * inner_w / (inner_h * 2.0));
            // stop before an older node would zoom past the cap; the newest
            // node is always kept so the frame is never empty
            if needed > max_half_width && min_x != f64::MAX {
                break;
            }
            min_x = cand_min_x;
            max_x = cand_max_x;
            min_y = cand_min_y;
            max_y = cand_max_y;
        }
        // center the camera on the kept box (world y is negated when drawn)
        self.viewport.center = ((min_x + max_x) / 2.0, -(min_y + max_y) / 2.0);
        // pick the half width that fits both extents, honoring cell aspect
        let half_w = (max_x - min_x) / 2.0 + 2.0;
        let needed_for_height = {
            // a row covers twice a column's world distance, so convert the
            // vertical extent into the horizontal half-width that shows it
            let half_h = (max_y - min_y) / 2.0 + 2.0;
            half_h * inner_w / (inner_h * 2.0)
        };
        self.viewport.half_width = half_w.max(needed_for_height).max(5.0);
    }
}

/// The most world units one column may cover (the zoom-out cap)
///
/// A layer is 18 world units wide, so one unit per column keeps a 16-character
/// label inside its layer. It also puts the 2.0-unit slot spacing exactly one
/// terminal row apart (a row covers twice a column's world distance), so
/// stacked labels never collide and names are always readable.
const MAX_UNITS_PER_COL: f64 = 1.0;

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

/// Buckets a node's visit count onto the five-step heat ramp
///
/// # Arguments
///
/// * `visits` - The node's visit count
/// * `max_visits` - The hottest visit count in the graph
fn heat_bucket(visits: u32, max_visits: u32) -> usize {
    // a cold graph (or node) sits at the bottom of the ramp
    if visits == 0 || max_visits == 0 {
        return 0;
    }
    // scale the count onto the ramp's five buckets
    let ratio = f64::from(visits) / f64::from(max_visits);
    ((ratio * 5.0).ceil() as usize).clamp(1, 5) - 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::super::state::TraversalEvent;

    /// Heat buckets scale from cold to the hottest ramp step
    #[test]
    fn heat_buckets_scale_with_visits() {
        // discovered-only nodes stay on the coldest step
        assert_eq!(heat_bucket(0, 10), 0);
        // the hottest node lands on the hottest step
        assert_eq!(heat_bucket(10, 10), 4);
        // a middling node lands mid-ramp
        assert_eq!(heat_bucket(5, 10), 2);
        // an empty graph cannot divide by zero
        assert_eq!(heat_bucket(0, 0), 0);
    }

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

    /// A populated view renders in live, replay, and heat modes without panicking
    #[test]
    fn view_renders_all_modes() {
        // seed a log with a visit, an edge-bearing result, and a global tool
        let log = TraversalLog::default();
        log.push(TraversalEvent::ToolCall {
            agent: "main".to_owned(),
            call_id: "c1".to_owned(),
            tool: "callees".to_owned(),
            args: r#"{"target":"entry"}"#.to_owned(),
            ts: Some(0),
        });
        log.push(TraversalEvent::ToolResult {
            agent: "main".to_owned(),
            call_id: "c1".to_owned(),
            tool: "callees".to_owned(),
            result: r#"{"function":"entry","callees":[
                {"name":"helper","address":"00401200","call_site":"00401010","depth":0}
            ],"count":1}"#
                .to_owned(),
            success: true,
            ts: Some(10),
        });
        log.push(TraversalEvent::ToolCall {
            agent: "main".to_owned(),
            call_id: "c2".to_owned(),
            tool: "list_strings".to_owned(),
            args: "{}".to_owned(),
            ts: Some(20),
        });
        // overlay an enrichment edge between the two known nodes
        log.add_enriched([(
            NodeId::Addr("401200".to_owned()),
            NodeId::Name("entry".to_owned()),
        )]);
        let mut view = TraversalView::new(log);
        view.toggle();
        // draw the live view on a headless terminal
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .expect("live frame");
        // heat mode and a running replay render too
        assert_eq!(view.handle_key(press('H')), ViewOutcome::None);
        // a pan pins the camera; starting a replay re-arms the follow mode
        assert_eq!(view.handle_key(press('h')), ViewOutcome::None);
        assert!(!view.viewport.auto_fit);
        assert_eq!(view.handle_key(press('r')), ViewOutcome::None);
        assert!(view.viewport.auto_fit);
        assert!(view.animating().load(Ordering::Relaxed));
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .expect("replay frame");
        // a tiny terminal (side strip hidden) renders without panicking
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("small terminal");
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .expect("small frame");
        // q asks the owner to close, which clears the animation flag
        assert_eq!(view.handle_key(press('q')), ViewOutcome::Close);
        view.toggle();
        assert!(!view.animating().load(Ordering::Relaxed));
    }

    /// Zoomed fully out, labels stay readable and Tab drives selection
    #[test]
    fn labels_never_minimize_and_tab_selects() {
        // seed a log with one visited function
        let log = TraversalLog::default();
        log.push(TraversalEvent::ToolCall {
            agent: "main".to_owned(),
            call_id: "c1".to_owned(),
            tool: "decomp_function".to_owned(),
            args: r#"{"target":"entry"}"#.to_owned(),
            ts: Some(0),
        });
        let mut view = TraversalView::new(log);
        view.toggle();
        // hide the side strip so the buffer only holds the graph and footer
        assert_eq!(view.handle_key(press('t')), ViewOutcome::None);
        // zoom out far past the old dot threshold
        for _ in 0..20 {
            assert_eq!(view.handle_key(press('-')), ViewOutcome::None);
        }
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .expect("zoomed frame");
        // the node still renders its name, never a bare dot
        let content: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(content.contains("entry"));
        assert!(!content.contains('●'));
        // Tab selects the first node in the visual order built by the render
        assert_eq!(view.handle_key(key(KeyCode::Tab)), ViewOutcome::None);
        assert!(view.selected.is_some());
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .expect("selected frame");
        // Esc clears the selection first, then closes the overlay
        assert_eq!(view.handle_key(key(KeyCode::Esc)), ViewOutcome::None);
        assert!(view.selected.is_none());
        assert_eq!(view.handle_key(key(KeyCode::Esc)), ViewOutcome::Close);
    }

    /// A graph taller than the zoom cap keeps the newest nodes in view
    #[test]
    fn follow_keeps_recent_nodes_in_view() {
        // seed a log with many isolated functions so the stack outgrows the cap
        let log = TraversalLog::default();
        for i in 0..40 {
            log.push(TraversalEvent::ToolCall {
                agent: "main".to_owned(),
                call_id: format!("c{i}"),
                tool: "decomp_function".to_owned(),
                args: format!(r#"{{"target":"fn_{i:02}"}}"#),
                ts: Some(i as u64),
            });
        }
        let mut view = TraversalView::new(log);
        view.toggle();
        // hide the side strip so the whole buffer is the graph pane
        assert_eq!(view.handle_key(press('t')), ViewOutcome::None);
        // a 60x16 pane caps the zoom far below the forty-component stack
        let backend = TestBackend::new(60, 16);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .expect("capped frame");
        let content: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        // the newest function is framed; the oldest fell out of the window
        assert!(content.contains("fn_39"));
        assert!(!content.contains("fn_00"));
    }
}
