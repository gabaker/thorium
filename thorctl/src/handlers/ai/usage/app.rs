//! The standalone usage TUI shell: event loop, screen switching, and chrome
//!
//! Mirrors the chat TUI's loop shape -- a kanal event channel fed by the
//! shared terminal/SIGTERM forwarders, burst-drained into a single draw per
//! wakeup -- but drives no session and owns no worker: the only background
//! tasks are the read-only scanner and a one-second ticker that keeps live
//! rows and relative times fresh.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use kanal::{AsyncReceiver, AsyncSender};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{DefaultTerminal, Frame};

use crate::handlers::ai::chat::ViewOutcome;

use super::drill::{self, SessionDrill};
use super::model::{GlobalStats, SessionSummary};
use super::scan::{self, PricingCtx, ScanUpdate};
use super::views::agents::AgentsView;
use super::views::heatmap::HeatmapView;
use super::views::models::ModelsView;
use super::views::overview::OverviewView;
use super::views::sessions::SessionsView;
use super::views::timeline::TimelineView;
use super::views::trend::TrendView;

/// One event the usage TUI's loop handles
pub enum UsageEvent {
    /// A terminal event (keyboard, mouse, resize, etc.)
    Terminal(Event),
    /// An update from the read-only session scanner
    Scan(ScanUpdate),
    /// A requested drill-down finished loading
    DrillReady {
        /// The session the drill covers
        name: String,
        /// The loaded drill state
        drill: Box<SessionDrill>,
    },
    /// A requested drill-down failed to load
    DrillFailed {
        /// The session whose drill failed
        name: String,
        /// The human-readable error describing the failure
        error: String,
    },
    /// Quit the TUI gracefully (e.g. a SIGTERM asked the process to stop)
    Quit,
    /// Redraw the tui
    Redraw,
}

/// The screens the usage TUI can show
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// The windowed totals dashboard
    Overview,
    /// The sortable per-session table
    Sessions,
    /// The global usage bars (the chat Usage view's global sibling)
    Usage,
    /// The cross-session timeline (the chat Timeline view's global sibling)
    Timeline,
    /// The day/hour heatmap
    Heatmap,
    /// The per-day trend
    Trend,
    /// The per-model breakdown
    Models,
}

impl Screen {
    /// The screens in their numeric/cycling order
    pub const ALL: [Screen; 7] = [
        Screen::Overview,
        Screen::Sessions,
        Screen::Usage,
        Screen::Timeline,
        Screen::Heatmap,
        Screen::Trend,
        Screen::Models,
    ];

    /// The human-facing label for this screen
    pub fn label(self) -> &'static str {
        match self {
            Screen::Overview => "overview",
            Screen::Sessions => "sessions",
            Screen::Usage => "usage",
            Screen::Timeline => "timeline",
            Screen::Heatmap => "heatmap",
            Screen::Trend => "trend",
            Screen::Models => "models",
        }
    }

    /// The next screen in the cycle, wrapping at the end
    pub fn next(self) -> Screen {
        // find this screen's position and step forward with wraparound
        let pos = Screen::ALL.iter().position(|s| *s == self).unwrap_or(0);
        Screen::ALL[(pos + 1) % Screen::ALL.len()]
    }

    /// The previous screen in the cycle, wrapping at the start
    pub fn prev(self) -> Screen {
        // find this screen's position and step backward with wraparound
        let pos = Screen::ALL.iter().position(|s| *s == self).unwrap_or(0);
        Screen::ALL[(pos + Screen::ALL.len() - 1) % Screen::ALL.len()]
    }
}

/// Sends a redraw once a second so live rows and relative times stay fresh
///
/// # Arguments
///
/// * `sender` - The channel to request redraws over
async fn ticker(sender: AsyncSender<UsageEvent>) {
    // keep ticking for the life of the app
    loop {
        // sleep for 1s between frames; this TUI animates nothing faster
        tokio::time::sleep(Duration::from_secs(1)).await;
        // try to send a redraw event; a failure just means the app is gone
        if sender.send(UsageEvent::Redraw).await.is_err() {
            return;
        }
    }
}

