//! The chat page for the Thorium chat tui

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::commands::{self, ParsedCommand};
use super::components::{
    MessageLog, RunActivity, ShortcutMenu, SlashMenu, SummaryModal, SummaryModalState,
    SummaryOutcome, TextBox, ThorPalClient, WorkerRequest, status_bar, throbber,
};
use super::{ActiveTabKind, AppEvent, ChatContext, ScrollEvent};

/// The main conversation's prompt placeholder text.
const MAIN_PLACEHOLDER: &str = "How can Thorium help?";

/// The prompt placeholder shown while revising a /done summary.
const REVISE_PLACEHOLDER: &str = "Describe how to revise the summary… (esc cancels)";

/// A parsed /report held while the overwrite-confirmation modal is shown
struct PendingReport {
    /// Where to write the generated report
    path: PathBuf,
    /// A template path overriding the configured and builtin defaults
    template: Option<PathBuf>,
}

/// The chat page for a conversation with ThorPal
pub struct Chat {
    /// The client for this chat pages ai
    thor_chat: Arc<ThorPalClient>,
    /// The chat log this page renders: the main conversation's shared context
    /// or a side conversation's own
    context: ChatContext,
    /// The run-activity this page's throbber and status bar read: the main
    /// conversation's bucket or a side conversation's own
    activity: RunActivity,
    /// The side conversation this page belongs to, or `None` for the main one
    scope: Option<String>,
    /// The message log
    messages: MessageLog,
    /// The text box for our prompt
    pub prompt: TextBox,
    /// The shortcut menu for this page
    pub shortcuts: ShortcutMenu,
    /// The slash-command autocomplete popup for the prompt
    slash_menu: SlashMenu,
    /// A parsed /report held while the overwrite-confirmation modal is shown
    confirm_report: Option<PendingReport>,
    /// The /done summary review modal, while one is open on this side page
    summary_modal: Option<SummaryModal>,
}

impl Chat {
    /// Create a new chat page
    ///
    /// # Arguments
    ///
    /// * `thor_chat` - A client to a ThorPal session
    /// * `log` - The initial chat logs to use
    pub async fn new(thor_chat: &Arc<ThorPalClient>, log: Vec<String>) -> Self {
        // assemble the page's components around the main conversation
        let page = Chat::assemble(thor_chat, None);
        // send the first log message when one exists (the home page's prompt),
        // degrading to a dead-worker note instead of panicking in raw mode
        if let Some(first) = log.into_iter().next() {
            page.send_to_worker(WorkerRequest::Chat(first)).await;
        }
        page
    }

    /// Create a chat page for a resumed session without sending anything
    ///
    /// A resumed session must wait for the analyst instead of auto-continuing:
    /// the prompt task already skips seeding and blocks on user input when it
    /// was built with `resumed`, and the saved history was replayed into the
    /// shared context before the worker tasks spawned, so this page only needs
    /// to render -- it must not submit a prompt.
    ///
    /// # Arguments
    ///
    /// * `thor_chat` - A client to a resumed ThorPal session
    pub fn restored(thor_chat: &Arc<ThorPalClient>) -> Self {
        // assemble the page's components without driving the worker
        Chat::assemble(thor_chat, None)
    }

    /// Create a chat page rendering a named side conversation
    ///
    /// The page shares the client (and so the forum, overlays, and cost) but
    /// renders the side's own context and activity, so its log and token
    /// readout stay separate from the main conversation's.
    ///
    /// # Arguments
    ///
    /// * `thor_chat` - A client to a ThorPal session
    /// * `name` - The side conversation this page renders
    pub fn for_side(thor_chat: &Arc<ThorPalClient>, name: &str) -> Self {
        // assemble the page's components around the side's own handle
        Chat::assemble(thor_chat, Some(name.to_owned()))
    }

