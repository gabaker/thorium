//! The telemetry explorer: six screens over bundles or a local store.
//!
//! `thorctl analyze telemetry explore BUNDLE...` merges mailed-in bundles and
//! cycles the fleet overview, tool league table, failure signatures, stuck
//! points, detector outcomes, and token economics over them; with no bundle
//! it walks the local sessions root instead, computing the same aggregates on
//! the fly (and only then offering the usage TUI's single-session drill-down,
//! since bundles carry no messages to replay, by design). Dollars are always
//! computed at render time from the local pricing table -- bundles and the
//! session store both carry raw token counts, so a pricing correction
//! retroactively fixes every view.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use kanal::{AsyncReceiver, AsyncSender};
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use thorium::{CtlConf, Error};
use thorpal::{SessionStore, UsageTotals};

use crate::args::Args;
use crate::args::ai::TelemetryExplore;
use crate::handlers::ai::chat::{ShortcutMenu, ViewOutcome, fmt_cost, with_thousands};
use crate::handlers::ai::pricing::{self, CostModel};
use crate::handlers::ai::tui;
use crate::handlers::ai::usage::drill::{SessionDrill, load_drill};
use crate::handlers::ai::usage::scan::{PricingCtx, ScanUpdate, run_scanner};
use crate::handlers::ai::usage::views::{clip, fill_bar};

use super::bundle::{Bundle, DetectorTally, EntryStatus, ForumEntry, MergedSession, merge};
use super::fold::{LocalForumDetail, assemble_session, percentile, read_forum_local};
use super::shell::{Popup, TelemetryTheme, sidebar_width, signature_popup};

/// Run the telemetry explorer over bundles or the local sessions root
///
/// # Arguments
///
/// * `args` - The base Thorctl arguments
/// * `explore` - The parsed `telemetry explore` command
pub async fn handle(args: &Args, explore: &TelemetryExplore) -> Result<(), Error> {
    // pricing is best-effort: without a usable config the cost figures thin
    let pricing = PricingCtx::from_conf(CtlConf::from_path(&args.config).ok());
    // resolve the source: merged bundle files, or the local sessions root
    let source = if explore.bundles.is_empty() {
        let root = match &explore.sessions_root {
            Some(root) => root.clone(),
            None => SessionStore::default_root().map_err(|err| Error::new(format!("{err:#}")))?,
        };
        Source::Local { root }
    } else {
        // read every bundle up front so a bad file fails before any screen
        let mut bundles = Vec::with_capacity(explore.bundles.len());
        for path in &explore.bundles {
            bundles.push(Bundle::read(path)?);
        }
        // the sidebar summarizes the fleet's provenance at a glance
        let levels = bundles.iter().map(|bundle| bundle.level).collect();
        let recipes = bundles.iter().map(Bundle::recipe).collect();
        let exporters = bundles
            .iter()
            .map(|bundle| bundle.tool_version.clone())
            .collect();
        Source::Bundles {
            rows: merge(&bundles),
            files: bundles.len(),
            levels,
            recipes,
            exporters,
        }
    };
    // route tracing to the shared log file before taking the screen
    tui::init_tracing_log(None);
    // take the screen only after everything that can fail has failed cleanly
    let mut terminal = tui::init_terminal()?;
    // build and run the app, capturing the result so teardown always runs
    let mut app = ExploreApp::new(source, pricing);
    let result = app.start(&mut terminal).await;
    // give the screen back before surfacing any error
    tui::restore_terminal();
    result?;
    Ok(())
}

/// What the explorer is exploring
enum Source {
    /// Merged bundle files, fixed at startup
    Bundles {
        /// The deduplicated session rows across every bundle
        rows: Vec<MergedSession>,
        /// How many bundle files were merged
        files: usize,
        /// The distinct levels the bundles were exported at
        levels: BTreeSet<u8>,
        /// The distinct signature recipes across the bundles
        recipes: BTreeSet<u32>,
        /// The distinct exporter versions across the bundles
        exporters: BTreeSet<String>,
    },
    /// The local sessions root, scanned live
    Local {
        /// The sessions root being scanned
        root: PathBuf,
    },
}

/// One event the explorer's loop handles
enum ExploreEvent {
    /// A terminal event (keyboard, mouse, resize, etc.)
    Terminal(Event),
    /// An update from the read-only session scanner (local mode only)
    Scan(ScanUpdate),
    /// A requested drill-down finished loading (local mode only)
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

/// The explorer's screens, in their numeric/cycling order
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    /// The fleet overview: totals, rates, and version/model splits
    Fleet,
    /// The per-tool league table, "offered, never called" included
    Tools,
    /// The level-1 failure-signature clusters
    Signatures,
    /// The stuck points: asks, deaths, pressure, stalls, forum end-state
    Stuck,
    /// The per-technique detector-lead outcomes
    Detectors,
    /// The token economics: cache savings, role shares, busiest hours
    Economics,
}

impl Screen {
    /// The screens in their numeric/cycling order
    const ALL: [Screen; 6] = [
        Screen::Fleet,
        Screen::Tools,
        Screen::Signatures,
        Screen::Stuck,
        Screen::Detectors,
        Screen::Economics,
    ];

    /// The human-facing label for this screen
    fn label(self) -> &'static str {
        match self {
            Screen::Fleet => "fleet",
            Screen::Tools => "tools",
            Screen::Signatures => "signatures",
            Screen::Stuck => "stuck",
            Screen::Detectors => "detectors",
            Screen::Economics => "economics",
        }
    }

    /// The next screen in the cycle, wrapping at the end
    fn next(self) -> Screen {
        // find this screen's position and step forward with wraparound
        let pos = Screen::ALL.iter().position(|s| *s == self).unwrap_or(0);
        Screen::ALL[(pos + 1) % Screen::ALL.len()]
    }

    /// The previous screen in the cycle, wrapping at the start
    fn prev(self) -> Screen {
        // find this screen's position and step backward with wraparound
        let pos = Screen::ALL.iter().position(|s| *s == self).unwrap_or(0);
        Screen::ALL[(pos + Screen::ALL.len() - 1) % Screen::ALL.len()]
    }
}

/// One explorable session row, however it arrived
struct ExploreRow {
    /// The session's local name, present only when exploring a local store
    name: Option<String>,
    /// The session's telemetry row
    session: MergedSession,
    /// The local-only forum prose for pop-outs, local stores only -- never
    /// part of the bundle row, by construction
    local: Option<LocalForumDetail>,
}

impl ExploreRow {
    /// The row's human-facing label: its local name, else its id prefix
    fn label(&self) -> String {
        // bundles carry no names, so the hashed id's prefix stands in
        self.name
            .clone()
            .unwrap_or_else(|| format!("{:.16}…", self.session.row.session_id))
    }
}

/// The explorer's application state
struct ExploreApp {
    /// The sender feeding the app's own event loop
    event_tx: AsyncSender<ExploreEvent>,
    /// The receiver the app's event loop drains
    event_rx: AsyncReceiver<ExploreEvent>,
    /// What is being explored
    source: Source,
    /// The config's best-effort pricing context
    pricing: PricingCtx,
    /// The scanner's rescan receiver, taken by `start` (local mode only)
    rescan_rx: Option<tokio::sync::mpsc::Receiver<()>>,
    /// The channel an `r` press nudges the scanner over (local mode only)
    rescan_tx: tokio::sync::mpsc::Sender<()>,
    /// The explorable rows, keyed for stable identity
    rows: BTreeMap<String, ExploreRow>,
    /// The initial scan's progress, until its first pass settles
    progress: Option<(usize, usize)>,
    /// The screen currently showing
    screen: Screen,
    /// Each screen's own cursor, indexed by the screen's position
    cursors: [usize; 6],
    /// Each screen's own scroll offset, indexed by the screen's position
    scrolls: [usize; 6],
    /// The open pop-out window, rendered over the screen and keyed first
    popup: Option<Popup>,
    /// The Space shortcut palette
    palette: ShortcutMenu,
    /// The shell's shared styles
    theme: TelemetryTheme,
    /// The harness-version filter the `f` key cycles, when one is active
    filter_version: Option<String>,
    /// The open single-session drill-down, if any (local mode only)
    drill: Option<SessionDrill>,
    /// The session a drill-down is currently loading for, if any
    drill_loading: Option<String>,
    /// The last error, shown in the footer until the next action
    error: Option<String>,
    /// Set once the user asked to quit
    should_quit: bool,
}

impl ExploreApp {
    /// Creates the explorer over a source
    ///
    /// # Arguments
    ///
    /// * `source` - What to explore
    /// * `pricing` - The config's best-effort pricing context
    fn new(source: Source, pricing: PricingCtx) -> Self {
        // the app's own event channel, shared with every background task
        let (event_tx, event_rx) = kanal::unbounded_async();
        // the nudge channel the refresh key pokes the scanner over
        let (rescan_tx, rescan_rx) = tokio::sync::mpsc::channel(1);
        // bundle rows are fixed at startup; local rows stream in live
        let mut rows = BTreeMap::new();
        let mut progress = None;
        match &source {
            Source::Bundles { rows: merged, .. } => {
                for session in merged {
                    // key by id AND creation stamp: merge() deliberately
                    // keeps same-named sessions with different stamps as
                    // distinct rows, and this map must not re-collapse them
                    // (an absent stamp keys apart from every real one)
                    let stamp = session
                        .row
                        .created_at
                        .map_or_else(|| "none".to_owned(), |at| at.timestamp_millis().to_string());
                    rows.insert(
                        format!("{}@{stamp}", session.row.session_id),
                        ExploreRow {
                            name: None,
                            session: session.clone(),
                            local: None,
                        },
                    );
                }
            }
            Source::Local { .. } => progress = Some((0, 0)),
        }
        ExploreApp {
            event_tx,
            event_rx,
            source,
            pricing,
            rescan_rx: Some(rescan_rx),
            rescan_tx,
            rows,
            progress,
            screen: Screen::Fleet,
            cursors: [0; 6],
            scrolls: [0; 6],
            popup: None,
            palette: ShortcutMenu::builder()
                .title(" Explore Shortcuts ")
                .add('h', "open this screen's help")
                .add('1', "fleet overview")
                .add('2', "tool league table")
                .add('3', "failure signatures")
                .add('4', "stuck points")
                .add('5', "detector outcomes")
                .add('6', "token economics")
                .add('f', "cycle the version filter")
                .add('r', "rescan a local store")
                .add('q', "quit")
                .build(),
            theme: TelemetryTheme::default(),
            filter_version: None,
            drill: None,
            drill_loading: None,
            error: None,
            should_quit: false,
        }
    }