/// The usage TUI's application state
pub struct UsageApp {
    /// The sender feeding the app's own event loop
    event_tx: AsyncSender<UsageEvent>,
    /// The receiver the app's event loop drains
    event_rx: AsyncReceiver<UsageEvent>,
    /// The sessions root being scanned
    root: PathBuf,
    /// The config's best-effort pricing context
    pricing: PricingCtx,
    /// The channel an `r` press nudges the scanner over
    rescan_tx: tokio::sync::mpsc::Sender<()>,
    /// The scanner's rescan receiver, taken by `start`
    rescan_rx: Option<tokio::sync::mpsc::Receiver<()>>,
    /// Every scanned session's summary, keyed by name
    sessions: BTreeMap<String, SessionSummary>,
    /// The cross-session aggregates the views render
    stats: GlobalStats,
    /// Whether the aggregates must rebuild before the next draw
    stats_dirty: bool,
    /// The initial scan's progress, until its first pass settles
    progress: Option<(usize, usize)>,
    /// The screen currently showing
    screen: Screen,
    /// The overview dashboard's view state
    overview: OverviewView,
    /// The sessions table's view state
    sessions_view: SessionsView,
    /// The global usage bars' view state
    agents_view: AgentsView,
    /// The cross-session timeline's view state
    timeline: TimelineView,
    /// The day/hour heatmap's view state
    heatmap: HeatmapView,
    /// The daily trend's view state
    trend: TrendView,
    /// The per-model breakdown's view state
    models: ModelsView,
    /// The open single-session drill-down, if any
    drill: Option<SessionDrill>,
    /// The session a drill-down is currently loading for, if any
    drill_loading: Option<String>,
    /// The last drill error, shown in the footer until the next action
    error: Option<String>,
    /// Set once the user asked to quit
    should_quit: bool,
}

impl UsageApp {
    /// Creates the app over a sessions root
    ///
    /// # Arguments
    ///
    /// * `root` - The sessions root to scan
    /// * `pricing` - The config's best-effort pricing context
    pub fn new(root: PathBuf, pricing: PricingCtx) -> Self {
        // the app's own event channel, shared with every background task
        let (event_tx, event_rx) = kanal::unbounded_async();
        // the nudge channel the refresh key pokes the scanner over
        let (rescan_tx, rescan_rx) = tokio::sync::mpsc::channel(1);
        UsageApp {
            event_tx,
            event_rx,
            root,
            pricing,
            rescan_tx,
            rescan_rx: Some(rescan_rx),
            sessions: BTreeMap::new(),
            stats: GlobalStats::default(),
            stats_dirty: false,
            progress: Some((0, 0)),
            screen: Screen::Overview,
            overview: OverviewView::default(),
            sessions_view: SessionsView::default(),
            agents_view: AgentsView::default(),
            timeline: TimelineView::default(),
            heatmap: HeatmapView::default(),
            trend: TrendView::default(),
            models: ModelsView::default(),
            drill: None,
            drill_loading: None,
            error: None,
            should_quit: false,
        }
    }

    /// Runs the app's event loop until the user quits
    ///
    /// # Arguments
    ///
    /// * `terminal` - The terminal to render frames into
    pub async fn start(&mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        // spawn the shared terminal and SIGTERM forwarders
        super::super::tui::spawn_forwarders(&self.event_tx, UsageEvent::Terminal, || {
            UsageEvent::Quit
        });
        // spawn the read-only scanner feeding summaries into this loop; the
        // receiver was created with the app, so a second start has none
        let Some(rescan_rx) = self.rescan_rx.take() else {
            return Ok(());
        };
        tokio::spawn(scan::run_scanner(
            self.root.clone(),
            self.pricing.clone(),
            self.event_tx.clone(),
            rescan_rx,
            // the usage TUI never needs the telemetry exporter's counters
            false,
            UsageEvent::Scan,
        ));
        // spawn the slow ticker keeping live rows and relative times fresh
        tokio::spawn(ticker(self.event_tx.clone()));
        // draw an initial frame until we get an event to handle
        terminal.draw(|frame| self.render(frame))?;
        // a non-blocking handle to the same event channel, used to coalesce a
        // burst of immediately-pending events into a single draw below
        let drain_rx = self.event_rx.clone_sync();
        while !self.should_quit {
            // get the next event to handle, blocking until one arrives
            match self.event_rx.recv().await {
                Ok(event) => {
                    // handle this event, then greedily drain any further
                    // events that are already queued so a flood of scanner
                    // updates collapses into one draw rather than one per event
                    self.handle_event(event);
                    while let Ok(Some(next)) = drain_rx.try_recv() {
                        // stop draining once asked to quit; the loop exits
                        if self.should_quit {
                            break;
                        }
                        self.handle_event(next);
                    }
                    // rebuild the aggregates once per draw when data changed
                    if self.stats_dirty {
                        self.stats = GlobalStats::rebuild(&self.sessions);
                        self.stats_dirty = false;
                    }
                    // draw a single frame reflecting all the events handled
                    terminal.draw(|frame| self.render(frame))?;
                }
                Err(_) => {
                    // Channel closed, exit
                    break;
                }
            }
        }
        Ok(())
    }

