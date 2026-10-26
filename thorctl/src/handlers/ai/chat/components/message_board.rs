//! The message board sidebar for the ThorPal forum agents discuss on
//!
//! ThorPal's agents post findings and questions to a shared forum and argue over
//! them in threaded comments. This component renders that forum as a tree in a
//! sidebar that slides in from the right of the chat, and lets an analyst browse
//! it, fold threads, reply, and change question statuses while a run is going.
//!
//! Comments indent one level per reply up to [`MAX_INDENT_DEPTH`]; deeper replies
//! stay in that last column and carry their depth as a number in the tree art
//! (e.g. `├4──`) so a deep thread never squeezes its text off the screen.

use crossterm::event::{KeyCode, KeyEvent};
use kanal::AsyncSender;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use thorpal::{
    AnalystAsk, CommentNode, ForumState, ForumView, Kind, ParentRef, QuestionStatus, ReviewFlag,
    SearchHit, Stance, Task, match_ranges, search_terms,
};
use unicode_width::UnicodeWidthStr;

use super::TextBox;
use super::markdown::markdown_rows;
use super::text_wrap::{wrap_unicode_preserve_ws, wrap_words};
use crate::handlers::ai::chat::AppEvent;
use uuid::Uuid;

/// The deepest reply level that still indents; deeper replies show their depth
const MAX_INDENT_DEPTH: usize = 3;

/// The author ThorPal attributes a human operator's posts and comments to
const USER_AUTHOR: &str = "user";

/// The width the board lays itself out at before it has ever been rendered
const DEFAULT_WIDTH: usize = 60;

/// The height of the reply text box, including its borders
const COMPOSE_HEIGHT: u16 = 6;

/// The height of the forum search query box, including its borders
const SEARCH_HEIGHT: u16 = 3;

/// The most hits the board's search shows at once
const BOARD_SEARCH_LIMIT: usize = 20;

/// How many columns each reply level indents by
const INDENT_WIDTH: usize = 4;

/// The narrowest a post or comment box is drawn, borders included
const MIN_CARD_WIDTH: usize = 12;

/// The style for tree-drawing characters, light enough to read on dark terminals
const GUIDE_STYLE: Style = Style::new().fg(Color::Rgb(130, 130, 130));

/// The background filling every post and comment box
const CARD_BG: Color = Color::Rgb(45, 45, 45);

/// The outline of a post or comment box that is not selected
const CARD_BORDER: Style = Style::new().fg(Color::Rgb(120, 120, 120)).bg(CARD_BG);

/// The outline of the selected box while the board has focus
const SELECTED_BORDER: Style = Style::new()
    .fg(Color::Rgb(120, 180, 255))
    .bg(CARD_BG)
    .add_modifier(Modifier::BOLD);

/// The outline of the selected box while focus is elsewhere
const SELECTED_BORDER_UNFOCUSED: Style = Style::new().fg(Color::Rgb(80, 120, 170)).bg(CARD_BG);

/// The color of body text inside a box
const TEXT_COLOR: Color = Color::Rgb(220, 220, 220);

/// The color of secondary details inside a box (neutral stances, folded counts)
const MUTED_COLOR: Color = Color::Rgb(150, 150, 150);

/// The style painted over matched search terms inside a box or the preview
///
/// Reversed video renders as a yellow block behind dark text, and survives
/// [`BoardLine::to_line`] forcing the card background onto every content span
/// (the board's styling is foreground-only by design).
const SEARCH_MATCH: Style = Style::new()
    .fg(Color::Yellow)
    .add_modifier(Modifier::REVERSED);

/// The style painted over matched search terms in the hits popup
///
/// The selected hit's whole line is already reversed, so a reversed match
/// would vanish there; a yellow bold foreground reads in both states.
const SEARCH_MATCH_FLAT: Style = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);

/// The rows of the chat log the search picker leaves visible beneath it
const OVERLAY_LOG_GAP: u16 = 10;

/// The shortest search picker worth drawing over the chat log
///
/// Two border rows, the three-row query box, and two hit rows; anything
/// smaller reads worse than the in-board fallback the picker degrades to.
const MIN_OVERLAY_HEIGHT: u16 = 7;

/// The narrowest search picker worth drawing over the chat log
const MIN_OVERLAY_WIDTH: u16 = 16;

/// The narrowest picker interior that still fits a preview beside the hits
///
/// A side-by-side page can be as narrow as 30 columns, so pickers below this
/// width really do occur and draw the hits list alone.
const OVERLAY_SPLIT_MIN_WIDTH: u16 = 52;

/// What the tab should do after the board handled a key
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardOutcome {
    /// Nothing beyond what the board already did
    None,
    /// Close the board
    Close,
    /// Return the keyboard to the page, leaving the board open
    Unfocus,
}

/// What the board is currently doing with key presses
#[derive(Debug)]
enum BoardMode {
    /// Moving around the forum
    Browse,
    /// Writing a reply to a post or comment
    Compose(Compose),
    /// Writing a completion-gate task for a question
    ComposeTask {
        /// The question the task will gate
        target: ParentRef,
        /// The text box the task is typed into
        text: TextBox,
    },
    /// Picking a new status for a question
    StatusPick {
        /// The question whose status is being changed
        target: ParentRef,
    },
    /// Searching the forum's posts and comments
    Search(ForumSearch),
}

/// An in-progress forum search from the analyst
#[derive(Debug)]
struct ForumSearch {
    /// The text box the query is typed into
    query: TextBox,
    /// The hits for the current query, best first
    hits: Vec<SearchHit>,
    /// The index of the selected hit
    selected: usize,
    /// The current query's terms, for highlighting what the hits matched on
    terms: Vec<String>,
    /// The flight number of the spawned, still-running search, if any
    ///
    /// Doubles as the single-flight latch: while set, new search requests only
    /// mark the search dirty instead of spawning another task. Flights are
    /// matched by board id and number, never by query text, so a same-text
    /// result from another tab's board (or from a flight this search already
    /// moved past) can neither clear the latch nor settle the search. A search
    /// task that dies without reporting (it never should: the search is pure
    /// in-process code) would stick the latch until the search is reopened.
    inflight: Option<u64>,
    /// Whether the query text or the forum changed while a search was in
    /// flight, so one more search must run when the in-flight one lands
    dirty: bool,
    /// The query string the displayed hits and terms currently answer
    ///
    /// Compared with the box's live content to gate Enter (never jump on hits
    /// for a different query) and to drive the searching indicator.
    answered: String,
}

impl ForumSearch {
    /// Start a new, empty forum search
    fn new() -> Self {
        // start with a focused query box and no hits until something is typed
        ForumSearch {
            query: TextBox::default().focused().title("Search forum"),
            hits: Vec::new(),
            selected: 0,
            terms: Vec::new(),
            inflight: None,
            dirty: false,
            answered: String::new(),
        }
    }
}

/// An in-progress reply from the analyst
#[derive(Debug)]
struct Compose {
    /// The post or comment being replied to
    parent: ParentRef,
    /// Whether this reply states something or asks a question
    kind: Kind,
    /// How this reply relates to what it is replying to
    stance: Stance,
    /// The text box the reply is typed into
    text: TextBox,
}

impl Compose {
    /// Start a new reply
    ///
    /// # Arguments
    ///
    /// * `parent` - The post or comment being replied to
    /// * `kind` - Whether this reply states something or asks a question
    fn new(parent: ParentRef, kind: Kind) -> Self {
        // start neutral with a focused text box ready for typing
        let mut compose = Compose {
            parent,
            kind,
            stance: Stance::Neutral,
            text: TextBox::default().focused(),
        };
        // title the box with what is being replied to and how
        compose.retitle();
        compose
    }

    /// Cycle to the next stance and update the box's title to match
    fn cycle_stance(&mut self) {
        // step through neutral, supports, and disproves in a loop
        self.stance = match self.stance {
            Stance::Neutral => Stance::Supports,
            Stance::Supports => Stance::Disproves,
            Stance::Disproves => Stance::Neutral,
        };
        // show the new stance in the box's title
        self.retitle();
    }

    /// Rebuild the text box title from the parent, kind, and stance
    fn retitle(&mut self) {
        // a question reply says so up front so it is not sent by mistake
        let verb = match self.kind {
            Kind::Statement => "Reply to",
            Kind::Question(_) => "Ask on",
        };
        // name the target and the stance the reply will be posted with
        self.text.title = Some(format!(
            "{verb} {} [{}]",
            reference(&self.parent),
            stance_label(self.stance)
        ));
    }
}

/// Where to scroll the selected item to on the next render
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reveal {
    /// Scroll just enough to bring it on screen
    Visible,
    /// Scroll so it sits about a third of the way down
    Third,
}

/// One post or comment laid out in the board's tree
#[derive(Debug, Clone, PartialEq)]
struct BoardItem {
    /// The post or comment this item shows
    target: ParentRef,
    /// The reply depth: 0 for a post, 1 for a top-level comment, and so on
    depth: usize,
    /// Whether this is a question or a statement
    kind: Kind,
    /// The tree art drawn before this item's header
    head_prefix: String,
    /// The tree art drawn before each of this item's continuation lines
    body_prefix: String,
    /// Whether this item has any replies to fold
    has_replies: bool,
    /// How many replies are hidden because this item is folded (0 when open)
    hidden: usize,
}

/// Which part of a post or comment box a board line draws
#[derive(Debug, Clone)]
enum RowKind {
    /// The box's top border
    Top,
    /// A row of text inside the box
    Content(Vec<Span<'static>>),
    /// The box's bottom border
    Bottom,
    /// A blank line separating posts
    Spacer,
}

/// A single laid out line of the board
///
/// The box outline is styled when the line is drawn rather than when it is laid
/// out, so moving the selection only recolors outlines instead of re-laying out.
#[derive(Debug, Clone)]
struct BoardLine {
    /// The index of the item this line belongs to, or `None` for a spacer
    item: Option<usize>,
    /// The tree art drawn to the left of the box
    gutter: String,
    /// The width of the box, borders included
    card_width: usize,
    /// Which part of the box this line draws
    row: RowKind,
}

impl BoardLine {
    /// A blank line separating posts
    fn spacer() -> Self {
        // a spacer belongs to no item and draws nothing
        BoardLine {
            item: None,
            gutter: String::new(),
            card_width: 0,
            row: RowKind::Spacer,
        }
    }

    /// Build the styled line to draw, outlining the box in a border style
    ///
    /// # Arguments
    ///
    /// * `border` - The style for this box's outline
    fn to_line(&self, border: Style) -> Line<'static> {
        // the width between the box's corners
        let inner = self.card_width.saturating_sub(2);
        // start with the tree art on the terminal's own background
        let mut spans = vec![Span::styled(self.gutter.clone(), GUIDE_STYLE)];
        match &self.row {
            // a spacer is just a blank line
            RowKind::Spacer => return Line::default(),
            // draw the top border with rounded corners
            RowKind::Top => spans.push(Span::styled(format!("╭{}╮", "─".repeat(inner)), border)),
            // draw the bottom border with rounded corners
            RowKind::Bottom => {
                spans.push(Span::styled(format!("╰{}╯", "─".repeat(inner)), border));
            }
            // draw a row of text inside the box
            RowKind::Content(content) => return self.content_line(content, border),
        }
        // join the pieces into one line
        Line::from(spans)
    }

    /// Build the styled line to draw with search matches painted over it
    ///
    /// # Arguments
    ///
    /// * `border` - The style for this box's outline
    /// * `terms` - The query terms to highlight in the content
    fn to_line_highlighted(&self, border: Style, terms: &[String]) -> Line<'static> {
        // only content rows carry text to highlight
        let RowKind::Content(content) = &self.row else {
            return self.to_line(border);
        };
        // paint the matches over a copy of the content spans
        let mut highlighted = content.clone();
        highlight_row(&mut highlighted, terms, SEARCH_MATCH);
        // draw the highlighted copy inside the regular box chrome
        self.content_line(&highlighted, border)
    }

    /// Build a content row's line: borders, text, and background fill
    ///
    /// # Arguments
    ///
    /// * `content` - The styled text spans inside the box
    /// * `border` - The style for this box's outline
    fn content_line(&self, content: &[Span<'static>], border: Style) -> Line<'static> {
        // start with the tree art on the terminal's own background
        let mut spans = vec![Span::styled(self.gutter.clone(), GUIDE_STYLE)];
        // draw the left border and its padding
        spans.push(Span::styled("│ ", border));
        // fill the box's dark background behind the text
        let mut used = 0;
        for span in content {
            used += span.width();
            spans.push(Span::styled(span.content.clone(), span.style.bg(CARD_BG)));
        }
        // pad out to the right border so every row lines up
        let text_width = self.card_width.saturating_sub(4);
        let pad = " ".repeat(text_width.saturating_sub(used));
        spans.push(Span::styled(pad, Style::new().bg(CARD_BG)));
        // draw the right padding and border
        spans.push(Span::styled(" │", border));
        // join the pieces into one line
        Line::from(spans)
    }
}

/// A post or comment visited while flattening the forum tree
#[derive(Debug)]
struct FlatEntry<'a> {
    /// The post or comment itself
    node: NodeRef<'a>,
    /// The completion-gate tasks on this post or comment
    tasks: &'a [Task],
    /// The reply depth: 0 for a post
    depth: usize,
    /// Whether this is the last reply under its parent
    is_last: bool,
    /// For each ancestor comment level, whether that ancestor has later siblings
    guides: Vec<bool>,
    /// Whether this item has any replies
    has_replies: bool,
    /// How many replies are hidden because this item is folded
    hidden: usize,
}

/// A borrowed post or comment from a [`ForumView`]
#[derive(Debug, Clone, Copy)]
enum NodeRef<'a> {
    /// A top-level post
    Post(&'a thorpal::Post),
    /// A comment on a post or another comment
    Comment(&'a thorpal::Comment),
}

impl NodeRef<'_> {
    /// The reference this node is addressed by
    fn target(&self) -> ParentRef {
        // posts are addressed by name and comments by id
        match self {
            NodeRef::Post(post) => ParentRef::Post(post.name.clone()),
            NodeRef::Comment(comment) => ParentRef::Comment(comment.id),
        }
    }

    /// Whether this node states something or asks a question
    fn kind(&self) -> Kind {
        // read the kind off whichever node this is
        match self {
            NodeRef::Post(post) => post.kind,
            NodeRef::Comment(comment) => comment.kind,
        }
    }
}

/// The message board sidebar
pub struct MessageBoard {
    /// The forum this board shows and writes to
    forum: Arc<ForumState>,
    /// The app event channel spawned forum searches report back on
    event_tx: AsyncSender<AppEvent>,
    /// This board's identity, stamped onto its search flights so broadcast
    /// results only ever settle the board that spawned them
    board_id: Uuid,
    /// How many search flights this board has ever spawned; the latest value
    /// names the in-flight search in [`ForumSearch::inflight`]
    flights: u64,
    /// The latest view of the forum, or `None` before the first update
    view: Option<ForumView>,
    /// Whether the board is visible
    pub open: bool,
    /// Whether the board receives key presses
    pub focused: bool,
    /// The posts and comments whose replies are folded away
    collapsed: HashSet<ParentRef>,
    /// The laid out posts and comments, in display order
    items: Vec<BoardItem>,
    /// The rendered lines for every item
    lines: Vec<BoardLine>,
    /// The index of each item's first line in `lines`
    item_starts: Vec<usize>,
    /// The width the current layout was built for
    width: usize,
    /// The index of the selected item
    selected: usize,
    /// An item to select as soon as it shows up in the layout
    want_select: Option<ParentRef>,
    /// The jumped-to search hit and the terms to highlight inside it
    highlight: Option<(ParentRef, Vec<String>)>,
    /// The first laid-out line of the highlighted item with a match in it
    highlight_line: Option<usize>,
    /// The scroll offset of the list, in lines
    scroll_y: usize,
    /// Where to scroll the selection to on the next render, if anywhere
    reveal: Option<Reveal>,
    /// What key presses currently do
    mode: BoardMode,
    /// A one-line status or error message for the footer
    notice: Option<String>,
    /// Whether the forum changed somewhere other than the selection
    unseen: bool,
    /// Whether Space was just pressed, so a following `m` closes the board
    leader_pending: bool,
    /// The whole area the board was last drawn into
    area: Option<Rect>,
    /// The area the list of lines was last drawn into
    list_area: Option<Rect>,
    /// Whether the search picker draws on the chat side this frame
    ///
    /// Set by the tab from [`MessageBoard::search_overlay_area`] before the
    /// board renders, so the board leaves its own search UI out exactly when
    /// the picker takes over.
    pub chat_overlay: bool,
    /// The whole search picker as last drawn, for click hit-testing
    overlay_area: Option<Rect>,
    /// The picker's hit rows as last drawn, for click-to-select
    overlay_hits_area: Option<Rect>,
    /// The index of the first hit row the picker last drew
    overlay_hits_start: usize,
}

impl MessageBoard {
    /// Create a closed message board over a forum
    ///
    /// # Arguments
    ///
    /// * `forum` - The forum to show and write to
    /// * `event_tx` - The app event channel spawned searches report back on
    pub fn new(forum: Arc<ForumState>, event_tx: AsyncSender<AppEvent>) -> Self {
        // start closed and empty until the first forum update arrives
        MessageBoard {
            forum,
            event_tx,
            board_id: Uuid::new_v4(),
            flights: 0,
            view: None,
            open: false,
            focused: false,
            collapsed: HashSet::new(),
            items: Vec::new(),
            lines: Vec::new(),
            item_starts: Vec::new(),
            width: DEFAULT_WIDTH,
            selected: 0,
            want_select: None,
            highlight: None,
            highlight_line: None,
            scroll_y: 0,
            reveal: None,
            mode: BoardMode::Browse,
            notice: None,
            unseen: false,
            leader_pending: false,
            area: None,
            list_area: None,
            chat_overlay: false,
            overlay_area: None,
            overlay_hits_area: None,
            overlay_hits_start: 0,
        }
    }

