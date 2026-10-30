//! The different events to handle in the TUI

use crossterm::event::Event;
use kanal::AsyncSender;

use crate::handlers::ai::chat::ActiveTabKind;

/// Events that can be received by the main event loop
pub enum AppEvent {
    /// A terminal event (keyboard, mouse, resize, etc.)
    Terminal(Event),
    /// Turn the home page into a Chat page
    HomeToChat,
    /// Set the active page
    SetActiveTabKind(ActiveTabKind),
    /// The ThorPal forum changed; carries a fresh view and what changed
    ForumUpdated {
        /// The whole forum as it stands after the change
        view: thorpal::ForumView,
        /// The post or comment that was just created or changed
        target: thorpal::ParentRef,
    },
    /// The forum changed with no post/comment jump target (an analyst ask or
    /// review flag was recorded); boards refresh without moving the selection
    ForumRefreshed {
        /// The whole forum as it stands after the change
        view: thorpal::ForumView,
    },
    /// The main agent asked the analyst a question and awaits the answer
    AnalystAsk {
        /// The pending ask to render as a question modal
        ask: thorpal::PendingAsk,
    },
    /// An analyst ask resolved; close a matching still-open question modal
    AnalystAskResolved {
        /// The resolved ask's link ticket id
        id: u64,
    },
    /// A spawned forum board search finished ranking its query
    BoardSearchResults {
        /// The board that spawned the search; only it may apply the results
        board: uuid::Uuid,
        /// The board's flight number for the search, so a landed result can
        /// only ever settle the exact flight that produced it
        flight: u64,
        /// The exact search-box text the hits were ranked against
        query: String,
        /// The ranked hits, best first
        hits: Vec<thorpal::SearchHit>,
        /// The query's terms, for highlighting what the hits matched on
        terms: Vec<String>,
    },
    /// Open (or switch to) the named side conversation's tab
    OpenBtw {
        /// The side conversation's name
        name: String,
    },
    /// A /done summary finished generating for a side conversation
    BtwSummaryReady {
        /// The side conversation the summary covers
        name: String,
        /// The generated (or revised) summary text
        summary: String,
    },
    /// A /done summary generation failed
    BtwSummaryFailed {
        /// The side conversation whose summary failed
        name: String,
        /// The human-readable error describing the failure
        error: String,
    },
    /// Close and remove a side conversation's tab
    CloseBtwTab {
        /// The side conversation whose tab is closing
        name: String,
    },
    /// Suspend the TUI and edit a /done summary in the user's default editor
    EditInEditor {
        /// The side conversation whose summary is being edited
        name: String,
        /// The current summary text to edit
        summary: String,
    },
    /// Open or close the message board sidebar
    ToggleMessageBoard,
    /// Open the message board with its forum search ready to type into
    OpenForumSearch,
    /// Open or close the traversal graph overlay
    ToggleTraversal,
    /// Open or close the agent stats overlay
    ToggleAgentStats,
    /// Open or close the gantt timeline overlay
    ToggleGantt,
    /// Quit the TUI gracefully (e.g. a SIGTERM asked the process to stop)
    Quit,
    /// Redraw the tui
    Redraw,
}

/// Spawn the core event forwarders
pub fn spawn_forwarders(sender: &AsyncSender<AppEvent>) {
    // forward terminal events and SIGTERM quits through the shared tui
    // helpers, so this loop's sequences cannot drift from the usage TUI's
    crate::handlers::ai::tui::spawn_forwarders(sender, AppEvent::Terminal, || AppEvent::Quit);
}

/// The different kind of scroll events
pub enum ScrollEvent {
    /// A scroll up event
    ScrollUp,
    /// A scroll down event
    ScrollDown,
    /// A scroll left event
    ScrollLeft,
    /// A Scroll right event
    ScrollRight,
}