    /// Runs the explorer's event loop until the user quits
    ///
    /// # Arguments
    ///
    /// * `terminal` - The terminal to render frames into
    async fn start(&mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        // spawn the shared terminal and SIGTERM forwarders
        tui::spawn_forwarders(&self.event_tx, ExploreEvent::Terminal, || {
            ExploreEvent::Quit
        });
        // local mode scans the root live, exactly like the usage TUI
        if let Source::Local { root } = &self.source {
            let Some(rescan_rx) = self.rescan_rx.take() else {
                return Ok(());
            };
            tokio::spawn(run_scanner(
                root.clone(),
                self.pricing.clone(),
                self.event_tx.clone(),
                rescan_rx,
                // the explorer's folds gather the telemetry counters
                true,
                ExploreEvent::Scan,
            ));
        }
        // spawn the shared slow ticker keeping relative figures fresh
        tokio::spawn(tui::redraw_ticker(self.event_tx.clone(), || {
            ExploreEvent::Redraw
        }));
        // draw an initial frame until we get an event to handle
        terminal.draw(|frame| self.render(frame))?;
        // handle events one at a time; an explorer's traffic never needs more
        while let Ok(event) = self.event_rx.recv().await {
            self.handle_event(event).await;
            if self.should_quit {
                break;
            }
            terminal.draw(|frame| self.render(frame))?;
        }
        Ok(())
    }

    /// Handles one event off the loop
    ///
    /// # Arguments
    ///
    /// * `event` - The event to handle
    async fn handle_event(&mut self, event: ExploreEvent) {
        match event {
            // keys route by what is open and showing
            ExploreEvent::Terminal(Event::Key(key)) => {
                // ignore key releases/repeat chatter on terminals that send it
                if key.kind == KeyEventKind::Press {
                    self.handle_key(key);
                }
            }
            ExploreEvent::Terminal(Event::Mouse(mouse)) => match mouse.kind {
                // the wheel scrolls whatever is showing
                MouseEventKind::ScrollUp => self.handle_scroll(true),
                MouseEventKind::ScrollDown => self.handle_scroll(false),
                _ => {}
            },
            // resizes and pastes just fall through to the coming redraw
            ExploreEvent::Terminal(_) => {}
            // fold a scanner update into the rows (local mode only)
            ExploreEvent::Scan(update) => self.handle_scan(update).await,
            // a drill finished loading; show it if it is still wanted
            ExploreEvent::DrillReady { name, drill } => {
                if self.drill_loading.as_deref() == Some(name.as_str()) {
                    self.drill_loading = None;
                    self.drill = Some(*drill);
                }
            }
            // a drill failed; surface the error in the footer
            ExploreEvent::DrillFailed { name, error } => {
                if self.drill_loading.as_deref() == Some(name.as_str()) {
                    self.drill_loading = None;
                }
                self.error = Some(format!("{name}: {error}"));
            }
            // quit gracefully
            ExploreEvent::Quit => self.should_quit = true,
            // a redraw happens after every handled event anyway
            ExploreEvent::Redraw => {}
        }
    }