    /// Show and focus the board, jumping to the latest forum activity
    pub fn show(&mut self) {
        // slide the board in and give it the keyboard
        self.open = true;
        self.focused = true;
        // forget any half-typed Space+m from before
        self.leader_pending = false;
        // open on whatever changed most recently
        self.jump_to_latest();
    }

    /// Hide the board, abandoning any reply or status change in progress
    pub fn hide(&mut self) {
        // slide the board out and release the keyboard
        self.open = false;
        self.focused = false;
        // forget any half-typed Space+m
        self.leader_pending = false;
        // drop any reply or status pick in progress
        self.mode = BoardMode::Browse;
        // closing the board retires any search-match highlight
        self.clear_highlight(None);
        // a hidden board has no picker to click or suppress for
        self.chat_overlay = false;
        self.overlay_area = None;
        self.overlay_hits_area = None;
    }

    /// Take in a fresh view of the forum after it changed
    ///
    /// The selection stays on the same post or comment so the analyst does not
    /// lose their place; a change elsewhere just flags new activity.
    ///
    /// # Arguments
    ///
    /// * `view` - The whole forum as it stands after the change
    /// * `target` - The post or comment that was just created or changed
    pub fn update(&mut self, view: ForumView, target: ParentRef) {
        // swap in the new view and lay it out
        self.view = Some(view);
        self.relayout();
        // flag the change unless it is already selected or about to be
        let selected = self.selected_target();
        if selected.as_ref() != Some(&target) && self.want_select.as_ref() != Some(&target) {
            self.unseen = true;
        }
        // fresh posts can change what a live query matches, so refresh an open
        // search (coalesced: an in-flight search just marks itself dirty)
        self.request_search();
    }

    /// Refresh the board's view with no selection jump target
    ///
    /// Used when an analyst ask or review flag was recorded: the board's data
    /// changes (the analyst tail, in-card flag rows) but no post or comment was
    /// created, so there is nothing to reveal and the selection stays put.
    ///
    /// # Arguments
    ///
    /// * `view` - The whole forum as it stands after the change
    pub fn refresh(&mut self, view: ForumView) {
        // swap in the new view and lay it out, keeping the selection in place
        self.view = Some(view);
        self.relayout();
        // the new analyst record is activity the user has not looked at yet
        self.unseen = true;
    }

    /// Select the most recently created or changed post or comment
    ///
    /// Any folded ancestors are unfolded so it is actually visible.
    pub fn jump_to_latest(&mut self) {
        // looking at the latest activity means nothing is unseen anymore
        self.unseen = false;
        // find the latest activity, if the forum has any yet
        let Some(latest) = self
            .view
            .as_ref()
            .and_then(|view| view.last_updated.clone())
        else {
            return;
        };
        // moving to the latest activity drops a highlight left elsewhere
        self.clear_highlight(Some(&latest));
        // select the latest activity and bring it into view
        self.jump_to(latest);
    }

    /// Select a post or comment, unfolding its ancestors so it is visible
    ///
    /// # Arguments
    ///
    /// * `target` - The post or comment to select and reveal
    fn jump_to(&mut self, target: ParentRef) {
        // unfold everything above it so it shows up in the layout
        if let Some(view) = &self.view {
            for ancestor in ancestors(view, &target) {
                self.collapsed.remove(&ancestor);
            }
        }
        // select it once the layout includes it, then bring it into view
        self.want_select = Some(target);
        self.relayout();
        self.reveal = Some(Reveal::Third);
    }

    /// Whether a point on screen falls inside the board
    ///
    /// # Arguments
    ///
    /// * `x` - The column of the point
    /// * `y` - The row of the point
    pub fn contains(&self, x: u16, y: u16) -> bool {
        // a closed or never-drawn board contains nothing
        self.open && self.area.is_some_and(|area| area_contains(area, x, y))
    }

    /// Handle a mouse click inside the board, focusing it and selecting the item
    ///
    /// # Arguments
    ///
    /// * `x` - The column of the click
    /// * `y` - The row of the click
    pub fn handle_click(&mut self, x: u16, y: u16) {
        // any click on the board gives it the keyboard
        self.focused = true;
        // only clicks on the list select anything
        let Some(list) = self.list_area else {
            return;
        };
        if !area_contains(list, x, y) {
            return;
        }
        // find the item on the clicked line, skipping spacer lines
        let line = self.scroll_y + (y - list.y) as usize;
        if let Some(index) = self.lines.get(line).and_then(|line| line.item) {
            self.selected = index;
            // clicking away from the highlighted item retires its highlight
            let target = self.selected_target();
            self.clear_highlight(target.as_ref());
        }
    }

    /// Scroll the list up without moving the selection
    ///
    /// # Arguments
    ///
    /// * `rows` - How many lines to scroll
    pub fn scroll_up(&mut self, rows: usize) {
        // a manual scroll overrides any pending jump to the selection
        self.reveal = None;
        // scroll up, stopping at the top
        self.scroll_y = self.scroll_y.saturating_sub(rows);
    }

    /// Scroll the list down without moving the selection
    ///
    /// The scroll is clamped to the content on the next render.
    ///
    /// # Arguments
    ///
    /// * `rows` - How many lines to scroll
    pub fn scroll_down(&mut self, rows: usize) {
        // a manual scroll overrides any pending jump to the selection
        self.reveal = None;
        // scroll down; rendering clamps this to the content
        self.scroll_y = self.scroll_y.saturating_add(rows);
    }

    /// Handle a key press while the board is focused
    ///
    /// # Arguments
    ///
    /// * `key` - The key that was pressed
    pub async fn handle_key(&mut self, key: KeyEvent) -> BoardOutcome {
        // any key press replaces a stale notice
        self.notice = None;
        // send the key to whatever the board is in the middle of
        match &self.mode {
            BoardMode::Browse => self.handle_browse_key(key),
            BoardMode::Compose(_) => {
                self.handle_compose_key(key).await;
                BoardOutcome::None
            }
            BoardMode::ComposeTask { .. } => {
                self.handle_task_key(key).await;
                BoardOutcome::None
            }
            BoardMode::StatusPick { .. } => {
                self.handle_status_key(key).await;
                BoardOutcome::None
            }
            BoardMode::Search(_) => {
                self.handle_search_key(key);
                BoardOutcome::None
            }
        }
    }

