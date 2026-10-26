//! Chat with an AI in thorctl

use kanal::{AsyncReceiver, AsyncSender};
use thorium::{CtlConf, Error};
use thorpal::{
    AnalystAnswer, PendingAsk, Session, SessionStore, ThorPalEventSender, ToolLogLayer,
    ToolLogRouter, TranscriptEntry, TranscriptItem, TranscriptLine, UsageTotals,
};

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::execute;
use ratatui::{DefaultTerminal, Frame};
use std::collections::BTreeMap;
use std::io::stdout;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration as StdDuration;
use uuid::Uuid;

use super::pricing::CostModel;
use super::session;
use crate::args::ai::Chat as ChatArgs;

mod chat;
mod commands;
mod components;
mod context;
mod events;
mod home;
mod tabs;

pub(super) use chat::Chat;
use components::{AskModal, AskModalState, AskOutcome, ReportSettings, TabBar, ThorPalClient};
// re-export the pieces the standalone `analyze usage` and `analyze setup` TUIs reuse:
// the stats and gantt views with their logs and transcript folds, the replay
// helpers, the shared formatting primitives, and the text input widget
pub(super) use components::{
    ActivityIntervals, AgentStatsLog, AgentStatsView, GanttLog, GanttView, SideRegistry, TextBox,
    Theme, TimeMap, TraversalLog, ViewOutcome, estimate_tokens, fmt_cost, fmt_duration, now_ms,
    replay_transcript, replayed_usage, usage_remainder, with_thousands,
};
pub(super) use context::{ChatContext, ChatEntry};
pub(super) use events::{AppEvent, ScrollEvent};
pub(super) use home::Home;
pub(super) use tabs::{ActiveTabKind, Tab};

/// Drive the periodic redraws that animate the banner and the thinking throbber
///
/// While `animate_home` is set the home page's banner needs a steady redraw; once
/// the page becomes a chat (the flag is cleared) the ticker keeps running but only
/// redraws while a run is busy -- the main conversation's or any side's -- so the
/// throbber animates during quiet gaps where no run event fires and the app
/// otherwise sits idle.
///
/// # Arguments
///
/// * `sender` - The channel to request redraws over
/// * `animate_home` - Set while the home page's banner should keep animating
/// * `client` - The chat client whose main and side activity is consulted
/// * `traversal_animating` - Set while a traversal replay is playing
async fn refresh(
    sender: AsyncSender<AppEvent>,
    animate_home: Arc<AtomicBool>,
    client: Arc<ThorPalClient>,
    traversal_animating: Arc<AtomicBool>,
) {
    // keep ticking for the life of the app
    loop {
        // redraw while the home banner is animating, any conversation's turn is
        // in flight, or a traversal replay is advancing
        if animate_home.load(Ordering::Relaxed)
            || client.activity.snapshot().busy
            || client.sides.any_busy()
            || traversal_animating.load(Ordering::Relaxed)
        {
            // try to send a redraw event; a failure just means the app is gone
            let _ = sender.send(AppEvent::Redraw).await;
        }
        // sleep for 100ms between frames
        tokio::time::sleep(StdDuration::from_millis(100)).await;
    }
}

/// Follow-up work an app event asks the event loop itself to run
///
/// Some events (running the external editor) need the loop's exclusive access
/// to the terminal, so handling them is deferred back to [`App::start`] rather
/// than done inside the event handler.
enum FollowUp {
    /// Suspend the TUI and edit a /done summary in the user's default editor
    EditInEditor {
        /// The side conversation whose summary is being edited
        name: String,
        /// The current summary text to edit
        summary: String,
    },
}

/// The thorium application state
pub struct App {
    /// The sender side of our event channel
    event_tx: AsyncSender<AppEvent>,
    /// The receive side of our event channel
    event_rx: AsyncReceiver<AppEvent>,
    /// The shared chat client every tab drives
    client: Arc<ThorPalClient>,
    /// Every open tab: the main conversation first, then its side conversations
    tabs: Vec<Tab>,
    /// The index of the currently focused tab
    active: usize,
    /// The clickable tab strip across the top of the screen
    tab_bar: TabBar,
    /// Whether the tui should exit or not
    should_quit: bool,
    /// Whether the home page's banner should keep animating (cleared once we
    /// switch to a chat page, after which the throbber drives its own redraws)
    animate_home: Arc<AtomicBool>,
    /// The ask_analyst question modal, while one is open
    ///
    /// App-level (not per page or tab) so the question draws over whichever
    /// tab or full-screen overlay is showing and owns the keyboard everywhere:
    /// the agent is parked mid-turn on this answer.
    ask_modal: Option<AskModal>,
    /// Pending asks waiting for the open question modal to resolve first
    ask_queue: std::collections::VecDeque<PendingAsk>,
}