    /// Folds one scanner update into the explorer's rows (local mode)
    ///
    /// # Arguments
    ///
    /// * `update` - The scanner update to fold
    async fn handle_scan(&mut self, update: ScanUpdate) {
        match update {
            // initial-pass progress for the footer
            ScanUpdate::Progress { done, total } => {
                if self.progress.is_some() {
                    self.progress = Some((done, total));
                }
            }
            // a fresh summary joins its forum pass and becomes a bundle row
            ScanUpdate::Loaded(summary) => {
                let Source::Local { root } = &self.source else {
                    return;
                };
                // the same assembly the exporter runs, computed on the fly,
                // plus the local-only prose the pop-outs may show
                let (outcomes, local) = read_forum_local(&root.join(&summary.name)).await;
                let row = assemble_session(&summary, outcomes);
                // a local row's split version is its manifest's, when any;
                // this binary stands in for manifest-less stores
                let version = row
                    .harness_version
                    .clone()
                    .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned());
                self.rows.insert(
                    summary.name.clone(),
                    ExploreRow {
                        name: Some(summary.name.clone()),
                        local: Some(local),
                        session: MergedSession {
                            row,
                            version,
                            // local rows normalize with this binary's recipe
                            recipe: crate::handlers::ai::telemetry::bundle::SIGNATURE_RECIPE,
                        },
                    },
                );
            }
            // a vanished session drops out entirely
            ScanUpdate::Removed(name) => {
                self.rows.remove(&name);
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
        // ctrl-c always quits, whatever layer is open
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }
        // an open drill owns the keyboard; its own close returns to the fleet
        if let Some(drill) = &mut self.drill {
            if let ViewOutcome::Close = drill.handle_key(key) {
                self.drill = None;
            }
            return;
        }
        // an open pop-out is the next layer; Close drops it
        if let Some(popup) = &mut self.popup {
            if let ViewOutcome::Close = popup.handle_key(key) {
                self.popup = None;
            }
            return;
        }
        // the Space palette is the next layer: it swallows its own keys
        if self.palette.active {
            match key.code {
                KeyCode::Esc | KeyCode::Char(' ') => self.palette.toggle(),
                KeyCode::Char('h') => {
                    self.palette.toggle();
                    self.open_help();
                }
                KeyCode::Char(digit @ '1'..='6') => {
                    self.palette.toggle();
                    self.jump_to(digit);
                }
                KeyCode::Char('f') => {
                    self.palette.toggle();
                    self.cycle_version_filter();
                }
                KeyCode::Char('r') => {
                    self.palette.toggle();
                    let _ = self.rescan_tx.try_send(());
                }
                KeyCode::Char('q') => self.should_quit = true,
                _ => {}
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
            // the palette and the help page
            KeyCode::Char(' ') => self.palette.toggle(),
            KeyCode::Char('?') | KeyCode::Char('h') => self.open_help(),
            // jump straight to a numbered screen
            KeyCode::Char(digit @ '1'..='6') => self.jump_to(digit),
            // cycle through the screens
            KeyCode::Tab => self.screen = self.screen.next(),
            KeyCode::BackTab => self.screen = self.screen.prev(),
            // `r` nudges the local scanner for a fresh pass
            KeyCode::Char('r') => {
                let _ = self.rescan_tx.try_send(());
            }
            // `f` cycles the harness-version filter over the fleet
            KeyCode::Char('f') => self.cycle_version_filter(),
            KeyCode::Up | KeyCode::Char('k') => self.handle_scroll(true),
            KeyCode::Down | KeyCode::Char('j') => self.handle_scroll(false),
            // page keys jump a cursor list by a page and scroll the rest
            KeyCode::PageUp => self.page(true),
            KeyCode::PageDown => self.page(false),
            // enter opens the screen's context pop-out (or the local drill)
            KeyCode::Enter => self.open_context(),
            // `i` opens the session-detail pop-out in both modes
            KeyCode::Char('i') => self.open_session_detail(),
            _ => {}
        }
    }

    /// Jumps to a numbered screen
    ///
    /// # Arguments
    ///
    /// * `digit` - The pressed screen digit (`'1'..='6'`)
    fn jump_to(&mut self, digit: char) {
        let index = (digit as usize - '1' as usize).min(Screen::ALL.len() - 1);
        self.screen = Screen::ALL[index];
    }

    /// Opens the help pop-out for the current screen
    fn open_help(&mut self) {
        // the per-screen page, index-aligned with the screen order
        self.popup = Some(Popup::markdown(
            format!(" help — {} ", self.screen.label()),
            super::help::explore_help(self.screen_index()),
        ));
    }

    /// Opens the current screen's context pop-out (fleet: drill or detail)
    fn open_context(&mut self) {
        match self.screen {
            // the fleet's Enter drills locally; bundles get the detail pop-out
            Screen::Fleet => match &self.source {
                Source::Local { .. } => self.open_drill(),
                Source::Bundles { .. } => self.open_session_detail(),
            },
            // the signature list's Enter opens the full multi-line cluster
            Screen::Signatures => self.open_signature_detail(),
            // the stuck screen's Enter lists the cursor category's entries
            Screen::Stuck => self.open_forum_entries(),
            // the detector list's Enter opens the technique's leads
            Screen::Detectors => self.open_detector_leads(),
            // the other screens' pop-outs land with their feature stages
            _ => {}
        }
    }

    /// The current screen's cursor-list length, when it is a cursor screen
    ///
    /// `None` marks the aggregate screens whose arrows scroll instead; one
    /// exhaustive match so a new screen must pick a side explicitly.
    fn cursor_list_len(&self) -> Option<usize> {
        // each cursor screen's list is the one its Enter pop-out indexes
        match self.screen {
            Screen::Fleet => Some(self.visible_rows().len()),
            Screen::Signatures => Some(self.signature_clusters().len()),
            Screen::Stuck => Some(Self::STUCK_CATEGORIES.len()),
            Screen::Detectors => Some(self.detector_order().len()),
            Screen::Tools | Screen::Economics => None,
        }
    }

    /// Handles a wheel scroll by what is showing
    ///
    /// # Arguments
    ///
    /// * `up` - Whether the wheel scrolled up
    fn handle_scroll(&mut self, up: bool) {
        // an open drill owns the wheel, then an open pop-out
        if let Some(drill) = &mut self.drill {
            drill.handle_scroll(up);
            return;
        }
        if let Some(popup) = &mut self.popup {
            popup.handle_scroll(up);
            return;
        }
        // cursor lists move their cursor; aggregate screens scroll
        match self.cursor_list_len() {
            Some(len) => {
                let limit = len.saturating_sub(1);
                let cursor = self.cursor_mut();
                if up {
                    *cursor = cursor.saturating_sub(1);
                } else {
                    *cursor = (*cursor + 1).min(limit);
                }
            }
            None => {
                let scroll = self.scroll_mut();
                if up {
                    *scroll = scroll.saturating_sub(1);
                } else {
                    *scroll = scroll.saturating_add(1);
                }
            }
        }
    }

    /// Pages the current screen by twenty rows
    ///
    /// Cursor lists jump their cursor; aggregate screens -- and the stuck
    /// page, whose header content can overflow short terminals even though
    /// its four category rows ride a cursor -- move their scroll offset, so
    /// every line stays reachable.
    ///
    /// # Arguments
    ///
    /// * `up` - Whether to page up
    fn page(&mut self, up: bool) {
        // one page of rows per press; the key router has already given open
        // drills and pop-outs their shot, so this is always the screen's page
        const PAGE: usize = 20;
        // the stuck screen pages its scroll: its cursor spans only four rows
        let cursor_len = match self.screen {
            Screen::Stuck => None,
            _ => self.cursor_list_len(),
        };
        match cursor_len {
            Some(len) => {
                // jump the cursor a page, clamped to the list
                let limit = len.saturating_sub(1);
                let cursor = self.cursor_mut();
                if up {
                    *cursor = cursor.saturating_sub(PAGE);
                } else {
                    *cursor = (*cursor + PAGE).min(limit);
                }
            }
            None => {
                // page the scroll; rendering clamps it to the content
                let scroll = self.scroll_mut();
                if up {
                    *scroll = scroll.saturating_sub(PAGE);
                } else {
                    *scroll = scroll.saturating_add(PAGE);
                }
            }
        }
    }

    /// Opens the drill-down for the fleet cursor's session (local mode)
    fn open_drill(&mut self) {
        // bundles carry no messages to replay, by design
        let Source::Local { root } = &self.source else {
            self.error =
                Some("drill-down needs a local store; bundles carry no messages".to_owned());
            return;
        };
        if self.screen != Screen::Fleet {
            return;
        }
        // resolve the cursor's session name in the fleet table's order
        let order = self.fleet_order();
        let Some(name) = order.get(self.cursor()).and_then(|row| row.name.clone()) else {
            return;
        };
        // load the drill off-loop so a huge transcript never freezes a frame
        self.drill_loading = Some(name.clone());
        let root = root.clone();
        let pricing = self.pricing.clone();
        let sender = self.event_tx.clone();
        tokio::spawn(async move {
            let event = match load_drill(root, name.clone(), pricing).await {
                Ok(drill) => ExploreEvent::DrillReady {
                    name,
                    drill: Box::new(drill),
                },
                Err(error) => ExploreEvent::DrillFailed { name, error },
            };
            let _ = sender.send(event).await;
        });
    }

    /// This screen's position in the cycling order, for per-screen state
    fn screen_index(&self) -> usize {
        // the ALL array is the single source of screen order
        Screen::ALL
            .iter()
            .position(|screen| *screen == self.screen)
            .unwrap_or(0)
    }

    /// This screen's cursor, mutably
    fn cursor_mut(&mut self) -> &mut usize {
        let index = self.screen_index();
        &mut self.cursors[index]
    }

    /// This screen's cursor
    fn cursor(&self) -> usize {
        self.cursors[self.screen_index()]
    }

    /// This screen's scroll offset, mutably
    fn scroll_mut(&mut self) -> &mut usize {
        let index = self.screen_index();
        &mut self.scrolls[index]
    }

    /// This screen's scroll offset
    fn scroll(&self) -> usize {
        self.scrolls[self.screen_index()]
    }

    /// The rows every screen aggregates, after the version filter
    fn visible_rows(&self) -> Vec<&ExploreRow> {
        self.rows
            .values()
            .filter(|row| {
                // no filter shows the whole fleet
                self.filter_version
                    .as_ref()
                    .is_none_or(|version| &row.session.version == version)
            })
            .collect()
    }

    /// Cycles the harness-version filter through the fleet's versions
    fn cycle_version_filter(&mut self) {
        // the distinct versions, in order, with "no filter" at the front
        let mut versions: Vec<String> = self
            .rows
            .values()
            .map(|row| row.session.version.clone())
            .collect();
        versions.sort();
        versions.dedup();
        // step to the version after the current one, wrapping to none
        self.filter_version = match &self.filter_version {
            None => versions.first().cloned(),
            Some(current) => versions
                .iter()
                .position(|version| version == current)
                .and_then(|position| versions.get(position + 1))
                .cloned(),
        };
    }

    /// Opens the fleet cursor's session-detail pop-out (both modes)
    ///
    /// Built from `SessionTelemetry` fields only, so a bundle-mode detail can
    /// never show anything the bundle itself does not carry.
    fn open_session_detail(&mut self) {
        let order = self.fleet_order();
        // `i` works from every screen, so it indexes the FLEET cursor (the
        // other screens' cursors point into unrelated lists); fleet sits at
        // slot 0 of the per-screen cursor array, and clamps here because the
        // fleet screen only re-clamps it when it renders
        let index = self.cursors[0].min(order.len().saturating_sub(1));
        let Some(row) = order.get(index) else {
            return;
        };
        let session = &row.session.row;
        let label = row.label();
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut push = |text: String| lines.push(Line::from(text));
        push(format!("session   {label}"));
        push(format!(
            "created   {}",
            session
                .created_at
                .map(|at| at.format("%Y-%m-%d %H:%M UTC").to_string())
                .unwrap_or_else(|| "—".to_owned())
        ));
        push(format!(
            "harness   {}    model {}    sig recipe {}",
            row.session.version,
            session.model.as_deref().unwrap_or("—"),
            row.session.recipe,
        ));
        push(format!(
            "duration  {}    active {}",
            session
                .duration_ms
                .map(crate::handlers::ai::chat::fmt_duration)
                .unwrap_or_else(|| "—".to_owned()),
            crate::handlers::ai::chat::fmt_duration(session.active_ms),
        ));
        push(String::new());
        push(format!(
            "tokens    in {}  out {}  cache-read {}  cache-write {}",
            with_thousands(session.totals.input_tokens),
            with_thousands(session.totals.output_tokens),
            with_thousands(session.totals.cached_input_tokens),
            with_thousands(session.totals.cache_creation_input_tokens),
        ));
        // the heaviest roles, then the most failing tools
        let mut roles: Vec<(&String, u64)> = session
            .agents
            .iter()
            .map(|(role, rollup)| (role, rollup.total_tokens))
            .collect();
        roles.sort_by_key(|role| std::cmp::Reverse(role.1));
        for (role, tokens) in roles.iter().take(5) {
            push(format!(
                "  role    {:<22} {}",
                role,
                with_thousands(*tokens)
            ));
        }
        push(String::new());
        let mut failing: Vec<(&String, u64, u64)> = session
            .tools
            .iter()
            .map(|(tool, stats)| (tool, stats.calls, stats.fails))
            .filter(|(_, _, fails)| *fails > 0)
            .collect();
        failing.sort_by_key(|tool| std::cmp::Reverse(tool.2));
        push(format!("failing tools ({})", failing.len()));
        for (tool, calls, fails) in failing.iter().take(8) {
            push(format!("  {tool:<28} {fails}/{calls} failed"));
        }
        push(String::new());
        let decisions = &session.decision_points;
        push(format!(
            "decisions  asks {}  flags {}  turn-failed {}  scrubs {} (rescued {})",
            decisions.asks.values().sum::<u64>(),
            decisions.review_flags,
            decisions.turn_failed,
            decisions.llm_scrubbed,
            decisions.scrub_rescued,
        ));
        let stalls = &session.provider_stalls;
        push(format!(
            "stalls     episodes {}  attempts {}  pauses {}  died-paused {}",
            stalls.retry_episodes,
            stalls.retry_attempts,
            stalls.pauses.values().map(|tally| tally.count).sum::<u64>(),
            if stalls.open_pause { "yes" } else { "no" },
        ));
        push(format!(
            "forum      {} unsolved · {} blocked · {} needs-evidence · {} tasks unmet",
            session.forum.unsolved,
            session.forum.blocked,
            session.forum.needs_evidence,
            session.forum.tasks_unmet,
        ));
        push(format!(
            "detectors  {} leads · {} signatures",
            session.detector_leads.len(),
            session.signatures.len(),
        ));
        self.popup = Some(Popup::lines(" session detail ", lines));
    }

    /// The fleet table's rows, newest sessions first
    fn fleet_order(&self) -> Vec<&ExploreRow> {
        let mut order = self.visible_rows();
        // newest first by created_at, session id as the stable tiebreak
        order.sort_by(|a, b| {
            b.session
                .row
                .created_at
                .cmp(&a.session.row.created_at)
                .then_with(|| a.session.row.session_id.cmp(&b.session.row.session_id))
        });
        order
    }

    /// A session row's render-time cost, from the local pricing table
    ///
    /// # Arguments
    ///
    /// * `row` - The session row to price
    fn row_cost(&self, row: &MergedSession) -> Option<f64> {
        cost_of_row(row, &self.pricing)
    }

    /// Renders the whole explorer frame
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        // an open drill owns the whole frame below the top bar
        let body = Rect {
            x: area.x,
            y: area.y.saturating_add(1),
            width: area.width,
            height: area.height.saturating_sub(2),
        };
        self.render_bar(frame, Rect { height: 1, ..area });
        if let Some(drill) = &mut self.drill {
            drill.render(frame, body);
        } else {
            // split a right sidebar off wide terminals; narrow ones give the
            // whole width to the main pane
            let side = sidebar_width(area.width);
            let (main, sidebar) = if side > 0 {
                let main = Rect {
                    width: body.width.saturating_sub(side),
                    ..body
                };
                let sidebar = Rect {
                    x: body.x + main.width,
                    width: side,
                    ..body
                };
                (main, Some(sidebar))
            } else {
                (body, None)
            };
            match self.screen {
                Screen::Fleet => self.render_fleet(frame, main),
                Screen::Tools => self.render_tools(frame, main),
                Screen::Signatures => self.render_signatures(frame, main),
                Screen::Stuck => self.render_stuck(frame, main),
                Screen::Detectors => self.render_detectors(frame, main),
                Screen::Economics => self.render_economics(frame, main),
            }
            if let Some(sidebar) = sidebar {
                self.render_sidebar(frame, sidebar);
            }
            // the pop-out floats over whatever the screen drew
            if let Some(popup) = &mut self.popup {
                popup.render(frame, body);
            }
        }
        let footer = Rect {
            y: area.y + area.height.saturating_sub(1),
            height: 1,
            ..area
        };
        self.render_footer(frame, footer);
        // the palette renders last, over everything
        self.palette.render(frame);
    }