    /// Handle a key press while browsing
    ///
    /// # Arguments
    ///
    /// * `key` - The key that was pressed
    fn handle_browse_key(&mut self, key: KeyEvent) -> BoardOutcome {
        // Space then m closes the board, mirroring how it was opened; any other
        // key after Space is handled normally
        if std::mem::take(&mut self.leader_pending) && key.code == KeyCode::Char('m') {
            return BoardOutcome::Close;
        }
        match key.code {
            // Space starts the Space+m close shortcut
            KeyCode::Char(' ') => self.leader_pending = true,
            // hand the keyboard back to the chat
            KeyCode::Esc => return BoardOutcome::Unfocus,
            // move to the previous or next item
            KeyCode::Up | KeyCode::Char('k') => self.select(self.selected.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.select(self.selected.saturating_add(1)),
            // move by about a screenful
            KeyCode::PageUp => self.page(false),
            KeyCode::PageDown => self.page(true),
            // jump to the first or last item
            KeyCode::Home | KeyCode::Char('g') => self.select(0),
            KeyCode::End | KeyCode::Char('G') => self.select(usize::MAX),
            // jump to the latest activity
            KeyCode::Char('n') => self.jump_to_latest(),
            // fold or unfold the selected thread
            KeyCode::Enter | KeyCode::Char('o') => self.toggle_fold(),
            KeyCode::Left => self.fold_or_climb(),
            KeyCode::Right => self.unfold(),
            // reply with a statement or a question
            KeyCode::Char('r') => self.start_reply(Kind::Statement),
            KeyCode::Char('?') => self.start_reply(Kind::Question(QuestionStatus::Unsolved)),
            // change the selected question's status
            KeyCode::Char('s') => self.start_status_pick(),
            // add a completion-gate task to the selected question
            KeyCode::Char('t') => self.start_add_task(),
            // search the forum's posts and comments
            KeyCode::Char('/') => self.start_search(),
            // ignore every other key
            _ => (),
        }
        // the tab has nothing further to do
        BoardOutcome::None
    }

    /// Handle a key press while writing a reply
    ///
    /// # Arguments
    ///
    /// * `key` - The key that was pressed
    async fn handle_compose_key(&mut self, key: KeyEvent) {
        // only a reply in progress takes compose keys
        let BoardMode::Compose(compose) = &mut self.mode else {
            return;
        };
        match key.code {
            // abandon the reply
            KeyCode::Esc => self.mode = BoardMode::Browse,
            // cycle how the reply relates to its parent
            KeyCode::Tab => compose.cycle_stance(),
            // everything else edits the text; Enter on a non-empty box sends it
            _ => {
                if compose.text.handle_key(key, false) {
                    self.submit_reply().await;
                }
            }
        }
    }

    /// Insert pasted text into whichever composer is open, if any
    ///
    /// # Arguments
    ///
    /// * `text` - The pasted text to insert
    pub async fn handle_paste(&mut self, text: &str) {
        // the in-progress composers (a reply, a completion-gate task, and a
        // search) hold a focused text box; browsing and status-picking do not
        match &mut self.mode {
            BoardMode::Compose(compose) => compose.text.insert_str(text),
            BoardMode::ComposeTask { text: task, .. } => task.insert_str(text),
            // a pasted query re-searches; Enter stays gated on the results
            // answering the box's current text, so it never jumps on stale hits
            BoardMode::Search(search) => {
                search.query.insert_str(text);
                self.request_search();
            }
            _ => (),
        }
    }

    /// Post the reply being written to the forum
    async fn submit_reply(&mut self) {
        // only a reply in progress can be submitted
        let BoardMode::Compose(compose) = &mut self.mode else {
            return;
        };
        // take the typed text, skipping an empty box
        let Some(body) = compose.text.consume() else {
            return;
        };
        // post it to the forum as the human operator
        let result = self
            .forum
            .add_user_comment(
                compose.parent.clone(),
                body.clone(),
                compose.stance,
                compose.kind,
            )
            .await;
        match result {
            // select the new comment once the forum update brings it in
            Ok(comment) => {
                self.want_select = Some(ParentRef::Comment(comment.id));
                self.notice = Some(format!("Posted !{}", comment.id));
                self.mode = BoardMode::Browse;
            }
            // keep the reply open with its text so nothing typed is lost;
            // set_content also restores the cursor's line/column state
            Err(err) => {
                compose.text.set_content(body);
                self.notice = Some(format!("Could not post: {err}"));
            }
        }
    }

    /// Handle a key press while writing a task for a question
    ///
    /// # Arguments
    ///
    /// * `key` - The key that was pressed
    async fn handle_task_key(&mut self, key: KeyEvent) {
        // only a task in progress takes these keys
        let BoardMode::ComposeTask { text, .. } = &mut self.mode else {
            return;
        };
        match key.code {
            // abandon the task
            KeyCode::Esc => self.mode = BoardMode::Browse,
            // everything else edits the text; Enter on a non-empty box sends it
            _ => {
                if text.handle_key(key, false) {
                    self.submit_task().await;
                }
            }
        }
    }

    /// Attach the task being written to its question on the forum
    async fn submit_task(&mut self) {
        // only a task in progress can be submitted
        let BoardMode::ComposeTask { target, text } = &mut self.mode else {
            return;
        };
        // take the typed text, skipping an empty box
        let Some(body) = text.consume() else {
            return;
        };
        // attach it to the question as the human operator
        let result = self.forum.add_user_task(target.clone(), body.clone()).await;
        match result {
            // report the new task's id and go back to browsing
            Ok(task) => {
                self.notice = Some(format!("Added task {}", task.id));
                self.mode = BoardMode::Browse;
            }
            // keep the task open with its text so nothing typed is lost;
            // set_content also restores the cursor's line/column state
            Err(err) => {
                text.set_content(body);
                // flatten any multi-line error so it fits the one-line footer
                self.notice = Some(format!("Could not add task: {err}").replace('\n', " "));
            }
        }
    }

    /// Handle a key press while picking a question's new status
    ///
    /// # Arguments
    ///
    /// * `key` - The key that was pressed
    async fn handle_status_key(&mut self, key: KeyEvent) {
        // only a status pick in progress takes these keys
        let BoardMode::StatusPick { target } = &self.mode else {
            return;
        };
        // map the number keys onto the statuses, in the order the picker lists them
        let status = match key.code {
            KeyCode::Char('1') => QuestionStatus::Unsolved,
            KeyCode::Char('2') => QuestionStatus::Solved,
            KeyCode::Char('3') => QuestionStatus::NeedsEvidence,
            KeyCode::Char('4') => QuestionStatus::Blocked,
            KeyCode::Esc => {
                self.mode = BoardMode::Browse;
                return;
            }
            // ignore anything else so a stray key does not cancel the pick
            _ => return,
        };
        // close the picker before applying the pick
        let target = target.clone();
        self.mode = BoardMode::Browse;
        // apply the status and report how it went in the footer
        match self.forum.set_question_status(target.clone(), status).await {
            Ok(_) => {
                self.notice = Some(format!("{} → {}", reference(&target), status_label(status)));
            }
            // flatten the multi-line gate refusal so it fits the one-line footer
            Err(err) => {
                self.notice = Some(format!("Could not set status: {err}").replace('\n', " "));
            }
        }
    }

    /// Start searching the forum's posts and comments
    ///
    /// A reply or task draft in progress survives: searching would silently
    /// destroy typed text, so it is refused with a footer notice instead.
    pub fn start_search(&mut self) {
        // never silently drop a reply or task draft the analyst typed
        if matches!(
            self.mode,
            BoardMode::Compose(_) | BoardMode::ComposeTask { .. }
        ) {
            self.notice = Some("Send or Esc the reply first, then search".to_owned());
            return;
        }
        // a new search retires the previous jump's highlight
        self.clear_highlight(None);
        // open a fresh search box, dropping any search already in progress
        self.mode = BoardMode::Search(ForumSearch::new());
    }

    /// Handle a key press while searching the forum
    ///
    /// # Arguments
    ///
    /// * `key` - The key that was pressed
    fn handle_search_key(&mut self, key: KeyEvent) {
        // only a search in progress takes these keys
        let BoardMode::Search(search) = &mut self.mode else {
            return;
        };
        match key.code {
            // abandon the search
            KeyCode::Esc => self.mode = BoardMode::Browse,
            // move through the hits (the query box ignores Up/Down itself)
            KeyCode::Up => search.selected = search.selected.saturating_sub(1),
            KeyCode::Down => {
                search.selected = (search.selected + 1).min(search.hits.len().saturating_sub(1));
            }
            // jump to the selected hit and go back to browsing; Enter with no
            // hits stays in the search so the typed query is never thrown away
            KeyCode::Enter => {
                // the hits must answer the box's current text before Enter may
                // jump: the inline search used to guarantee that by blocking,
                // and the spawned search preserves it by refusing to jump (and
                // showing "searching…") until the live results land
                if search.answered == search.query.content
                    && let Some(target) = search
                        .hits
                        .get(search.selected)
                        .map(|hit| hit.target.clone())
                {
                    // take the terms before the mode swap drops the search
                    let terms = std::mem::take(&mut search.terms);
                    // remember what matched so the jump can highlight it
                    self.highlight = Some((target.clone(), terms));
                    self.mode = BoardMode::Browse;
                    self.jump_to(target);
                }
            }
            // everything else edits the query, re-searching off the event loop
            _ => {
                // only re-search when the key actually changed the query text,
                // so cursor moves and ignored keys never wipe the hit selection
                let before = search.query.content.clone();
                search.query.handle_key(key, false);
                if search.query.content != before {
                    self.request_search();
                }
            }
        }
    }

    /// Request a (re)search of the current query without blocking the UI loop
    ///
    /// Single-flight with rerun-if-dirty coalescing: at most one search task
    /// per board is ever in flight, and requests made meanwhile fold into one
    /// follow-up search when the in-flight one lands, so a burst of keystrokes
    /// costs at most two searches. Results come back as an
    /// [`AppEvent::BoardSearchResults`] applied by [`Self::apply_search_results`].
    fn request_search(&mut self) {
        // only an open search can be (re)searched
        let BoardMode::Search(search) = &mut self.mode else {
            return;
        };
        // an empty query shows no hits rather than everything, and needs no
        // task: the empty display answers the empty query exactly
        if search.query.content.trim().is_empty() {
            search.hits.clear();
            search.terms.clear();
            search.selected = 0;
            search.answered = search.query.content.clone();
            search.dirty = false;
            return;
        }
        // a search is already in flight: fold this request into its landing
        if search.inflight.is_some() {
            search.dirty = true;
            return;
        }
        // latch the flight number and spawn its search off the event loop
        let query = search.query.content.clone();
        self.flights += 1;
        let flight = self.flights;
        search.inflight = Some(flight);
        search.dirty = false;
        let board = self.board_id;
        let forum = self.forum.clone();
        let event_tx = self.event_tx.clone();
        tokio::spawn(async move {
            // rank off the event loop so a slow search never freezes typing
            let hits = forum.search(&query, BOARD_SEARCH_LIMIT).await;
            // derive the highlight terms the same way the ranker scores with
            let terms = search_terms(&query);
            // the TUI may be shutting down, so a failed send is dropped quietly
            let _ = event_tx
                .send(AppEvent::BoardSearchResults {
                    board,
                    flight,
                    query,
                    hits,
                    terms,
                })
                .await;
        });
    }

    /// Take in finished search results, applying them only if still current
    ///
    /// Latest-wins: only this board's own latest flight may land, and its hits
    /// are shown only when they answer the box's exact current text; anything
    /// else (another board's flight, a flight this search already moved past,
    /// an Esc'd search) is dropped untouched. When the flight lands on a query
    /// the box has moved past -- or the forum changed while it flew -- one
    /// follow-up search is spawned for the current text.
    ///
    /// # Arguments
    ///
    /// * `board` - The board that spawned the search
    /// * `flight` - The spawning board's flight number for the search
    /// * `query` - The exact search-box text the hits were ranked against
    /// * `hits` - The ranked hits, best first
    /// * `terms` - The query's terms, for highlighting what the hits matched
    pub fn apply_search_results(
        &mut self,
        board: Uuid,
        flight: u64,
        query: &str,
        hits: Vec<SearchHit>,
        terms: Vec<String>,
    ) {
        // only this board's own latest flight may touch its search; matching
        // by identity (not query text) keeps a same-text result from another
        // board or a superseded flight from spoofing the single-flight latch
        // or overwriting fresher hits with staler ones
        if board != self.board_id {
            return;
        }
        // decide inside the mode borrow, then re-request on self afterwards
        let rerun = {
            // the search may have been Esc'd or jumped away from: drop it all
            let BoardMode::Search(search) = &mut self.mode else {
                return;
            };
            // a flight the search already moved past can no longer land
            if search.inflight != Some(flight) {
                return;
            }
            search.inflight = None;
            // latest-wins: only hits answering the current text are ever shown
            if search.query.content == query {
                // a same-query refresh (the forum changed) keeps the analyst's
                // picked hit even when the fresh ranking reorders it; a new
                // query's first results select the best hit the way the old
                // inline search did on every keystroke
                if search.answered == query {
                    let picked = search
                        .hits
                        .get(search.selected)
                        .map(|hit| hit.target.clone());
                    search.selected = picked
                        .and_then(|target| hits.iter().position(|hit| hit.target == target))
                        .unwrap_or(0);
                } else {
                    search.selected = 0;
                }
                search.hits = hits;
                search.terms = terms;
                search.answered = query.to_owned();
            }
            // rerun when the text moved on or the forum changed mid-flight
            search.query.content != query || std::mem::take(&mut search.dirty)
        };
        // request_search also handles a query that emptied in the meantime
        if rerun {
            self.request_search();
        }
    }

    /// Start writing a reply to the selected post or comment
    ///
    /// # Arguments
    ///
    /// * `kind` - Whether the reply states something or asks a question
    fn start_reply(&mut self, kind: Kind) {
        // open a reply box aimed at the selection, if anything is selected
        if let Some(target) = self.selected_target() {
            self.mode = BoardMode::Compose(Compose::new(target, kind));
        }
    }

    /// Start picking a new status for the selected question
    fn start_status_pick(&mut self) {
        // nothing to change when nothing is selected
        let Some(item) = self.items.get(self.selected) else {
            return;
        };
        // only questions carry a status
        if matches!(item.kind, Kind::Question(_)) {
            self.mode = BoardMode::StatusPick {
                target: item.target.clone(),
            };
        } else {
            self.notice = Some(format!("{} is not a question", reference(&item.target)));
        }
    }

    /// Start writing a completion-gate task for the selected question
    fn start_add_task(&mut self) {
        // nothing to gate when nothing is selected
        let Some(item) = self.items.get(self.selected) else {
            return;
        };
        // only questions carry completion-gate tasks
        if matches!(item.kind, Kind::Question(_)) {
            // open a focused text box titled with the question being gated
            self.mode = BoardMode::ComposeTask {
                target: item.target.clone(),
                text: TextBox::default()
                    .focused()
                    .title(format!("Add task to {}", reference(&item.target))),
            };
        } else {
            self.notice = Some(format!("{} is not a question", reference(&item.target)));
        }
    }

    /// The post or comment that is currently selected, if any
    fn selected_target(&self) -> Option<ParentRef> {
        // the selection may be out of range while the forum is empty
        self.items
            .get(self.selected)
            .map(|item| item.target.clone())
    }

    /// Clear the search-match highlight unless it sits on a kept target
    ///
    /// # Arguments
    ///
    /// * `keep` - The target whose highlight should survive, if any
    fn clear_highlight(&mut self, keep: Option<&ParentRef>) {
        // keep the highlight when it still points at the kept target
        if let Some((target, _)) = &self.highlight
            && keep == Some(target)
        {
            return;
        }
        // drop the highlight and its scroll anchor
        self.highlight = None;
        self.highlight_line = None;
    }

    /// Select an item by index, clamped to the items that exist
    ///
    /// # Arguments
    ///
    /// * `index` - The index of the item to select
    fn select(&mut self, index: usize) {
        // clamp to the last item so overshooting lands on the end
        self.selected = index.min(self.items.len().saturating_sub(1));
        // moving off the highlighted item retires its highlight
        let target = self.selected_target();
        self.clear_highlight(target.as_ref());
        // scroll the new selection into view on the next render
        self.reveal = Some(Reveal::Visible);
    }

    /// Move the selection by about a page of lines
    ///
    /// # Arguments
    ///
    /// * `down` - Whether to page down rather than up
    fn page(&mut self, down: bool) {
        // move by the list's height, or a sensible default before the first render
        let height = self
            .list_area
            .map_or(20, |area| area.height as usize)
            .max(1);
        // aim a screenful above or below the selection's first line
        let current = self.item_starts.get(self.selected).copied().unwrap_or(0);
        let goal = if down {
            current.saturating_add(height)
        } else {
            current.saturating_sub(height)
        };
        // select the last item starting at or before the goal line
        let index = self.item_starts.partition_point(|start| *start <= goal);
        self.select(index.saturating_sub(1));
    }

    /// Fold or unfold the selected item's replies
    fn toggle_fold(&mut self) {
        // nothing to fold when nothing is selected
        let Some(item) = self.items.get(self.selected) else {
            return;
        };
        if self.collapsed.contains(&item.target) {
            // a folded item opens back up
            self.unfold();
        } else if item.has_replies {
            // an open item with replies folds them away
            self.collapsed.insert(item.target.clone());
            self.relayout();
        }
    }

    /// Fold the selected item, or move to its parent if it is already folded
    fn fold_or_climb(&mut self) {
        // nothing to fold when nothing is selected
        let Some(item) = self.items.get(self.selected) else {
            return;
        };
        // fold an open item that has replies
        if item.has_replies && !self.collapsed.contains(&item.target) {
            self.collapsed.insert(item.target.clone());
            self.relayout();
            return;
        }
        // climb to the closest earlier item one level up
        let depth = item.depth;
        if depth > 0
            && let Some(parent) = self.items[..self.selected]
                .iter()
                .rposition(|other| other.depth < depth)
        {
            self.select(parent);
        }
    }

    /// Unfold the selected item's replies
    fn unfold(&mut self) {
        // nothing to unfold when nothing is selected
        let Some(item) = self.items.get(self.selected) else {
            return;
        };
        // only re-lay out if the item was actually folded
        if self.collapsed.remove(&item.target) {
            self.relayout();
        }
    }

    /// Rebuild the laid out items and lines from the view
    ///
    /// The selection follows the post or comment it was on (or a pending one to
    /// select) rather than its index, so updates never yank the analyst around.
    fn relayout(&mut self) {
        // remember what was selected before the items shift
        let previous = self.selected_target();
        // there is nothing to lay out before the first forum update
        let Some(view) = &self.view else {
            return;
        };
        // lay the forum out, marking the latest activity
        let latest = view.last_updated.clone();
        let (items, lines) = layout(view, &self.collapsed, self.width, latest.as_ref());
        self.items = items;
        self.lines = lines;
        // index the first line of each item for scrolling and paging
        self.item_starts = vec![0; self.items.len()];
        for (index, line) in self.lines.iter().enumerate().rev() {
            if let Some(item) = line.item {
                self.item_starts[item] = index;
            }
        }
        // settle a pending selection once it has arrived
        let position =
            |target: &ParentRef| self.items.iter().position(|item| &item.target == target);
        if let Some(index) = self.want_select.as_ref().and_then(position) {
            self.selected = index;
            self.want_select = None;
            self.reveal = Some(Reveal::Third);
        } else if let Some(index) = previous.as_ref().and_then(position) {
            // otherwise stay on whatever was selected before the items shifted
            self.selected = index;
        }
        // keep the selection in range if items disappeared
        self.selected = self.selected.min(self.items.len().saturating_sub(1));
        // re-anchor the highlight on its first matched line, since any
        // relayout (updates, resizes) can shift the laid out lines
        self.highlight_line = self.find_highlight_line();
    }

    /// Find the first laid-out line of the highlighted item with a match
    fn find_highlight_line(&self) -> Option<usize> {
        // nothing to anchor without a highlight
        let (target, terms) = self.highlight.as_ref()?;
        // the highlighted item may have left the layout entirely
        let item = self.items.iter().position(|item| &item.target == target)?;
        // the item's lines run from its first line to the next item's
        let start = self.item_starts.get(item).copied()?;
        let end = self
            .item_starts
            .get(item + 1)
            .copied()
            .unwrap_or(self.lines.len());
        // find the first content line of the box with a match in it
        self.lines[start..end]
            .iter()
            .position(|line| match &line.row {
                // match against the row's text the same way the painter does
                RowKind::Content(content) => {
                    let joined: String = content.iter().map(|span| span.content.as_ref()).collect();
                    !match_ranges(&joined, terms).is_empty()
                }
                // borders and spacers carry no matchable text
                _ => false,
            })
            .map(|index| start + index)
    }

    /// Clamp the scroll offset, bringing the selection into view if requested
    ///
    /// # Arguments
    ///
    /// * `height` - The number of lines the list can show
    fn settle_scroll(&mut self, height: usize) {
        if let Some(reveal) = self.reveal.take() {
            // the selected item's first and last lines
            let start = self.item_starts.get(self.selected).copied().unwrap_or(0);
            let end = self
                .item_starts
                .get(self.selected + 1)
                .copied()
                .unwrap_or(self.lines.len())
                .saturating_sub(1)
                .max(start);
            match reveal {
                // park the item about a third of the way down, anchoring on
                // its first matched line when a search highlight sits on it
                Reveal::Third => {
                    let anchor = self
                        .highlight_line
                        .filter(|line| (start..=end).contains(line))
                        .unwrap_or(start);
                    self.scroll_y = anchor.saturating_sub(height / 3);
                }
                Reveal::Visible => {
                    // scroll up to an item above the screen
                    if start < self.scroll_y {
                        self.scroll_y = start;
                    } else if end >= self.scroll_y + height {
                        // show as much of the item as fits, header first
                        self.scroll_y = (end + 1).saturating_sub(height).min(start);
                    }
                }
            }
        }
        // never scroll past the last screenful
        self.scroll_y = self.scroll_y.min(self.lines.len().saturating_sub(height));
    }

    /// Render the board into an area
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render to
    /// * `area` - The area to render the board in
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        // remember where the board is so clicks can find it
        self.area = Some(area);
        // forget the picker's rects; the overlay render re-records them later
        // this same frame, so a skipped picker never leaves stale click targets
        self.overlay_area = None;
        self.overlay_hits_area = None;
        // wipe whatever the page drew underneath
        frame.render_widget(Clear, area);
        // the title flags activity the analyst has not looked at yet
        let mut title = vec![Span::styled(
            " Message Board ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )];
        if self.unseen {
            title.push(Span::styled(
                "● new activity — n ",
                Style::default().fg(Color::Yellow),
            ));
        }
        // highlight the border while the board has focus
        let border = if self.focused {
            Color::Cyan
        } else {
            Color::DarkGray
        };
        // draw the board's frame and work inside it
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border))
            .title(Line::from(title));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        // split the inside into the list, an optional input box, and a footer;
        // a search whose picker draws on the chat side keeps its box there
        let compose_height = match self.mode {
            BoardMode::Compose(_) | BoardMode::ComposeTask { .. } => COMPOSE_HEIGHT,
            BoardMode::Search(_) if !self.chat_overlay => SEARCH_HEIGHT,
            _ => 0,
        };
        let [list, compose, footer] = Layout::vertical([
            Constraint::Min(0),
            Constraint::Length(compose_height),
            Constraint::Length(1),
        ])
        .areas(inner);
        // remember where the list is so clicks can pick items
        self.list_area = Some(list);
        // re-lay out when the width changed, e.g. on a terminal resize
        let width = list.width as usize;
        if width > 0 && width != self.width {
            self.width = width;
            self.relayout();
        }
        // draw the forum itself
        self.render_list(frame, list);
        // draw the reply, task, or search box being typed into
        let board_focused = self.focused;
        let overlay_active = self.chat_overlay;
        match &mut self.mode {
            BoardMode::Compose(compose_state) => compose_state.text.render(frame, compose),
            BoardMode::ComposeTask { text, .. } => text.render(frame, compose),
            // the chat-side picker renders the query box when it is active
            BoardMode::Search(search) if !overlay_active => {
                // falling back into the board brings the box's title back,
                // since no outer picker frame names the search here
                if search.query.title.is_none() {
                    search.query.title = Some("Search forum".to_owned());
                }
                // the terminal cursor belongs to whoever has the keyboard, so
                // an unfocused board must not steal it from the page's prompt
                search.query.is_focused = board_focused;
                search.query.render(frame, compose);
            }
            _ => (),
        }
        // draw the key hints or the latest notice
        self.render_footer(frame, footer);
        // draw the status picker over the list
        if matches!(self.mode, BoardMode::StatusPick { .. }) {
            render_status_picker(frame, list);
        }
        // draw the search hits over the list unless the picker has them
        if let BoardMode::Search(search) = &self.mode
            && !self.chat_overlay
        {
            render_search_hits(frame, list, search);
        }
    }

    /// Render the visible slice of the forum
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render to
    /// * `area` - The area to render the list in
    fn render_list(&mut self, frame: &mut Frame, area: Rect) {
        // say so plainly when there is nothing to show yet
        if self.lines.is_empty() {
            let empty =
                Paragraph::new("(no posts yet)").style(Style::default().fg(Color::DarkGray));
            frame.render_widget(empty, area);
            return;
        }
        // settle the scroll for this many visible lines
        let height = area.height as usize;
        self.settle_scroll(height);
        // outline the selected box in blue, dimmer when the board is not focused
        let selected_border = if self.focused {
            SELECTED_BORDER
        } else {
            SELECTED_BORDER_UNFOCUSED
        };
        // resolve the jumped-to hit's item and terms to paint this frame
        let highlight = self.highlight.as_ref().and_then(|(target, terms)| {
            self.items
                .iter()
                .position(|item| &item.target == target)
                .map(|index| (index, terms.as_slice()))
        });
        // build only the lines that fit on screen
        let end = (self.scroll_y + height).min(self.lines.len());
        let visible: Vec<Line> = self.lines[self.scroll_y..end]
            .iter()
            .map(|line| {
                // outline the selected box in blue, every other box in grey
                let border = if line.item == Some(self.selected) {
                    selected_border
                } else {
                    CARD_BORDER
                };
                // paint the search matches over the jumped-to box's text
                match highlight {
                    Some((index, terms)) if line.item == Some(index) => {
                        line.to_line_highlighted(border, terms)
                    }
                    _ => line.to_line(border),
                }
            })
            .collect();
        // draw the visible lines
        frame.render_widget(Paragraph::new(visible), area);
    }

