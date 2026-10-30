//! The different tabs in the ThorPal chat tui

use crossterm::event::KeyEvent;
use kanal::AsyncSender;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use std::sync::Arc;
use uuid::Uuid;

use crate::handlers::ai::chat::components::{
    AgentStatsView, BoardOutcome, GanttView, MessageBoard, RunActivity, ShortcutMenu, TextBox,
    ThorPalClient, TraversalView, ViewOutcome,
};

use super::{AppEvent, Chat, Home, ScrollEvent};

/// Handle a key press on the active page or fallback to home if this doesn't exist
macro_rules! handle_key_fallback {
    ($content:expr, $kind:ident, $key:expr, $active:expr) => {
        // if we have a chat page then forward this event
        match &mut $content.$kind {
            Some(kind) => kind.handle_key($key).await,
            None => {
                // we don't have a chat window so fallback to the home page handling this
                $content.home.handle_key($key);
                // update our active page to be home
                $active = ActiveTabKind::Home;
                None
            }
        }
    };
}

/// Handle a click on the active page or fallback to home if this doesn't exist
macro_rules! handle_click_fallback {
    ($content:expr, $kind:ident, $x:expr, $y:expr, $active:expr) => {
        // if we have a chat page then forward this event
        match &mut $content.$kind {
            Some(kind) => kind.handle_click($x, $y),
            None => {
                // we don't have a chat window so fallback to the home page handling this
                $content.home.handle_click($x, $y);
                // update our active page to be home
                $active = ActiveTabKind::Home;
            }
        }
    };
}

/// Handle a scroll event on the active page or fallback to home if this doesn't exist
macro_rules! handle_scroll_fallback {
    ($content:expr, $kind:ident, $scroll:expr) => {
        // if we have a chat page then forward this event
        match &mut $content.$kind {
            Some(kind) => kind.handle_scroll($scroll),
            None => (),
        }
    };
}

/// Render the active page or fallback to home if it doesn't exist
macro_rules! render_fallback {
    ($content:expr, $kind:ident, $frame:expr, $area:expr, $active:expr) => {
        // if we have a chat page then forward this event
        match &mut $content.$kind {
            Some(kind) => kind.render($frame, $area),
            None => {
                // we don't have a chat window so fallback to the home page handling this
                $content.home.render($frame, $area);
                // update our active page to be home
                $active = ActiveTabKind::Home;
            }
        }
    };
}

/// The currently active page on this tab
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveTabKind {
    /// The home page
    Home,
    /// The chat page
    Chat,
}

/// The different pages and their content
pub struct TabContent {
    /// The content on the home page
    home: Home,
    /// The content on the chat page
    chat: Option<Chat>,
}

impl Default for TabContent {
    /// Create a default [`TabContent`] with only home initialized
    fn default() -> Self {
        TabContent {
            home: Home::default(),
            chat: None,
        }
    }
}

/// A single tab in the application
pub struct Tab {
    /// The display label for this tab
    pub label: String,
    /// The side conversation this tab renders, or `None` for the main one
    pub scope: Option<String>,
    /// The currently active page kind
    pub active: ActiveTabKind,
    /// The content to display
    pub tab_content: TabContent,
    /// The ThorPal chat client for this tab
    thor_chat: Arc<ThorPalClient>,
    /// The message board sidebar showing the ThorPal forum
    board: MessageBoard,
    /// The traversal graph overlay showing how agents moved through the binary
    traversal: TraversalView,
    /// The agent stats overlay showing per-agent cost and activity
    agent_stats: AgentStatsView,
    /// The gantt timeline overlay showing the run's lifetime as a trace
    gantt: GanttView,
    /// The sender side of our event channel
    event_tx: AsyncSender<AppEvent>,
}

/// The narrowest terminal that fits the chat beside the message board; below
/// this the board takes the whole width
const BOARD_SIDE_BY_SIDE_MIN_WIDTH: u16 = 70;

/// Pick how many columns the message board takes out of a terminal width
///
/// # Arguments
///
/// * `width` - The width of the whole terminal
fn board_width(width: u16) -> u16 {
    if width < BOARD_SIDE_BY_SIDE_MIN_WIDTH {
        // too narrow to share, so the board takes the whole width
        width
    } else {
        // take two fifths of the width, within readable bounds
        (width * 2 / 5).clamp(40, 100)
    }
}