    /// Assemble a chat page's components around a client
    ///
    /// # Arguments
    ///
    /// * `thor_chat` - A client to a ThorPal session
    /// * `scope` - The side conversation this page renders, or `None` for main
    fn assemble(thor_chat: &Arc<ThorPalClient>, scope: Option<String>) -> Self {
        // the page renders its scope's context and activity: the client's own
        // for the main conversation, the side's handle otherwise
        let (context, activity, title) = match &scope {
            Some(name) => {
                let handle = thor_chat.sides.handle(name);
                (
                    handle.context,
                    handle.activity,
                    format!("Prompt — btw:{name}"),
                )
            }
            None => (
                thor_chat.context.clone(),
                thor_chat.activity.clone(),
                "Prompt".to_owned(),
            ),
        };
        // build our prompt
        let prompt = TextBox::default()
            .title(title)
            .placeholder(MAIN_PLACEHOLDER)
            .focused();
        // build the shortcut menu for this page; going home clears the main
        // conversation, so side pages do not offer it
        let mut shortcuts = ShortcutMenu::builder();
        if scope.is_none() {
            shortcuts = shortcuts.add('h', "Go home");
        }
        let shortcuts = shortcuts
            .add('m', "Message board")
            .add('g', "Traversal graph")
            .add('s', "Search forum")
            .add('t', "Timeline")
            .add('u', "Usage")
            .build();
        // build our chat component
        Chat {
            thor_chat: thor_chat.clone(),
            context,
            activity,
            scope,
            messages: MessageLog::default(),
            prompt,
            shortcuts,
            slash_menu: SlashMenu::default(),
            confirm_report: None,
            summary_modal: None,
        }
    }

    /// Send a request to the chat worker, degrading gracefully when it is gone
    ///
    /// A closed request channel means the prompt worker died (its supervisor
    /// has already set the client's death note and banner); the failed submit
    /// is answered with a resume-hint bubble instead of the `unwrap` panic
    /// that used to corrupt the raw-mode terminal.
    ///
    /// # Arguments
    ///
    /// * `request` - The worker request to send
    async fn send_to_worker(&self, request: WorkerRequest) {
        // a closed channel means the worker died; note it instead of panicking
        if self.thor_chat.to_chat.send(request).await.is_err() {
            self.note_worker_stopped();
        }
    }

    /// Drop a dead-worker note into this page's log with the resume hint
    fn note_worker_stopped(&self) {
        // the banner names the reason; this bubble answers the failed submit
        self.context.add_ai(format!(
            "⚠ the chat worker has stopped and cannot take requests — quit and \
             resume with `thorctl analyze chat --resume {}`",
            self.thor_chat.session_name
        ));
    }

    /// Clear this chat and reset it to its default state
    pub fn clear_and_go_home(&mut self) -> Option<AppEvent> {
        // clear our messages
        self.messages.clear();
        // clear our pompt
        self.prompt.clear();
        // clear any messages in our ai chat client
        self.thor_chat.context.clear();
        // return an event that changes our active tab to home
        Some(AppEvent::SetActiveTabKind(ActiveTabKind::Home))
    }

    /// Installs a freshly generated /done summary into this page's modal
    ///
    /// A modal dismissed while the summarizer was still working is reopened,
    /// so the finished summary is never silently lost.
    ///
    /// # Arguments
    ///
    /// * `summary` - The generated (or revised) summary text
    pub fn summary_ready(&mut self, summary: String) {
        // revising hands the prompt back, so restore its usual placeholder
        self.prompt.placeholder = MAIN_PLACEHOLDER.to_owned();
        // reopen the modal if it was dismissed mid-generation
        let modal = self.summary_modal.get_or_insert_with(|| {
            SummaryModal::generating(self.scope.clone().unwrap_or_default())
        });
        modal.set_summary(summary);
    }

    /// Surfaces a failed /done summary generation on this page
    ///
    /// A revision failure keeps the prior summary on screen; a first
    /// generation's failure drops the modal since there is nothing to review.
    ///
    /// # Arguments
    ///
    /// * `error` - The human-readable error describing the failure
    pub fn summary_failed(&mut self, error: String) {
        // the failure lands in the side's own log either way
        self.context
            .add_ai(format!("⚠ failed to summarize: {error}"));
        // restore the prompt's usual placeholder in case a revision failed
        self.prompt.placeholder = MAIN_PLACEHOLDER.to_owned();
        // keep a prior summary on screen; drop a modal that never got one
        match &mut self.summary_modal {
            Some(modal) if !modal.summary().is_empty() => {
                modal.state = SummaryModalState::Menu;
            }
            _ => self.summary_modal = None,
        }
    }