    /// Render the footer with the current notice or key hints
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render to
    /// * `area` - The area to render the footer in
    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        // the full browse hints only fit a wide board
        let browse = if area.width >= 75 {
            "j/k move ⏎ fold r reply ? ask s status t task / search n latest esc chat"
        } else {
            "j/k ⏎ fold r reply ? ask s status t task / search"
        };
        // a notice wins over the hints until the next key press
        let (text, color) = match (&self.notice, &self.mode) {
            (Some(notice), _) => (notice.as_str(), Color::Yellow),
            (None, _) if self.leader_pending => ("m close board", Color::Yellow),
            (None, BoardMode::Browse) => (browse, Color::DarkGray),
            (None, BoardMode::Compose(_)) => (
                "⏎ send  ^J newline  tab stance  esc cancel",
                Color::DarkGray,
            ),
            (None, BoardMode::ComposeTask { .. }) => ("⏎ add task  esc cancel", Color::DarkGray),
            (None, BoardMode::StatusPick { .. }) => {
                ("1-4 pick status  esc cancel", Color::DarkGray)
            }
            (None, BoardMode::Search(_)) => (
                "type to search  ↑/↓ pick  ⏎ jump  esc cancel",
                Color::DarkGray,
            ),
        };
        // draw the chosen text on one line
        let footer = Paragraph::new(text).style(Style::default().fg(color));
        frame.render_widget(footer, area);
    }

    /// The chat-side rect the search picker would draw into, if it draws
    ///
    /// One predicate decides both whether the picker renders and where, so
    /// the tab's board-side suppression flag and the actual draw can never
    /// disagree. `None` means the search UI stays in the board instead.
    ///
    /// # Arguments
    ///
    /// * `anchor` - The chat-log area to anchor the picker over
    pub fn search_overlay_area(&self, anchor: Rect) -> Option<Rect> {
        // only a focused, open board that is searching shows the picker
        if !self.open || !self.focused || !matches!(self.mode, BoardMode::Search(_)) {
            return None;
        }
        // skip anchors too small to host a useful picker
        if anchor.width < MIN_OVERLAY_WIDTH || anchor.height < MIN_OVERLAY_HEIGHT {
            return None;
        }
        // leave the bottom of the chat log visible, shrinking the gap before
        // the picker ever drops below its minimum height
        let height = anchor
            .height
            .saturating_sub(OVERLAY_LOG_GAP)
            .max(MIN_OVERLAY_HEIGHT);
        Some(Rect::new(anchor.x, anchor.y, anchor.width, height))
    }

    /// Render the search picker over the chat side, helix style
    ///
    /// The picker owns the whole search UI while it is up: the query box and
    /// the ranked hits fill its left half, and a live preview of the selected
    /// hit fills its right half, with the matched text highlighted in both.
    /// It draws for any focused search — even a hitless one, so the query the
    /// analyst is typing is always visible — and leaves the bottom rows of
    /// the chat log readable beneath it.
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render to
    /// * `anchor` - The chat-log area to anchor the picker over
    pub fn render_search_overlay(&mut self, frame: &mut Frame, anchor: Rect) {
        // settle the picker's rect, bailing when it should not draw
        let Some(overlay) = self.search_overlay_area(anchor) else {
            return;
        };
        // remember where the picker is so clicks can find it
        self.overlay_area = Some(overlay);
        // capture the focus and view before borrowing the search mode
        let focused = self.focused;
        let view = self.view.as_ref();
        let BoardMode::Search(search) = &mut self.mode else {
            return;
        };
        // title the frame with the hit count once a query has been typed
        let title = if search.query.content.trim().is_empty() {
            " Search forum ".to_owned()
        } else if search.answered != search.query.content {
            // the hits below answer an older text until the live search lands
            " Search forum — searching… ".to_owned()
        } else {
            format!(" Search forum — {} hits ", search.hits.len())
        };
        // frame the picker on the shared popup grey over the chat log
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Gray))
            .style(Style::default().bg(Color::Rgb(60, 60, 60)));
        let inner = block.inner(overlay);
        // clear what is underneath, then draw the frame
        frame.render_widget(Clear, overlay);
        frame.render_widget(block, overlay);
        // split the hits from the preview when the picker is wide enough
        let (list, preview) = if inner.width >= OVERLAY_SPLIT_MIN_WIDTH {
            let [list, preview] =
                Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .areas(inner);
            (list, Some(preview))
        } else {
            (inner, None)
        };
        // the query box sits above the hits it ranks
        let [query, hits] =
            Layout::vertical([Constraint::Length(SEARCH_HEIGHT), Constraint::Min(0)]).areas(list);
        // the outer frame already names the search, so the box drops its
        // title; the terminal cursor still belongs to whoever has the keyboard
        search.query.title = None;
        search.query.is_focused = focused;
        search.query.render(frame, query);
        // list the hits windowed around the selection
        let (lines, start) = search_hit_lines(search, hits.height as usize);
        frame.render_widget(Paragraph::new(lines), hits);
        // remember the hit rows so clicks can pick one
        self.overlay_hits_area = Some(hits);
        self.overlay_hits_start = start;
        // draw the preview of the selected hit beside the hits
        if let Some(preview) = preview {
            render_overlay_preview(frame, preview, view, search);
        }
    }

    /// Whether a point on screen falls inside the search picker
    ///
    /// Checks the live search state besides the recorded rect, so a click
    /// processed between a state change and the next redraw can never hit a
    /// ghost picker.
    ///
    /// # Arguments
    ///
    /// * `x` - The column of the point
    /// * `y` - The row of the point
    pub fn overlay_contains(&self, x: u16, y: u16) -> bool {
        // the picker only exists for a focused, open board that is searching
        if !self.open || !self.focused || !matches!(self.mode, BoardMode::Search(_)) {
            return false;
        }
        // check the rect the picker was last drawn into
        self.overlay_area
            .is_some_and(|area| area_contains(area, x, y))
    }

    /// Handle a mouse click inside the search picker
    ///
    /// A click on a hit row selects that hit; clicks on the query box, the
    /// preview, or the frame just keep the picker alive.
    ///
    /// # Arguments
    ///
    /// * `x` - The column of the click
    /// * `y` - The row of the click
    pub fn handle_overlay_click(&mut self, x: u16, y: u16) {
        // any click on the picker keeps the board's keyboard
        self.focused = true;
        // only clicks on the hit rows select anything
        let Some(hits) = self.overlay_hits_area else {
            return;
        };
        if !area_contains(hits, x, y) {
            return;
        }
        // map the clicked row to a hit, ignoring rows below the last one
        let index = self.overlay_hits_start + (y - hits.y) as usize;
        if let BoardMode::Search(search) = &mut self.mode
            && index < search.hits.len()
        {
            search.selected = index;
        }
    }
}

/// Draw the question status picker centered over an area
///
/// # Arguments
///
/// * `frame` - The frame to render to
/// * `area` - The area to center the picker in
fn render_status_picker(frame: &mut Frame, area: Rect) {
    // list the statuses in the order their number keys pick them
    let options = [
        QuestionStatus::Unsolved,
        QuestionStatus::Solved,
        QuestionStatus::NeedsEvidence,
        QuestionStatus::Blocked,
    ];
    // label each status with its number key and its color
    let lines: Vec<Line> = options
        .iter()
        .enumerate()
        .map(|(index, status)| {
            Line::from(vec![
                Span::styled(
                    format!(" {} ", index + 1),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(status_label(*status), status_style(*status)),
            ])
        })
        .collect();
    // center a small box, shrinking it if the list is tiny
    let width = 22.min(area.width);
    let height = (lines.len() as u16 + 2).min(area.height);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    // frame the picker on a grey background over the list
    let block = Block::default()
        .title(" Set status ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Gray))
        .style(Style::default().bg(Color::Rgb(60, 60, 60)));
    // clear what is underneath, then draw the picker
    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

/// Build the hit list's lines, windowed around the selection
///
/// Each hit renders as `reference title — snippet` with the query's terms
/// marked; the selected hit's whole line is reversed. When the search has no
/// hits yet, a single muted state line says why. Returns the lines and the
/// index of the first hit shown, so a click on a row can find its hit.
///
/// # Arguments
///
/// * `search` - The search whose hits to list
/// * `rows` - The number of lines the list may use
fn search_hit_lines(search: &ForumSearch, rows: usize) -> (Vec<Line<'static>>, usize) {
    // show as many hits as fit, keeping one row for the empty-state line
    let shown = search.hits.len().clamp(1, rows.max(1));
    // window the hits around the selection when they overflow the list
    let start = search
        .selected
        .saturating_sub(shown / 2)
        .min(search.hits.len().saturating_sub(shown));
    // render each visible hit as `reference title — snippet`
    let mut lines: Vec<Line> = search.hits[start..(start + shown).min(search.hits.len())]
        .iter()
        .enumerate()
        .map(|(offset, hit)| {
            // color a question hit's reference by its status, statements cyan
            let reference_style = match hit.kind {
                Kind::Question(status) => status_style(status),
                Kind::Statement => Style::default().fg(Color::Cyan),
            };
            // mark the matched terms in the title and snippet, leaving the
            // reference span its clean status color
            let mut tail = vec![
                Span::raw(hit.title.clone()),
                Span::styled(
                    format!(" — {}", hit.snippet),
                    Style::default().fg(MUTED_COLOR),
                ),
            ];
            highlight_row(&mut tail, &search.terms, SEARCH_MATCH_FLAT);
            // lead with the hit's reference, then the highlighted tail
            let mut spans = vec![Span::styled(
                format!(" {} ", hit.reference),
                reference_style.add_modifier(Modifier::BOLD),
            )];
            spans.extend(tail);
            let mut line = Line::from(spans);
            // highlight the selected hit so Enter's target is obvious
            if start + offset == search.selected {
                line = line.style(Style::default().add_modifier(Modifier::REVERSED));
            }
            line
        })
        .collect();
    // say so plainly when there is nothing to show yet
    if search.hits.is_empty() {
        let empty = if search.query.content.trim().is_empty() {
            " (type to search)"
        } else if search.answered != search.query.content {
            // results for this text have not landed, so "no matches" would lie
            " (searching…)"
        } else {
            " (no matches)"
        };
        lines.push(Line::from(Span::styled(
            empty,
            Style::default().fg(MUTED_COLOR),
        )));
    }
    (lines, start)
}

/// Build the styled header spans naming a hit: reference, title, and author
///
/// # Arguments
///
/// * `hit` - The hit to build a header for
fn hit_header_spans(hit: &SearchHit) -> Vec<Span<'static>> {
    // color the reference by status for questions, cyan for statements
    let reference_style = match hit.kind {
        Kind::Question(status) => status_style(status),
        Kind::Statement => Style::default().fg(Color::Cyan),
    };
    // name the hit by its reference, title, and author
    let mut spans = vec![
        Span::styled(
            format!(" {} ", hit.reference),
            reference_style.add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("{} ", hit.title)),
        Span::styled(format!("· {} ", hit.author), author_style(&hit.author)),
    ];
    // a comment hit also shows its stance toward its parent
    if let Some(stance) = hit.stance {
        spans.push(Span::styled(
            format!("· {} ", stance_label(stance)),
            stance_style(stance),
        ));
    }
    spans
}

/// Build the preview rows of a hit's full content, matches painted
///
/// Falls back to the hit's one-line snippet when the forum view no longer
/// holds the hit (a racing update) or when its body is empty (a title-only
/// post), so the preview always has something true to show.
///
/// # Arguments
///
/// * `view` - The forum view to pull the hit's full body from
/// * `hit` - The hit to preview
/// * `text_width` - The number of columns the rows may use
/// * `terms` - The lowercased query terms to highlight
fn preview_body_rows(
    view: Option<&ForumView>,
    hit: &SearchHit,
    text_width: usize,
    terms: &[String],
) -> (Vec<Vec<Span<'static>>>, Option<usize>) {
    // pull the hit's full body out of the forum view, falling back to the
    // hit's snippet when a racing update has already dropped the node
    let body = view
        .and_then(|view| find_node(view, &hit.target))
        .map(|node| match node {
            NodeRef::Post(post) => post.body.as_str(),
            NodeRef::Comment(comment) => comment.body.as_str(),
        })
        .unwrap_or(&hit.snippet);
    // a body-less hit (e.g. a title-only post) previews its snippet line
    let body = if body.trim().is_empty() {
        &hit.snippet
    } else {
        body
    };
    // render the body as markdown rows sized to the pane's text width
    let mut rows = markdown_rows(body, text_width, Style::default().fg(TEXT_COLOR));
    // paint the matched terms, remembering the first match for the window
    let first = highlight_rows(&mut rows, terms, SEARCH_MATCH);
    (rows, first)
}

/// The first preview row to show, parking the first match a third down
///
/// # Arguments
///
/// * `first` - The index of the first matched row, if any matched
/// * `len` - The number of rows built
/// * `view_height` - The number of rows the pane can show
fn preview_window_start(first: Option<usize>, len: usize, view_height: usize) -> usize {
    // aim the first match about a third of the way down the pane
    first
        .unwrap_or(0)
        .saturating_sub(view_height / 3)
        .min(len.saturating_sub(view_height))
}

/// Draw the search picker's preview pane: the selected hit's full content
///
/// # Arguments
///
/// * `frame` - The frame to render to
/// * `pane` - The pane to draw the preview into, divider included
/// * `view` - The forum view to pull the hit's full body from
/// * `search` - The search whose selected hit to preview
fn render_overlay_preview(
    frame: &mut Frame,
    pane: Rect,
    view: Option<&ForumView>,
    search: &ForumSearch,
) {
    // a vertical divider separates the preview from the hits
    let divider = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(Color::Gray));
    let inner = divider.inner(pane);
    frame.render_widget(divider, pane);
    // an empty search leaves the pane blank behind the divider
    let Some(hit) = search.hits.get(search.selected) else {
        return;
    };
    // lead with the hit's header, since a left-only border carries no title
    let mut lines = vec![Line::from(hit_header_spans(hit)), Line::default()];
    // build the body rows with the matched terms painted
    let text_width = (inner.width as usize).saturating_sub(2);
    let (rows, first) = preview_body_rows(view, hit, text_width, &search.terms);
    // window the rows so the first matched row sits about a third down
    let body_height = (inner.height as usize).saturating_sub(lines.len()).max(1);
    let start = preview_window_start(first, rows.len(), body_height);
    // pad each visible row a column off the divider like the cards do
    lines.extend(
        rows[start..(start + body_height).min(rows.len())]
            .iter()
            .map(|row| {
                let mut spans = vec![Span::raw(" ")];
                spans.extend(row.iter().cloned());
                Line::from(spans)
            }),
    );
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Draw the forum search hits as a picker over an area
///
/// This is the narrow-terminal fallback: when the chat side has no room for
/// the split picker, the hits pop up over the board's own list instead.
///
/// # Arguments
///
/// * `frame` - The frame to render to
/// * `area` - The list area to overlay the hits on
/// * `search` - The search whose hits to draw
fn render_search_hits(frame: &mut Frame, area: Rect, search: &ForumSearch) {
    // the picker spans nearly the full list width, centered
    let width = area.width.saturating_sub(2).max(area.width.min(12));
    // size the picker to its hits (one line each) plus the borders, and keep
    // at least one content row for the empty-state line
    let rows = (area.height.saturating_sub(2)).max(1) as usize;
    let shown = search.hits.len().clamp(1, rows);
    let height = (shown as u16 + 2).min(area.height);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    // build the hit lines windowed around the selection
    let (lines, _) = search_hit_lines(search, rows);
    // title the popup with the search's progress, mirroring the picker, so a
    // pending search is visible even while older hits are still on screen
    let title = if search.query.content.trim().is_empty() {
        " Search results ".to_owned()
    } else if search.answered != search.query.content {
        // the hits below answer an older text until the live search lands
        " Search results — searching… ".to_owned()
    } else {
        format!(" Search results — {} hits ", search.hits.len())
    };
    // frame the hits on a grey background over the list
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Gray))
        .style(Style::default().bg(Color::Rgb(60, 60, 60)));
    // clear what is underneath, then draw the hits
    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}

/// Lay the forum out as a tree of items and styled lines
///
/// # Arguments
///
/// * `view` - The forum to lay out
/// * `collapsed` - The posts and comments whose replies are folded away
/// * `width` - The number of columns lines may use
/// * `latest` - The most recently changed post or comment, marked with a dot
fn layout(
    view: &ForumView,
    collapsed: &HashSet<ParentRef>,
    width: usize,
    latest: Option<&ParentRef>,
) -> (Vec<BoardItem>, Vec<BoardLine>) {
    // walk the forum tree in display order, skipping folded replies
    let entries = flatten(view, collapsed);
    // for each entry in the capped column, whether that column carries on below it
    let column_continues = capped_column_continues(&entries);
    // group the anchored review flags by the post/comment they point at, so
    // each card can list its own flags under its checklist
    let mut anchored: HashMap<&ParentRef, Vec<&ReviewFlag>> = HashMap::new();
    for flag in &view.flags {
        if let Some(target) = &flag.reference {
            anchored.entry(target).or_default().push(flag);
        }
    }
    // preallocate room for every item and a few lines each
    let mut items = Vec::with_capacity(entries.len());
    let mut lines = Vec::with_capacity(entries.len() * 3);
    for (index, entry) in entries.iter().enumerate() {
        let target = entry.node.target();
        // build the tree art beside this item's box
        let (head_prefix, body_prefix) = tree_prefixes(
            entry.depth,
            entry.is_last,
            &entry.guides,
            column_continues[index],
        );
        // separate each post's thread from the one before it
        if entry.depth == 0 && index > 0 {
            lines.push(BoardLine::spacer());
        }
        // the box fills the rest of the line after its tree art
        let gutter_width = UnicodeWidthStr::width(head_prefix.as_str());
        let card_width = width.saturating_sub(gutter_width).max(MIN_CARD_WIDTH);
        // leave room for the borders and a space of padding on each side
        let text_width = card_width - 4;
        // start the box with its header, dotted if it is the latest activity
        let marked = latest == Some(&target);
        let mut rows = wrap_segments(&header_segments(entry, marked), text_width);
        // a post's title stands out under its header, followed by its author
        if let NodeRef::Post(post) = entry.node {
            let title = Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD);
            rows.extend(text_rows(&post.title, title, text_width));
            // a post's author gets its own row so the header wraps less
            let author = (format!("by {}", post.author), author_style(&post.author));
            rows.extend(wrap_segments(&[author], text_width));
        }
        // finish the box with the markdown-rendered body
        let body = match entry.node {
            NodeRef::Post(post) => &post.body,
            NodeRef::Comment(comment) => &comment.body,
        };
        rows.extend(markdown_rows(
            body,
            text_width,
            Style::default().fg(TEXT_COLOR),
        ));
        // list the completion-gate tasks under the body as a checklist
        rows.extend(task_rows(entry.tasks, text_width));
        // list the review flags anchored to this post or comment, if any
        if let Some(flags) = anchored.get(&target) {
            rows.extend(flag_rows(flags, text_width));
        }
        // outline the rows into a box hung off the tree art
        push_card(
            &mut lines,
            index,
            &head_prefix,
            &body_prefix,
            card_width,
            rows,
        );
        // record the item so the board can select and fold it
        items.push(BoardItem {
            target,
            depth: entry.depth,
            kind: entry.node.kind(),
            head_prefix,
            body_prefix,
            has_replies: entry.has_replies,
            hidden: entry.hidden,
        });
    }
    // close with the analyst-attention tail: every recorded ask plus the
    // review flags whose anchor is not currently on screen (unanchored flags,
    // and flags anchored inside a folded thread) — visibly anchored ones
    // render in their card above instead
    let visible: HashSet<ParentRef> = entries.iter().map(|entry| entry.node.target()).collect();
    let unanchored: Vec<&ReviewFlag> = view
        .flags
        .iter()
        .filter(|flag| match &flag.reference {
            None => true,
            Some(target) => !visible.contains(target),
        })
        .collect();
    if !view.asks.is_empty() || !unanchored.is_empty() {
        // the tail card spans the full width with no tree art beside it
        let card_width = width.max(MIN_CARD_WIDTH);
        let text_width = card_width - 4;
        // open with a bold section header inside the box
        let header = (
            "Analyst asks & review flags".to_owned(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
        let mut rows = wrap_segments(&[header], text_width);
        rows.extend(ask_rows(&view.asks, text_width));
        rows.extend(flag_rows(&unanchored, text_width));
        // separate the tail from the last thread's box
        if !lines.is_empty() {
            lines.push(BoardLine::spacer());
        }
        push_tail_card(&mut lines, card_width, rows);
    }
    (items, lines)
}

/// Flatten the forum tree into display order, skipping folded replies
///
/// # Arguments
///
/// * `view` - The forum to flatten
/// * `collapsed` - The posts and comments whose replies are folded away
fn flatten<'a>(view: &'a ForumView, collapsed: &HashSet<ParentRef>) -> Vec<FlatEntry<'a>> {
    let mut out = Vec::new();
    for thread in &view.threads {
        // each post starts its own tree at depth 0
        let node = NodeRef::Post(&thread.post);
        let folded = collapsed.contains(&node.target());
        out.push(FlatEntry {
            node,
            tasks: &thread.tasks,
            depth: 0,
            is_last: true,
            guides: Vec::new(),
            has_replies: !thread.comments.is_empty(),
            hidden: if folded {
                count_replies(&thread.comments)
            } else {
                0
            },
        });
        // follow the post's comments unless the post is folded
        if !folded {
            flatten_comments(&thread.comments, 1, &mut Vec::new(), collapsed, &mut out);
        }
    }
    out
}