    /// Handles one event off the loop
    ///
    /// # Arguments
    ///
    /// * `event` - The event to handle
    fn handle_event(&mut self, event: UsageEvent) {
        match event {
            // keys and clicks route by what is open and showing
            UsageEvent::Terminal(Event::Key(key)) => {
                // ignore key releases/repeat chatter on terminals that send it
                if key.kind == KeyEventKind::Press {
                    self.handle_key(key);
                }
            }
            UsageEvent::Terminal(Event::Mouse(mouse)) => match mouse.kind {
                // wheel scrolls whatever is showing
                MouseEventKind::ScrollUp => self.handle_scroll(true),
                MouseEventKind::ScrollDown => self.handle_scroll(false),
                // clicks switch screens on the top bar or route into the drill
                MouseEventKind::Down(MouseButton::Left) => {
                    self.handle_click(mouse.column, mouse.row);
                }
                _ => {}
            },
            // resizes and pastes just fall through to the coming redraw
            UsageEvent::Terminal(_) => {}
            // fold a scanner update into the summary map
            UsageEvent::Scan(update) => self.handle_scan(update),
            // a drill finished loading; show it if it is still wanted
            UsageEvent::DrillReady { name, drill } => {
                if self.drill_loading.as_deref() == Some(name.as_str()) {
                    self.drill_loading = None;
                    self.drill = Some(*drill);
                }
            }
            // a drill failed; surface the error in the footer
            UsageEvent::DrillFailed { name, error } => {
                if self.drill_loading.as_deref() == Some(name.as_str()) {
                    self.drill_loading = None;
                }
                self.error = Some(format!("{name}: {error}"));
            }
            // quit gracefully
            UsageEvent::Quit => self.should_quit = true,
            // a redraw happens after every handled event anyway
            UsageEvent::Redraw => {}
        }
    }

    /// Folds one scanner update into the app's state
    ///
    /// # Arguments
    ///
    /// * `update` - The scanner update to fold
    fn handle_scan(&mut self, update: ScanUpdate) {
        match update {
            // initial-pass progress for the footer
            ScanUpdate::Progress { done, total } => {
                if self.progress.is_some() {
                    self.progress = Some((done, total));
                }
            }
            // a fresh summary replaces (or creates) its session's row
            ScanUpdate::Loaded(summary) => {
                self.sessions.insert(summary.name.clone(), *summary);
                self.stats_dirty = true;
            }
            // a vanished session drops out entirely
            ScanUpdate::Removed(name) => {
                self.sessions.remove(&name);
                self.stats_dirty = true;
            }
            // the first settled pass clears the progress readout for good
            ScanUpdate::PassDone => self.progress = None,
        }
    }