impl Tab {
    /// Create a new tab from an already-built chat client
    ///
    /// The tab opens on its home page.
    ///
    /// # Arguments
    ///
    /// * `label` - The label for this new tab
    /// * `thor_chat` - The pre-built chat client this tab drives
    /// * `event_tx` - The sender side of the app's event channel
    pub fn from_client(
        label: impl Into<String>,
        thor_chat: Arc<ThorPalClient>,
        event_tx: &AsyncSender<AppEvent>,
    ) -> Self {
        // build a message board to show the session's forum in, wired to the
        // app event channel its spawned searches report back on
        let board = MessageBoard::new(thor_chat.forum.clone(), event_tx.clone());
        // build the traversal graph overlay around the client's shared log
        let traversal = TraversalView::new(thor_chat.traversal.clone());
        // build the agent stats overlay around the client's shared stats log
        let agent_stats = AgentStatsView::new(
            thor_chat.agent_stats.clone(),
            thor_chat.context.clone(),
            thor_chat.cost,
        );
        // build the gantt timeline overlay around the client's shared gantt log
        let gantt = GanttView::new(thor_chat.gantt.clone(), thor_chat.cost);
        // build a new tab around the supplied client, starting on the home page
        let mut tab = Tab {
            label: label.into(),
            scope: None,
            active: ActiveTabKind::Home,
            tab_content: TabContent::default(),
            thor_chat,
            board,
            traversal,
            agent_stats,
            gantt,
            event_tx: event_tx.clone(),
        };
        // a resumed session opens directly on its chat page: the saved history
        // is already in the shared context, and the restored constructor sends
        // nothing so the analysis waits for the analyst instead of continuing
        if tab.thor_chat.resumed {
            tab.tab_content.chat = Some(Chat::restored(&tab.thor_chat));
            tab.active = ActiveTabKind::Chat;
        }
        // offer the message board from the home page's shortcuts too
        tab.add_home_shortcuts();
        // return the finished tab
        tab
    }

    /// Build a side-conversation tab that opens directly on its chat page
    ///
    /// The tab shares the client -- and so the forum board, overlay logs, and
    /// cost estimator -- but its chat page renders the side's own context and
    /// activity, so the side's log and token readout stay separate from the
    /// main conversation's.
    ///
    /// # Arguments
    ///
    /// * `name` - The side conversation this tab renders
    /// * `thor_chat` - The shared chat client this tab drives
    /// * `event_tx` - The sender side of the app's event channel
    pub fn for_side(
        name: &str,
        thor_chat: Arc<ThorPalClient>,
        event_tx: &AsyncSender<AppEvent>,
    ) -> Self {
        // build a message board to show the session's shared forum in, wired
        // to the app event channel its spawned searches report back on
        let board = MessageBoard::new(thor_chat.forum.clone(), event_tx.clone());
        // build the overlays around the client's shared logs, so the stats and
        // timeline views show the whole session (including btw:* rows) here too
        let traversal = TraversalView::new(thor_chat.traversal.clone());
        let agent_stats = AgentStatsView::new(
            thor_chat.agent_stats.clone(),
            thor_chat.context.clone(),
            thor_chat.cost,
        );
        let gantt = GanttView::new(thor_chat.gantt.clone(), thor_chat.cost);
        // the side's chat page renders its own context and activity
        let chat = Chat::for_side(&thor_chat, name);
        // assemble the tab, opening directly on the side's chat page
        let mut tab = Tab {
            label: format!("btw:{name}"),
            scope: Some(name.to_owned()),
            active: ActiveTabKind::Chat,
            tab_content: TabContent::default(),
            thor_chat,
            board,
            traversal,
            agent_stats,
            gantt,
            event_tx: event_tx.clone(),
        };
        tab.tab_content.chat = Some(chat);
        // the home page is only a fallback here, but keep its shortcuts whole
        tab.add_home_shortcuts();
        tab
    }

    /// Whether one of this tab's full-screen overlays currently owns the screen
    pub fn overlay_open(&self) -> bool {
        // any open overlay takes the whole screen, including the tab strip
        self.agent_stats.open || self.traversal.open || self.gantt.open
    }

    /// Hand a /done summary outcome to this tab's chat page, if one is open
    ///
    /// # Arguments
    ///
    /// * `summary` - The generated (or revised) summary text
    pub fn summary_ready(&mut self, summary: String) {
        // forward to the chat page's modal when the page exists
        if let Some(chat) = &mut self.tab_content.chat {
            chat.summary_ready(summary);
        }
    }