/// Flatten one level of comments and, recursively, their replies
///
/// # Arguments
///
/// * `nodes` - The sibling comments at this level
/// * `depth` - The reply depth of these comments
/// * `guides` - For each ancestor comment level, whether it has later siblings
/// * `collapsed` - The posts and comments whose replies are folded away
/// * `out` - The display-order list to append to
fn flatten_comments<'a>(
    nodes: &'a [CommentNode],
    depth: usize,
    guides: &mut Vec<bool>,
    collapsed: &HashSet<ParentRef>,
    out: &mut Vec<FlatEntry<'a>>,
) {
    for (index, node) in nodes.iter().enumerate() {
        // the last sibling closes its column in the tree art
        let is_last = index + 1 == nodes.len();
        let entry = NodeRef::Comment(&node.comment);
        let folded = collapsed.contains(&entry.target());
        // record this comment, counting what its fold hides
        out.push(FlatEntry {
            node: entry,
            tasks: &node.tasks,
            depth,
            is_last,
            guides: guides.clone(),
            has_replies: !node.replies.is_empty(),
            hidden: if folded {
                count_replies(&node.replies)
            } else {
                0
            },
        });
        if !folded && !node.replies.is_empty() {
            // tell the replies whether this comment's column keeps going past them
            guides.push(!is_last);
            flatten_comments(&node.replies, depth + 1, guides, collapsed, out);
            guides.pop();
        }
    }
}

/// Count every reply nested under a list of comments
///
/// # Arguments
///
/// * `nodes` - The comments to count, along with all of their replies
fn count_replies(nodes: &[CommentNode]) -> usize {
    // count each comment plus everything nested under it
    nodes
        .iter()
        .map(|node| 1 + count_replies(&node.replies))
        .sum()
}

/// Work out where the shared, capped column keeps going below each entry
///
/// Comments at [`MAX_INDENT_DEPTH`] and deeper all sit in the same column, so
/// that column's guide carries on below an entry whenever any later entry in it
/// comes before the thread climbs back above that depth.
///
/// # Arguments
///
/// * `entries` - The flattened forum in display order
fn capped_column_continues(entries: &[FlatEntry]) -> Vec<bool> {
    // nothing continues until a later entry says so
    let mut continues = vec![false; entries.len()];
    // walk backwards tracking whether the capped column has anything further down
    let mut later = false;
    for (index, entry) in entries.iter().enumerate().rev() {
        // this entry's column continues if anything below it is in the column
        continues[index] = later;
        // an entry above the cap breaks the column for everything before it
        later = entry.depth >= MAX_INDENT_DEPTH;
    }
    continues
}

/// Build the tree art before an item's header and continuation lines
///
/// Depths up to [`MAX_INDENT_DEPTH`] indent [`INDENT_WIDTH`] columns per level
/// and draw plain tree art. Deeper replies stay in the last column and write
/// their depth into the connector (e.g. `├4──`) so the nesting stays readable.
///
/// # Arguments
///
/// * `depth` - The reply depth: 0 for a post
/// * `is_last` - Whether this is the last reply under its parent
/// * `guides` - For each ancestor comment level, whether it has later siblings
/// * `column_continues` - Whether the capped column carries on below this item
fn tree_prefixes(
    depth: usize,
    is_last: bool,
    guides: &[bool],
    column_continues: bool,
) -> (String, String) {
    // posts sit flush left with no tree art
    if depth == 0 {
        return (String::new(), String::new());
    }
    // indent at most to the cap, however deep the reply is
    let columns = depth.min(MAX_INDENT_DEPTH);
    // draw a guide for each ancestor column that has more replies below
    let unit = " ".repeat(INDENT_WIDTH - 1);
    let mut base = String::new();
    for has_more in guides.iter().take(columns - 1) {
        base.push(if *has_more { '│' } else { ' ' });
        base.push_str(&unit);
    }
    // the capped column is shared, so it continues whenever anything follows in it
    let continues = if depth >= MAX_INDENT_DEPTH {
        column_continues
    } else {
        !is_last
    };
    // a tee when more follows in this column, a corner when it ends here
    let elbow = if continues { '├' } else { '└' };
    // past the cap, the connector carries the depth instead of indenting again;
    // either way it runs right up to the box's corner
    let label = if depth > MAX_INDENT_DEPTH {
        format!("{elbow}{depth}")
    } else {
        elbow.to_string()
    };
    // pad the connector with dashes out to a full indent unit
    let dashes = INDENT_WIDTH.saturating_sub(label.chars().count());
    let connector = format!("{label}{}", "─".repeat(dashes));
    // continuation lines carry the guide down beside the box
    let pad = UnicodeWidthStr::width(connector.as_str()) - 1;
    let guide = if continues { "│" } else { " " };
    // the connector sits beside the top border, the guide beside every other row
    let head = format!("{base}{connector}");
    let body = format!("{base}{guide}{}", " ".repeat(pad));
    (head, body)
}

/// Build the styled pieces of an item's header line
///
/// # Arguments
///
/// * `entry` - The post or comment the header is for
/// * `marked` - Whether to mark this as the latest activity
fn header_segments(entry: &FlatEntry, marked: bool) -> Vec<(String, Style)> {
    let mut segments = Vec::with_capacity(6);
    // lead with a dot when this is the latest activity
    if marked {
        segments.push(("● ".to_string(), Style::default().fg(Color::Yellow)));
    }
    match entry.node {
        // a post shows its reference name and kind
        NodeRef::Post(post) => {
            let name = Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD);
            segments.push((format!("#{}", post.name), name));
            segments.push((
                format!("  [{}]", kind_label(post.kind)),
                kind_style(post.kind),
            ));
        }
        // a comment shows its id, author, stance, and kind
        NodeRef::Comment(comment) => {
            let id = Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD);
            segments.push((format!("!{}", comment.id), id));
            segments.push((
                format!(" {}", comment.author),
                author_style(&comment.author),
            ));
            segments.push((
                format!(" · {}", stance_label(comment.stance)),
                stance_style(comment.stance),
            ));
            segments.push((
                format!(" · {}", kind_label(comment.kind)),
                kind_style(comment.kind),
            ));
        }
    }
    // say how much is folded away so nothing silently disappears
    if entry.hidden > 0 {
        let noun = if entry.hidden == 1 {
            "reply"
        } else {
            "replies"
        };
        segments.push((
            format!(" [+{} {noun}]", entry.hidden),
            Style::default().fg(MUTED_COLOR),
        ));
    }
    segments
}

/// Wrap styled header pieces into rows, breaking between pieces
///
/// Pieces stay whole where they fit; a piece that would overflow a row with
/// content moves to the next row (dropping its leading separator), and a piece
/// wider than a whole row is broken across rows.
///
/// # Arguments
///
/// * `segments` - The styled pieces to lay out
/// * `width` - The number of columns each row may use
fn wrap_segments(segments: &[(String, Style)], width: usize) -> Vec<Vec<Span<'static>>> {
    // the finished rows, the row being filled, and how wide it is so far
    let mut rows = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for (text, style) in segments {
        // headers are single-line, so fold any stray new lines into spaces
        let flat = text.replace('\n', " ");
        let mut text = flat.as_str();
        // move a piece that would overflow onto a fresh row
        if !spans.is_empty() && used + UnicodeWidthStr::width(text) > width {
            rows.push(std::mem::take(&mut spans));
            used = 0;
        }
        // a piece starting a wrapped row has no need for its leading separator
        if spans.is_empty() && !rows.is_empty() {
            text = text.trim_start().trim_start_matches('·').trim_start();
        }
        // how much of this row is left
        let room = width.saturating_sub(used).max(1);
        if UnicodeWidthStr::width(text) <= room {
            // the piece fits on this row whole
            used += UnicodeWidthStr::width(text);
            spans.push(Span::styled(text.to_string(), *style));
        } else {
            // fill the rest of this row, then wrap the remainder at full width
            let first = wrap_unicode_preserve_ws(text, room)
                .into_iter()
                .next()
                .unwrap_or_default();
            let remainder = &text[first.len()..];
            spans.push(Span::styled(first, *style));
            for piece in wrap_unicode_preserve_ws(remainder, width.max(1)) {
                rows.push(std::mem::take(&mut spans));
                used = UnicodeWidthStr::width(piece.as_str());
                spans.push(Span::styled(piece, *style));
            }
        }
    }
    // keep the last row, and always return at least one
    if !spans.is_empty() || rows.is_empty() {
        rows.push(spans);
    }
    rows
}

/// Word-wrap text into rows drawn in a single style
///
/// Empty text yields no rows rather than a blank one.
///
/// # Arguments
///
/// * `text` - The text to wrap
/// * `style` - The style to draw the text in
/// * `width` - The number of columns each row may use
fn text_rows(text: &str, style: Style, width: usize) -> Vec<Vec<Span<'static>>> {
    // empty text adds nothing to the box
    if text.trim().is_empty() {
        return Vec::new();
    }
    // wrap at word boundaries, one styled span per row
    wrap_words(text.trim_end(), width.max(1))
        .into_iter()
        .map(|row| vec![Span::styled(row, style)])
        .collect()
}

/// Render a question's completion-gate tasks as checklist rows
///
/// Each task takes one line that wraps if it is too wide: an open task shows an
/// empty checkbox and its description, a completed one a checked box and who
/// completed it citing which comment.
///
/// # Arguments
///
/// * `tasks` - The tasks to render, open or completed
/// * `width` - The number of columns each row may use
fn task_rows(tasks: &[Task], width: usize) -> Vec<Vec<Span<'static>>> {
    // most boxes carry no tasks and add no rows
    let mut rows = Vec::new();
    for task in tasks {
        // name each task the way complete_task addresses it
        let label = (
            format!("task {}: ", task.id),
            Style::default().fg(MUTED_COLOR),
        );
        let segments = match &task.completed {
            // an open task shows an empty checkbox and a readable description
            None => vec![
                ("[ ] ".to_string(), Style::default().fg(Color::Yellow)),
                label,
                (task.description.clone(), Style::default().fg(TEXT_COLOR)),
            ],
            // a completed task mutes itself and cites the satisfying comment
            Some(done) => vec![
                ("[x] ".to_string(), Style::default().fg(Color::Green)),
                label,
                (task.description.clone(), Style::default().fg(MUTED_COLOR)),
                (
                    format!(" — {} via !{}", done.author, done.comment),
                    Style::default().fg(MUTED_COLOR),
                ),
            ],
        };
        // wrap each task onto its own row or rows
        rows.extend(wrap_segments(&segments, width));
    }
    rows
}

/// Render recorded analyst asks as compact two-row entries
///
/// Each ask shows its question and, underneath, the answer that was taken and
/// who or what chose it (an analyst pick, free text, auto-default, or decline).
///
/// # Arguments
///
/// * `asks` - The recorded asks to render, in id order
/// * `width` - The number of columns each row may use
fn ask_rows(asks: &[AnalystAsk], width: usize) -> Vec<Vec<Span<'static>>> {
    // most forums carry no asks and add no rows
    let mut rows = Vec::new();
    for ask in asks {
        // name each ask the way the forum dump addresses it
        let head = vec![
            (
                format!("❓ ask {}: ", ask.id),
                Style::default().fg(Color::Yellow),
            ),
            (ask.question.clone(), Style::default().fg(TEXT_COLOR)),
        ];
        rows.extend(wrap_segments(&head, width));
        // the answer row names the taken fork and its source
        let answer = vec![
            ("   → ".to_owned(), Style::default().fg(MUTED_COLOR)),
            (ask.chosen.clone(), Style::default().fg(Color::Green)),
            (
                format!(" ({})", ask.source.label()),
                Style::default().fg(MUTED_COLOR),
            ),
        ];
        rows.extend(wrap_segments(&answer, width));
    }
    rows
}

/// Render review flags as compact entries under a card or in the tail
///
/// Each flag shows its title and author, then its reason, and names the ask it
/// was auto-filed for when there is one.
///
/// # Arguments
///
/// * `flags` - The flags to render, in id order
/// * `width` - The number of columns each row may use
fn flag_rows(flags: &[&ReviewFlag], width: usize) -> Vec<Vec<Span<'static>>> {
    // most cards carry no flags and add no rows
    let mut rows = Vec::new();
    for flag in flags {
        // the header names the flag, its title, and who raised it
        let head = vec![
            (
                format!("⚑ flag {}: ", flag.id),
                Style::default().fg(Color::Red),
            ),
            (flag.title.clone(), Style::default().fg(TEXT_COLOR)),
            (
                format!(" (by {})", flag.author),
                Style::default().fg(MUTED_COLOR),
            ),
        ];
        rows.extend(wrap_segments(&head, width));
        // the reason renders muted underneath
        let reason = vec![(
            format!("   {}", flag.reason),
            Style::default().fg(MUTED_COLOR),
        )];
        rows.extend(wrap_segments(&reason, width));
        // note the recorded ask this flag was auto-filed for, if any
        if let Some(ask) = flag.ask {
            let note = vec![(
                format!("   (auto-filed for ask {ask})"),
                Style::default().fg(MUTED_COLOR),
            )];
            rows.extend(wrap_segments(&note, width));
        }
    }
    rows
}

/// Append the analyst-attention tail as a non-selectable, full-width box
///
/// The tail belongs to no item, so it cannot be selected or folded; the
/// painter draws it with the default border like any unselected card.
///
/// # Arguments
///
/// * `lines` - The lines to append to
/// * `card_width` - The width of the box, borders included
/// * `rows` - The rows of text inside the box
fn push_tail_card(lines: &mut Vec<BoardLine>, card_width: usize, rows: Vec<Vec<Span<'static>>>) {
    // every line of the box floats free of the selectable item list
    let line = |row: RowKind| BoardLine {
        item: None,
        gutter: String::new(),
        card_width,
        row,
    };
    // the box has no tree connector, just its borders and rows
    lines.push(line(RowKind::Top));
    for row in rows {
        lines.push(line(RowKind::Content(row)));
    }
    lines.push(line(RowKind::Bottom));
}

/// Append a post or comment box: its top border, text rows, and bottom border
///
/// # Arguments
///
/// * `lines` - The lines to append to
/// * `item` - The index of the item this box shows
/// * `head_gutter` - The tree art beside the top border, holding the connector
/// * `rest_gutter` - The tree art beside every other row of the box
/// * `card_width` - The width of the box, borders included
/// * `rows` - The rows of text inside the box
fn push_card(
    lines: &mut Vec<BoardLine>,
    item: usize,
    head_gutter: &str,
    rest_gutter: &str,
    card_width: usize,
    rows: Vec<Vec<Span<'static>>>,
) {
    // every line of the box belongs to the same item and width
    let line = |gutter: &str, row: RowKind| BoardLine {
        item: Some(item),
        gutter: gutter.to_string(),
        card_width,
        row,
    };
    // the top border carries the tree connector
    lines.push(line(head_gutter, RowKind::Top));
    // each row of text sits inside the box
    for row in rows {
        lines.push(line(rest_gutter, RowKind::Content(row)));
    }
    // close the box
    lines.push(line(rest_gutter, RowKind::Bottom));
}