    /// Handles a key press by what is open and showing
    ///
    /// # Arguments
    ///
    /// * `key` - The key event to handle
    fn handle_key(&mut self, key: KeyEvent) {
        // any action clears a stale footer error
        self.error = None;
        // ctrl-c always quits, drill or not
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        // an open drill owns the keyboard; its own close returns to the table
        if let Some(drill) = &mut self.drill {
            if let ViewOutcome::Close = drill.handle_key(key) {
                self.drill = None;
            }
            return;
        }
        // escape cancels a still-loading drill instead of quitting the app
        if key.code == KeyCode::Esc && self.drill_loading.take().is_some() {
            return;
        }
        match key.code {
            // quit from the top level
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            // jump straight to a numbered screen, abandoning a pending drill
            KeyCode::Char(digit @ '1'..='7') => {
                let index = (digit as usize - '1' as usize).min(Screen::ALL.len() - 1);
                self.screen = Screen::ALL[index];
                self.drill_loading = None;
            }
            // cycle through the screens, abandoning a pending drill
            KeyCode::Tab => {
                self.screen = self.screen.next();
                self.drill_loading = None;
            }
            KeyCode::BackTab => {
                self.screen = self.screen.prev();
                self.drill_loading = None;
            }
            // nudge the scanner for an immediate rescan
            KeyCode::Char('r') => {
                let _ = self.rescan_tx.try_send(());
            }
            // everything else belongs to the showing view
            _ => {
                // a view returning a session name is asking for a drill-down
                let drill = match self.screen {
                    Screen::Overview => {
                        self.overview.handle_key(key);
                        None
                    }
                    Screen::Sessions => self.sessions_view.handle_key(key, &self.sessions),
                    Screen::Usage => self
                        .agents_view
                        .handle_key(key, &self.sessions, &self.stats),
                    Screen::Timeline => {
                        self.timeline.handle_key(key, &self.sessions);
                        None
                    }
                    Screen::Heatmap => {
                        self.heatmap.handle_key(key, &self.stats);
                        None
                    }
                    Screen::Trend => {
                        self.trend.handle_key(key, &self.stats);
                        None
                    }
                    Screen::Models => {
                        self.models.handle_key(key, &self.stats);
                        None
                    }
                };
                if let Some(name) = drill {
                    self.open_drill(name);
                }
            }
        }
    }

    /// Starts loading a session's drill-down in the background
    ///
    /// # Arguments
    ///
    /// * `name` - The session to drill into
    fn open_drill(&mut self, name: String) {
        // remember what is loading so a stale result cannot clobber a newer one
        self.drill_loading = Some(name.clone());
        // load off the loop so a 7 MB transcript never freezes the UI
        let root = self.root.clone();
        let pricing = self.pricing.clone();
        let events = self.event_tx.clone();
        tokio::spawn(async move {
            // load the drill and report however it went; a dropped channel
            // just means the app is gone
            match drill::load_drill(root, name.clone(), pricing).await {
                Ok(drill) => {
                    let _ = events
                        .send(UsageEvent::DrillReady {
                            name,
                            drill: Box::new(drill),
                        })
                        .await;
                }
                Err(error) => {
                    let _ = events.send(UsageEvent::DrillFailed { name, error }).await;
                }
            }
        });
    }

    /// Routes a wheel scroll to whatever is showing
    ///
    /// # Arguments
    ///
    /// * `up` - Whether the wheel scrolled up
    fn handle_scroll(&mut self, up: bool) {
        // an open drill owns the wheel
        if let Some(drill) = &mut self.drill {
            drill.handle_scroll(up);
            return;
        }
        match self.screen {
            Screen::Overview => self.overview.handle_scroll(up),
            // the list views move their identity-keyed selections, so they
            // need the data to resolve the current row
            Screen::Sessions => self.sessions_view.handle_scroll(up, &self.sessions),
            Screen::Usage => self
                .agents_view
                .handle_scroll(up, &self.sessions, &self.stats),
            Screen::Timeline => self.timeline.handle_scroll(up),
            Screen::Heatmap => self.heatmap.handle_scroll(up),
            Screen::Trend => self.trend.handle_scroll(up),
            Screen::Models => self.models.handle_scroll(up),
        }
    }