impl App {
    /// Create a new chat app from a client built against the app's event channel
    ///
    /// Because the client's background tasks must send [`AppEvent`]s (redraws,
    /// failures) onto the *same* channel this app's event loop reads, the client
    /// is built here via `make_client`, which is handed the app's own event sender.
    /// A resumed session's saved side conversations each reopen as their own tab
    /// alongside the main one.
    ///
    /// # Arguments
    ///
    /// * `make_client` - A factory that builds the client from the app's event sender
    pub fn from_client(
        make_client: impl FnOnce(&AsyncSender<AppEvent>) -> Arc<ThorPalClient>,
    ) -> Self {
        // build our event channel
        let (event_tx, event_rx) = kanal::unbounded_async();
        // build the client against this app's event sender so its worker tasks'
        // redraw/failure events reach the loop below
        let client = make_client(&event_tx);
        // build the main conversation's tab around the supplied client
        let main_tab = Tab::from_client("main", client.clone(), &event_tx);
        // only animate the home banner when the tab actually opens on home (a
        // resumed session opens on its chat page instead)
        let animate_home = main_tab.active == ActiveTabKind::Home;
        // reopen a resumed session's saved side conversations as their own tabs
        let mut tabs = vec![main_tab];
        for name in client.sides.names() {
            tabs.push(Tab::for_side(&name, client.clone(), &event_tx));
        }
        App {
            event_tx,
            event_rx,
            client,
            tabs,
            active: 0,
            tab_bar: TabBar::default(),
            should_quit: false,
            animate_home: Arc::new(AtomicBool::new(animate_home)),
            ask_modal: None,
            ask_queue: std::collections::VecDeque::new(),
        }
    }

    /// Opens the question modal for a pending analyst ask (or queues it)
    ///
    /// Concurrent conversations can raise several asks at once; the app shows
    /// one modal at a time and poses the rest in arrival order.
    ///
    /// # Arguments
    ///
    /// * `ask` - The pending ask to pose to the analyst
    fn ask_pending(&mut self, ask: PendingAsk) {
        // show the ask now, or park it behind the one already open
        match self.ask_modal {
            Some(_) => self.ask_queue.push_back(ask),
            None => self.ask_modal = Some(AskModal::new(ask)),
        }
    }

    /// Closes a still-open question modal whose ask already resolved
    ///
    /// The normal path closes the modal when the analyst answers it here; this
    /// covers the races where the ask resolved elsewhere (the link detached, or
    /// the awaiting turn died) so a dead modal never lingers.
    ///
    /// # Arguments
    ///
    /// * `id` - The resolved ask's link ticket id
    fn ask_resolved(&mut self, id: u64) {
        // only a modal posing exactly this ask is stale now
        if self
            .ask_modal
            .as_ref()
            .is_some_and(|modal| modal.ask.id == id)
        {
            self.ask_modal = None;
            // surface the next queued ask, if any arrived meanwhile
            self.open_next_ask();
        }
        // drop a queued copy of the resolved ask too, so it is never re-posed
        self.ask_queue.retain(|ask| ask.id != id);
    }

    /// Opens the next queued ask's modal, if any is waiting
    fn open_next_ask(&mut self) {
        // pose the oldest queued ask next, preserving arrival order
        if let Some(next) = self.ask_queue.pop_front() {
            self.ask_modal = Some(AskModal::new(next));
        }
    }

    /// Applies the question modal's outcome, answering through the analyst link
    ///
    /// # Arguments
    ///
    /// * `outcome` - What the modal asked the app to do
    fn handle_ask_outcome(&mut self, outcome: AskOutcome) {
        // map the outcome onto the analyst's answer, if the modal is done
        let answer = match &outcome {
            AskOutcome::None => return,
            AskOutcome::Pick(index) => {
                // answer with the picked option's exact label
                let Some(modal) = &self.ask_modal else { return };
                match modal.ask.options.get(*index) {
                    Some(option) => AnalystAnswer::Picked {
                        label: option.label.clone(),
                    },
                    // a stale index cannot happen from the modal, but decline
                    // beats panicking in raw mode if it ever does
                    None => AnalystAnswer::Declined,
                }
            }
            AskOutcome::Other(text) => AnalystAnswer::Other { text: text.clone() },
            AskOutcome::Decline => AnalystAnswer::Declined,
        };
        // close the modal and send the answer; a false return means the ask
        // was already resolved or its asking turn died, so tell the analyst
        // their answer went nowhere instead of silently discarding it
        if let Some(modal) = self.ask_modal.take()
            && !self.client.analyst.answer(modal.ask.id, answer)
        {
            self.client.context.add_ai(format!(
                "⚠ the question \"{}\" was already resolved (or its turn ended); \
                 your answer was not used",
                modal.ask.question
            ));
        }
        // surface the next queued ask, if any arrived meanwhile
        self.open_next_ask();
    }

    /// The currently focused tab
    fn active_tab(&mut self) -> &mut Tab {
        // the active index is clamped whenever a tab closes, so this is safe
        &mut self.tabs[self.active]
    }

    /// Take the chat worker's join handle for the quit path's finish-await
    ///
    /// Consumed by [`tui`] after the event loop exits so the worker's final
    /// [`thorpal::Session::finish`] can be awaited rather than cancelled by
    /// process exit.
    fn take_worker_handle(&self) -> Option<tokio::task::JoinHandle<Result<(), Error>>> {
        // defer to the client's one-shot handle slot
        self.client.take_join_handle()
    }