    /// Renders the right sidebar: source, fleet stats, filters, warnings
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The sidebar's area
    fn render_sidebar(&self, frame: &mut Frame, area: Rect) {
        let rows = self.visible_rows();
        let mut lines: Vec<Line<'static>> = Vec::new();
        // the source summary
        lines.push(Line::from(Span::styled(" source", self.theme.title)));
        match &self.source {
            Source::Bundles {
                files,
                levels,
                recipes,
                exporters,
                ..
            } => {
                lines.push(Line::from(format!("  {files} bundle file(s)")));
                let levels: Vec<String> = levels.iter().map(ToString::to_string).collect();
                lines.push(Line::from(format!("  level(s) {}", levels.join(", "))));
                let exporters: Vec<String> = exporters.iter().cloned().collect();
                lines.push(Line::from(format!(
                    "  exporter(s) {}",
                    exporters.join(", ")
                )));
                // mixed recipes mean signatures cluster per recipe, not fleet-wide
                if recipes.len() > 1 {
                    lines.push(Line::from(Span::styled(
                        "  ⚠ mixed signature recipes",
                        self.theme.warn,
                    )));
                }
                if levels.iter().all(|level| level == "0") {
                    lines.push(Line::from(Span::styled(
                        "  level-0 fleet: no signatures",
                        self.theme.dim,
                    )));
                }
                // exporters predating per-entry rows thin the stuck pop-outs
                // (questions OR tasks: either alone should have entry rows)
                if rows.iter().any(|row| {
                    row.session.row.forum.entries.is_empty()
                        && (row.session.row.forum.questions > 0 || row.session.row.forum.tasks > 0)
                }) {
                    lines.push(Line::from(Span::styled(
                        "  ⚠ some bundles predate entry rows",
                        self.theme.warn,
                    )));
                }
            }
            Source::Local { root } => {
                lines.push(Line::from(format!("  {}", root.display())));
                lines.push(Line::from(Span::styled(
                    "  live scan · titles available",
                    self.theme.dim,
                )));
            }
        }
        // the fleet quick stats over the filtered rows
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(" fleet", self.theme.title)));
        let tokens: u64 = rows
            .iter()
            .map(|row| row.session.row.totals.total_tokens)
            .sum();
        lines.push(Line::from(format!("  {} session(s)", rows.len())));
        lines.push(Line::from(format!("  {} tokens", with_thousands(tokens))));
        // the active filter, when one is set
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(" filter", self.theme.title)));
        match &self.filter_version {
            Some(version) => {
                lines.push(Line::from(format!("  harness = {version}")));
                lines.push(Line::from(Span::styled(
                    "  (f cycles, wraps to all)",
                    self.theme.dim,
                )));
            }
            None => lines.push(Line::from(Span::styled(
                "  none (f cycles)",
                self.theme.dim,
            ))),
        }
        // this screen's key legend
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(" keys", self.theme.title)));
        for hint in [
            " 1-6/Tab  screens",
            " Enter    open details",
            " i        session detail",
            " Space    palette · ? help",
            " q        quit",
        ] {
            lines.push(Line::from(Span::styled(hint.to_owned(), self.theme.dim)));
        }
        let block = Block::default()
            .borders(Borders::LEFT)
            .border_style(Style::default().fg(Color::DarkGray));
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }

    /// Renders the numbered screen bar
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The bar's row
    fn render_bar(&self, frame: &mut Frame, area: Rect) {
        // name the source once, then the numbered screens
        let source = match &self.source {
            Source::Bundles { rows, files, .. } => {
                format!("{} session(s) from {files} bundle(s)", rows.len())
            }
            Source::Local { root } => format!("local store {}", root.display()),
        };
        let mut spans = vec![Span::styled(
            format!(" telemetry explore · {source} "),
            Style::default().add_modifier(Modifier::BOLD),
        )];
        for (index, screen) in Screen::ALL.iter().enumerate() {
            let style = if *screen == self.screen {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            spans.push(Span::styled(
                format!("[{}]{} ", index + 1, screen.label()),
                style,
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    /// Renders the footer hints and any pending error
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The footer row
    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        // a pending error outranks the hints until the next action
        let text = if let Some(error) = &self.error {
            format!(" ⚠ {error}")
        } else if let Some((done, total)) = self.progress {
            format!(" scanning sessions… {done}/{total}")
        } else if self.drill_loading.is_some() {
            " loading drill-down… (esc cancels)".to_owned()
        } else {
            let drill = match (&self.source, self.screen) {
                (Source::Local { .. }, Screen::Fleet) => " · enter drill into session",
                _ => "",
            };
            format!(" 1-6/tab screens · ↑/↓ move · r rescan{drill} · q quit")
        };
        let style = if self.error.is_some() {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        frame.render_widget(Paragraph::new(text).style(style), area);
    }

    /// Renders the fleet overview: totals, rates, splits, and the sessions
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The body area
    fn render_fleet(&mut self, frame: &mut Frame, area: Rect) {
        // clamp the cursor as sessions appear and vanish live
        let limit = self.visible_rows().len().saturating_sub(1);
        let cursor = (*self.cursor_mut()).min(limit);
        *self.cursor_mut() = cursor;
        let rows: Vec<&ExploreRow> = self.fleet_order();
        // fold the fleet totals once per frame; fleets stay small
        let mut tokens: u64 = 0;
        let mut cost = 0.0;
        let mut priced = 0usize;
        let mut calls: u64 = 0;
        let mut fails: u64 = 0;
        let mut interrupted = 0usize;
        let mut open_pauses = 0usize;
        let mut splits: BTreeMap<(String, String), usize> = BTreeMap::new();
        for row in &rows {
            let session = &row.session.row;
            tokens += session.totals.total_tokens;
            if let Some(dollars) = self.row_cost(&row.session) {
                cost += dollars;
                priced += 1;
            }
            for tool in session.tools.values() {
                calls += tool.calls;
                fails += tool.fails;
            }
            if session.decision_points.interrupted_resumes > 0 {
                interrupted += 1;
            }
            if session.provider_stalls.open_pause {
                open_pauses += 1;
            }
            let model = session
                .model
                .clone()
                .unwrap_or_else(|| "unknown".to_owned());
            *splits
                .entry((row.session.version.clone(), model))
                .or_default() += 1;
        }
        let fail_rate = if calls > 0 {
            format!("{:.1}%", fails as f64 * 100.0 / calls as f64)
        } else {
            "—".to_owned()
        };
        let mut lines = vec![Line::from(Span::styled(
            format!(
                " sessions {}   tokens {}   est cost {} ({priced} priced)   tool failure {}   interrupted {}   open pauses {}",
                rows.len(),
                with_thousands(tokens),
                fmt_cost(cost),
                fail_rate,
                interrupted,
                open_pauses,
            ),
            Style::default().add_modifier(Modifier::BOLD),
        ))];
        // split lines appear only when versions or models actually disagree
        if splits.len() > 1 {
            for ((version, model), count) in &splits {
                lines.push(Line::from(format!(
                    "   {count:>4} session(s) · harness {version} · {model}"
                )));
            }
        }
        lines.push(Line::from(""));
        // the header's names come from the help module, so the help page's
        // column coverage test pins every rendered column
        let [session, tokens, fails, stalls, asks, model] = super::help::FLEET_COLUMNS;
        lines.push(Line::from(Span::styled(
            format!("  {session:<30} {tokens:>14} {fails:>9} {stalls:>8} {asks:>8}  {model}"),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        // window the session table around the cursor
        let used = lines.len();
        let height = (area.height as usize).saturating_sub(used);
        let first = cursor.saturating_sub(height.saturating_sub(1) / 2);
        for (index, row) in rows.iter().enumerate().skip(first).take(height) {
            let session = &row.session.row;
            let label = row.label();
            let fails: u64 = session.tools.values().map(|tool| tool.fails).sum();
            let asks: u64 = session.decision_points.asks.values().sum();
            let text = format!(
                "  {:<30} {:>14} {:>9} {:>8} {:>8}  {}",
                clip(&label, 30),
                with_thousands(session.totals.total_tokens),
                fails,
                session.provider_stalls.retry_episodes,
                asks,
                session.model.as_deref().unwrap_or("—"),
            );
            let style = if index == cursor {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            lines.push(Line::from(Span::styled(text, style)));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    /// Renders the per-tool league table
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The body area
    fn render_tools(&mut self, frame: &mut Frame, area: Rect) {
        // merge every session's per-tool telemetry
        let mut tools: BTreeMap<String, ToolTally> = BTreeMap::new();
        let mut offered: BTreeSet<String> = BTreeSet::new();
        for row in self.visible_rows() {
            for (tool, stats) in &row.session.row.tools {
                let entry = tools.entry(tool.clone()).or_default();
                entry.0 += stats.calls;
                entry.1 += stats.fails;
                if let Some(p50) = stats.p50_ms {
                    entry.2.push(p50);
                }
            }
            for roster in row.session.row.tools_offered.values() {
                offered.extend(roster.iter().cloned());
            }
        }
        // call-weight order makes the league table a league table
        let mut order: Vec<(&String, &ToolTally)> = tools.iter().collect();
        order.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(b.0)));
        // the header's names come from the help module, so the help page's
        // column coverage test pins every rendered column
        let [tool, calls, fails, rate, p50] = super::help::TOOLS_COLUMNS;
        let mut lines = vec![Line::from(Span::styled(
            format!(" {tool:<28} {calls:>10} {fails:>8} {rate:>8} {p50:>10}"),
            Style::default().add_modifier(Modifier::BOLD),
        ))];
        for (tool, (calls, fails, p50s)) in &order {
            // the median of the per-session medians is the honest merge,
            // ranked by the same rule the per-session p50s were built with
            let p50 = percentile(p50s, 0.5).map_or("—".to_owned(), |ms| ms.to_string());
            let rate = if *calls > 0 {
                format!("{:.1}%", *fails as f64 * 100.0 / *calls as f64)
            } else {
                "—".to_owned()
            };
            lines.push(Line::from(format!(
                " {:<28} {:>10} {:>8} {:>8} {:>10}",
                clip(tool, 28),
                with_thousands(*calls),
                fails,
                rate,
                p50,
            )));
        }
        // the exact "offered, never called" set difference, when manifests
        // shipped; older stores list nothing here rather than a weaker claim
        let never: Vec<&String> = offered
            .iter()
            .filter(|tool| !tools.contains_key(*tool))
            .collect();
        if !never.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!(" offered, never called ({}):", never.len()),
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .fg(Color::Yellow),
            )));
            let listed: Vec<String> = never.iter().map(|tool| tool.to_string()).collect();
            lines.push(Line::from(format!("   {}", listed.join(", "))));
        }
        self.render_scrolled(frame, area, lines);
    }

    /// The fleet's signature clusters: (tool, sig) -> (count, looped, sessions)
    ///
    /// Merged across the filtered rows and ordered heaviest-first; the list
    /// screen and its Enter pop-out share this ordering, so the cursor's row
    /// is always the pop-out's cluster.
    fn signature_clusters(&self) -> Vec<((String, String), SigTally)> {
        // merge every session's clusters on exact (tool, signature) equality
        let mut clusters: BTreeMap<(String, String), SigTally> = BTreeMap::new();
        for row in self.visible_rows() {
            for cluster in &row.session.row.signatures {
                let entry = clusters
                    .entry((cluster.tool.clone(), cluster.sig.clone()))
                    .or_default();
                // sum the weight, keep any loop flag, count carrying sessions
                entry.0 += cluster.count;
                entry.1 |= cluster.looped;
                entry.2 += 1;
            }
        }
        // heaviest clusters first, the key as the stable tiebreak
        let mut order: Vec<((String, String), SigTally)> = clusters.into_iter().collect();
        order.sort_by(|a, b| b.1.0.cmp(&a.1.0).then_with(|| a.0.cmp(&b.0)));
        order
    }

    /// Opens the signature cursor's detail pop-out
    fn open_signature_detail(&mut self) {
        let order = self.signature_clusters();
        let Some(((tool, sig), (count, looped, sessions))) = order.get(self.cursor()) else {
            return;
        };
        // the recipes in play, so mixed fleets explain their split clusters
        let recipes = match &self.source {
            Source::Bundles { recipes, .. } => Some(recipes.clone()),
            Source::Local { .. } => None,
        };
        self.popup = Some(signature_popup(
            tool,
            sig,
            *count,
            *looped,
            Some(*sessions),
            recipes.as_ref(),
        ));
    }

    /// Renders the failure-signature clusters, loops highlighted
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The body area
    fn render_signatures(&mut self, frame: &mut Frame, area: Rect) {
        let order = self.signature_clusters();
        // clamp this screen's cursor against the merged list
        let limit = order.len().saturating_sub(1);
        let cursor = (*self.cursor_mut()).min(limit);
        *self.cursor_mut() = cursor;
        // the header's names come from the help module, so the help page's
        // column coverage test pins every rendered column
        let [tool, count, signature] = super::help::SIGNATURES_COLUMNS;
        let mut lines = vec![Line::from(Span::styled(
            format!(" {tool:<20} {count:>7}  {signature} (level-1 bundles only)"),
            Style::default().add_modifier(Modifier::BOLD),
        ))];
        if order.is_empty() {
            lines.push(Line::from(
                "   no signatures: level-0 bundles, or no failing tool results",
            ));
        }
        // window the cursor list like the fleet table does
        let height = (area.height as usize).saturating_sub(1);
        let first_row = cursor.saturating_sub(height.saturating_sub(1) / 2);
        for (index, ((tool, sig), (count, looped, _))) in
            order.iter().enumerate().skip(first_row).take(height)
        {
            let mut spans = vec![Span::raw(format!(
                " {:<20} {:>7}  ",
                clip(tool, 20),
                with_thousands(*count)
            ))];
            if *looped {
                spans.push(Span::styled(
                    "loop! ",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ));
            }
            // list rows show the first line; Enter opens the full signature
            let (first, more) = super::sig::sig_summary(sig);
            spans.push(Span::raw(first.to_owned()));
            if more > 0 {
                spans.push(Span::styled(
                    format!(" (+{more} lines)"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            let mut line = Line::from(spans);
            if index == cursor {
                line = line.style(Style::default().add_modifier(Modifier::REVERSED));
            }
            lines.push(line);
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    /// The stuck screen's four explorable end-state categories, in row order
    const STUCK_CATEGORIES: [(EntryStatus, &'static str); 4] = [
        (EntryStatus::Unsolved, "unsolved questions"),
        (EntryStatus::NeedsEvidence, "needs evidence"),
        (EntryStatus::Blocked, "blocked questions"),
        (EntryStatus::Unmet, "tasks unmet"),
    ];

    /// Opens the stuck cursor's per-entry pop-out, grouped by session
    fn open_forum_entries(&mut self) {
        let (status, label) = Self::STUCK_CATEGORIES[self.cursor().min(3)];
        let mut lines: Vec<Line<'static>> = Vec::new();
        for row in self.visible_rows() {
            let session = &row.session.row;
            let matching: Vec<(usize, &ForumEntry)> = session
                .forum
                .entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.status == status)
                .collect();
            // the category's status-count, which covers the whole snapshot
            // even when the per-session row cap dropped its entries
            let count = match status {
                EntryStatus::Unsolved => session.forum.unsolved,
                EntryStatus::NeedsEvidence => session.forum.needs_evidence,
                EntryStatus::Blocked => session.forum.blocked,
                _ => session.forum.tasks_unmet,
            };
            // a bundle predating per-entry rows can only say so
            if session.forum.entries.is_empty() {
                if count > 0 {
                    let name = row.label();
                    lines.push(Line::from(format!(
                        " {name}: {count} (bundle predates per-entry rows)"
                    )));
                }
                continue;
            }
            if matching.is_empty() {
                // the screen's count row can be nonzero while every matching
                // entry fell past the per-session row cap; say so instead of
                // silently reading as an empty category
                if count > 0 && session.forum.entries_dropped > 0 {
                    let name = row.label();
                    lines.push(Line::from(format!(
                        " {name}: {count} (beyond the per-session row cap)"
                    )));
                }
                continue;
            }
            // one session group: its label, then one row per entry
            let name = row.label();
            lines.push(Line::from(Span::styled(
                format!(" {name}"),
                Style::default().add_modifier(Modifier::BOLD),
            )));
            for (index, entry) in matching {
                // the entry's closed-vocabulary facts, one compact row
                let who = match (&entry.name_class, &entry.role) {
                    (Some(class), _) => match entry.sample {
                        Some(sample) => format!("detector {class} s{sample}"),
                        None => format!("detector {class}"),
                    },
                    (None, Some(role)) => role.clone(),
                    (None, None) => "user".to_owned(),
                };
                let techniques = if entry.techniques.is_empty() {
                    String::new()
                } else {
                    format!("  [{}]", entry.techniques.join(", "))
                };
                let mut facts = format!("   {who}{techniques}  rev {}", entry.updated);
                if entry.tasks > 0 {
                    facts.push_str(&format!("  tasks {}/{}", entry.tasks_unmet, entry.tasks));
                }
                if entry.replies > 0 {
                    facts.push_str(&format!("  replies {}", entry.replies));
                }
                if entry.untouched == Some(true) {
                    facts.push_str("  untouched");
                }
                lines.push(Line::from(facts));
                // the title: local prose first, then a level-1 bundle title,
                // else the privacy note
                let detail = row
                    .local
                    .as_ref()
                    .and_then(|local| local.entries.get(index));
                let title = detail
                    .map(|detail| detail.title.clone())
                    .or_else(|| entry.title.clone());
                match title {
                    Some(title) => {
                        // the node's real name shows only over a local store
                        let name = detail
                            .and_then(|detail| detail.name.as_deref())
                            .map(|name| format!(" ({name})"))
                            .unwrap_or_default();
                        lines.push(Line::from(format!("     “{title}”{name}")));
                    }
                    None => lines.push(Line::from(Span::styled(
                        "     title not in bundle (privacy)",
                        Style::default().fg(Color::DarkGray),
                    ))),
                }
            }
            if session.forum.entries_dropped > 0 {
                lines.push(Line::from(Span::styled(
                    format!(
                        "   +{} not listed (per-session cap)",
                        session.forum.entries_dropped
                    ),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
        if lines.is_empty() {
            lines.push(Line::from(format!(" no {label} in this fleet")));
        }
        self.popup = Some(Popup::lines(format!(" {label} "), lines));
    }

    /// Renders the stuck points: decisions, stalls, and the forum end-state
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The body area
    fn render_stuck(&mut self, frame: &mut Frame, area: Rect) {
        // sum the decision points and stalls across every session
        let mut asks: BTreeMap<String, u64> = BTreeMap::new();
        let mut terminations: BTreeMap<String, u64> = BTreeMap::new();
        let mut extensions: BTreeMap<String, u64> = BTreeMap::new();
        let mut pauses: BTreeMap<String, (u64, u64, u64, u64)> = BTreeMap::new();
        let (mut flags, mut failed, mut warn, mut critical) = (0u64, 0u64, 0u64, 0u64);
        let (mut scrubbed, mut rescued, mut degraded) = (0u64, 0u64, 0u64);
        let (mut interrupted, mut clean) = (0u64, 0u64);
        let (mut episodes, mut attempts, mut backoff, mut exhausted) = (0u64, 0u64, 0u64, 0u64);
        let mut open = 0u64;
        let (mut unsolved, mut needs, mut blocked, mut tasks_unmet) = (0u64, 0u64, 0u64, 0u64);
        for row in self.visible_rows() {
            let session = &row.session.row;
            let decisions = &session.decision_points;
            for (source, count) in &decisions.asks {
                *asks.entry(source.clone()).or_default() += count;
            }
            for (class, count) in &decisions.terminations {
                *terminations.entry(class.clone()).or_default() += count;
            }
            for (trigger, count) in &decisions.extensions {
                *extensions.entry(trigger.clone()).or_default() += count;
            }
            flags += decisions.review_flags;
            failed += decisions.turn_failed;
            warn += decisions.context_pressure_warn;
            critical += decisions.context_pressure_critical;
            scrubbed += decisions.llm_scrubbed;
            rescued += decisions.scrub_rescued;
            degraded += decisions.report_degraded;
            interrupted += decisions.interrupted_resumes;
            clean += decisions.clean_resumes;
            let stalls = &session.provider_stalls;
            episodes += stalls.retry_episodes;
            attempts += stalls.retry_attempts;
            backoff += stalls.retry_backoff_ms;
            exhausted += stalls.retries_exhausted;
            open += u64::from(stalls.open_pause);
            for (kind, tally) in &stalls.pauses {
                let entry = pauses.entry(kind.clone()).or_default();
                entry.0 += tally.count;
                entry.1 += tally.total_paused_ms;
                entry.2 += tally.probe_attempts;
                entry.3 += tally.aborted;
            }
            unsolved += session.forum.unsolved;
            needs += session.forum.needs_evidence;
            blocked += session.forum.blocked;
            tasks_unmet += session.forum.tasks_unmet;
        }
        // the auto-default rate is the headline unattended-decision measure
        let auto = asks.get("auto_default").copied().unwrap_or(0);
        let total_asks: u64 = asks.values().sum();
        let auto_rate = if total_asks > 0 {
            format!("{:.0}%", auto as f64 * 100.0 / total_asks as f64)
        } else {
            "—".to_owned()
        };
        let fmt_map = |map: &BTreeMap<String, u64>| -> String {
            if map.is_empty() {
                "—".to_owned()
            } else {
                map.iter()
                    .map(|(key, count)| format!("{key} {count}"))
                    .collect::<Vec<_>>()
                    .join(" · ")
            }
        };
        let mut lines = vec![
            Line::from(Span::styled(
                format!(
                    " asks: {}   (auto-default rate {auto_rate})",
                    fmt_map(&asks)
                ),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(format!(" terminations: {}", fmt_map(&terminations))),
            Line::from(format!(" extensions: {}", fmt_map(&extensions))),
            Line::from(format!(
                " context pressure: warn {warn} · critical {critical}    review flags: {flags}    turns failed: {failed}"
            )),
            Line::from(format!(
                " scrubs: {scrubbed} (rescued {rescued})    report degraded: {degraded}    resumes: interrupted {interrupted} · clean {clean}"
            )),
            Line::from(""),
            Line::from(Span::styled(
                format!(
                    " provider stalls: {episodes} episode(s) · {attempts} attempt(s) · {} backing off · {exhausted} exhausted · {open} died paused",
                    fmt_ms(backoff),
                ),
                Style::default().add_modifier(Modifier::BOLD),
            )),
        ];
        for (kind, (count, paused_ms, probes, aborted)) in &pauses {
            lines.push(Line::from(format!(
                "   {kind}: {count} pause(s) · {} paused · {probes} failed probe(s) · {aborted} aborted",
                fmt_ms(*paused_ms),
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " forum end-state (↑/↓ pick a row, enter lists its entries)".to_owned(),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        // the four explorable categories, the cursor's row highlighted
        let cursor = self.cursor().min(3);
        let counts = [unsolved, needs, blocked, tasks_unmet];
        for (index, ((_, label), count)) in Self::STUCK_CATEGORIES.iter().zip(counts).enumerate() {
            let mut line = Line::from(format!("   {count:>5}  {label}"));
            if index == cursor {
                line = line.style(Style::default().add_modifier(Modifier::REVERSED));
            }
            lines.push(line);
        }
        self.render_scrolled(frame, area, lines);
    }

    /// The fleet's techniques with their summed tallies, busiest first
    ///
    /// The list screen and its Enter pop-out share this ordering, so the
    /// cursor's row is always the opened technique.
    fn detector_order(&self) -> Vec<(String, DetectorTally)> {
        // merge every session's per-technique tally into one fleet tally
        let mut techniques: BTreeMap<String, DetectorTally> = BTreeMap::new();
        for row in self.visible_rows() {
            for (id, tally) in &row.session.row.detectors {
                let entry = techniques.entry(id.clone()).or_default();
                // sum the verdict and lead counters field by field
                entry.confirmed += tally.confirmed;
                entry.disputed += tally.disputed;
                entry.neutral += tally.neutral;
                entry.ignored += tally.ignored;
                entry.leads += tally.leads;
                // merge the confidence histograms bucket by bucket
                for (bucket, count) in &tally.confidence {
                    *entry.confidence.entry(bucket.clone()).or_default() += count;
                }
            }
        }
        // the busiest techniques first: most verdicts, then id for stability
        let mut order: Vec<(String, DetectorTally)> = techniques.into_iter().collect();
        order.sort_by(|a, b| {
            let total_a = a.1.confirmed + a.1.disputed + a.1.neutral + a.1.ignored;
            let total_b = b.1.confirmed + b.1.disputed + b.1.neutral + b.1.ignored;
            total_b.cmp(&total_a).then_with(|| a.0.cmp(&b.0))
        });
        order
    }

    /// Opens the detector cursor's per-lead pop-out across the fleet
    fn open_detector_leads(&mut self) {
        let order = self.detector_order();
        let Some((technique, _)) = order.get(self.cursor()) else {
            return;
        };
        let technique = technique.clone();
        let mut lines: Vec<Line<'static>> = Vec::new();
        for row in self.visible_rows() {
            let session = &row.session.row;
            let matching: Vec<(usize, &crate::handlers::ai::telemetry::bundle::DetectorLead)> =
                session
                    .detector_leads
                    .iter()
                    .enumerate()
                    .filter(|(_, lead)| lead.technique == technique)
                    .collect();
            if matching.is_empty() {
                // the tally can count leads whose rows all fell past the
                // per-session cap; say so instead of reading as none at all
                let counted = session
                    .detectors
                    .get(&technique)
                    .map_or(0, |tally| tally.leads);
                if counted > 0 && session.detector_leads_dropped > 0 {
                    let name = row.label();
                    lines.push(Line::from(format!(
                        " {name}: {counted} lead(s) beyond the per-session cap"
                    )));
                }
                continue;
            }
            let name = row.label();
            lines.push(Line::from(Span::styled(
                format!(" {name}"),
                Style::default().add_modifier(Modifier::BOLD),
            )));
            for (index, lead) in matching {
                // one compact fact row per lead
                let confidence = match lead.confidence {
                    Some(level) => format!("{level:?}").to_lowercase(),
                    None => "unrated".to_owned(),
                };
                let mut facts = format!(
                    "   {}{}  {:?}  {confidence}",
                    lead.post_class,
                    lead.sample.map(|s| format!(" s{s}")).unwrap_or_default(),
                    lead.shape,
                );
                if let Some(suspicion) = lead.suspicion {
                    facts.push_str(&format!("  susp {suspicion}"));
                }
                if let Some(sites) = lead.sites {
                    facts.push_str(&format!("  sites {sites}"));
                }
                if let Some(artifact) = &lead.artifact {
                    facts.push_str(&format!("  artifact {artifact}"));
                }
                if let Some(source) = &lead.source {
                    facts.push_str(&format!("  via {source}"));
                    if let Some(detection) = &lead.detection {
                        facts.push_str(&format!(":{detection}"));
                    }
                }
                facts.push_str(&format!("  → {:?}", lead.verdict));
                if lead.exact {
                    facts.push_str("  (exact)");
                }
                lines.push(Line::from(facts.to_lowercase()));
                // the seeded post's prose shows only over a local store
                if let Some(detail) = row.local.as_ref().and_then(|local| local.leads.get(index)) {
                    lines.push(Line::from(Span::styled(
                        format!("     {} — “{}”", detail.post_name, detail.post_title),
                        Style::default().fg(Color::DarkGray),
                    )));
                    // the finding's first line, when the seed carried one
                    if !detail.finding.is_empty() {
                        lines.push(Line::from(Span::styled(
                            format!("       {}", detail.finding),
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                }
            }
            if session.detector_leads_dropped > 0 {
                lines.push(Line::from(Span::styled(
                    format!(
                        "   +{} lead(s) not listed (per-session cap)",
                        session.detector_leads_dropped
                    ),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
        if lines.is_empty() {
            lines.push(Line::from(
                " no per-lead rows: bundles predate them, or the cap dropped these",
            ));
        }
        self.popup = Some(Popup::lines(format!(" {technique} leads "), lines));
    }

    /// Renders the per-technique detector-lead outcomes
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The body area
    fn render_detectors(&mut self, frame: &mut Frame, area: Rect) {
        let order = self.detector_order();
        // clamp this screen's cursor against the technique list
        let limit = order.len().saturating_sub(1);
        let cursor = (*self.cursor_mut()).min(limit);
        *self.cursor_mut() = cursor;
        // the detector-question statuses sum across the filtered fleet
        let (mut solved, mut blocked, mut unsolved, mut needs) = (0u64, 0u64, 0u64, 0u64);
        for row in self.visible_rows() {
            let questions = &row.session.row.detector_questions;
            solved += questions.solved;
            blocked += questions.blocked;
            unsolved += questions.unsolved;
            needs += questions.needs_evidence;
        }
        let mut lines = vec![
            Line::from(Span::styled(
                format!(
                    " detector questions: {solved} solved · {blocked} blocked · {unsolved} unsolved · {needs} needs evidence"
                ),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            {
                // the header's names come from the help module, so the help
                // page's column coverage test pins every rendered column
                let [
                    technique,
                    leads,
                    fact,
                    likely,
                    unsure,
                    untrust,
                    confirmed,
                    disputed,
                    neutral,
                    ignored,
                ] = super::help::DETECTORS_COLUMNS;
                Line::from(Span::styled(
                    format!(
                        " {technique:<22} {leads:>5} {fact:>5} {likely:>6} {unsure:>6} {untrust:>9} {confirmed:>9} {disputed:>8} {neutral:>7} {ignored:>7}"
                    ),
                    Style::default().add_modifier(Modifier::BOLD),
                ))
            },
        ];
        if order.is_empty() {
            lines.push(Line::from("   no detector-seeded leads in this fleet"));
        }
        // window the cursor list
        let used = lines.len();
        let height = (area.height as usize).saturating_sub(used + 4);
        let first = cursor.saturating_sub(height.saturating_sub(1) / 2);
        for (index, (id, tally)) in order.iter().enumerate().skip(first).take(height.max(1)) {
            let bucket = |key: &str| tally.confidence.get(key).copied().unwrap_or(0);
            let mut line = Line::from(format!(
                " {:<22} {:>5} {:>5} {:>6} {:>6} {:>9} {:>9} {:>8} {:>7} {:>7}",
                clip(id, 22),
                tally.leads,
                bucket("fact"),
                bucket("likely"),
                bucket("unsure"),
                bucket("untrusted"),
                tally.confirmed,
                tally.disputed,
                tally.neutral,
                tally.ignored
            ));
            if index == cursor {
                line = line.style(Style::default().add_modifier(Modifier::REVERSED));
            }
            lines.push(line);
        }
        // the registry detections that never fired across the fleet: merge
        // hits, runs, and failures so a crashing detection never reads as a
        // clean miss
        let mut merged: BTreeMap<String, (u64, u64, u64)> = BTreeMap::new();
        let mut rosters = 0usize;
        for row in self.visible_rows() {
            if !row.session.row.detectors_run.is_empty() {
                rosters += 1;
            }
            for (name, tally) in &row.session.row.detectors_run {
                let entry = merged.entry(name.clone()).or_default();
                entry.0 += tally.hits;
                entry.1 += tally.runs;
                entry.2 += tally.failed;
            }
        }
        // never-fired means it completed at least one pass and still hit
        // nothing; a detection that failed every pass is broken, not useless
        let never: Vec<&str> = merged
            .iter()
            .filter(|(_, (hits, runs, failed))| *hits == 0 && failed < runs)
            .map(|(name, _)| name.as_str())
            .collect();
        let broken: Vec<&str> = merged
            .iter()
            .filter(|(_, (hits, runs, failed))| *hits == 0 && failed >= runs)
            .map(|(name, _)| name.as_str())
            .collect();
        if rosters > 0 {
            lines.push(Line::from(""));
            if never.is_empty() && broken.is_empty() {
                lines.push(Line::from(Span::styled(
                    " every rostered detector fired at least once".to_owned(),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            if !never.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!(
                        " never fired ({} across {rosters} rostered session(s)):",
                        never.len()
                    ),
                    Style::default()
                        .add_modifier(Modifier::BOLD)
                        .fg(Color::Yellow),
                )));
                lines.push(Line::from(format!("   {}", never.join(", "))));
            }
            // broken detections get their own line so they are never sold as
            // clean zero-hit evidence
            if !broken.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!(" failed every pass ({}):", broken.len()),
                    Style::default().add_modifier(Modifier::BOLD).fg(Color::Red),
                )));
                lines.push(Line::from(format!("   {}", broken.join(", "))));
            }
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    /// Renders the token economics: savings, role shares, busiest hours
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The body area
    fn render_economics(&mut self, frame: &mut Frame, area: Rect) {
        // sum the fleet's buckets, role shares, savings, and hour burn
        let mut totals = UsageTotals::default();
        let mut savings = 0.0;
        let mut priced = 0usize;
        let mut roles: BTreeMap<String, u64> = BTreeMap::new();
        let mut hours: BTreeMap<String, u64> = BTreeMap::new();
        for row in self.visible_rows() {
            let session = &row.session.row;
            totals += session.totals;
            if let Some(model) = cost_model_of(&row.session, &self.pricing) {
                savings += model.cache_savings(
                    session.totals.cached_input_tokens,
                    session.totals.cache_creation_input_tokens,
                );
                priced += 1;
            }
            for (role, rollup) in &session.agents {
                *roles.entry(role.clone()).or_default() += rollup.total_tokens;
            }
            for (hour, bucket) in &session.hours {
                *hours.entry(hour.clone()).or_default() += bucket.total_tokens;
            }
        }
        let mut lines = vec![
            Line::from(Span::styled(
                format!(
                    " input {} · output {} · cache read {} · cache write {} · reasoning {}",
                    with_thousands(totals.input_tokens),
                    with_thousands(totals.output_tokens),
                    with_thousands(totals.cached_input_tokens),
                    with_thousands(totals.cache_creation_input_tokens),
                    with_thousands(totals.reasoning_tokens),
                ),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(format!(
                " est caching saved {} across {priced} priced session(s)",
                fmt_cost(savings)
            )),
            Line::from(""),
            Line::from(Span::styled(
                " tokens by role".to_owned(),
                Style::default().add_modifier(Modifier::BOLD),
            )),
        ];
        // the heaviest roles first, with proportional bars
        let mut role_order: Vec<(&String, &u64)> = roles.iter().collect();
        role_order.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        let heaviest = role_order.first().map(|(_, tokens)| **tokens).unwrap_or(0);
        for (role, tokens) in role_order.iter().take(12) {
            let frac = if heaviest > 0 {
                **tokens as f64 / heaviest as f64
            } else {
                0.0
            };
            lines.push(Line::from(format!(
                "   {:<22} {:>14} {}",
                clip(role, 22),
                with_thousands(**tokens),
                fill_bar(24, frac),
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " busiest hours (local time)".to_owned(),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        // the top hours by burn, with proportional bars
        let mut hour_order: Vec<(&String, &u64)> = hours.iter().collect();
        hour_order.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        let busiest = hour_order.first().map(|(_, tokens)| **tokens).unwrap_or(0);
        for (hour, tokens) in hour_order.iter().take(12) {
            let frac = if busiest > 0 {
                **tokens as f64 / busiest as f64
            } else {
                0.0
            };
            lines.push(Line::from(format!(
                "   {hour}  {:>14} {}",
                with_thousands(**tokens),
                fill_bar(24, frac),
            )));
        }
        self.render_scrolled(frame, area, lines);
    }

    /// Renders a line list under the shared scroll offset
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The body area
    /// * `lines` - The screen's full line list
    fn render_scrolled(&mut self, frame: &mut Frame, area: Rect, lines: Vec<Line<'static>>) {
        // clamp this screen's offset to the list and store it back, so an
        // overshot scroll never has to be unwound before the view moves again
        let height = area.height as usize;
        let first = self
            .scroll()
            .min(lines.len().saturating_sub(height.min(lines.len())));
        *self.scroll_mut() = first;
        // window the visible slice to the area
        let windowed: Vec<Line> = lines.into_iter().skip(first).take(height).collect();
        frame.render_widget(Paragraph::new(windowed), area);
    }
}

/// A tool's merged league row: calls, fails, and the per-session p50 medians
type ToolTally = (u64, u64, Vec<u64>);

/// A merged signature cluster's weight: count, loop flag, carrying sessions
type SigTally = (u64, bool, usize);

/// A session row's render-time cost model, from the local pricing table
///
/// Bundles carry a model id but not a provider protocol, so cache semantics
/// are conservatively non-inclusive (never understating fresh-input cost).
///
/// # Arguments
///
/// * `row` - The session row to price
/// * `pricing` - The config's best-effort pricing context
fn cost_model_of(row: &MergedSession, pricing: &PricingCtx) -> Option<CostModel> {
    let model = row.row.model.as_deref()?;
    // config overrides are written for the configured model only
    let overrides = match (&pricing.configured_model, &pricing.overrides) {
        (Some(configured), Some(overrides)) if configured.eq_ignore_ascii_case(model) => {
            Some(overrides)
        }
        _ => None,
    };
    Some(CostModel {
        pricing: pricing::resolve(model, overrides)?,
        input_includes_cached: false,
    })
}

/// A session row's render-time dollar cost, when its model prices
///
/// # Arguments
///
/// * `row` - The session row to price
/// * `pricing` - The config's best-effort pricing context
fn cost_of_row(row: &MergedSession, pricing: &PricingCtx) -> Option<f64> {
    let model = cost_model_of(row, pricing)?;
    let totals = &row.row.totals;
    Some(model.cost(
        totals.input_tokens,
        totals.output_tokens,
        totals.cached_input_tokens,
        totals.cache_creation_input_tokens,
    ))
}

/// Formats a millisecond count coarsely for the stall lines
///
/// # Arguments
///
/// * `ms` - The milliseconds to format
fn fmt_ms(ms: u64) -> String {
    // climb the units coarsely, truncating so a value just under a boundary
    // never reads as the next unit's full count ("59s", never "60s")
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{}s", ms / 1_000)
    } else if ms < 3_600_000 {
        format!("{}m", ms / 60_000)
    } else {
        format!("{:.1}h", ms as f64 / 3_600_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// Builds a pressed key event
    ///
    /// # Arguments
    ///
    /// * `code` - The key code to press
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// Builds a bundle-mode app over one synthetic session row
    fn bundle_app() -> ExploreApp {
        let mut row = crate::handlers::ai::telemetry::bundle::SessionTelemetry {
            session_id: "e".repeat(64),
            model: Some("claude-opus-5-5".to_owned()),
            ..Default::default()
        };
        row.totals.total_tokens = 1_234;
        let source = Source::Bundles {
            rows: vec![MergedSession {
                row,
                version: "0.2.0".to_owned(),
                recipe: 2,
            }],
            files: 1,
            levels: std::iter::once(1).collect(),
            recipes: std::iter::once(2u32).collect(),
            exporters: std::iter::once("0.2.0".to_owned()).collect(),
        };
        ExploreApp::new(source, PricingCtx::default())
    }

    /// Draws one frame and returns the buffer's visible text
    ///
    /// # Arguments
    ///
    /// * `app` - The app to render
    /// * `width` - The terminal width
    /// * `height` - The terminal height
    fn draw(app: &mut ExploreApp, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("a test terminal");
        terminal.draw(|frame| app.render(frame)).expect("a frame");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    /// Bundle-mode Enter opens the session-detail pop-out, never the drill
    #[test]
    fn bundle_enter_opens_detail_not_drill() {
        let mut app = bundle_app();
        app.handle_key(key(KeyCode::Enter));
        assert!(app.popup.is_some(), "the detail pop-out must open");
        assert!(app.drill_loading.is_none(), "bundles never load a drill");
        // the pop-out renders from bundle fields only: the id prefix labels it
        let text = draw(&mut app, 120, 40);
        assert!(text.contains("session detail"), "got {text:?}");
        assert!(
            text.contains("eeeeeeeeeeeeeeee…"),
            "the id prefix labels it"
        );
    }

    /// Escape closes the pop-out, then the palette, then quits
    #[test]
    fn escape_order_is_popup_palette_quit() {
        let mut app = bundle_app();
        // open the palette, then the help pop-out over it
        app.handle_key(key(KeyCode::Char(' ')));
        assert!(app.palette.active);
        app.handle_key(key(KeyCode::Char('h')));
        assert!(app.popup.is_some());
        assert!(!app.palette.active, "the palette closed to open help");
        // first escape: the pop-out
        app.handle_key(key(KeyCode::Esc));
        assert!(app.popup.is_none());
        assert!(!app.should_quit);
        // reopen the palette; the next escape closes it, not the app
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.palette.active);
        assert!(!app.should_quit);
        // and only now does escape quit
        app.handle_key(key(KeyCode::Esc));
        assert!(app.should_quit);
    }

    /// `?` opens help directly; Space-then-h opens it through the palette
    #[test]
    fn help_opens_from_both_paths() {
        let mut app = bundle_app();
        app.handle_key(key(KeyCode::Char('?')));
        assert!(app.popup.is_some());
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Char('h')));
        assert!(app.popup.is_some());
    }

    /// `i` opens the fleet cursor's session, whatever screen is active
    #[test]
    fn i_opens_the_fleet_cursor_from_any_screen() {
        let mut app = bundle_app();
        // a second, alphabetically-first session joins the fleet
        if let Source::Bundles { rows, .. } = &mut app.source {
            let mut other = rows[0].clone();
            other.row.session_id = "a".repeat(64);
            rows.push(other.clone());
            app.rows.insert(
                "extra".to_owned(),
                ExploreRow {
                    name: None,
                    session: other,
                    local: None,
                },
            );
        }
        // move the SIGNATURES cursor away from zero, then press `i` there
        app.screen = Screen::Signatures;
        *app.cursor_mut() = 1;
        app.handle_key(key(KeyCode::Char('i')));
        // the pop-out shows the fleet cursor's row (slot 0: the `a…` id),
        // not the signature cursor's index applied to the fleet
        let text = draw(&mut app, 120, 40);
        assert!(text.contains("aaaaaaaaaaaaaaaa…"), "got {text:?}");
        assert!(!text.contains("eeeeeeeeeeeeeeee…"), "got {text:?}");
    }

    /// Same-id rows with distinct creation stamps stay distinct in the fleet
    #[test]
    fn merged_rows_with_distinct_stamps_stay_distinct() {
        use chrono::TimeZone;
        let mut app = {
            // two exports of the same-named session, re-created at new times
            let mut first = crate::handlers::ai::telemetry::bundle::SessionTelemetry {
                session_id: "e".repeat(64),
                ..Default::default()
            };
            first.created_at = Some(chrono::Utc.timestamp_opt(1_000, 0).unwrap());
            let mut second = first.clone();
            second.created_at = Some(chrono::Utc.timestamp_opt(2_000, 0).unwrap());
            let source = Source::Bundles {
                rows: vec![
                    MergedSession {
                        row: first,
                        version: "0.2.0".to_owned(),
                        recipe: 2,
                    },
                    MergedSession {
                        row: second,
                        version: "0.2.0".to_owned(),
                        recipe: 2,
                    },
                ],
                files: 2,
                levels: std::iter::once(0).collect(),
                recipes: std::iter::once(2u32).collect(),
                exporters: std::iter::once("0.2.0".to_owned()).collect(),
            };
            ExploreApp::new(source, PricingCtx::default())
        };
        // merge() kept both rows, so the fleet must show both
        assert_eq!(app.rows.len(), 2, "distinct stamps must not re-collapse");
        let text = draw(&mut app, 120, 40);
        assert!(text.contains("sessions 2"), "got {text:?}");
    }

    /// Page keys page the cursor on cursor lists and scroll elsewhere
    #[test]
    fn page_keys_page_the_cursor_lists() {
        let mut app = bundle_app();
        // the fleet's single row clamps a page-down to the list's end
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(*app.cursor_mut(), 0, "one row clamps to index zero");
        assert_eq!(*app.scroll_mut(), 0, "cursor screens never move scroll");
        // an aggregate screen pages its scroll offset instead
        app.handle_key(key(KeyCode::Char('2')));
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(*app.scroll_mut(), 20, "tools pages the scroll");
        // the stuck screen pages its scroll too, so content past a short
        // terminal stays reachable, while its arrows still move the cursor
        app.handle_key(key(KeyCode::Char('4')));
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(*app.scroll_mut(), 20, "stuck pages the scroll");
        app.handle_key(key(KeyCode::Down));
        assert_eq!(*app.cursor_mut(), 1, "stuck arrows move the cursor");
    }

    /// A category whose rows all fell past the cap says so instead of hiding
    #[test]
    fn stuck_popup_notes_cap_starved_categories() {
        let mut app = bundle_app();
        if let Source::Bundles { rows, .. } = &mut app.source {
            let session = &mut rows[0].row;
            // the counts say two blocked questions exist, but the capped row
            // list only kept unsolved entries
            session.forum.blocked = 2;
            session.forum.entries = vec![ForumEntry {
                status: EntryStatus::Unsolved,
                ..ForumEntry::default()
            }];
            session.forum.entries_dropped = 2;
            let session = rows[0].clone();
            app.rows.insert(
                session.row.session_id.clone(),
                ExploreRow {
                    name: None,
                    session,
                    local: None,
                },
            );
        }
        app.screen = Screen::Stuck;
        // category 2 (blocked): every blocked row fell past the cap
        *app.cursor_mut() = 2;
        app.handle_key(key(KeyCode::Enter));
        let text = draw(&mut app, 130, 44);
        assert!(
            text.contains("beyond the per-session row cap"),
            "got {text:?}"
        );
        assert!(!text.contains("no blocked questions"), "got {text:?}");
    }

    /// The per-screen help renders real content, even at a narrow size
    #[test]
    fn help_renders_per_screen_at_narrow_sizes() {
        let mut app = bundle_app();
        // the fleet page, at the smallest size the drive matrix cares about
        app.handle_key(key(KeyCode::Char('?')));
        let text = draw(&mut app, 80, 24);
        assert!(text.contains("help — fleet"), "got {text:?}");
        assert!(text.contains("Every session"), "the body must render");
        // the help follows the active screen
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Char('3')));
        app.handle_key(key(KeyCode::Char('?')));
        let text = draw(&mut app, 80, 24);
        assert!(text.contains("help — signatures"), "got {text:?}");
    }

    /// The sidebar appears on wide terminals and hides on narrow ones
    #[test]
    fn sidebar_tracks_terminal_width() {
        let mut app = bundle_app();
        let wide = draw(&mut app, 120, 40);
        assert!(wide.contains("source"), "the sidebar labels its sections");
        assert!(wide.contains("1 bundle file(s)"), "got sidebar-less frame");
        let narrow = draw(&mut app, 90, 30);
        assert!(
            !narrow.contains("1 bundle file(s)"),
            "narrow hides the sidebar"
        );
    }

    /// Enter on the signatures screen opens the full multi-line cluster
    #[test]
    fn signature_enter_opens_multiline_detail() {
        let mut app = bundle_app();
        // give the one session a multi-line signature cluster
        if let Source::Bundles { rows, .. } = &mut app.source {
            rows[0]
                .row
                .signatures
                .push(crate::handlers::ai::telemetry::bundle::SignatureCluster {
                    tool: "run_script".to_owned(),
                    sig: "script error\n  at frame N\n  at PATH".to_owned(),
                    count: 7,
                    looped: true,
                });
            let session = rows[0].clone();
            app.rows.insert(
                session.row.session_id.clone(),
                ExploreRow {
                    name: None,
                    session,
                    local: None,
                },
            );
        }
        app.screen = Screen::Signatures;
        app.handle_key(key(KeyCode::Enter));
        assert!(app.popup.is_some(), "the signature pop-out must open");
        // the pop-out keeps the signature's line structure
        let text = draw(&mut app, 120, 40);
        assert!(text.contains("failure signature"), "got {text:?}");
        assert!(text.contains("at frame N"), "line two renders: {text:?}");
        assert!(text.contains("loop!"), "the loop legend renders: {text:?}");
        // and the list rows behind it never carry a raw newline
        app.popup = None;
        let list = draw(&mut app, 120, 40);
        assert!(list.contains("(+2 lines)"), "the badge renders: {list:?}");
    }

    /// The stuck pop-out filters by category and honors the title posture
    #[test]
    fn stuck_popup_filters_and_gates_titles() {
        let mut app = bundle_app();
        // the bundle row: one unsolved question with a level-1 title, one
        // blocked question whose title level 0 cleared
        if let Source::Bundles { rows, .. } = &mut app.source {
            rows[0].row.forum.entries = vec![
                ForumEntry {
                    status: EntryStatus::Unsolved,
                    title: Some("Where is the C2 config?".to_owned()),
                    ..ForumEntry::default()
                },
                ForumEntry {
                    status: EntryStatus::Blocked,
                    title: None,
                    ..ForumEntry::default()
                },
            ];
            let session = rows[0].clone();
            app.rows.insert(
                session.row.session_id.clone(),
                ExploreRow {
                    name: None,
                    session,
                    local: None,
                },
            );
        }
        app.screen = Screen::Stuck;
        // category 0 (unsolved): the level-1 title shows
        app.handle_key(key(KeyCode::Enter));
        let text = draw(&mut app, 120, 40);
        assert!(text.contains("Where is the C2 config?"), "got {text:?}");
        assert!(!text.contains("title not in bundle"), "got {text:?}");
        app.popup = None;
        // category 2 (blocked): the cleared title shows the privacy note
        *app.cursor_mut() = 2;
        app.handle_key(key(KeyCode::Enter));
        let text = draw(&mut app, 120, 40);
        assert!(text.contains("blocked questions"), "got {text:?}");
        assert!(
            text.contains("title not in bundle (privacy)"),
            "got {text:?}"
        );
    }

    /// Local prose outranks bundle titles, and legacy bundles say so
    #[test]
    fn stuck_popup_prefers_local_titles_and_flags_legacy() {
        use crate::handlers::ai::telemetry::fold::{EntryDetail, LocalForumDetail};
        let mut app = bundle_app();
        if let Source::Bundles { rows, .. } = &mut app.source {
            // a local-mode-shaped row: entry title None, local prose present
            rows[0].row.forum.entries = vec![ForumEntry {
                status: EntryStatus::Unsolved,
                title: None,
                ..ForumEntry::default()
            }];
            let session = rows[0].clone();
            app.rows.insert(
                session.row.session_id.clone(),
                ExploreRow {
                    name: Some("case-alpha".to_owned()),
                    session: session.clone(),
                    local: Some(LocalForumDetail {
                        entries: vec![EntryDetail {
                            name: Some("q-unpack".to_owned()),
                            title: "Can the payload be unpacked?".to_owned(),
                        }],
                        leads: Vec::new(),
                    }),
                },
            );
            // and a legacy row: counts without entry rows
            let mut legacy = session;
            legacy.row.session_id = "f".repeat(64);
            legacy.row.forum.entries = Vec::new();
            legacy.row.forum.unsolved = 3;
            app.rows.insert(
                legacy.row.session_id.clone(),
                ExploreRow {
                    name: None,
                    session: legacy,
                    local: None,
                },
            );
        }
        app.screen = Screen::Stuck;
        app.handle_key(key(KeyCode::Enter));
        let text = draw(&mut app, 130, 44);
        assert!(
            text.contains("Can the payload be unpacked?"),
            "got {text:?}"
        );
        assert!(
            text.contains("bundle predates per-entry rows"),
            "got {text:?}"
        );
    }

    /// The detector pop-out filters by technique and keeps prose local-only
    #[test]
    fn detector_popup_filters_by_technique() {
        use crate::handlers::ai::telemetry::bundle::{
            DetectorLead, DetectorTally, LeadConfidence, LeadVerdict,
        };
        use crate::handlers::ai::telemetry::fold::{LeadDetail, LocalForumDetail};
        let mut app = bundle_app();
        if let Source::Bundles { rows, .. } = &mut app.source {
            let session = &mut rows[0].row;
            session.detectors.insert(
                "T1486".to_owned(),
                DetectorTally {
                    ignored: 1,
                    leads: 1,
                    ..DetectorTally::default()
                },
            );
            session.detectors.insert(
                "T1055".to_owned(),
                DetectorTally {
                    confirmed: 1,
                    leads: 1,
                    ..DetectorTally::default()
                },
            );
            session.detector_leads = vec![
                DetectorLead {
                    technique: "T1486".to_owned(),
                    post_class: "fn".to_owned(),
                    confidence: Some(LeadConfidence::Fact),
                    suspicion: Some(80),
                    verdict: LeadVerdict::Ignored,
                    ..DetectorLead::default()
                },
                DetectorLead {
                    technique: "T1055".to_owned(),
                    post_class: "fn".to_owned(),
                    verdict: LeadVerdict::Confirmed,
                    ..DetectorLead::default()
                },
            ];
            let session = rows[0].clone();
            app.rows.insert(
                session.row.session_id.clone(),
                ExploreRow {
                    name: Some("case-alpha".to_owned()),
                    session: session.clone(),
                    local: Some(LocalForumDetail {
                        entries: Vec::new(),
                        leads: vec![
                            LeadDetail {
                                post_name: "fn-00401000".to_owned(),
                                post_title: "ransom loop".to_owned(),
                                finding: "encrypt loop".to_owned(),
                            },
                            LeadDetail {
                                post_name: "fn-00402000".to_owned(),
                                post_title: "inject stub".to_owned(),
                                finding: "injection".to_owned(),
                            },
                        ],
                    }),
                },
            );
        }
        app.screen = Screen::Detectors;
        // both techniques tally 1 lead; order is by total then id: T1055 first
        app.handle_key(key(KeyCode::Enter));
        let text = draw(&mut app, 140, 44);
        assert!(text.contains("T1055 leads"), "got {text:?}");
        assert!(text.contains("inject stub"), "local prose shows: {text:?}");
        assert!(
            !text.contains("ransom loop"),
            "the other technique filters out"
        );
        app.popup = None;
        // the second row opens the other technique
        *app.cursor_mut() = 1;
        app.handle_key(key(KeyCode::Enter));
        let text = draw(&mut app, 140, 44);
        assert!(text.contains("T1486 leads"), "got {text:?}");
        assert!(
            text.contains("susp 80"),
            "the lead's facts render: {text:?}"
        );
    }

    /// The never-fired section lists zero-hit rostered detectors
    #[test]
    fn never_fired_lists_zero_hit_detectors() {
        use crate::handlers::ai::telemetry::bundle::DetectorRunTally;
        let mut app = bundle_app();
        if let Source::Bundles { rows, .. } = &mut app.source {
            let session = &mut rows[0].row;
            session.detectors_run.insert(
                "packing".to_owned(),
                DetectorRunTally {
                    technique: "T1027.002".to_owned(),
                    runs: 1,
                    hits: 4,
                    failed: 0,
                },
            );
            session.detectors_run.insert(
                "self_deletion".to_owned(),
                DetectorRunTally {
                    technique: "T1070.004".to_owned(),
                    runs: 1,
                    hits: 0,
                    failed: 0,
                },
            );
            let session = rows[0].clone();
            app.rows.insert(
                session.row.session_id.clone(),
                ExploreRow {
                    name: None,
                    session,
                    local: None,
                },
            );
        }
        app.screen = Screen::Detectors;
        let text = draw(&mut app, 140, 44);
        assert!(text.contains("never fired"), "got {text:?}");
        assert!(text.contains("self_deletion"), "got {text:?}");
        assert!(!text.contains("never fired (2"), "packing fired");
    }

    /// The version filter cycles through the fleet's versions and back off
    #[test]
    fn version_filter_cycles_and_wraps() {
        let mut app = bundle_app();
        assert!(app.filter_version.is_none());
        app.handle_key(key(KeyCode::Char('f')));
        assert_eq!(app.filter_version.as_deref(), Some("0.2.0"));
        assert_eq!(app.visible_rows().len(), 1);
        // one more cycle wraps back to no filter
        app.handle_key(key(KeyCode::Char('f')));
        assert!(app.filter_version.is_none());
    }
}