    /// Installs an externally edited summary back into this page's modal
    ///
    /// # Arguments
    ///
    /// * `summary` - The summary text as it came back from the editor
    pub fn summary_edited(&mut self, summary: String) {
        // reopen the modal if it somehow closed while the editor was open
        let modal = self.summary_modal.get_or_insert_with(|| {
            SummaryModal::generating(self.scope.clone().unwrap_or_default())
        });
        modal.set_summary(summary);
    }

    /// Falls back to in-modal editing when no external editor could run
    pub fn summary_edit_fallback(&mut self) {
        // tell the analyst why the editor never appeared
        self.context
            .add_ai("⚠ no usable editor ($VISUAL/$EDITOR); editing inline instead");
        // hand the modal's own buffer the keyboard
        if let Some(modal) = &mut self.summary_modal {
            modal.edit_inline();
        }
    }

    /// Maps a summary-modal outcome onto worker requests and app events
    ///
    /// # Arguments
    ///
    /// * `outcome` - The outcome the modal produced for a key press
    async fn handle_summary_outcome(&mut self, outcome: SummaryOutcome) -> Option<AppEvent> {
        // every outcome is meaningless without the side's name
        let name = self.scope.clone()?;
        match outcome {
            // the modal handled the key itself
            SummaryOutcome::None => None,
            // accept: hand the summary to the worker and drop the modal (the
            // tab closes when the worker confirms; a failure reopens nothing
            // but lands as a bubble in this side's log)
            SummaryOutcome::Accept => {
                if let Some(modal) = self.summary_modal.take() {
                    self.send_to_worker(WorkerRequest::BtwAccept {
                        name,
                        summary: modal.summary().to_owned(),
                    })
                    .await;
                }
                None
            }
            // edit: ask the app shell to suspend the TUI and run the editor
            SummaryOutcome::EditExternally => {
                let summary = self
                    .summary_modal
                    .as_ref()
                    .map(|modal| modal.summary().to_owned())?;
                Some(AppEvent::EditInEditor { name, summary })
            }
            // revise: hand the prompt box back to the analyst for feedback
            SummaryOutcome::Revise => {
                if let Some(modal) = &mut self.summary_modal {
                    modal.state = SummaryModalState::Revising;
                }
                self.prompt.placeholder = REVISE_PLACEHOLDER.to_owned();
                self.prompt.is_focused = true;
                None
            }
            // skip: close the side without returning anything
            SummaryOutcome::Skip => {
                self.summary_modal = None;
                self.send_to_worker(WorkerRequest::BtwClose { name }).await;
                None
            }
            // dismiss: back to the side chat, keeping the side open
            SummaryOutcome::Dismiss => {
                self.summary_modal = None;
                None
            }
        }
    }

    /// Handle key input for this text box
    ///
    /// # Arguments
    ///
    /// * `code` - The key code that was pressed
    /// Insert pasted text into whichever of this page's boxes has the keyboard
    ///
    /// Mirrors [`ChatPage::handle_key`]'s precedence: the in-modal fallback
    /// editor takes the paste while the summary modal is editing, the other
    /// modal states (and the overwrite confirm) own the keyboard with no text
    /// input so the paste is swallowed, and otherwise the focused prompt takes
    /// it (revision feedback is typed through the same prompt box).
    ///
    /// # Arguments
    ///
    /// * `text` - The pasted text to insert
    pub fn handle_paste(&mut self, text: &str) {
        if let Some(modal) = &mut self.summary_modal {
            match modal.state {
                // the in-modal fallback editor is a live text box
                SummaryModalState::Editing => {
                    modal.editor.insert_str(text);
                    return;
                }
                // revision feedback types through the page prompt below
                SummaryModalState::Revising => (),
                // every other modal state has nothing to paste into
                _ => return,
            }
        }
        // the overwrite confirm swallows everything while it is open
        if self.confirm_report.is_some() {
            return;
        }
        // the paste lands in the focused prompt and never submits
        if self.prompt.is_focused {
            self.prompt.insert_str(text);
            // keep the slash popup in sync with the prompt's new content
            self.slash_menu.update(&self.prompt.content);
        }
    }