    /// Surface a failed /done summary on this tab's chat page, if one is open
    ///
    /// # Arguments
    ///
    /// * `error` - The human-readable error describing the failure
    pub fn summary_failed(&mut self, error: String) {
        // forward to the chat page's modal when the page exists
        if let Some(chat) = &mut self.tab_content.chat {
            chat.summary_failed(error);
        }
    }

    /// Install an externally edited summary on this tab's chat page
    ///
    /// # Arguments
    ///
    /// * `summary` - The summary text as it came back from the editor
    pub fn summary_edited(&mut self, summary: String) {
        // forward to the chat page's modal when the page exists
        if let Some(chat) = &mut self.tab_content.chat {
            chat.summary_edited(summary);
        }
    }

    /// Fall back to in-modal summary editing on this tab's chat page
    pub fn summary_edit_fallback(&mut self) {
        // forward to the chat page's modal when the page exists
        if let Some(chat) = &mut self.tab_content.chat {
            chat.summary_edit_fallback();
        }
    }

    /// Offer the message board in the home page's shortcut menu
    fn add_home_shortcuts(&mut self) {
        // add the overlays to the home page's shortcuts
        self.tab_content.home.shortcuts = ShortcutMenu::builder()
            .add('m', "Message board")
            .add('g', "Traversal graph")
            .add('s', "Search forum")
            .add('t', "Timeline")
            .add('u', "Usage")
            .build();
    }

    /// The prompt of whichever page is currently showing
    fn page_prompt(&mut self) -> &mut TextBox {
        // the chat page's prompt once it exists, otherwise the home page's
        match (self.active, &mut self.tab_content.chat) {
            (ActiveTabKind::Chat, Some(chat)) => &mut chat.prompt,
            _ => &mut self.tab_content.home.prompt,
        }
    }

    /// Open or close the message board
    ///
    /// Opening focuses the board and jumps to the latest forum activity; closing
    /// leaves the keyboard unowned so Space shortcuts work immediately (press
    /// `i` or click the prompt to type).
    pub fn toggle_board(&mut self) {
        if self.board.open {
            // close the board, leaving the keyboard in navigation mode
            self.board.hide();
        } else {
            // open the board and keep the prompt from taking its keys
            self.board.show();
            self.page_prompt().is_focused = false;
        }
    }

    /// Open or close the traversal graph overlay
    ///
    /// Opening hands the overlay the keyboard; closing leaves the keyboard
    /// unowned so Space shortcuts work immediately (press `i` or click the
    /// prompt to type).
    pub fn toggle_traversal(&mut self) {
        // only one full-screen overlay may hold the keyboard at a time
        if !self.traversal.open && self.agent_stats.open {
            self.toggle_agent_stats();
        }
        if !self.traversal.open && self.gantt.open {
            self.toggle_gantt();
        }
        self.traversal.toggle();
        if self.traversal.open {
            // keep the prompt from eating the overlay's keys
            self.page_prompt().is_focused = false;
        }
    }

    /// Open or close the agent stats overlay
    ///
    /// Opening hands the overlay the keyboard; closing leaves the keyboard
    /// unowned so Space shortcuts work immediately (press `i` or click the
    /// prompt to type).
    pub fn toggle_agent_stats(&mut self) {
        // only one full-screen overlay may hold the keyboard at a time
        if !self.agent_stats.open && self.traversal.open {
            self.toggle_traversal();
        }
        if !self.agent_stats.open && self.gantt.open {
            self.toggle_gantt();
        }
        self.agent_stats.toggle();
        if self.agent_stats.open {
            // keep the prompt from eating the overlay's keys
            self.page_prompt().is_focused = false;
        }
    }

    /// Open or close the gantt timeline overlay
    ///
    /// Opening hands the overlay the keyboard; closing leaves the keyboard
    /// unowned so Space shortcuts work immediately (press `i` or click the
    /// prompt to type).
    pub fn toggle_gantt(&mut self) {
        // only one full-screen overlay may hold the keyboard at a time
        if !self.gantt.open && self.traversal.open {
            self.toggle_traversal();
        }
        if !self.gantt.open && self.agent_stats.open {
            self.toggle_agent_stats();
        }
        self.gantt.toggle();
        if self.gantt.open {
            // keep the prompt from eating the overlay's keys
            self.page_prompt().is_focused = false;
        }
    }