    /// Detach the analyst link so a quit mid-question can never wedge the join
    ///
    /// Every pending ask resolves as declined: an in-flight turn parked on
    /// `ask_analyst` takes its recommended default (recorded and auto-flagged
    /// for review) and the worker loop then exits through its normal finish.
    fn detach_analyst(&self) {
        // resolve every pending ask so no tool future stays parked
        self.client.analyst.detach();
    }

    /// Find the tab rendering a named side conversation, if it is open
    ///
    /// # Arguments
    ///
    /// * `name` - The side conversation to find the tab of
    fn tab_by_scope(&mut self, name: &str) -> Option<&mut Tab> {
        // match on each tab's scope; the main tab never matches
        self.tabs
            .iter_mut()
            .find(|tab| tab.scope.as_deref() == Some(name))
    }

    /// Handle key input when the query box is focused
    ///
    /// # Arguments
    ///
    /// * `code` - The key code that was pressed
    pub async fn handle_key(&mut self, key: KeyEvent) {
        // check if this is a ctrl-c
        match key.code {
            // if this is a ctrl-c then we need to quit
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true
            }
            // Alt+Left/Right cycle through the open tabs
            KeyCode::Left if key.modifiers.contains(KeyModifiers::ALT) => {
                self.active = self.active.checked_sub(1).unwrap_or(self.tabs.len() - 1);
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::ALT) => {
                self.active = (self.active + 1) % self.tabs.len();
            }
            // Alt+1..9 jump straight to a tab by its position
            KeyCode::Char(c @ '1'..='9') if key.modifiers.contains(KeyModifiers::ALT) => {
                // ignore a jump past the open tabs rather than clamping it
                let index = c as usize - '1' as usize;
                if index < self.tabs.len() {
                    self.active = index;
                }
            }
            // an open analyst question modal owns every other key, whatever
            // tab or overlay is showing: the agent is parked on this answer
            _ => match &mut self.ask_modal {
                Some(modal) => {
                    let outcome = modal.handle_key(key);
                    self.handle_ask_outcome(outcome);
                }
                // otherwise forward the key to the focused tab
                None => self.tabs[self.active].handle_key(key).await,
            },
        }
    }

    /// Handle a mouse click event
    ///
    /// # Arguments
    ///
    /// * `x` - The column position of the click
    /// * `y` - The row position of the click
    fn handle_click(&mut self, x: u16, y: u16) {
        // an open analyst question modal owns clicks (row selection only)
        if let Some(modal) = &mut self.ask_modal {
            modal.handle_click(x, y);
            return;
        }
        // a click on the tab strip switches tabs instead of hitting the page
        if let Some(index) = self.tab_bar.hit(x, y)
            && !self.active_tab().overlay_open()
        {
            self.active = index;
            return;
        }
        self.active_tab().handle_click(x, y);
    }

    /// Handle an incoming terminal event
    ///
    /// Dispatches key presses and mouse clicks to the appropriate handlers.
    ///
    /// # Arguments
    ///
    /// * `event` - The terminal event to process
    pub async fn handle_event(&mut self, event: Event) {
        match event {
            // handle key press events
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                self.handle_key(key).await;
            }
            // a bracketed paste arrives as one event: route the whole text to
            // the focused text box so embedded newlines become content instead
            // of Enter presses that would submit the prompt early; an open
            // question modal takes it only into its free-text answer box
            Event::Paste(text) => match &mut self.ask_modal {
                Some(modal) if modal.state == AskModalState::Typing => {
                    modal.editor.insert_str(&text);
                }
                // every other modal state has nothing to paste into
                Some(_) => (),
                None => self.active_tab().handle_paste(&text).await,
            },
            // handle mouse click events (work in any mode)
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    self.handle_click(mouse.column, mouse.row);
                }
                MouseEventKind::ScrollUp => self.active_tab().handle_scroll(ScrollEvent::ScrollUp),
                MouseEventKind::ScrollDown => {
                    self.active_tab().handle_scroll(ScrollEvent::ScrollDown);
                }
                MouseEventKind::ScrollLeft => {
                    self.active_tab().handle_scroll(ScrollEvent::ScrollLeft);
                }
                MouseEventKind::ScrollRight => {
                    self.active_tab().handle_scroll(ScrollEvent::ScrollRight);
                }
                _ => {}
            },
            // ignore all other events
            _ => {}
        }
    }

    /// Handle a single app event without drawing
    ///
    /// Drawing is decoupled from event handling so the event loop can drain a
    /// burst of queued events and then draw once, rather than once per event.
    /// An event needing the event loop's exclusive terminal access (running the
    /// external editor) is handed back as a [`FollowUp`] instead of being done
    /// here.
    ///
    /// # Arguments
    ///
    /// * `event` - The app event to handle
    async fn handle_app_event(&mut self, event: AppEvent) -> Option<FollowUp> {
        match event {
            // handle this terminal event
            AppEvent::Terminal(terminal_event) => {
                self.handle_event(terminal_event).await;
            }
            // convert the main tab's home page to a chat page
            AppEvent::HomeToChat => {
                // convert our home page to a chat page
                self.tabs[0].home_to_chat().await;
                // stop our refresh worker
                self.animate_home.store(false, Ordering::Relaxed);
            }
            // process a tool call failure by surfacing it to the user as
            // an assistant bubble rather than silently dropping it (main-turn
            // failures only; side turns bubble into their own tabs directly)
            AppEvent::ToolCallFailure { error } => self.tabs[0].push_error(&error),
            // Change the main tab's active content kind
            AppEvent::SetActiveTabKind(kind) => self.set_active_tab_kind(kind),
            // refresh every tab's message board with the forum's latest state
            // (the forum is session-wide, so each tab shows the same board)
            AppEvent::ForumUpdated { view, target } => {
                for tab in &mut self.tabs {
                    tab.update_board(view.clone(), target.clone());
                }
            }
            // refresh every tab's board without a selection jump: an ask or
            // review flag was recorded, and neither is a post to reveal
            AppEvent::ForumRefreshed { view } => {
                for tab in &mut self.tabs {
                    tab.refresh_board(view.clone());
                }
            }
            // pose a pending analyst ask app-wide: the modal draws over
            // whichever tab or overlay is showing
            AppEvent::AnalystAsk { ask } => self.ask_pending(ask),
            // close a stale question modal whose ask already resolved elsewhere
            AppEvent::AnalystAskResolved { id } => self.ask_resolved(id),
            // deliver finished board-search hits to every tab's board, like
            // ForumUpdated: the event names the spawning board and flight, so
            // only that board applies it and every other board drops it
            // (routing to the active tab only would strand a background tab's
            // in-flight search latch)
            AppEvent::BoardSearchResults {
                board,
                flight,
                query,
                hits,
                terms,
            } => {
                for tab in &mut self.tabs {
                    tab.apply_board_search(board, flight, &query, hits.clone(), terms.clone());
                }
            }
            // open (or switch to) a side conversation's tab
            AppEvent::OpenBtw { name } => {
                // focus the side's tab when it is already open
                match self
                    .tabs
                    .iter()
                    .position(|tab| tab.scope.as_deref() == Some(name.as_str()))
                {
                    Some(index) => self.active = index,
                    // otherwise open a fresh tab for it and focus that
                    None => {
                        self.tabs
                            .push(Tab::for_side(&name, self.client.clone(), &self.event_tx));
                        self.active = self.tabs.len() - 1;
                    }
                }
            }
            // deliver a finished /done summary to its side's tab
            AppEvent::BtwSummaryReady { name, summary } => {
                if let Some(tab) = self.tab_by_scope(&name) {
                    tab.summary_ready(summary);
                }
            }
            // deliver a failed /done summary to its side's tab
            AppEvent::BtwSummaryFailed { name, error } => {
                if let Some(tab) = self.tab_by_scope(&name) {
                    tab.summary_failed(error);
                }
            }
            // a side conversation closed: drop its tab and clamp the focus
            AppEvent::CloseBtwTab { name } => {
                self.tabs
                    .retain(|tab| tab.scope.as_deref() != Some(name.as_str()));
                // focus the main tab when the closed tab was focused
                if self.active >= self.tabs.len() {
                    self.active = 0;
                }
            }
            // the external-editor edit needs the loop's terminal access
            AppEvent::EditInEditor { name, summary } => {
                return Some(FollowUp::EditInEditor { name, summary });
            }
            // open or close the focused tab's message board sidebar
            AppEvent::ToggleMessageBoard => self.active_tab().toggle_board(),
            // open the board (if needed) and drop straight into its search box
            AppEvent::OpenForumSearch => self.active_tab().open_board_search(),
            // open or close the focused tab's traversal graph overlay
            AppEvent::ToggleTraversal => self.active_tab().toggle_traversal(),
            // open or close the focused tab's agent stats overlay
            AppEvent::ToggleAgentStats => self.active_tab().toggle_agent_stats(),
            // open or close the focused tab's gantt timeline overlay
            AppEvent::ToggleGantt => self.active_tab().toggle_gantt(),
            // an external signal asked us to stop: take the graceful quit path
            AppEvent::Quit => self.should_quit = true,
            // don't do anything just redraw the tui
            AppEvent::Redraw => (),
        }
        None
    }

    /// Update the main tab's active content kind
    fn set_active_tab_kind(&mut self, new: ActiveTabKind) {
        // handle the change to the new tab kind
        match new {
            ActiveTabKind::Home | ActiveTabKind::Chat => self.tabs[0].active = new,
        }
    }

    /// Render the application to the terminal
    ///
    /// Draws the focused tab's content, then the clickable tab strip into the
    /// 3-row band the pages reserve at the top (only when more than one tab is
    /// open, and never over a full-screen overlay).
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render widgets to
    pub fn render(&mut self, frame: &mut Frame) {
        // render the focused tab's content first
        self.tabs[self.active].render(frame);
        // paint the tab strip into the reserved band unless an overlay owns the
        // whole screen
        if !self.tabs[self.active].overlay_open() {
            let area = frame.area();
            let strip = ratatui::layout::Rect::new(area.x, area.y, area.width, area.height.min(3));
            self.tab_bar.render(frame, strip, &self.tabs, self.active);
        }
        // render the analyst question modal above every tab and overlay last,
        // so the question is always visible while it owns the keyboard
        if let Some(modal) = &mut self.ask_modal {
            let area = frame.area();
            modal.render(frame, area);
        }
    }

    /// Suspend only the terminal UI and edit a summary in the user's editor
    ///
    /// Resolves `$VISUAL`, then `$EDITOR`, then `vi`, writes the summary to a
    /// temp file, leaves the alternate screen, and waits for the editor
    /// asynchronously -- so every background task (main turns, side turns,
    /// sub-agents, the event drain, session autosaves) keeps running untouched
    /// while the editor owns the terminal; events simply queue on the app's
    /// unbounded channels and the next draw catches the UI up. The terminal is
    /// always restored before the result is inspected, and any failure falls
    /// back to the in-modal editor.
    ///
    /// # Arguments
    ///
    /// * `terminal` - The terminal to suspend and restore around the editor
    /// * `name` - The side conversation whose summary is being edited
    /// * `summary` - The current summary text to edit
    async fn edit_in_editor(
        &mut self,
        terminal: &mut DefaultTerminal,
        name: String,
        summary: String,
    ) {
        // resolve the user's editor, falling back to vi like git does
        let editor = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .unwrap_or_else(|_| "vi".to_owned());
        // write the summary to a temp file for the editor to work on
        let path = std::env::temp_dir().join(format!("thorctl-btw-{name}-{}.md", Uuid::new_v4()));
        if tokio::fs::write(&path, &summary).await.is_err() {
            // nothing to edit on disk, so fall back to the in-modal editor
            if let Some(tab) = self.tab_by_scope(&name) {
                tab.summary_edit_fallback();
            }
            return;
        }
        // suspend only the UI: leave the alternate screen and give the editor
        // a cooked terminal; every background task keeps running
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = execute!(
            stdout(),
            DisableMouseCapture,
            DisableBracketedPaste,
            crossterm::terminal::LeaveAlternateScreen
        );
        // run the editor and wait for it asynchronously, so the tokio runtime
        // keeps polling the prompt/drain/side tasks the whole time
        let status = tokio::process::Command::new(&editor)
            .arg(&path)
            .status()
            .await;
        // always restore the terminal before inspecting the result, so even a
        // failed editor never leaves the shell in the TUI's raw mode
        let _ = crossterm::terminal::enable_raw_mode();
        let _ = execute!(
            stdout(),
            crossterm::terminal::EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        );
        // force a full repaint; the queued event backlog then catches the UI up
        let _ = terminal.clear();
        // a clean editor exit hands the edited file back to the summary modal
        if status.map(|status| status.success()).unwrap_or(false)
            && let Ok(edited) = tokio::fs::read_to_string(&path).await
        {
            // the temp file is done with; losing the cleanup is harmless
            let _ = tokio::fs::remove_file(&path).await;
            if let Some(tab) = self.tab_by_scope(&name) {
                tab.summary_edited(edited);
            }
            return;
        }
        // the editor failed to run or exited unhappily: edit inline instead
        let _ = tokio::fs::remove_file(&path).await;
        if let Some(tab) = self.tab_by_scope(&name) {
            tab.summary_edit_fallback();
        }
    }

    /// Start handling events and updating our tui
    pub async fn start(&mut self, terminal: &mut DefaultTerminal) -> std::io::Result<()> {
        // spawn our event forwarders
        events::spawn_forwarders(&self.event_tx);
        // draw an initial frame until we get an event to handle
        terminal.draw(|frame| self.render(frame))?;
        // TODO only send this when we are on the home page
        tokio::task::spawn(refresh(
            self.event_tx.clone(),
            self.animate_home.clone(),
            self.client.clone(),
            self.tabs[0].traversal_animating(),
        ));
        // a non-blocking handle to the same event channel, used to coalesce a burst
        // of immediately-pending events into a single draw below
        let drain_rx = self.event_rx.clone_sync();
        // draw an initial frame until we get an event to handle
        while !self.should_quit {
            // get the next event to handle, blocking until one arrives
            match self.event_rx.recv().await {
                Ok(event) => {
                    // handle this event, then greedily drain any further events that
                    // are already queued so a flood of redraws collapses into one
                    // draw rather than one expensive draw per event
                    let mut follow_up = self.handle_app_event(event).await;
                    // pull every event that is ready right now without blocking
                    while let Ok(Some(next)) = drain_rx.try_recv() {
                        // stop draining once asked to quit; the outer loop exits
                        if self.should_quit {
                            break;
                        }
                        // keep the first follow-up; at most one editor can run
                        let next_follow_up = self.handle_app_event(next).await;
                        follow_up = follow_up.or(next_follow_up);
                    }
                    // run any follow-up needing the loop's terminal access
                    if let Some(FollowUp::EditInEditor { name, summary }) = follow_up {
                        self.edit_in_editor(terminal, name, summary).await;
                    }
                    // draw a single frame reflecting all the events just handled
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
}

/// Reopens a saved session from `~/.thorium/sessions/<name>` for the TUI
///
/// Loads the session store's saved state, re-injects live credentials into the
/// stored (sanitized) provider config, and rebuilds the [`Session`] around the
/// same store so it keeps saving into the same directory. Returns the restored
/// session plus the saved conversation rendered as transcript entries so the
/// TUI can replay the prior chat, the saved per-agent usage ledger so the
/// status bar's readout and the stats view's per-agent split continue them,
/// and the session's cost estimator when its model is
/// priceable. A missing session errors with the names of the sessions that do
/// exist, and a vanished binary is a hard error since Ghidra cannot reopen the
/// project without it.
///
/// # Arguments
///
/// * `conf` - The Thorctl config whose `ai` block supplies live credentials
/// * `cmd` - The parsed `chat` command, consulted for ignored override flags
/// * `name` - The name of the saved session to resume
/// * `events` - The event sink the resumed run mirrors its activity onto
pub(super) async fn resume_session(
    conf: &CtlConf,
    cmd: &ChatArgs,
    name: &str,
    events: ThorPalEventSender,
    config_path: &Path,
) -> Result<
    (
        Session,
        Vec<TranscriptLine>,
        BTreeMap<String, UsageTotals>,
        Option<CostModel>,
    ),
    Error,
> {
    // the stored session carries its own Ghidra and workflow settings, so the
    // override flags do not apply on a resume; warn rather than silently differ
    // (the TUI has not taken the screen yet, so printing here is safe)
    if cmd.ghidra_install.is_some() || cmd.workflow.is_some() {
        eprintln!(
            "warning: --ghidra-install/--workflow are ignored on --resume; the saved \
             session's settings are used instead"
        );
    }
    // resolve the sessions root the store saves under
    let root = SessionStore::default_root().map_err(|err| Error::new(format!("{err:#}")))?;
    // open the saved session, hinting at the sessions that do exist on a miss
    let (store, meta, mut snapshot, forum) = match SessionStore::open(root.clone(), name).await {
        Ok(opened) => opened,
        Err(err) => {
            // list what is resumable so a typo'd name is easy to correct
            let hint = match SessionStore::list(&root).await {
                Ok(sessions) if !sessions.is_empty() => {
                    format!("saved sessions: {}", sessions.join(", "))
                }
                _ => "no saved sessions exist yet".to_owned(),
            };
            return Err(Error::new(format!("{err:#} ({hint})")));
        }
    };
    // a vanished binary no longer blocks the resume: the analyzed Ghidra
    // project usually still holds the imported program, chat/forum/report
    // work needs no binary at all, and a bootstrap that does end up needing
    // the file now degrades with precise per-program guidance instead of
    // poisoning the session -- so warn and continue
    if !snapshot.ghidra.binary.is_file() {
        eprintln!(
            "warning: the analyzed binary '{}' no longer exists; Ghidra reopens the \
             saved project, but re-imports and raw file tools will fail",
            snapshot.ghidra.binary.display(),
        );
    }
    // a multi-sample session's missing siblings degrade the same way
    for extra in &snapshot.ghidra.extra_binaries {
        if !extra.is_file() {
            eprintln!(
                "warning: the sibling binary '{}' no longer exists; its program stays \
                 usable only if the saved project already holds it",
                extra.display(),
            );
        }
    }
    // a legacy session may have saved a projects dir Ghidra 12.1+ rejects (any
    // path element starting with '.'); remap it to the valid default when its
    // project was never actually created, otherwise keep it and warn (the
    // restore's trailing autosave persists the remap for future resumes)
    if let Err(reason) = thorpal::validate_projects_dir(&snapshot.ghidra.projects_dir) {
        if snapshot.ghidra.project_exists() {
            // a real project lives there; keep it but surface the risk
            eprintln!("warning: {reason}; keeping the saved project in place");
        } else {
            // no project was ever created there, so moving to the default is
            // safe; nest it as <root>/<binary-stem>/<session-name> to match
            // the nested analysis layout (session names are path-safe)
            let fallback = thorpal::default_ghidra_projects_dir()
                .map_err(|err| Error::new(format!("{err:#}")))?
                .join(&snapshot.ghidra.project_name)
                .join(name);
            eprintln!(
                "note: {reason}; remapping this session's Ghidra projects directory to '{}'",
                fallback.display(),
            );
            snapshot.ghidra.projects_dir = fallback;
        }
    }
    // the stored provider config was sanitized of secrets on disk, so resolve
    // the live provider from the Thorctl config and re-inject its credentials
    let ai = conf.ai.as_ref().ok_or_else(|| {
        Error::new(
            "No `ai` config set: ThorPal needs an `ai` block with a provider \
            section such as `openai`, `litellm`, `anthropic`, or `bedrock`",
        )
    })?;
    let resolved = ai.resolve()?;
    // grab the pricing overrides before the resolved settings move into the provider
    let overrides = resolved.pricing();
    let live = session::to_thorpal_provider(resolved);
    let provider = session::reinject_secrets(snapshot.provider.clone(), live);
    // build the cost estimator from the effective provider, whose model is the
    // stored one (a resume keeps the saved session's model)
    let cost = CostModel::for_provider(&provider, overrides.as_ref());
    // keep the saved per-agent usage ledger so the TUI's running totals and the
    // stats view's per-agent split both continue where they left off
    let usage = snapshot.usage.clone();
    // load the ordered display transcript so the chat log replays the full
    // interleaved history; a pre-transcript session falls back to the
    // snapshot's embedded user/assistant history rendered the old way
    let transcript = match store.load_transcript().await {
        Ok(items) if !items.is_empty() => items,
        // fall back (or degrade on a read failure) to the snapshot's history
        result => {
            // a read failure should not block the resume, just lose the replay
            if let Err(err) = result {
                eprintln!("warning: failed to load the session transcript: {err:#}");
            }
            // map the legacy entries onto the same replay items; a legacy
            // session predates timestamps, so the lines carry none
            snapshot
                .transcript()
                .into_iter()
                .map(|entry| TranscriptLine {
                    ts: None,
                    item: match entry {
                        // a legacy session predates side conversations entirely
                        TranscriptEntry::User(text) => TranscriptItem::User { text, scope: None },
                        TranscriptEntry::Assistant(text) => {
                            TranscriptItem::Assistant { text, scope: None }
                        }
                    },
                })
                .collect()
        }
    };
    // rebuild the session around the reopened store so saving continues in
    // place; the library locations always come from the live config
    let scripts_dir = conf.ai.as_ref().and_then(|ai| ai.scripts_dir.clone());
    let memories_dir = conf.ai.as_ref().and_then(|ai| ai.memories_dir.clone());
    // let an auth pause pick up fresh credentials from the config file
    let refresher = session::credential_refresher(config_path.to_owned());
    // the deny-token and blind-family lists are never persisted, so a resumed
    // blinded run must pass --redact-tokens/--blind-family again; blinding
    // itself comes from the saved meta
    let redact_tokens = session::load_redact_tokens(cmd.redact_tokens.as_ref()).await?;
    let session = Session::restore(
        store,
        snapshot,
        forum,
        provider,
        Some(events),
        scripts_dir,
        memories_dir,
        Some(refresher),
        meta.anonymized,
        redact_tokens,
        cmd.blind_family.clone(),
        // the saved workflow keeps the thorium_mcp flag but never the
        // credentials, so the resolver is re-supplied on every resume
        Some(session::thorium_mcp_resolver(conf)),
    )
    .await
    .map_err(|err| Error::new(format!("{err:#}")))?;
    Ok((session, transcript, usage, cost))
}

/// Start a ThorPal-backed ai chat terminal UI over a local binary
///
/// This builds a ThorPal [`Session`] over a local binary -- or resumes a saved
/// one from `~/.thorium/sessions` when `--resume` is passed -- and runs the
/// ratatui shell against it, so a user can hold a multi-turn conversation with
/// the reverse-engineering agents while seeing their tool calls and sub-agent
/// activity live. Every turn is saved to the session's store as it settles, so
/// a crash or quit can always be resumed.
///
/// # Arguments
///
/// * `conf` - The Thorctl config, whose `ai` block supplies the ThorPal settings
/// * `cmd` - The parsed `chat` command
/// * `config_path` - The thorctl config file, re-read on auth-pause probes
pub async fn tui(conf: &CtlConf, cmd: &ChatArgs, config_path: &Path) -> Result<(), Error> {
    // build the channel ThorPal emits its run events on before tracing is
    // installed, so the capture layer can mirror tool logs onto the same stream
    let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
    // build the shared router streaming tool calls register on; the capture
    // layer routes matching log lines through it onto the event channel
    let tool_log_router = ToolLogRouter::new();
    // route tracing to a log file (plus the tool-log capture layer) before any
    // session work emits events; the TUI owns the terminal so tracing must
    // never print to it
    super::tui::init_tracing_log(Some(ToolLogLayer::new(
        Arc::clone(&tool_log_router),
        events_tx.clone(),
    )));
    // resolve the default report template: the flag outranks the ai config, and
    // leaving both unset falls back to the builtin template inside thorpal; an
    // explicitly resolved template must exist, failing before the session claims
    // a store directory and before taking the screen so errors print cleanly
    let template = session::resolve_report_template(conf, cmd.report_template.as_ref())?;
    // bundle the resolved report settings the chat worker consults
    let report = ReportSettings {
        auto_path: cmd.report.clone(),
        template,
        defang: cmd.defang,
    };
    // resume the named saved session, or build a fresh one over the binary
    let (session, restored, restored_usage, cost) = match &cmd.resume {
        // reopen the saved session along with its prior conversation and usage
        Some(name) => {
            let (session, transcript, usage, cost) =
                resume_session(conf, cmd, name, events_tx, config_path).await?;
            (session, Some(transcript), Some(usage), cost)
        }
        // build a fresh session over the binaries the operator named
        None => {
            // clap guarantees at least one binary whenever --resume is absent;
            // the first is the primary and the rest are sibling samples
            let (binary, extras) = cmd
                .binary
                .split_first()
                .ok_or_else(|| Error::new("a binary path is required unless --resume is used"))?;
            // load the operator's library deny tokens before any heavier work
            let redact_tokens = session::load_redact_tokens(cmd.redact_tokens.as_ref()).await?;
            // build the ThorPal run config from our config and args
            let mut config = session::build_config(
                conf,
                binary,
                extras,
                cmd.ghidra_install.as_ref(),
                cmd.workflow.as_ref(),
                cmd.name.as_ref(),
                cmd.fresh_project,
                cmd.anonymize,
                redact_tokens,
                cmd.blind_family.clone(),
            )
            .await?;
            // raise the main loop's turn cap when the operator asked for one
            config.main_max_turns = cmd.max_turns;
            // honor the analyst-interaction opt-out so a gated run's tool
            // lists and preambles stay byte-identical to pre-feature runs
            if cmd.no_analyst_asks {
                config.workflow.analyst_asks = false;
            }
            // install the event sink so the run's activity flows to the TUI
            config.events = Some(events_tx);
            // let an auth pause pick up fresh credentials from the config file
            config.credential_refresher =
                Some(session::credential_refresher(config_path.to_owned()));
            // honor the operator's opt-out; seeding is on unless --disable-detectors
            config.seed_detectors = !cmd.disable_detectors;
            // build the cost estimator before the session consumes the config;
            // build_config already proved the ai block resolves, so re-resolving
            // for the pricing overrides here is cheap and cannot fail anew
            let overrides = conf
                .ai
                .as_ref()
                .and_then(|ai| ai.resolve().ok())
                .and_then(|resolved| resolved.pricing());
            let cost = CostModel::for_provider(&config.provider, overrides.as_ref());
            // build the session before taking the screen so errors print cleanly
            let session = Session::build(config)
                .await
                .map_err(|err| Error::new(format!("{err:#}")))?;
            (session, None, None, cost)
        }
    };
    // read the session's name so the TUI can tell the user how to resume it
    let session_name = session.session_name().await;
    // keep a pause-controller clone for teardown: quitting with a turn still
    // in flight latches the shutdown abort so the turn freezes for resume
    // instead of running on (or salvaging itself as a failure) behind the
    // bounded finish-await below
    let pause_controller = session.pause_controller();
    // read any restored side conversations' names before the session moves into
    // the client, so their registry handles and tabs can be pre-created
    let restored_sides = session.side_names().await;
    // take the screen: eyre hooks, mouse + paste capture, raw mode
    let mut terminal = super::tui::init_terminal()?;
    // build the app, constructing the ThorPal-backed client against the app's own
    // event sender so the worker tasks' redraw/failure events reach its loop. the
    // session and event receiver are moved into the factory closure exactly once.
    let mut app = App::from_client(move |event_tx| {
        Arc::new(ThorPalClient::new(
            session,
            session_name,
            events_rx,
            event_tx,
            restored,
            restored_usage,
            restored_sides,
            cost,
            report,
            tool_log_router,
        ))
    });
    // start rendering our app, capturing the result (instead of `?`-ing it) so
    // the one teardown path below always runs, even when the app loop errors
    let start_result = app.start(&mut terminal).await;
    // take the worker's handle before the app (and with it every request
    // sender) drops, which is what lets the worker's loop exit and finish
    let worker = app.take_worker_handle();
    // latch the shutdown abort: an in-flight turn (and its sub-agents) fails
    // fast at the gate as an interruption, freezing cleanly for resume -- no
    // salvage posts, no failure events, the pending turn stays pending -- so
    // the bounded finish-await below is not held hostage by a running turn
    pause_controller.abort_for_shutdown();
    // detach the analyst link first: a turn parked on an unanswered ask
    // resolves to its recommended default, so the bounded join below can
    // never wedge behind a question nobody is left to answer
    app.detach_analyst();
    // give the screen back, best-effort: a failure must not skip the worker's
    // bounded finish-await below
    super::tui::restore_terminal();
    // drop the app so the last request sender closes and the worker's loop
    // breaks into its best-effort Session::finish
    drop(app);
    // await the finish (Ghidra flush + analysis-dir rename) with a bound, so a
    // wedged bridge can still never hold the terminal hostage forever
    if let Some(worker) = worker {
        eprintln!("finishing session (flushing Ghidra analysis)...");
        match tokio::time::timeout(std::time::Duration::from_secs(60), worker).await {
            // a clean finish needs no commentary
            Ok(Ok(Ok(()))) => (),
            // the finish failed: say so now that the terminal is cooked again
            Ok(Ok(Err(err))) => eprintln!("warning: the session did not finish cleanly: {err}"),
            // the worker itself died; its banner already named the reason
            Ok(Err(join_error)) => {
                eprintln!("warning: the chat worker ended abnormally: {join_error}");
            }
            // the bound tripped: tell the user what may still be running
            Err(_) => eprintln!(
                "warning: timed out waiting for the session to finish; the Ghidra \
                 bridge may still be flushing in the background"
            ),
        }
    }
    // surface the app's own error only now that the session finished and the
    // terminal is restored, so a render fault can no longer skip the flush
    start_result?;
    Ok(())
}