/// Patch a style over every query-term occurrence in one laid-out row
///
/// The row's text is matched as one string, so a term crossing span
/// boundaries (e.g. a word split by markdown styling) still highlights.
/// Splitting never changes the row's text, so its drawn width is unchanged.
/// Headers and author segments highlight too: posts rank on their name and
/// title, so every occurrence is a real match.
///
/// # Arguments
///
/// * `row` - The row of styled spans to highlight
/// * `terms` - The lowercased query terms to highlight
/// * `style` - The style to patch over the matched text
fn highlight_row(row: &mut Vec<Span<'static>>, terms: &[String], style: Style) -> bool {
    // join the row's text so matches can cross span boundaries
    let joined: String = row.iter().map(|span| span.content.as_ref()).collect();
    // find every match in the row, leaving it untouched when there are none
    let ranges = match_ranges(&joined, terms);
    if ranges.is_empty() {
        return false;
    }
    // rebuild the row, splitting spans where a match starts or ends
    let mut rebuilt: Vec<Span<'static>> = Vec::with_capacity(row.len() + ranges.len() * 2);
    // the range the walk is in or approaching, and the next char's offset
    let mut range = 0;
    let mut offset = 0;
    for span in row.iter() {
        // the piece of this span being grown and whether it is matched
        let mut piece = String::new();
        let mut piece_matched = false;
        for c in span.content.chars() {
            // step past any range that ended before this char
            while range < ranges.len() && offset >= ranges[range].end {
                range += 1;
            }
            // whether this char falls inside the current range
            let matched = range < ranges.len() && offset >= ranges[range].start;
            // close the open piece whenever the matched state flips
            if matched != piece_matched && !piece.is_empty() {
                let closed = std::mem::take(&mut piece);
                rebuilt.push(highlight_piece(closed, span, piece_matched, style));
            }
            // grow the piece with this char
            piece_matched = matched;
            piece.push(c);
            offset += 1;
        }
        // close the span's final piece
        if !piece.is_empty() {
            rebuilt.push(highlight_piece(piece, span, piece_matched, style));
        }
    }
    *row = rebuilt;
    true
}

/// Build one split-off piece of a span, patching the match style if it matched
///
/// # Arguments
///
/// * `piece` - The piece's text
/// * `span` - The span the piece was split from
/// * `matched` - Whether the piece falls inside a match
/// * `style` - The style to patch over matched pieces
fn highlight_piece(
    piece: String,
    span: &Span<'static>,
    matched: bool,
    style: Style,
) -> Span<'static> {
    // matched pieces carry the match style over their own
    let styled = if matched {
        span.style.patch(style)
    } else {
        span.style
    };
    Span::styled(piece, styled)
}

/// Highlight every row of a card, returning the first matched row's index
///
/// # Arguments
///
/// * `rows` - The rows of styled spans to highlight
/// * `terms` - The lowercased query terms to highlight
/// * `style` - The style to patch over the matched text
fn highlight_rows(
    rows: &mut [Vec<Span<'static>>],
    terms: &[String],
    style: Style,
) -> Option<usize> {
    // remember the first matched row so a preview can scroll to it
    let mut first = None;
    for (index, row) in rows.iter_mut().enumerate() {
        // highlight this row, remembering the first that matched
        if highlight_row(row, terms, style) && first.is_none() {
            first = Some(index);
        }
    }
    first
}

/// Find a post or comment in a forum view by its reference
///
/// # Arguments
///
/// * `view` - The forum to search
/// * `target` - The post or comment to find
fn find_node<'a>(view: &'a ForumView, target: &ParentRef) -> Option<NodeRef<'a>> {
    /// Search a list of comments and their replies for the target
    fn search<'a>(nodes: &'a [CommentNode], target: &ParentRef) -> Option<NodeRef<'a>> {
        for node in nodes {
            // stop at this comment when it is the target
            if matches!(target, ParentRef::Comment(id) if *id == node.comment.id) {
                return Some(NodeRef::Comment(&node.comment));
            }
            // otherwise keep looking through its replies
            if let Some(found) = search(&node.replies, target) {
                return Some(found);
            }
        }
        // the target is not under these comments
        None
    }
    // walk each thread, checking the post before descending to its comments
    for thread in &view.threads {
        // the post itself may be the target
        if matches!(target, ParentRef::Post(name) if name == &thread.post.name) {
            return Some(NodeRef::Post(&thread.post));
        }
        if let Some(found) = search(&thread.comments, target) {
            return Some(found);
        }
    }
    // the target is not on the forum
    None
}

/// Find every post and comment above a target, outermost first
///
/// # Arguments
///
/// * `view` - The forum to search
/// * `target` - The post or comment whose ancestors to find
fn ancestors(view: &ForumView, target: &ParentRef) -> Vec<ParentRef> {
    /// Search a list of comments for the target, recording the path down to it
    fn search(nodes: &[CommentNode], target: &ParentRef, path: &mut Vec<ParentRef>) -> bool {
        for node in nodes {
            // stop once the target itself is reached
            let here = ParentRef::Comment(node.comment.id);
            if &here == target {
                return true;
            }
            // descend through this comment, backing out if the target is not below
            path.push(here);
            if search(&node.replies, target, path) {
                return true;
            }
            path.pop();
        }
        // the target is not under these comments
        false
    }
    // a post has nothing above it
    if matches!(target, ParentRef::Post(_)) {
        return Vec::new();
    }
    // search each post's thread, starting the path at the post
    for thread in &view.threads {
        let mut path = vec![ParentRef::Post(thread.post.name.clone())];
        if search(&thread.comments, target, &mut path) {
            return path;
        }
    }
    // the target is not on the forum
    Vec::new()
}

/// Whether a point falls inside an area
///
/// # Arguments
///
/// * `area` - The area to check
/// * `x` - The column of the point
/// * `y` - The row of the point
fn area_contains(area: Rect, x: u16, y: u16) -> bool {
    // inside means within both the column and row ranges
    x >= area.x && x < area.x + area.width && y >= area.y && y < area.y + area.height
}

/// Render a post or comment reference the way agents write it
///
/// # Arguments
///
/// * `target` - The post or comment to reference
fn reference(target: &ParentRef) -> String {
    // posts are written #name and comments !id
    match target {
        ParentRef::Post(name) => format!("#{name}"),
        ParentRef::Comment(id) => format!("!{id}"),
    }
}

/// A short human-readable label for a question status
///
/// # Arguments
///
/// * `status` - The status to label
fn status_label(status: QuestionStatus) -> &'static str {
    // spell each status out for display
    match status {
        QuestionStatus::Unsolved => "Unsolved",
        QuestionStatus::Solved => "Solved",
        QuestionStatus::NeedsEvidence => "Needs evidence",
        QuestionStatus::Blocked => "Blocked",
    }
}

/// The color a question status is drawn in
///
/// # Arguments
///
/// * `status` - The status to style
fn status_style(status: QuestionStatus) -> Style {
    // open questions are warm colors, resolved ones green or red
    let color = match status {
        QuestionStatus::Unsolved => Color::Yellow,
        QuestionStatus::Solved => Color::Green,
        QuestionStatus::NeedsEvidence => Color::Magenta,
        QuestionStatus::Blocked => Color::Red,
    };
    // apply the color as the foreground
    Style::default().fg(color)
}

/// A short human-readable label for a post or comment kind
///
/// # Arguments
///
/// * `kind` - The kind to label
fn kind_label(kind: Kind) -> String {
    // questions carry their status alongside the kind
    match kind {
        Kind::Statement => "Statement".to_string(),
        Kind::Question(status) => format!("Question · {}", status_label(status)),
    }
}

/// The style a post or comment kind is drawn in
///
/// # Arguments
///
/// * `kind` - The kind to style
fn kind_style(kind: Kind) -> Style {
    // questions take their status color, statements stay grey
    match kind {
        Kind::Statement => Style::default().fg(Color::Gray),
        Kind::Question(status) => status_style(status),
    }
}

/// A short human-readable label for a stance
///
/// # Arguments
///
/// * `stance` - The stance to label
fn stance_label(stance: Stance) -> &'static str {
    // spell each stance out for display
    match stance {
        Stance::Supports => "supports",
        Stance::Disproves => "disproves",
        Stance::Neutral => "neutral",
    }
}

/// The color a stance is drawn in
///
/// # Arguments
///
/// * `stance` - The stance to style
fn stance_style(stance: Stance) -> Style {
    // support is green, disproval red, and neutral muted
    let color = match stance {
        Stance::Supports => Color::Green,
        Stance::Disproves => Color::Red,
        Stance::Neutral => MUTED_COLOR,
    };
    // apply the color as the foreground
    Style::default().fg(color)
}