    /// Open the message board if needed and start a forum search in it
    ///
    /// Reached from the pages' Space+s shortcut, which only fires while no
    /// full-screen overlay owns the keyboard, so only the board needs handling.
    pub fn open_board_search(&mut self) {
        // open the board if it is closed
        if !self.board.open {
            self.board.show();
        }
        // make sure the board has the keyboard even if it was already open
        self.board.focused = true;
        // keep the prompt from eating the search box's keys
        self.page_prompt().is_focused = false;
        // drop the board straight into its search box
        self.board.start_search();
    }

    /// A handle to the flag that is set while a replay animation is playing
    ///
    /// The app's redraw ticker reads this so a replay keeps animating even
    /// when no run event fires.
    pub fn traversal_animating(&self) -> Arc<std::sync::atomic::AtomicBool> {
        self.traversal.animating()
    }

    /// Hand the message board a fresh view of the forum after it changed
    ///
    /// # Arguments
    ///
    /// * `view` - The whole forum as it stands after the change
    /// * `target` - The post or comment that was just created or changed
    pub fn update_board(&mut self, view: thorpal::ForumView, target: thorpal::ParentRef) {
        // hand the board the forum's latest state
        self.board.update(view, target);
    }

    /// Refresh this tab's message board with no selection jump target
    ///
    /// Used when an analyst ask or review flag was recorded: the board's data
    /// changes but there is no post or comment to reveal.
    ///
    /// # Arguments
    ///
    /// * `view` - The whole forum as it stands after the change
    pub fn refresh_board(&mut self, view: thorpal::ForumView) {
        // hand the board the latest state without moving its selection
        self.board.refresh(view);
    }

    /// Hand finished forum-search results to this tab's message board
    ///
    /// # Arguments
    ///
    /// * `board` - The board that spawned the search
    /// * `flight` - The spawning board's flight number for the search
    /// * `query` - The exact search-box text the hits answer
    /// * `hits` - The ranked hits, best first
    /// * `terms` - The query's terms, for highlighting what the hits matched
    pub fn apply_board_search(
        &mut self,
        board: Uuid,
        flight: u64,
        query: &str,
        hits: Vec<thorpal::SearchHit>,
        terms: Vec<String>,
    ) {
        // the board decides whether the results are still current
        self.board
            .apply_search_results(board, flight, query, hits, terms);
    }

    /// Handle key input when the query box is focused
    ///
    /// # Arguments
    ///
    /// * `code` - The key code that was pressed
    pub async fn handle_key(&mut self, key: KeyEvent) {
        // an open agent stats overlay owns the whole keyboard
        if self.agent_stats.open {
            match self.agent_stats.handle_key(key) {
                // the overlay handled the key itself
                ViewOutcome::None => (),
                // Esc/q closes the overlay
                ViewOutcome::Close => self.toggle_agent_stats(),
            }
            return;
        }
        // an open traversal overlay owns the whole keyboard
        if self.traversal.open {
            match self.traversal.handle_key(key) {
                // the overlay handled the key itself
                ViewOutcome::None => (),
                // Esc/q closes the overlay
                ViewOutcome::Close => self.toggle_traversal(),
            }
            return;
        }
        // an open gantt timeline overlay owns the whole keyboard
        if self.gantt.open {
            match self.gantt.handle_key(key) {
                // the overlay handled the key itself
                ViewOutcome::None => (),
                // Esc/q closes the overlay
                ViewOutcome::Close => self.toggle_gantt(),
            }
            return;
        }
        // a focused message board takes every key
        if self.board.open && self.board.focused {
            match self.board.handle_key(key).await {
                // the board handled the key itself
                BoardOutcome::None => (),
                // Space+m closes the board
                BoardOutcome::Close => self.toggle_board(),
                // Esc returns the keyboard to navigation, leaving the board open
                BoardOutcome::Unfocus => self.board.focused = false,
            }
            return;
        }
        // pass this key event to the currently active tab kind
        let event = match self.active {
            ActiveTabKind::Home => self.tab_content.home.handle_key(key),
            ActiveTabKind::Chat => {
                handle_key_fallback!(self.tab_content, chat, key, self.active)
            }
        };
        // If we got an event then put it into the queue to be handled; the
        // channel only closes while the app is shutting down, so a failed send
        // is dropped rather than panicking the TUI in raw mode
        if let Some(event) = event {
            let _ = self.event_tx.send(event).await;
        }
    }