    pub async fn handle_key(&mut self, key: KeyEvent) -> Option<AppEvent> {
        // an open /done summary modal owns the keyboard, except while revising
        // (the prompt box owns it then, and Esc below cancels the revision)
        if let Some(modal) = &mut self.summary_modal {
            if modal.state != SummaryModalState::Revising {
                let outcome = modal.handle_key(key);
                return self.handle_summary_outcome(outcome).await;
            }
            // Esc while typing revision feedback returns to the summary menu
            if key.code == KeyCode::Esc {
                modal.state = SummaryModalState::Menu;
                self.prompt.placeholder = MAIN_PLACEHOLDER.to_owned();
                return None;
            }
        }
        // the overwrite-confirmation modal swallows every key while it is open
        if self.confirm_report.is_some() {
            match key.code {
                // confirm the overwrite and send the pending report request
                KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                    // take the pending report and hand it to the worker
                    if let Some(pending) = self.confirm_report.take() {
                        self.send_to_worker(WorkerRequest::Report {
                            path: pending.path,
                            template: pending.template,
                        })
                        .await;
                    }
                }
                // cancel the overwrite, telling the user nothing was written
                KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                    self.confirm_report = None;
                    self.thor_chat.context.add_ai("Report cancelled.");
                }
                // every other key is swallowed while the modal is open
                _ => (),
            }
            return None;
        }
        // if our prompt is focused then send the key to the prompt
        if self.prompt.is_focused {
            // route navigation keys to the autocomplete popup while it is open
            if self.slash_menu.active {
                match key.code {
                    // move the highlighted match up or down
                    KeyCode::Up => {
                        self.slash_menu.move_up();
                        return None;
                    }
                    KeyCode::Down => {
                        self.slash_menu.move_down();
                        return None;
                    }
                    // accept the highlighted completion; while the popup is
                    // visible Enter completes rather than submits, and the
                    // trailing space hides the popup so the next Enter submits
                    KeyCode::Tab | KeyCode::Enter => {
                        self.prompt
                            .set_content(format!("/{} ", self.slash_menu.selection().name));
                        self.slash_menu.update(&self.prompt.content);
                        return None;
                    }
                    // dismiss the popup without unfocusing the prompt; the
                    // dismissal sticks until the prompt's content changes
                    KeyCode::Esc => {
                        self.slash_menu.dismiss(&self.prompt.content);
                        return None;
                    }
                    // every other key falls through to the prompt below
                    _ => (),
                }
            }
            // pass this input to our prompt text box
            if self.prompt.handle_key(key, false) {
                // consume our current prompt box
                if let Some(message) = self.prompt.consume() {
                    // dispatch the message as a slash command or a chat message,
                    // bubbling any app event (e.g. opening a btw tab) upward
                    let event = self.submit(message).await;
                    // keep the popup in sync with the now-empty prompt
                    self.slash_menu.update(&self.prompt.content);
                    return event;
                }
            }
            // track the prompt's content so the popup follows the typing live
            self.slash_menu.update(&self.prompt.content);
        } else {
            // handle this key
            match (key.code, self.shortcuts.active) {
                // toggle shortcut mode off and on
                (KeyCode::Char(' '), _) => self.shortcuts.toggle(),
                // toggle our shortcut mode off
                (KeyCode::Esc, true) => self.shortcuts.toggle(),
                // hand the keyboard to the prompt box for typing
                (KeyCode::Char('i'), false) => self.prompt.is_focused = true,
                // clear our context and go back home (main tab only; a side
                // tab's page never offers this shortcut)
                (KeyCode::Char('h'), true) if self.scope.is_none() => {
                    return self.clear_and_go_home();
                }
                // open or close the message board
                (KeyCode::Char('m'), true) => {
                    // a shortcut was picked, so hide the shortcut menu
                    self.shortcuts.toggle();
                    return Some(AppEvent::ToggleMessageBoard);
                }
                // open or close the traversal graph overlay
                (KeyCode::Char('g'), true) => {
                    // a shortcut was picked, so hide the shortcut menu
                    self.shortcuts.toggle();
                    return Some(AppEvent::ToggleTraversal);
                }
                // open the message board with its forum search ready to type into
                (KeyCode::Char('s'), true) => {
                    // a shortcut was picked, so hide the shortcut menu
                    self.shortcuts.toggle();
                    return Some(AppEvent::OpenForumSearch);
                }
                // open or close the usage (agent stats) overlay
                (KeyCode::Char('u'), true) => {
                    // a shortcut was picked, so hide the shortcut menu
                    self.shortcuts.toggle();
                    return Some(AppEvent::ToggleAgentStats);
                }
                // open or close the gantt timeline overlay
                (KeyCode::Char('t'), true) => {
                    // a shortcut was picked, so hide the shortcut menu
                    self.shortcuts.toggle();
                    return Some(AppEvent::ToggleGantt);
                }
                _ => (),
            }
        }
        None
    }

    /// Dispatch a submitted prompt as a slash command or a chat message
    ///
    /// A message starting with `/` is parsed as a slash command: a `/report`
    /// targeting an existing file raises the overwrite-confirmation modal,
    /// `/btw` forks a side conversation and bubbles an event up to open its
    /// tab, `/done` and `/close` drive this side conversation, any other valid
    /// command goes straight to the worker, and a parse error becomes an
    /// inline error bubble. Everything else is a chat message routed to this
    /// page's conversation -- or, while a /done revision is pending, consumed
    /// as the revision feedback.
    ///
    /// # Arguments
    ///
    /// * `message` - The submitted prompt text
    async fn submit(&mut self, message: String) -> Option<AppEvent> {
        // a dead worker cannot run anything; answer every submit with the hint
        if self.thor_chat.worker_dead.get().is_some() {
            self.note_worker_stopped();
            return None;
        }
        // a leading slash marks a command instead of a chat message
        if message.starts_with('/') {
            match commands::parse(&message) {
                // reports cover the whole session, so they stay on the main tab
                Ok(ParsedCommand::Report { path, template }) => {
                    if self.scope.is_some() {
                        self.context
                            .add_ai("⚠ /report is only available on the main tab");
                        return None;
                    }
                    // confirm before clobbering an existing file the user chose
                    if tokio::fs::try_exists(&path).await.unwrap_or(false) {
                        self.confirm_report = Some(PendingReport { path, template });
                    } else {
                        // the path is new, so write the report without asking
                        self.send_to_worker(WorkerRequest::Report { path, template })
                            .await;
                    }
                }
                // promotions go straight to the worker; failures bubble back
                Ok(ParsedCommand::Promote { name, description }) => {
                    if self.scope.is_some() {
                        self.context
                            .add_ai("⚠ /promote is only available on the main tab");
                        return None;
                    }
                    self.send_to_worker(WorkerRequest::Promote { name, description })
                        .await;
                }
                // fork (or switch to) a side conversation and open its tab
                Ok(ParsedCommand::Btw { name }) => {
                    // side conversations fork off the main one, never off each other
                    if self.scope.is_some() {
                        self.context
                            .add_ai("⚠ open side conversations from the main tab");
                        return None;
                    }
                    // the worker forks the side; the app opens/focuses its tab
                    self.send_to_worker(WorkerRequest::BtwFork { name: name.clone() })
                        .await;
                    return Some(AppEvent::OpenBtw { name });
                }
                // summarize this side conversation back to the main chat
                Ok(ParsedCommand::Done) => match self.scope.clone() {
                    Some(name) => {
                        // open the modal in its generating state and ask the
                        // worker for a fresh summary
                        self.summary_modal = Some(SummaryModal::generating(&name));
                        self.send_to_worker(WorkerRequest::BtwSummarize {
                            name,
                            prior: None,
                            feedback: None,
                        })
                        .await;
                    }
                    None => self
                        .context
                        .add_ai("⚠ /done is only available inside a side conversation"),
                },
                // close this side conversation without returning anything
                Ok(ParsedCommand::Close) => match self.scope.clone() {
                    Some(name) => {
                        self.send_to_worker(WorkerRequest::BtwClose { name }).await;
                    }
                    None => self
                        .context
                        .add_ai("⚠ /close is only available inside a side conversation"),
                },
                // probe the paused provider now instead of waiting out backoff
                Ok(ParsedCommand::Retry) => {
                    self.thor_chat.pause.retry_now();
                    self.context.add_ai("probing the provider now…");
                }
                // abort the pause so the parked requests surface their errors
                Ok(ParsedCommand::Abort) => self.thor_chat.pause.abort(),
                // an unknown or malformed command becomes an inline error bubble
                Err(msg) => self.context.add_ai(format!("⚠ {msg}")),
            }
            return None;
        }
        // a pending /done revision consumes the message as its feedback
        if let (Some(name), Some(modal)) = (&self.scope, &mut self.summary_modal)
            && modal.state == SummaryModalState::Revising
        {
            // regenerate the summary from the prior text plus this feedback
            let name = name.clone();
            let prior = modal.summary().to_owned();
            modal.state = SummaryModalState::Generating;
            self.prompt.placeholder = MAIN_PLACEHOLDER.to_owned();
            self.send_to_worker(WorkerRequest::BtwSummarize {
                name,
                prior: Some(prior),
                feedback: Some(message),
            })
            .await;
            return None;
        }
        // send the latest message to this page's conversation
        let request = match &self.scope {
            Some(name) => WorkerRequest::BtwChat {
                name: name.clone(),
                msg: message,
            },
            None => WorkerRequest::Chat(message),
        };
        self.send_to_worker(request).await;
        None
    }

    /// Handle a mouse click event
    ///
    /// # Arguments
    ///
    /// * `x` - The column position of the click
    /// * `y` - The row position of the click
    pub fn handle_click(&mut self, x: u16, y: u16) {
        // an open /done summary modal owns clicks (menu row selection)
        if let Some(modal) = &mut self.summary_modal
            && modal.state != SummaryModalState::Revising
        {
            modal.handle_click(x, y);
            return;
        }
        // a click on a tool-call bubble toggles its expansion; only fall back to
        // the prompt box when the message log did not consume the click
        if self.messages.handle_click(x, y) {
            return;
        }
        self.prompt.handle_click(x, y);
    }

    /// Handle a scroll event
    ///
    /// # Arguments
    ///
    /// * `event` - The scroll event to handle
    pub fn handle_scroll(&mut self, event: ScrollEvent) {
        match event {
            ScrollEvent::ScrollUp => self.messages.scroll_up(3),
            ScrollEvent::ScrollDown => self.messages.scroll_down(3),
            // there is nothing to scroll left or right on
            ScrollEvent::ScrollLeft | ScrollEvent::ScrollRight => (),
        }
    }

    /// The area the chat log was last rendered into
    ///
    /// The tab anchors the forum-search preview on this rect so the overlay
    /// matches the log exactly, however the banner and throbber rows shift it.
    pub fn log_area(&self) -> Rect {
        // the log records its own rect every frame it draws
        self.messages.last_area()
    }

    /// Render the home tab content
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render to
    /// * `area` - The area to render this page in
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        // snapshot this page's live run-activity (the main conversation's or
        // this side's) so the throbber and status bar reflect it
        let activity = self.activity.snapshot();
        // reserve a one-row throbber strip above the prompt only while a turn is
        // in flight; a zero-height constraint keeps the layout stable when idle
        let throbber_h = if activity.busy { 1 } else { 0 };
        // reserve a one-row banner strip while the worker is dead or the
        // session is paused, so the state stays visible however the log scrolls
        let worker_dead = self.thor_chat.worker_dead.get();
        let paused = self
            .thor_chat
            .pause_banner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|banner| {
                // freeze the countdown target rather than a stale remainder
                format!(
                    "⏸ paused — {}: {} · {} failed probe(s) · next probe {} · /retry now · /abort",
                    banner.kind,
                    banner.error,
                    banner.probe_attempts,
                    banner.next_probe_at.format("%H:%M:%S UTC"),
                )
            });
        let dead_h = u16::from(worker_dead.is_some() || paused.is_some());
        // split the area into vertical chunks: tabs, content, dead-worker
        // banner, throbber, prompt, status
        let chunks = Layout::vertical([
            Constraint::Length(3),          // tabs
            Constraint::Min(0),             // content
            Constraint::Length(dead_h),     // dead-worker banner (0 while alive)
            Constraint::Length(throbber_h), // thinking throbber (0 when idle)
            Constraint::Length(8),          // prompt input
            Constraint::Length(2),          // status bar
        ])
        .split(area);
        // get a guard to this page's chat log
        let guard = self.context.access();
        // render any visible messages
        self.messages.render(frame, chunks[1], &guard);
        // render the persistent dead-worker banner with the resume hint; a
        // dead worker outranks a pause (the pause can never resolve then)
        if let Some(reason) = worker_dead {
            let banner = Paragraph::new(format!(
                "⛔ worker stopped: {reason} — session saved; resume with \
                 `thorctl analyze chat --resume {}`",
                self.thor_chat.session_name
            ))
            .style(Style::default().fg(Color::White).bg(Color::Red));
            frame.render_widget(banner, chunks[2]);
        } else if let Some(line) = paused {
            // render the live pause banner while requests are parked
            let banner =
                Paragraph::new(line).style(Style::default().fg(Color::Black).bg(Color::Yellow));
            frame.render_widget(banner, chunks[2]);
        }
        // render the thinking throbber while the run is busy
        if activity.busy {
            throbber::render(frame, chunks[3], &activity);
        }
        // render our prompt
        self.prompt.render(frame, chunks[4]);
        // render the autocomplete popup floating above the prompt if its active
        self.slash_menu.render(frame, chunks[4]);
        // render our status bar, carrying this page's token-usage and cost
        // readout plus the session-wide total (shown only when they differ)
        let session = self.thor_chat.session_total.snapshot();
        status_bar::render(
            frame,
            chunks[5],
            self.prompt.is_focused,
            &activity,
            Some(&session),
            self.thor_chat.cost.as_ref(),
        );
        // render our shortcut menu if its active
        self.shortcuts.render(frame);
        // render the overwrite-confirmation modal on top when one is pending
        if let Some(pending) = &self.confirm_report {
            render_confirm_modal(frame, area, &pending.path);
        }
        // render the /done summary modal on top when one is open
        if let Some(modal) = &mut self.summary_modal {
            modal.render(frame, area);
        }
    }
}