/// The color an author is drawn in, singling out the human operator
///
/// # Arguments
///
/// * `author` - The author to style
fn author_style(author: &str) -> Style {
    // the analyst's own posts stand out from the agents'
    if author == USER_AUTHOR {
        Style::default()
            .fg(Color::LightBlue)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thorpal::{Comment, Post, PostWithThread, TaskCompletion};

    /// Build a comment node with the given id, parent, and replies
    ///
    /// # Arguments
    ///
    /// * `id` - The comment's id, also used as its revision and in its body
    /// * `parent` - The post or comment this comment replies to
    /// * `replies` - The replies nested under this comment
    fn node(id: usize, parent: ParentRef, replies: Vec<CommentNode>) -> CommentNode {
        // a neutral statement from a generic agent
        CommentNode {
            comment: Comment {
                id,
                parent,
                author: "agent".to_string(),
                body: format!("body {id}"),
                stance: Stance::Neutral,
                kind: Kind::Statement,
                updated: id as u64,
            },
            tasks: Vec::new(),
            replies,
        }
    }

    /// Build a view with one post holding a single chain of replies `depth` deep,
    /// plus a second top-level comment after the chain
    ///
    /// # Arguments
    ///
    /// * `depth` - How many replies deep the chain goes (comments `1..=depth`)
    fn chain_view(depth: usize) -> ForumView {
        // build the chain from the deepest reply up
        let mut current: Option<CommentNode> = None;
        for id in (1..=depth).rev() {
            // the first comment replies to the post, the rest to the one above
            let parent = if id == 1 {
                ParentRef::Post("post".to_string())
            } else {
                ParentRef::Comment(id - 1)
            };
            current = Some(node(id, parent, current.into_iter().collect()));
        }
        // add a later top-level sibling so the chain's column keeps going
        let sibling = node(100, ParentRef::Post("post".to_string()), Vec::new());
        // wrap it all in a single open question post, latest at the chain's end
        ForumView {
            threads: vec![PostWithThread {
                post: Post {
                    name: "post".to_string(),
                    author: "detector".to_string(),
                    title: "A title".to_string(),
                    body: "A body".to_string(),
                    kind: Kind::Question(QuestionStatus::Unsolved),
                    updated: 0,
                },
                tasks: Vec::new(),
                comments: current.into_iter().chain(Some(sibling)).collect(),
            }],
            last_updated: Some(ParentRef::Comment(depth)),
            asks: Vec::new(),
            flags: Vec::new(),
        }
    }

    /// Find a laid out item by comment id
    ///
    /// # Arguments
    ///
    /// * `items` - The laid out items to search
    /// * `id` - The id of the comment to find
    fn item(items: &[BoardItem], id: usize) -> &BoardItem {
        // look the item up by index so both helpers agree
        &items[index_of(items, id)]
    }

    /// The index of a comment's laid out item
    ///
    /// # Arguments
    ///
    /// * `items` - The laid out items to search
    /// * `id` - The id of the comment to find
    fn index_of(items: &[BoardItem], id: usize) -> usize {
        // a missing comment is a test bug, so fail loudly
        items
            .iter()
            .position(|item| item.target == ParentRef::Comment(id))
            .expect("comment should be laid out")
    }

    /// Render a laid out line to plain text
    ///
    /// # Arguments
    ///
    /// * `line` - The laid out line to render
    fn text(line: &BoardLine) -> String {
        // join every span's text, ignoring styles
        line.to_line(CARD_BORDER)
            .spans
            .iter()
            .map(|span| span.content.to_string())
            .collect()
    }

    /// Replies up to the cap indent four columns per level with no depth numbers
    #[test]
    fn shallow_replies_indent_four_columns_per_level() {
        // lay out a chain exactly as deep as the cap
        let (items, _) = layout(&chain_view(3), &HashSet::new(), 80, None);
        // a top-level comment with a later sibling, then four columns per level
        assert_eq!(item(&items, 1).head_prefix, "├───");
        assert_eq!(item(&items, 2).head_prefix, "│   └───");
        assert_eq!(item(&items, 3).head_prefix, "│       └───");
        // the last top-level comment closes the tree
        assert_eq!(item(&items, 100).head_prefix, "└───");
        // no depth numbers appear at or under the cap
        for item in &items {
            assert!(!item.head_prefix.chars().any(|c| c.is_ascii_digit()));
        }
    }

    /// Replies past the cap stay in the third column and show their depth
    #[test]
    fn deep_replies_stop_indenting_and_show_their_depth() {
        // lay out a chain well past the cap, into two-digit depths
        let (items, _) = layout(&chain_view(12), &HashSet::new(), 80, None);
        // depth 3 opens the capped column, which continues down to depth 12
        assert_eq!(item(&items, 3).head_prefix, "│       ├───");
        assert_eq!(item(&items, 4).head_prefix, "│       ├4──");
        assert_eq!(item(&items, 11).head_prefix, "│       ├11─");
        // the last reply in the column closes it
        assert_eq!(item(&items, 12).head_prefix, "│       └12─");
        // every capped reply's box starts at the same column as depth 3
        let column = item(&items, 3).head_prefix.chars().count();
        for id in 4..=12 {
            assert_eq!(item(&items, id).head_prefix.chars().count(), column);
        }
        // continuation rows carry the column's guide down beside the box
        assert_eq!(item(&items, 4).body_prefix, "│       │   ");
        assert_eq!(item(&items, 12).body_prefix, format!("│{}", " ".repeat(11)));
    }

    /// The last reply at a level blanks its column for everything below it
    #[test]
    fn last_sibling_blanks_its_guide_for_its_replies() {
        // make the chain the only top-level comment so its column ends
        let mut view = chain_view(2);
        view.threads[0].comments.pop();
        let (items, _) = layout(&view, &HashSet::new(), 80, None);
        // the lone comment closes its column, so its reply's guide is blank
        assert_eq!(item(&items, 1).head_prefix, "└───");
        assert_eq!(item(&items, 1).body_prefix, "    ");
        assert_eq!(item(&items, 2).head_prefix, "    └───");
    }

    /// Each comment box starts one indent to the right of its parent's box
    #[test]
    fn comment_boxes_sit_four_columns_right_of_their_post() {
        let (items, lines) = layout(&chain_view(2), &HashSet::new(), 40, None);
        // render an item's top border to text
        let top = |index: usize| {
            lines
                .iter()
                .find(|line| line.item == Some(index) && matches!(line.row, RowKind::Top))
                .map(text)
                .expect("item should have a top border")
        };
        // the post's box starts flush left
        assert!(top(0).starts_with('╭'));
        // the connector runs into the comment boxes' corners one level apart
        assert!(top(index_of(&items, 1)).starts_with("├───╭"));
        assert!(top(index_of(&items, 2)).starts_with("│   └───╭"));
    }

    /// Every box row is exactly as wide as the list so right borders line up
    #[test]
    fn every_row_fills_the_list_width() {
        // right borders line up at any width, including through capped depths
        for width in [46, 80] {
            let (_, lines) = layout(&chain_view(6), &HashSet::new(), width, None);
            // spacers draw nothing, so only box rows are measured
            for line in lines.iter().filter(|line| line.item.is_some()) {
                assert_eq!(line.to_line(CARD_BORDER).width(), width, "{}", text(line));
            }
        }
    }

    /// Only the selected box is outlined in blue, and every box is dark grey
    #[test]
    fn selected_box_is_outlined_in_blue_on_a_dark_background() {
        use ratatui::{Terminal, backend::TestBackend};
        // open a board on a small thread, selecting the latest reply
        let forum = Arc::new(ForumState::new());
        let (mut board, _rx) = board_with_events(forum);
        board.update(chain_view(2), ParentRef::Comment(2));
        board.show();
        // draw it into an in-memory terminal
        let mut terminal = Terminal::new(TestBackend::new(60, 30)).unwrap();
        terminal.draw(|f| board.render(f, f.area())).unwrap();
        let buffer = terminal.backend().buffer();
        // collect the outline color of every box's top-left corner
        let mut corners = Vec::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let cell = &buffer[(x, y)];
                if cell.symbol() == "╭" {
                    corners.push((cell.fg, cell.bg));
                }
            }
        }
        // four boxes: the post, the two-reply chain, and its sibling, one selected
        assert_eq!(corners.len(), 4, "{corners:?}");
        // count the blue and grey outlines
        let blue = corners
            .iter()
            .filter(|(fg, _)| Some(*fg) == SELECTED_BORDER.fg)
            .count();
        let grey = corners
            .iter()
            .filter(|(fg, _)| Some(*fg) == CARD_BORDER.fg)
            .count();
        assert_eq!((blue, grey), (1, 3));
        // every box sits on the dark grey background
        assert!(corners.iter().all(|(_, bg)| *bg == CARD_BG));
    }

    /// Folding hides a comment's replies and says how many are hidden
    #[test]
    fn folding_hides_replies_and_counts_them() {
        // fold comment 2 in a four-deep chain
        let view = chain_view(4);
        let collapsed = HashSet::from([ParentRef::Comment(2)]);
        let (items, lines) = layout(&view, &collapsed, 80, None);
        // comments 3 and 4 are hidden under comment 2
        assert!(
            items
                .iter()
                .all(|item| item.target != ParentRef::Comment(3))
        );
        assert_eq!(item(&items, 2).hidden, 2);
        // the folded box's text reports the hidden count
        let folded = index_of(&items, 2);
        let header: String = lines
            .iter()
            .filter(|line| line.item == Some(folded))
            .map(text)
            .collect();
        assert!(header.contains("[+2 replies]"), "{header}");
    }

    /// A question's tasks render as a checklist inside its own box
    #[test]
    fn task_rows_render_checkboxes_under_their_card() {
        // give the post one open task and one completed task
        let mut view = chain_view(1);
        view.threads[0].tasks = vec![
            Task {
                id: 1,
                parent: ParentRef::Post("post".into()),
                author: "user".into(),
                description: "check the mutex name".into(),
                completed: None,
                updated: 0,
            },
            Task {
                id: 2,
                parent: ParentRef::Post("post".into()),
                author: "user".into(),
                description: "list the C2 domains".into(),
                completed: Some(TaskCompletion {
                    comment: 7,
                    author: "agent".into(),
                }),
                updated: 0,
            },
        ];
        // lay it out and collect the post box's text (the post is item 0)
        let (_, lines) = layout(&view, &HashSet::new(), 80, None);
        let card: String = lines
            .iter()
            .filter(|line| line.item == Some(0))
            .map(text)
            .collect();
        // the open task shows an empty checkbox, the completed one its citation
        assert!(card.contains("[ ] task 1:"), "{card}");
        assert!(card.contains("[x] task 2:"), "{card}");
        assert!(card.contains("via !7"), "{card}");
    }

    /// Opening the board unfolds and selects the latest activity
    #[test]
    fn jump_to_latest_unfolds_ancestors_and_selects_it() {
        let forum = Arc::new(ForumState::new());
        let (mut board, _rx) = board_with_events(forum);
        let view = chain_view(5);
        // fold the post and a comment above the latest reply first
        board.collapsed.insert(ParentRef::Post("post".to_string()));
        board.collapsed.insert(ParentRef::Comment(2));
        // an update elsewhere than the selection is flagged as unseen
        board.update(view, ParentRef::Comment(5));
        assert!(board.unseen);
        // opening jumps to the latest reply, unfolding everything above it
        board.show();
        assert_eq!(board.selected_target(), Some(ParentRef::Comment(5)));
        assert!(board.collapsed.is_empty());
        assert!(!board.unseen);
    }

    /// Long headers wrap inside their box without dangling separators
    #[test]
    fn long_headers_wrap_between_pieces() {
        // lay out at a width too narrow for any header to fit on one row
        let (_, lines) = layout(&chain_view(1), &HashSet::new(), 24, None);
        // the headers split across rows without pushing past the box
        for line in lines.iter().filter(|line| line.item.is_some()) {
            assert_eq!(line.to_line(CARD_BORDER).width(), 24, "{}", text(line));
        }
        // a wrapped row never starts with a dangling separator
        assert!(lines.iter().all(|line| !text(line).contains("│ ·")));
    }

    /// A pending selection waits for its item without moving the current one
    #[test]
    fn pending_selection_keeps_the_current_one_until_it_arrives() {
        // select comment 2 on a small thread
        let forum = Arc::new(ForumState::new());
        let (mut board, _rx) = board_with_events(forum);
        board.update(chain_view(2), ParentRef::Comment(2));
        board.select(index_of(&board.items, 2));
        // the analyst just posted a reply that has not reached the view yet
        board.want_select = Some(ParentRef::Comment(999));
        // an unrelated update shifts the items by adding a comment ahead of it
        let mut view = chain_view(2);
        let early = node(50, ParentRef::Post("post".to_string()), Vec::new());
        view.threads[0].comments.insert(0, early);
        board.update(view, ParentRef::Comment(50));
        // the selection stays put while the reply is still pending
        assert_eq!(board.selected_target(), Some(ParentRef::Comment(2)));
        assert_eq!(board.want_select, Some(ParentRef::Comment(999)));
        // once the reply arrives it is selected
        let mut view = chain_view(2);
        let reply = node(999, ParentRef::Comment(2), Vec::new());
        view.threads[0].comments[0].replies[0].replies.push(reply);
        board.update(view, ParentRef::Comment(999));
        assert_eq!(board.selected_target(), Some(ParentRef::Comment(999)));
        assert!(board.want_select.is_none());
    }

    /// Builds a plain key press event for the search-mode tests
    ///
    /// # Arguments
    ///
    /// * `code` - The key code to press
    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    /// Builds a board over a forum wired to a capturing test event channel
    ///
    /// The receiver stands in for the app's event loop: spawned searches send
    /// their results onto it, and [`pump_search`] feeds them back to the board.
    ///
    /// # Arguments
    ///
    /// * `forum` - The forum the board shows and searches
    fn board_with_events(forum: Arc<ForumState>) -> (MessageBoard, kanal::AsyncReceiver<AppEvent>) {
        // the board's searches report onto this test-held channel
        let (event_tx, event_rx) = kanal::unbounded_async();
        (MessageBoard::new(forum, event_tx), event_rx)
    }

    /// Receives one finished search event and applies it to the board
    ///
    /// Awaiting the receive yields to the test runtime, which is what lets the
    /// spawned search task actually run on the current-thread executor.
    ///
    /// # Arguments
    ///
    /// * `board` - The board to hand the results to
    /// * `rx` - The test event channel the spawned search reports on
    async fn pump_search(board: &mut MessageBoard, rx: &kanal::AsyncReceiver<AppEvent>) {
        // receive the one event a spawned board search can send
        let event = rx.recv().await.expect("a search result should arrive");
        let AppEvent::BoardSearchResults {
            board: owner,
            flight,
            query,
            hits,
            terms,
        } = event
        else {
            panic!("the board should only send search results");
        };
        board.apply_search_results(owner, flight, &query, hits, terms);
    }

    /// Types a query into an open search and settles its spawned searches
    ///
    /// The first key spawns one search and the rest coalesce into a single
    /// follow-up, so settling pumps results until the displayed hits answer
    /// the box's current text (at most two flights per typed burst).
    ///
    /// # Arguments
    ///
    /// * `board` - The board whose open search to type into
    /// * `rx` - The test event channel the spawned searches report on
    /// * `text` - The query text to type one key at a time
    async fn type_and_settle(
        board: &mut MessageBoard,
        rx: &kanal::AsyncReceiver<AppEvent>,
        text: &str,
    ) {
        // type the query one key at a time, like an analyst would
        for c in text.chars() {
            board.handle_key(press(KeyCode::Char(c))).await;
        }
        // pump results until the hits answer the current text (bounded so a
        // coalescing bug fails the test instead of hanging it)
        for _ in 0..4 {
            let BoardMode::Search(search) = &board.mode else {
                return;
            };
            if search.answered == search.query.content {
                return;
            }
            pump_search(board, rx).await;
        }
        panic!("the search never settled on the typed query");
    }

    /// `/` opens the search box while browsing and Esc abandons it
    #[tokio::test]
    async fn slash_opens_search_and_esc_returns_to_browse() {
        let (mut board, _rx) = board_with_events(Arc::new(ForumState::new()));
        board.show();
        // `/` while browsing opens the search box with no hits yet
        board.handle_key(press(KeyCode::Char('/'))).await;
        assert!(matches!(board.mode, BoardMode::Search(_)));
        // Esc abandons the search and returns to browsing
        board.handle_key(press(KeyCode::Esc)).await;
        assert!(matches!(board.mode, BoardMode::Browse));
    }

    /// Typing re-runs the search live and Enter jumps to the selected hit
    #[tokio::test]
    async fn typing_searches_and_enter_jumps_to_hit() {
        // seed a question and an evidence comment through the user-entry seams
        let forum = Arc::new(ForumState::new());
        let post = forum
            .add_user_question(
                "Entry point".to_owned(),
                "where does the unpacking start?".to_owned(),
            )
            .await
            .expect("the question should seed");
        let comment = forum
            .add_user_comment(
                ParentRef::Post(post.name.clone()),
                "the xor loop lives at 0x401000".to_owned(),
                Stance::Neutral,
                Kind::Statement,
            )
            .await
            .expect("the comment should seed");
        // show the board with the seeded forum laid out
        let (mut board, rx) = board_with_events(forum.clone());
        board.show();
        board.update(forum.view().await, ParentRef::Comment(comment.id));
        // open the search and type a query only the comment matches
        board.start_search();
        type_and_settle(&mut board, &rx, "0x401000").await;
        // the live search found exactly the comment
        let BoardMode::Search(search) = &board.mode else {
            panic!("the board should be searching");
        };
        assert_eq!(search.hits.len(), 1);
        assert_eq!(search.hits[0].reference, format!("!{}", comment.id));
        // Enter jumps to the hit and returns to browsing
        board.handle_key(press(KeyCode::Enter)).await;
        assert!(matches!(board.mode, BoardMode::Browse));
        assert_eq!(
            board.selected_target(),
            Some(ParentRef::Comment(comment.id))
        );
    }

    /// Cursor moves keep the hit selection and Enter on no hits keeps the query
    #[tokio::test]
    async fn cursor_moves_keep_selection_and_empty_enter_keeps_the_search() {
        // seed two posts so a query can rank more than one hit
        let forum = Arc::new(ForumState::new());
        for name in ["first entry", "second entry"] {
            forum
                .add_user_question(name.to_owned(), "an entry worth finding".to_owned())
                .await
                .expect("the question should seed");
        }
        let (mut board, rx) = board_with_events(forum.clone());
        board.show();
        board.update(
            forum.view().await,
            ParentRef::Post("first-entry".to_owned()),
        );
        // search for a term both posts match and select the second hit
        board.start_search();
        type_and_settle(&mut board, &rx, "entry").await;
        board.handle_key(press(KeyCode::Down)).await;
        // cursor-only keys must not wipe the selection back to the first hit
        board.handle_key(press(KeyCode::Left)).await;
        board.handle_key(press(KeyCode::End)).await;
        let BoardMode::Search(search) = &board.mode else {
            panic!("the board should be searching");
        };
        assert_eq!(search.hits.len(), 2);
        assert_eq!(search.selected, 1);
        // a query with no hits keeps the search (and its text) on Enter
        board.start_search();
        type_and_settle(&mut board, &rx, "zzzmissing").await;
        board.handle_key(press(KeyCode::Enter)).await;
        let BoardMode::Search(search) = &board.mode else {
            panic!("Enter with no hits should stay in the search");
        };
        assert_eq!(search.query.content, "zzzmissing");
    }

    /// A burst of keystrokes coalesces into two searches, not one per key
    #[tokio::test]
    async fn typing_burst_coalesces_to_two_searches() {
        // seed a comment the finished query will match
        let forum = Arc::new(ForumState::new());
        let post = forum
            .add_user_question("Entry point".to_owned(), "where does it start?".to_owned())
            .await
            .expect("the question should seed");
        forum
            .add_user_comment(
                ParentRef::Post(post.name.clone()),
                "the xor loop lives at 0x401000".to_owned(),
                Stance::Neutral,
                Kind::Statement,
            )
            .await
            .expect("the comment should seed");
        let (mut board, rx) = board_with_events(forum.clone());
        board.show();
        board.update(forum.view().await, ParentRef::Post(post.name.clone()));
        // burst-type the whole query without letting any search land
        board.start_search();
        for c in "0x401000".chars() {
            board.handle_key(press(KeyCode::Char(c))).await;
        }
        // flight one answered the first key alone and respawns for the full
        // text; flight two answers the full text and is applied
        pump_search(&mut board, &rx).await;
        pump_search(&mut board, &rx).await;
        let BoardMode::Search(search) = &board.mode else {
            panic!("the board should be searching");
        };
        assert_eq!(search.hits.len(), 1);
        assert_eq!(search.answered, "0x401000");
        // no third search was ever spawned for the six keys in between
        assert!(matches!(rx.try_recv(), Ok(None)));
    }

    /// Enter never jumps while the hits still answer an older query
    #[tokio::test]
    async fn enter_is_ignored_while_results_are_pending() {
        // seed a comment so the first query lands real hits
        let forum = Arc::new(ForumState::new());
        let post = forum
            .add_user_question("Entry point".to_owned(), "where does it start?".to_owned())
            .await
            .expect("the question should seed");
        forum
            .add_user_comment(
                ParentRef::Post(post.name.clone()),
                "the xor loop lives at 0x401000".to_owned(),
                Stance::Neutral,
                Kind::Statement,
            )
            .await
            .expect("the comment should seed");
        let (mut board, rx) = board_with_events(forum.clone());
        board.show();
        board.update(forum.view().await, ParentRef::Post(post.name.clone()));
        // settle a first query so hits are on screen
        board.start_search();
        type_and_settle(&mut board, &rx, "xor").await;
        // extend the query: the shown hits now answer the older text
        for c in " loop".chars() {
            board.handle_key(press(KeyCode::Char(c))).await;
        }
        // Enter on the pending search must not jump anywhere
        board.handle_key(press(KeyCode::Enter)).await;
        assert!(matches!(board.mode, BoardMode::Search(_)));
        // flight one answered "xor " and respawns; flight two lands "xor loop"
        pump_search(&mut board, &rx).await;
        pump_search(&mut board, &rx).await;
        // with the live results applied, Enter jumps as usual
        board.handle_key(press(KeyCode::Enter)).await;
        assert!(matches!(board.mode, BoardMode::Browse));
    }

    /// Results landing after Esc are dropped instead of resurrecting the search
    #[tokio::test]
    async fn stale_results_drop_after_esc() {
        // seed a post so the spawned search has something to rank
        let forum = Arc::new(ForumState::new());
        let post = forum
            .add_user_question(
                "Entry point".to_owned(),
                "where does the xor start?".to_owned(),
            )
            .await
            .expect("the question should seed");
        let (mut board, rx) = board_with_events(forum.clone());
        board.show();
        board.update(forum.view().await, ParentRef::Post(post.name.clone()));
        // type one key (spawning a flight), then abandon the search
        board.start_search();
        board.handle_key(press(KeyCode::Char('x'))).await;
        board.handle_key(press(KeyCode::Esc)).await;
        assert!(matches!(board.mode, BoardMode::Browse));
        // the orphan flight's results arrive and are quietly discarded
        pump_search(&mut board, &rx).await;
        assert!(matches!(board.mode, BoardMode::Browse));
    }

    /// Agent posts landing mid-search refresh the hits without a keystroke
    #[tokio::test]
    async fn forum_update_mid_search_refreshes_hits() {
        // seed one match, search it, and settle on a single hit
        let forum = Arc::new(ForumState::new());
        let post = forum
            .add_user_question("Entry point".to_owned(), "where does it start?".to_owned())
            .await
            .expect("the question should seed");
        let picked = forum
            .add_user_comment(
                ParentRef::Post(post.name.clone()),
                "the xor loop lives at 0x401000".to_owned(),
                Stance::Neutral,
                Kind::Statement,
            )
            .await
            .expect("the comment should seed");
        let (mut board, rx) = board_with_events(forum.clone());
        board.show();
        board.update(forum.view().await, ParentRef::Post(post.name.clone()));
        board.start_search();
        type_and_settle(&mut board, &rx, "xor").await;
        let BoardMode::Search(search) = &board.mode else {
            panic!("the board should be searching");
        };
        assert_eq!(search.hits.len(), 1);
        // a second matching comment arrives while the search sits open
        let comment = forum
            .add_user_comment(
                ParentRef::Post(post.name.clone()),
                "a second xor sighting".to_owned(),
                Stance::Neutral,
                Kind::Statement,
            )
            .await
            .expect("the comment should seed");
        board.update(forum.view().await, ParentRef::Comment(comment.id));
        // the refresh search lands without any keystroke being typed
        pump_search(&mut board, &rx).await;
        let BoardMode::Search(search) = &board.mode else {
            panic!("the board should still be searching");
        };
        assert_eq!(search.hits.len(), 2);
        assert_eq!(search.answered, "xor");
        // the analyst's picked hit survives the refresh even if it reordered
        assert_eq!(
            search.hits[search.selected].target,
            ParentRef::Comment(picked.id)
        );
    }

    /// Another board's same-text results never touch this board's search
    #[tokio::test]
    async fn foreign_same_text_results_never_apply() {
        // seed a post so a real flight has something to rank
        let forum = Arc::new(ForumState::new());
        let post = forum
            .add_user_question(
                "Entry point".to_owned(),
                "where does the xor start?".to_owned(),
            )
            .await
            .expect("the question should seed");
        let (mut board, rx) = board_with_events(forum.clone());
        board.show();
        board.update(forum.view().await, ParentRef::Post(post.name.clone()));
        // open a search with one flight airborne for the typed text
        board.start_search();
        board.handle_key(press(KeyCode::Char('x'))).await;
        // a same-text result from a different board must change nothing: no
        // hits applied, and the single-flight latch stays held
        let fake = vec![SearchHit {
            target: ParentRef::Post(post.name.clone()),
            reference: format!("#{}", post.name),
            title: "spoofed".to_owned(),
            snippet: "spoofed".to_owned(),
            kind: Kind::Statement,
            stance: None,
            author: "tester".to_owned(),
            score: 1.0,
        }];
        board.apply_search_results(Uuid::new_v4(), 1, "x", fake, vec!["x".to_owned()]);
        let BoardMode::Search(search) = &board.mode else {
            panic!("the board should be searching");
        };
        assert!(search.hits.is_empty());
        assert!(search.inflight.is_some());
        // the board's own flight still lands and settles the search
        pump_search(&mut board, &rx).await;
        let BoardMode::Search(search) = &board.mode else {
            panic!("the board should still be searching");
        };
        assert_eq!(search.answered, "x");
        assert!(search.inflight.is_none());
    }

    /// Starting a search never destroys a reply draft in progress
    #[tokio::test]
    async fn start_search_refuses_while_a_reply_is_in_progress() {
        let forum = Arc::new(ForumState::new());
        let (mut board, _rx) = board_with_events(forum);
        board.show();
        board.update(chain_view(1), ParentRef::Comment(1));
        // start a reply on the selected post and type a draft into it
        board.handle_key(press(KeyCode::Char('r'))).await;
        board.handle_key(press(KeyCode::Char('d'))).await;
        // a search request must refuse rather than drop the typed draft
        board.start_search();
        let BoardMode::Compose(compose) = &board.mode else {
            panic!("the reply draft should survive a search request");
        };
        assert_eq!(compose.text.content, "d");
        assert!(board.notice.is_some());
    }

    /// Highlighting splits spans without changing a row's text or width
    #[test]
    fn highlighted_rows_keep_their_width_and_text() {
        // lay out a small thread and highlight the deepest comment's card
        let (items, lines) = layout(&chain_view(2), &HashSet::new(), 46, None);
        let target = index_of(&items, 2);
        let terms = vec!["body".to_owned()];
        // join a drawn line back into plain text, ignoring styles
        let flatten = |line: &Line| -> String {
            line.spans
                .iter()
                .map(|span| span.content.to_string())
                .collect()
        };
        // every line of the card keeps its text and width when highlighted
        let mut matched = 0;
        for line in lines.iter().filter(|line| line.item == Some(target)) {
            let plain = line.to_line(CARD_BORDER);
            let painted = line.to_line_highlighted(CARD_BORDER, &terms);
            // the text reads the same with and without the highlight
            assert_eq!(flatten(&painted), flatten(&plain));
            // the row still fills the full layout width
            assert_eq!(painted.width(), 46, "{}", flatten(&painted));
            // count the spans carrying the match style
            matched += painted
                .spans
                .iter()
                .filter(|span| {
                    span.style.fg == SEARCH_MATCH.fg
                        && span.style.add_modifier.contains(Modifier::REVERSED)
                })
                .count();
        }
        // the body row carries at least one highlighted span
        assert!(matched >= 1);
    }

    /// Enter on a hit highlights its matched text and reveals the match line
    #[tokio::test]
    async fn enter_jump_highlights_and_reveals_the_match() {
        use ratatui::{Terminal, backend::TestBackend};
        // seed a question whose answer buries the match deep in a long body
        let forum = Arc::new(ForumState::new());
        let post = forum
            .add_user_question("Entry point".to_owned(), "where does it start?".to_owned())
            .await
            .expect("the question should seed");
        let padding = "nothing to see here. ".repeat(60);
        let comment = forum
            .add_user_comment(
                ParentRef::Post(post.name.clone()),
                format!("{padding} the xor loop lives at 0x401000. {padding}"),
                Stance::Neutral,
                Kind::Statement,
            )
            .await
            .expect("the comment should seed");
        // show the board and search for the buried term
        let (mut board, rx) = board_with_events(forum.clone());
        board.show();
        board.update(forum.view().await, ParentRef::Comment(comment.id));
        board.start_search();
        type_and_settle(&mut board, &rx, "xor").await;
        board.handle_key(press(KeyCode::Enter)).await;
        // the jump kept the matched terms on the comment it landed on
        let Some((target, terms)) = board.highlight.clone() else {
            panic!("the jump should highlight the hit");
        };
        assert_eq!(target, ParentRef::Comment(comment.id));
        assert!(terms.contains(&"xor".to_owned()));
        // the match is anchored below the card's first line, not on it
        let match_line = board
            .highlight_line
            .expect("the match line should be anchored");
        let start = board.item_starts[index_of(&board.items, comment.id)];
        assert!(match_line > start, "{match_line} vs {start}");
        // drawing paints the matched text in reversed yellow on screen
        let mut terminal = Terminal::new(TestBackend::new(60, 30)).unwrap();
        terminal.draw(|f| board.render(f, f.area())).unwrap();
        let buffer = terminal.backend().buffer();
        let mut painted = Vec::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                let cell = &buffer[(x, y)];
                if cell.fg == Color::Yellow && cell.modifier.contains(Modifier::REVERSED) {
                    painted.push((x, y));
                }
            }
        }
        // the match is painted and revealed in the upper half of the board
        assert!(!painted.is_empty());
        assert!(painted.iter().any(|(_, y)| *y < 15), "{painted:?}");
    }

    /// Moving the selection or starting a new search clears the highlight
    #[tokio::test]
    async fn moving_the_selection_clears_the_highlight() {
        // seed a question and a matching comment to jump to
        let forum = Arc::new(ForumState::new());
        let post = forum
            .add_user_question("Entry point".to_owned(), "where does it start?".to_owned())
            .await
            .expect("the question should seed");
        forum
            .add_user_comment(
                ParentRef::Post(post.name.clone()),
                "the xor loop lives here".to_owned(),
                Stance::Neutral,
                Kind::Statement,
            )
            .await
            .expect("the comment should seed");
        let (mut board, rx) = board_with_events(forum.clone());
        board.show();
        board.update(forum.view().await, ParentRef::Post(post.name.clone()));
        // search and jump, landing a highlight on the comment
        board.start_search();
        type_and_settle(&mut board, &rx, "xor").await;
        board.handle_key(press(KeyCode::Enter)).await;
        assert!(board.highlight.is_some());
        // keys that keep the selection on the hit keep the highlight
        board.handle_key(press(KeyCode::Down)).await;
        assert!(board.highlight.is_some());
        // moving off the hit retires it
        board.handle_key(press(KeyCode::Up)).await;
        assert!(board.highlight.is_none());
        // a fresh search retires the next jump's highlight the same way
        board.start_search();
        type_and_settle(&mut board, &rx, "xor").await;
        board.handle_key(press(KeyCode::Enter)).await;
        assert!(board.highlight.is_some());
        board.start_search();
        assert!(board.highlight.is_none());
    }

    /// Build a board searching over two seeded posts, ready to preview
    ///
    /// The test event channel rides along so the board's spawned searches keep
    /// a live receiver for as long as the test drives the board.
    async fn searching_board() -> (MessageBoard, kanal::AsyncReceiver<AppEvent>) {
        // seed two posts with long bodies so the preview has rows to window
        let forum = Arc::new(ForumState::new());
        for (title, flavor) in [("first entry", "alpha"), ("second entry", "beta")] {
            let body = format!("the {flavor} xor key material. ").repeat(80);
            forum
                .add_user_question(title.to_owned(), body)
                .await
                .expect("the question should seed");
        }
        // show the board and type a query both posts match
        let (mut board, rx) = board_with_events(forum.clone());
        board.show();
        board.update(
            forum.view().await,
            ParentRef::Post("first-entry".to_owned()),
        );
        board.start_search();
        type_and_settle(&mut board, &rx, "xor").await;
        (board, rx)
    }

    /// Render the picker over a sentinel-filled anchor and hand back the buffer
    ///
    /// # Arguments
    ///
    /// * `board` - The board whose picker to draw
    /// * `anchor` - The chat-log anchor to draw it over
    fn draw_overlay(board: &mut MessageBoard, anchor: Rect) -> ratatui::buffer::Buffer {
        use ratatui::{Terminal, backend::TestBackend};
        // draw a sentinel into the whole anchor, then the picker over it
        let mut terminal = Terminal::new(TestBackend::new(anchor.width, anchor.height)).unwrap();
        terminal
            .draw(|f| {
                let sentinel =
                    Paragraph::new(vec![Line::from("#".repeat(anchor.width as usize)); 40]);
                f.render_widget(sentinel, anchor);
                board.render_search_overlay(f, anchor);
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    /// Join one buffer row back into the string it shows
    ///
    /// # Arguments
    ///
    /// * `buffer` - The drawn buffer to read
    /// * `y` - The row to join
    /// * `xs` - The columns to include
    fn row_text(buffer: &ratatui::buffer::Buffer, y: u16, xs: std::ops::Range<u16>) -> String {
        // join every cell's symbol, ignoring styles
        xs.map(|x| buffer[(x, y)].symbol().to_string()).collect()
    }

    /// The picker covers the anchor's top and leaves its bottom rows alone
    #[tokio::test]
    async fn overlay_covers_the_anchor_and_leaves_the_bottom_rows() {
        let (mut board, _rx) = searching_board().await;
        // a 60x40 anchor hosts a 30-row picker over a 10-row visible gap
        let buffer = draw_overlay(&mut board, Rect::new(0, 0, 60, 40));
        // the picker paints the popup grey over the anchor's top rows
        assert_eq!(buffer[(0, 0)].bg, Color::Rgb(60, 60, 60));
        assert_eq!(buffer[(0, 29)].bg, Color::Rgb(60, 60, 60));
        // the frame names the search and counts its hits
        let title = row_text(&buffer, 0, 0..60);
        assert!(title.contains("Search forum"), "{title}");
        assert!(title.contains("2 hits"), "{title}");
        // the bottom ten anchor rows still show the chat log beneath
        for y in 30..40 {
            assert_eq!(buffer[(0, y)].symbol(), "#", "row {y}");
        }
    }

    /// The picker lists the hits beside the query box it ranks them for
    #[tokio::test]
    async fn overlay_lists_hits_beside_the_query_box() {
        let (mut board, _rx) = searching_board().await;
        let buffer = draw_overlay(&mut board, Rect::new(0, 0, 60, 40));
        // the typed query shows inside the query box on the left
        assert!(row_text(&buffer, 2, 0..29).contains("xor"));
        // both hit references list under it, one per row
        let hits = row_text(&buffer, 4, 0..29) + row_text(&buffer, 5, 0..29).as_str();
        assert!(hits.contains("first-entry"), "{hits}");
        assert!(hits.contains("second-entry"), "{hits}");
        // the selected hit's row is reversed so Enter's target is obvious
        assert!(buffer[(2, 4)].modifier.contains(Modifier::REVERSED));
        assert!(!buffer[(2, 5)].modifier.contains(Modifier::REVERSED));
    }

    /// The preview pane follows the hit selection and highlights the terms
    #[tokio::test]
    async fn overlay_preview_follows_the_hit_selection_and_highlights_terms() {
        let (mut board, _rx) = searching_board().await;
        let anchor = Rect::new(0, 0, 60, 40);
        // the preview header names the selected hit right of the divider
        let buffer = draw_overlay(&mut board, anchor);
        let before = row_text(&buffer, 1, 31..60);
        // the matched terms paint reversed yellow in the preview pane
        let yellow = |buffer: &ratatui::buffer::Buffer| {
            let mut count = 0;
            for y in 1..30 {
                for x in 31..60 {
                    let cell = &buffer[(x, y)];
                    if cell.fg == Color::Yellow && cell.modifier.contains(Modifier::REVERSED) {
                        count += 1;
                    }
                }
            }
            count
        };
        assert!(yellow(&buffer) > 0);
        // and bold yellow in the hit list on the left
        let flat = (1..30)
            .flat_map(|y| (1..30).map(move |x| (x, y)))
            .any(|(x, y)| {
                let cell = &buffer[(x, y)];
                cell.fg == Color::Yellow
                    && cell.modifier.contains(Modifier::BOLD)
                    && !cell.modifier.contains(Modifier::REVERSED)
            });
        assert!(flat);
        // moving the hit selection retargets the preview and the list marker
        board.handle_key(press(KeyCode::Down)).await;
        let buffer = draw_overlay(&mut board, anchor);
        let after = row_text(&buffer, 1, 31..60);
        assert_ne!(before, after);
        assert!(yellow(&buffer) > 0);
        assert!(buffer[(2, 5)].modifier.contains(Modifier::REVERSED));
    }

    /// A narrow picker drops the preview pane and keeps the hits readable
    #[tokio::test]
    async fn overlay_skips_the_preview_pane_when_narrow() {
        let (mut board, _rx) = searching_board().await;
        // a 40-wide anchor leaves a 38-column interior, too narrow to split
        let buffer = draw_overlay(&mut board, Rect::new(0, 0, 40, 40));
        // the query and the hits still draw at full width
        assert!(row_text(&buffer, 2, 0..38).contains("xor"));
        let hits = row_text(&buffer, 4, 0..38) + row_text(&buffer, 5, 0..38).as_str();
        assert!(hits.contains("first-entry"), "{hits}");
        // no divider column splits the interior below the query box
        let row = row_text(&buffer, 10, 2..38);
        assert!(!row.contains('│'), "{row}");
    }

    /// An unfocused board hides the picker, but an empty search still draws
    #[tokio::test]
    async fn overlay_hides_when_unfocused_but_draws_its_empty_state() {
        let (mut board, _rx) = searching_board().await;
        let anchor = Rect::new(0, 0, 60, 40);
        // an unfocused board leaves the chat alone entirely
        board.focused = false;
        let buffer = draw_overlay(&mut board, anchor);
        assert!((0..40).all(|y| buffer[(0, y)].symbol() == "#"));
        // a focused search with nothing typed yet still shows the picker,
        // since the query box the analyst types into lives inside it
        board.focused = true;
        board.start_search();
        let buffer = draw_overlay(&mut board, anchor);
        assert_eq!(buffer[(0, 0)].bg, Color::Rgb(60, 60, 60));
        let states: String = (1..30).map(|y| row_text(&buffer, y, 0..29)).collect();
        assert!(states.contains("(type to search)"), "{states}");
        // the bottom rows stay visible beneath the empty picker too
        for y in 30..40 {
            assert_eq!(buffer[(0, y)].symbol(), "#", "row {y}");
        }
    }

    /// The board leaves its own search UI out while the picker is active
    #[tokio::test]
    async fn board_suppresses_its_search_ui_while_the_chat_overlay_is_active() {
        use ratatui::{Terminal, backend::TestBackend};
        let (mut board, _rx) = searching_board().await;
        // render the board itself with the picker claimed by the chat side
        let draw_board = |board: &mut MessageBoard| {
            let mut terminal = Terminal::new(TestBackend::new(60, 30)).unwrap();
            terminal.draw(|f| board.render(f, f.area())).unwrap();
            let buffer = terminal.backend().buffer().clone();
            // join the whole buffer so text asserts can scan it
            let text: String = (0..30)
                .map(|y| row_text(&buffer, y, 0..60) + "\n")
                .collect();
            (buffer, text)
        };
        board.chat_overlay = true;
        let (buffer, text) = draw_board(&mut board);
        // no hits popup, no query box, and no popup grey anywhere on the board
        assert!(!text.contains("Search results"), "{text}");
        assert!(!text.contains("Search forum"), "{text}");
        let grey = Color::Rgb(60, 60, 60);
        let greys = (0..30)
            .flat_map(|y| (0..60).map(move |x| (x, y)))
            .filter(|(x, y)| buffer[(*x, *y)].bg == grey)
            .count();
        assert_eq!(greys, 0);
        // the footer still hints at the search the picker owns
        assert!(text.contains("type to search"), "{text}");
        // without the picker the board falls back to its own search UI
        board.chat_overlay = false;
        let (_, text) = draw_board(&mut board);
        assert!(text.contains("Search results"), "{text}");
        assert!(text.contains("Search forum"), "{text}");
    }

    /// Clicking a picker hit row selects it; dead zones change nothing
    #[tokio::test]
    async fn overlay_click_selects_a_hit_row() {
        use ratatui::{Terminal, backend::TestBackend};
        let (mut board, _rx) = searching_board().await;
        let anchor = Rect::new(0, 0, 60, 40);
        draw_overlay(&mut board, anchor);
        // the picker's selected hit, read back out of the search mode
        let selected = |board: &MessageBoard| match &board.mode {
            BoardMode::Search(search) => search.selected,
            _ => panic!("the board should be searching"),
        };
        // the picker claims clicks inside it and a hit row click selects it
        assert!(board.overlay_contains(5, 5));
        board.handle_overlay_click(5, 5);
        assert_eq!(selected(&board), 1);
        // clicks on the query box or below the last hit change nothing
        board.handle_overlay_click(5, 2);
        board.handle_overlay_click(5, 10);
        assert_eq!(selected(&board), 1);
        // the visible chat gap below the picker is not the picker's
        assert!(!board.overlay_contains(0, 35));
        // an unfocused board gives its clicks back to the chat
        board.focused = false;
        assert!(!board.overlay_contains(5, 5));
        // a fallback board render clears the recorded picker rects
        board.focused = true;
        board.chat_overlay = false;
        let mut terminal = Terminal::new(TestBackend::new(60, 30)).unwrap();
        terminal.draw(|f| board.render(f, f.area())).unwrap();
        assert!(!board.overlay_contains(5, 5));
    }

    /// The picker only earns a rect for a focused search with room to draw
    #[tokio::test]
    async fn search_overlay_area_requires_focus_search_and_room() {
        let (mut board, _rx) = searching_board().await;
        // a roomy anchor leaves the ten-row gap beneath the picker
        let area = board.search_overlay_area(Rect::new(0, 0, 60, 40));
        assert_eq!(area, Some(Rect::new(0, 0, 60, 30)));
        // a short anchor shrinks the gap before the picker
        let area = board.search_overlay_area(Rect::new(0, 0, 60, 12));
        assert_eq!(area, Some(Rect::new(0, 0, 60, 7)));
        // anchors below the minimums draw nothing
        assert_eq!(board.search_overlay_area(Rect::new(0, 0, 60, 6)), None);
        assert_eq!(board.search_overlay_area(Rect::new(0, 0, 10, 40)), None);
        // an unfocused board keeps the search in the sidebar instead
        board.focused = false;
        assert_eq!(board.search_overlay_area(Rect::new(0, 0, 60, 40)), None);
        board.focused = true;
        // leaving search mode retires the picker
        board.handle_key(press(KeyCode::Esc)).await;
        assert_eq!(board.search_overlay_area(Rect::new(0, 0, 60, 40)), None);
    }
}