    /// Route a pasted text to whichever text box currently has the keyboard
    ///
    /// Mirrors [`Tab::handle_key`]'s precedence: an open full-screen overlay
    /// has no text input and swallows the paste, a focused message board
    /// pastes into its composer, and otherwise the active page decides.
    ///
    /// # Arguments
    ///
    /// * `text` - The pasted text to insert
    pub async fn handle_paste(&mut self, text: &str) {
        // the full-screen overlays have no text inputs: swallow the paste so
        // it can never leak into a box the user cannot see
        if self.agent_stats.open || self.traversal.open || self.gantt.open {
            return;
        }
        // a focused message board pastes into its reply composer or search box
        if self.board.open && self.board.focused {
            self.board.handle_paste(text).await;
            return;
        }
        // otherwise the active page routes to its own focused box
        match self.active {
            ActiveTabKind::Home => self.tab_content.home.handle_paste(text),
            ActiveTabKind::Chat => {
                // a side tab falls back to the home page exactly like keys do
                match &mut self.tab_content.chat {
                    Some(chat) => chat.handle_paste(text),
                    None => self.tab_content.home.handle_paste(text),
                }
            }
        }
    }

    /// Handle a mouse click event
    ///
    /// # Arguments
    ///
    /// * `x` - The column position of the click
    /// * `y` - The row position of the click
    pub fn handle_click(&mut self, x: u16, y: u16) {
        // an open agent stats overlay owns clicks (drill-down tool bubbles)
        if self.agent_stats.open {
            self.agent_stats.handle_click(x, y);
            return;
        }
        // an open gantt timeline overlay owns clicks (row selection/expansion)
        if self.gantt.open {
            self.gantt.handle_click(x, y);
            return;
        }
        // a click on the open message board focuses it instead of the page
        if self.board.contains(x, y) {
            // select whatever was clicked and keep the prompt out of the way
            self.board.handle_click(x, y);
            self.page_prompt().is_focused = false;
            return;
        }
        // a click on the chat-side search picker keeps it alive and may pick
        // the clicked hit row
        if self.board.overlay_contains(x, y) {
            self.board.handle_overlay_click(x, y);
            self.page_prompt().is_focused = false;
            return;
        }
        // a click anywhere else moves focus off the board
        self.board.focused = false;
        // pass this mouse click event to the currently active tab kind
        match self.active {
            ActiveTabKind::Home => self.tab_content.home.handle_click(x, y),
            ActiveTabKind::Chat => {
                handle_click_fallback!(self.tab_content, chat, x, y, self.active)
            }
        };
    }

    /// Handle a scroll event
    ///
    /// # Arguments
    ///
    /// * `event` - The scroll event to handle
    pub fn handle_scroll(&mut self, event: ScrollEvent) {
        // an open agent stats overlay scrolls its bars or drill-down chat
        if self.agent_stats.open {
            match event {
                ScrollEvent::ScrollUp => self.agent_stats.handle_scroll(true),
                ScrollEvent::ScrollDown => self.agent_stats.handle_scroll(false),
                // there is nothing to scroll left or right on
                ScrollEvent::ScrollLeft | ScrollEvent::ScrollRight => (),
            }
            return;
        }
        // an open traversal overlay turns the wheel into zoom
        if self.traversal.open {
            match event {
                ScrollEvent::ScrollUp => self.traversal.handle_scroll(true),
                ScrollEvent::ScrollDown => self.traversal.handle_scroll(false),
                // there is nothing to zoom sideways on
                ScrollEvent::ScrollLeft | ScrollEvent::ScrollRight => (),
            }
            return;
        }
        // an open gantt overlay scrolls its rows and pans its time window
        if self.gantt.open {
            match event {
                ScrollEvent::ScrollUp => self.gantt.handle_scroll(true),
                ScrollEvent::ScrollDown => self.gantt.handle_scroll(false),
                // a sideways wheel pans the time window
                ScrollEvent::ScrollLeft => self.gantt.handle_pan(true),
                ScrollEvent::ScrollRight => self.gantt.handle_pan(false),
            }
            return;
        }
        // a focused message board scrolls itself rather than the page
        if self.board.open && self.board.focused {
            match event {
                ScrollEvent::ScrollUp => self.board.scroll_up(3),
                ScrollEvent::ScrollDown => self.board.scroll_down(3),
                // there is nothing to scroll left or right on
                ScrollEvent::ScrollLeft | ScrollEvent::ScrollRight => (),
            }
            return;
        }
        // only the chat page supports scrolling right now
        match self.active {
            // the home page doesn't do anything with scroll events yet
            ActiveTabKind::Home => (),
            // send this scroll event to our chat page
            ActiveTabKind::Chat => handle_scroll_fallback!(self.tab_content, chat, event),
        }
    }