/// Render the centered overwrite-confirmation modal for a pending report
///
/// # Arguments
///
/// * `frame` - The frame to render to
/// * `area` - The page area to center the modal within
/// * `path` - The report path that already exists
fn render_confirm_modal(frame: &mut Frame, area: Rect, path: &Path) {
    // build the modal's body: the clashing path and the y/n key hints
    let lines = vec![
        Line::from(format!(" {} already exists. ", path.display())),
        Line::default(),
        Line::from(vec![
            Span::raw(" "),
            Span::styled(
                "y",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  overwrite    "),
            Span::styled(
                "n",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  cancel "),
        ]),
    ];
    // size the modal to its widest line, clamped to the page's bounds
    let longest = lines.iter().map(Line::width).max().unwrap_or(0);
    let width = (longest as u16 + 2).min(area.width);
    let height = (lines.len() as u16 + 2).min(area.height);
    // center the modal within the page
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let overlay_area = Rect::new(x, y, width, height);
    // build the modal block with the overlay gray background
    let block = Block::default()
        .title(" Overwrite report? ")
        .title_alignment(Alignment::Center)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Gray))
        // set the background color to a dark gray
        .style(Style::default().bg(Color::Rgb(60, 60, 60)));
    // wrap the body lines in a paragraph
    let paragraph = Paragraph::new(lines)
        .block(block)
        // set the background color to a dark gray
        .style(Style::default().fg(Color::White).bg(Color::Rgb(60, 60, 60)));
    // Clear the area first (important for overlays), then render the widget
    frame.render_widget(Clear, overlay_area);
    frame.render_widget(paragraph, overlay_area);
}