    /// Routes a click: the top bar switches screens, a drill takes the rest
    ///
    /// # Arguments
    ///
    /// * `x` - The click's column
    /// * `y` - The click's row
    fn handle_click(&mut self, x: u16, y: u16) {
        // clicks on the top bar switch screens even over a drill
        if y == 0 {
            // walk the same label layout the bar renders to find the target
            let mut cursor = 1u16;
            for screen in Screen::ALL {
                // each label occupies "N:label" plus two trailing spaces
                let width = 2 + screen.label().len() as u16 + 2;
                if x >= cursor && x < cursor + width {
                    self.screen = screen;
                    // leaving by click abandons the drill, loaded or pending
                    self.drill = None;
                    self.drill_loading = None;
                    return;
                }
                cursor += width;
            }
            return;
        }
        // an open drill owns every other click
        if let Some(drill) = &mut self.drill {
            drill.handle_click(x, y);
        }
    }

    /// Renders one frame: the screen bar, the body, and the footer
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    fn render(&mut self, frame: &mut Frame) {
        // carve the chrome off the body
        let [bar, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        self.render_bar(frame, bar);
        // an open drill owns the whole body; otherwise the active screen does
        if let Some(drill) = &mut self.drill {
            drill.render(frame, body);
        } else {
            match self.screen {
                Screen::Overview => self.overview.render(frame, body, &self.stats),
                Screen::Sessions => self.sessions_view.render(frame, body, &self.sessions),
                Screen::Usage => self
                    .agents_view
                    .render(frame, body, &self.sessions, &self.stats),
                Screen::Timeline => self
                    .timeline
                    .render(frame, body, &self.sessions, &self.stats),
                Screen::Heatmap => self.heatmap.render(frame, body, &self.stats),
                Screen::Trend => self.trend.render(frame, body, &self.stats),
                Screen::Models => self.models.render(frame, body, &self.stats),
            }
        }
        self.render_footer(frame, footer);
    }

    /// Renders the top screen-switcher bar
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The bar's one-line area
    fn render_bar(&self, frame: &mut Frame, area: Rect) {
        // highlight the active screen; everything else stays dim
        let active = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let idle = Style::default().fg(Color::DarkGray);
        let mut spans = vec![Span::raw(" ")];
        for (index, screen) in Screen::ALL.iter().enumerate() {
            let style = if *screen == self.screen { active } else { idle };
            spans.push(Span::styled(
                format!("{}:{}", index + 1, screen.label()),
                style,
            ));
            spans.push(Span::raw("  "));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    /// Renders the bottom status/help footer
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The footer's one-line area
    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        // the right side carries status: scan progress, then standing counts
        let status = if let Some(error) = &self.error {
            format!("error: {error} ")
        } else if let Some(name) = &self.drill_loading {
            format!("loading {name}… ")
        } else if let Some((done, total)) = self.progress {
            format!("scanning sessions {done}/{total}… ")
        } else {
            // the standing summary: sessions, live count, estimate marker
            let live = if self.stats.live_sessions > 0 {
                format!(" · {} live", self.stats.live_sessions)
            } else {
                String::new()
            };
            let estimated = if self.stats.estimated {
                " · ~ estimates"
            } else {
                ""
            };
            format!("{} sessions{live}{estimated} ", self.stats.total_sessions)
        };
        // the left side carries the showing screen's actual key bindings
        let help = if self.drill.is_some() {
            " u usage · t timeline · esc back · q quit"
        } else {
            match self.screen {
                Screen::Overview => " 1-7 views · tab cycle · j/k scroll · r rescan · q quit",
                Screen::Sessions => " 1-7 views · ↵ open · ←/→ sort · o order · j/k row · q quit",
                Screen::Usage => " 1-7 views · ↵ open · m metric · g group · j/k row · q quit",
                Screen::Timeline => " 1-7 views · ↵ expand · h/l pan · +/- zoom · 0 fit · q quit",
                Screen::Heatmap => " 1-7 views · m metric · g weekday · j/k days · q quit",
                Screen::Trend => " 1-7 views · m metric · j/k days · q quit",
                Screen::Models => " 1-7 views · j/k scroll · r rescan · q quit",
            }
        };
        let width = area.width as usize;
        let pad = width
            .saturating_sub(help.chars().count())
            .saturating_sub(status.chars().count());
        let line = Line::from(vec![
            Span::styled(help, Style::default().fg(Color::DarkGray)),
            Span::raw(" ".repeat(pad)),
            Span::styled(status, Style::default().fg(Color::Gray)),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }
}