    /// Get a handle to this tab's live run-activity
    ///
    /// The app's redraw ticker and the tab strip's busy markers read this to
    /// keep animating during quiet gaps where no run event fires. A side tab
    /// hands back its own side's activity rather than the main conversation's.
    pub fn activity(&self) -> RunActivity {
        // a side tab's activity is its side's own bucket
        match &self.scope {
            Some(name) => self.thor_chat.sides.handle(name).activity,
            // the client owns the main handle both worker tasks write to
            None => self.thor_chat.activity.clone(),
        }
    }

    /// Convert this tabs home page to a chat page
    pub async fn home_to_chat(&mut self) {
        if self.active == ActiveTabKind::Home {
            // get our current home tab and replace it with a default
            let home = std::mem::take(&mut self.tab_content.home);
            // build a chat page from our home page content
            let chat = Chat::new(&self.thor_chat, vec![home.prompt.content]).await;
            // insert this into our chat tab content
            self.tab_content.chat = Some(chat);
            // set our active tab to the chat tab
            self.active = ActiveTabKind::Chat;
            // the fresh home page we left behind needs its board shortcut back
            self.add_home_shortcuts();
            // keep the chat prompt from stealing keys while the board has focus
            if self.board.open && self.board.focused {
                self.page_prompt().is_focused = false;
            }
        }
    }

    /// Render this tabs content
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render to
    /// * `area` - The area to render the input in
    pub fn render(&mut self, frame: &mut Frame) {
        // start from the whole terminal
        let area = frame.area();
        // an open agent stats overlay takes the whole screen instead of the page
        if self.agent_stats.open {
            self.agent_stats.render(frame, area);
            return;
        }
        // an open traversal overlay takes the whole screen instead of the page
        if self.traversal.open {
            self.traversal.render(frame, area);
            return;
        }
        // an open gantt timeline overlay takes the whole screen too
        if self.gantt.open {
            self.gantt.render(frame, area);
            return;
        }
        // slide the message board in from the right when it is open
        let (page_area, board_area) = if self.board.open {
            let [page, board] = Layout::horizontal([
                Constraint::Min(0),
                Constraint::Length(board_width(area.width)),
            ])
            .areas(area);
            (page, Some(board))
        } else {
            (area, None)
        };
        // a board taking the whole width leaves no room to draw the page in
        if page_area.width > 0 {
            self.render_page(frame, page_area);
        }
        // draw the board over its slice of the screen
        if let Some(board_area) = board_area {
            // anchor the search picker on the active page's content rect,
            // which the page render above refreshed this same frame
            let anchor = if page_area.width > 0 {
                let anchor = match (self.active, &self.tab_content.chat) {
                    (ActiveTabKind::Chat, Some(chat)) => chat.log_area(),
                    _ => self.tab_content.home.content_area(),
                };
                // a stale or degenerate rect must never escape the page's slice
                let anchor = anchor.intersection(page_area);
                (anchor.width > 0 && anchor.height > 0).then_some(anchor)
            } else {
                None
            };
            // whether the board's search picker will actually draw there
            let overlay_anchor =
                anchor.filter(|anchor| self.board.search_overlay_area(*anchor).is_some());
            // tell the board before it renders, so it leaves its own search
            // UI out exactly when the picker takes over
            self.board.chat_overlay = overlay_anchor.is_some();
            self.board.render(frame, board_area);
            // draw the search picker over the chat side, leaving the bottom
            // of the chat log visible beneath it
            if let Some(anchor) = overlay_anchor {
                self.board.render_search_overlay(frame, anchor);
            }
            // the page drew its shortcut menu before the board painted over the
            // bottom-right corner, so draw it again on top; an unfocused open
            // board leaves the keyboard on the page, where Space opens the menu
            match (self.active, &mut self.tab_content.chat) {
                (ActiveTabKind::Chat, Some(chat)) => chat.shortcuts.render(frame),
                _ => self.tab_content.home.shortcuts.render(frame),
            }
        }
    }

    /// Render the active page into an area
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render to
    /// * `area` - The area to render the page in
    fn render_page(&mut self, frame: &mut Frame, area: Rect) {
        // draw whichever page is active
        match self.active {
            ActiveTabKind::Home => self.tab_content.home.render(frame, area),
            ActiveTabKind::Chat => {
                render_fallback!(self.tab_content, chat, frame, area, self.active)
            }
        }
    }
}
