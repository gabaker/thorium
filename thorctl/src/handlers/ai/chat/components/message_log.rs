//! The message log component for messages between the user and ThorPal
//!
//! The log renders as a flat, left-aligned feed rather than bordered bubbles:
//! meaningful content (a user's message, an agent's response, its thinking, and
//! forum posts/comments) is shown prominently, while routine tool calls collapse
//! to a single dim grey line prefixed with a green/red status marker and expand
//! on click. Every entry is attributed to the agent that produced it.

use ratatui::{prelude::*, widgets::Paragraph};
use serde_json::{Map, Value};
use std::cmp::Ordering;
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use uuid::Uuid;

use super::Theme;
use super::markdown::markdown_lines;
use super::text_wrap::wrap_unicode_preserve_ws;
use crate::handlers::ai::chat::ChatEntry;

/// The tool name rig's built-in thinking tool uses.
const THINK_TOOL: &str = "think";
/// The forum tool names whose output is surfaced prominently.
const FORUM_TOOLS: [&str; 2] = ["create_post", "create_comment"];
/// The columns a thinking block is indented by, to set it apart.
const THINKING_INDENT: usize = 2;
/// The most log lines kept per tool call; the oldest lines are dropped first.
const LOG_BUFFER_CAP: usize = 500;
/// The log lines shown under a running tool call by default.
const LOG_TAIL_COLLAPSED: usize = 5;
/// The log lines shown when the user enlarges a running call's log window.
const LOG_TAIL_EXPANDED: usize = 20;
/// The columns a live log strip is indented by, to set it apart.
const LOG_INDENT: usize = 2;

/// The content for a single message log entry
#[derive(Debug, Clone)]
pub enum EntryKind {
    /// A message from the user
    User(String),
    /// An assistant response from an agent, attributed to it when known
    Response {
        /// The display name of the agent that produced the response, if known
        agent: Option<String>,
        /// The response text
        text: String,
    },
    /// An error surfaced to the user (e.g. a failed turn)
    Error(String),
    /// A tool call and any results, classified for rendering by its tool name
    ///
    /// The `think` tool renders as prominent thinking, the forum tools render as
    /// prominent posts/comments, and every other tool renders as a dim,
    /// click-to-expand grey line with a success/failure status marker.
    Tool {
        /// The id for this tool call
        id: Uuid,
        /// The display name of the agent that made the call, if known
        agent: Option<String>,
        /// The name of the tool that was called
        name: String,
        /// The arguments used for this tool
        arguments: Option<Map<String, Value>>,
        /// The results collected for this tool
        results: Vec<String>,
        /// The captured log lines streamed while this call runs, capped at
        /// [`LOG_BUFFER_CAP`] (the oldest lines are dropped first)
        logs: VecDeque<String>,
        /// Whether the call succeeded, once its result has arrived
        success: Option<bool>,
    },
}

impl EntryKind {
    /// Whether this entry is a routine tool call the user can expand/collapse
    ///
    /// Only a generic tool call is clickable; a user message, response, error,
    /// thinking block, or forum post is always shown in full.
    fn is_expandable(&self) -> bool {
        // only a generic (non-think, non-forum) tool call collapses
        match self {
            EntryKind::Tool { name, .. } => {
                name != THINK_TOOL && !FORUM_TOOLS.contains(&name.as_str())
            }
            _ => false,
        }
    }

    /// The tool-call id this entry owns, if it is an expandable tool call
    fn tool_id(&self) -> Option<Uuid> {
        // only an expandable tool call carries an id a click can toggle
        match self {
            EntryKind::Tool { id, .. } if self.is_expandable() => Some(*id),
            _ => None,
        }
    }
}

/// The outcome of folding a chat message into the entry list
///
/// A message either starts a fresh entry, mutates an existing entry (a tool
/// result appended to its call entry), or is not renderable on its own.
enum EntryFold {
    /// This message produced a brand new entry to append
    New(Box<MessageBubble>),
    /// This message mutated an existing entry, which `from_msg` marked stale
    Updated,
    /// This message produced no renderable change
    Ignored,
}

/// A single message log entry that can render itself into `RenderRow`s.
#[derive(Debug, Clone)]
struct MessageBubble {
    /// The core content that is in this entry
    content: EntryKind,
    /// The cached rendered rows for this entry, rebuilt only when it changes
    rows: Vec<RenderRow>,
    /// Whether this entry's cached rows are stale and need rebuilding
    dirty: bool,
}

impl MessageBubble {
    /// Create a new entry from its classified content.
    ///
    /// # Arguments
    ///
    /// * `content` - The classified content for this entry
    pub fn new(content: EntryKind) -> Self {
        Self {
            content,
            rows: Vec::new(),
            dirty: true,
        }
    }

    /// Check if this entry is the tool call for the given id
    ///
    /// # Arguments
    ///
    /// * `id` - The id of the tool call entry to look for
    pub fn is_tool_call(&self, id: Uuid) -> bool {
        // only a tool entry carries a tool call id
        match &self.content {
            EntryKind::Tool { id: entry_id, .. } => id == *entry_id,
            _ => false,
        }
    }

    /// Fold a chat message into the entry list
    ///
    /// Returns an [`EntryFold`] describing whether the message created a new
    /// entry, mutated an existing one (a tool result appended to its call
    /// entry), or was not renderable, so the caller can invalidate only the
    /// entry that actually changed.
    ///
    /// # Arguments
    ///
    /// * `msg_kind` - The chat message to fold into the entries
    /// * `entries` - The existing entries, searched to merge a tool result
    pub fn from_msg(msg_kind: &ChatEntry, entries: &mut [MessageBubble]) -> EntryFold {
        // get the info to build the entry for this message kind
        match msg_kind {
            ChatEntry::User(msg) => {
                // build this new user entry
                let content = EntryKind::User(msg.clone());
                EntryFold::New(Box::new(MessageBubble::new(content)))
            }
            ChatEntry::Ai { agent, text } => {
                // a failure is pushed as assistant text prefixed with a
                // warning glyph; surface it as a prominent error entry
                let content = if text.trim_start().starts_with('⚠') {
                    EntryKind::Error(text.clone())
                } else {
                    EntryKind::Response {
                        agent: agent.clone(),
                        text: text.clone(),
                    }
                };
                EntryFold::New(Box::new(MessageBubble::new(content)))
            }
            ChatEntry::ToolCall {
                id,
                agent,
                name,
                arguments,
            } => {
                // build the content for this tool call entry; its render class is
                // decided from the tool name when the rows are built
                let content = EntryKind::Tool {
                    id: *id,
                    agent: agent.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                    results: Vec::with_capacity(1),
                    logs: VecDeque::new(),
                    success: None,
                };
                EntryFold::New(Box::new(MessageBubble::new(content)))
            }
            ChatEntry::ToolLog { id, line } => {
                // find the tool-call entry this log line belongs to, searching
                // from the back since the matching call is the most recent one
                let found = entries
                    .iter_mut()
                    .rev()
                    .find(|entry| entry.is_tool_call(*id));
                // append the line to that entry and report which one changed
                match found {
                    Some(entry) => {
                        // push this line into the call's bounded log buffer
                        if let EntryKind::Tool { logs, .. } = &mut entry.content {
                            // drop the oldest line once the buffer is full
                            if logs.len() == LOG_BUFFER_CAP {
                                logs.pop_front();
                            }
                            logs.push_back(line.clone());
                        }
                        // the entry's cached rows are now stale
                        entry.dirty = true;
                        // report that an existing entry was mutated
                        EntryFold::Updated
                    }
                    // no matching call entry: nothing renderable changed
                    None => EntryFold::Ignored,
                }
            }
            ChatEntry::ToolResult {
                id,
                result,
                success,
            } => {
                // find the tool-call entry this result belongs to, searching from
                // the back since the matching call is the most recent one
                let found = entries
                    .iter_mut()
                    .rev()
                    .find(|entry| entry.is_tool_call(*id));
                // merge the result into that entry and report which one changed
                match found {
                    Some(entry) => {
                        // add this result and record the call's success status
                        if let EntryKind::Tool {
                            results,
                            success: entry_success,
                            ..
                        } = &mut entry.content
                        {
                            results.push(result.clone());
                            *entry_success = *success;
                        }
                        // the entry's cached rows are now stale
                        entry.dirty = true;
                        // report that an existing entry was mutated
                        EntryFold::Updated
                    }
                    // no matching call entry: nothing renderable changed
                    None => EntryFold::Ignored,
                }
            }
        }
    }

    /// Rebuild this entry's cached rows for the given render area
    ///
    /// Fills `self.rows` from scratch and clears the dirty flag. This is the only
    /// place an entry is (re)wrapped, so it runs only when the entry is new, its
    /// content changed, or the render width changed. Every row starts at the
    /// left edge of `area`; there are no bubbles or borders.
    ///
    /// # Arguments
    ///
    /// * `area` - The area the log is rendered into, bounding the entry width
    /// * `theme` - The theme used to style the entry
    /// * `expanded` - The set of tool-call ids the user has expanded
    /// * `log_expanded` - The set of tool-call ids with an enlarged log window
    fn rebuild_rows(
        &mut self,
        area: Rect,
        theme: &Arc<Theme>,
        expanded: &HashSet<Uuid>,
        log_expanded: &HashSet<Uuid>,
    ) {
        // the tool-call this entry owns (if any), tagged onto every emitted row
        // so a click anywhere on the entry toggles the right part of it
        let id = self.content.tool_id();
        // reserve a right margin so text never touches the terminal edge
        let width = area.width.saturating_sub(1).max(1) as usize;
        // build the styled lines for this entry based on its class
        let lines = self.render_lines(width, theme, expanded, log_expanded);
        // every line is left-aligned at the area's left edge, tagged with the
        // part of the entry it belongs to so clicks route to the right toggle
        let rows = lines
            .into_iter()
            .map(|(line, kind)| RenderRow {
                x: area.x,
                line,
                owner: id.map(|id| match kind {
                    RowKind::Entry => RowOwner::Entry(id),
                    RowKind::Log => RowOwner::Log(id),
                }),
            })
            .collect();
        // store the freshly built rows and mark the cache clean
        self.rows = rows;
        self.dirty = false;
    }

    /// Render this entry into styled, wrapped lines for the given width
    ///
    /// Each line carries its [`RowKind`] so the caller can route a click on it
    /// to the right toggle; every renderer but the tool call emits only
    /// [`RowKind::Entry`] rows.
    ///
    /// # Arguments
    ///
    /// * `width` - The maximum display width for each line
    /// * `theme` - The theme used to style the lines
    /// * `expanded` - The set of tool-call ids the user has expanded
    /// * `log_expanded` - The set of tool-call ids with an enlarged log window
    fn render_lines(
        &self,
        width: usize,
        theme: &Arc<Theme>,
        expanded: &HashSet<Uuid>,
        log_expanded: &HashSet<Uuid>,
    ) -> Vec<(Line<'static>, RowKind)> {
        // tag a plain renderer's lines as entry rows for click routing
        let as_entry_rows = |lines: Vec<Line<'static>>| {
            lines
                .into_iter()
                .map(|line| (line, RowKind::Entry))
                .collect()
        };
        // dispatch to the renderer for this entry's class
        match &self.content {
            // a user's own message, shown plainly and prominently
            EntryKind::User(text) => as_entry_rows(styled_block(text, width, theme.messages.user)),
            // an agent's response, headed by who produced it
            EntryKind::Response { agent, text } => {
                as_entry_rows(render_response(agent.as_deref(), text, width, theme))
            }
            // a failure surfaced prominently in red
            EntryKind::Error(text) => {
                as_entry_rows(styled_block(text, width, theme.messages.error))
            }
            // a tool call, whose render class comes from its tool name
            EntryKind::Tool {
                id,
                agent,
                name,
                arguments,
                results,
                logs,
                success,
            } => {
                // thinking and forum output are shown prominently in full
                if name == THINK_TOOL {
                    as_entry_rows(render_thinking(
                        agent.as_deref(),
                        arguments,
                        results,
                        width,
                        theme,
                    ))
                } else if FORUM_TOOLS.contains(&name.as_str()) {
                    as_entry_rows(render_forum(
                        agent.as_deref(),
                        name,
                        arguments,
                        width,
                        theme,
                    ))
                } else {
                    // a routine tool call collapses to one grey line by default
                    render_tool_call(
                        agent.as_deref(),
                        name,
                        arguments,
                        results,
                        logs,
                        *success,
                        expanded.contains(id),
                        log_expanded.contains(id),
                        width,
                        theme,
                    )
                }
            }
        }
    }
}

/// A message log of messages between the user and an AI
#[derive(Debug, Clone)]
pub struct MessageLog {
    /// The gap between entries vertically in rows
    vertical_gap: u16,
    /// The theme to use
    theme: Arc<Theme>,
    /// Scroll in terminal rows from the top (0 = top).
    scroll_y: usize,
    /// If true, view is pinned to bottom (latest content).
    follow: bool,
    /// The rendered entries
    bubbles: Vec<MessageBubble>,
    /// The index of the last rendered entry
    last_bubbled: usize,
    /// The flat list of visible rows, assembled from each entry's cached rows
    rendered: Vec<RenderRow>,
    /// The set of tool-call ids the user has expanded (collapsed is the default)
    expanded: HashSet<Uuid>,
    /// The set of tool-call ids whose live log window is enlarged (the 5-line
    /// strip is the default)
    log_expanded: HashSet<Uuid>,
    /// The area the log was last rendered into, used to map a click to a row
    last_area: Rect,
    /// The render width the entry row caches were last built at; a change to it
    /// forces every entry to re-wrap (only happens on a terminal resize)
    cached_width: u16,
    /// Whether the flat visible-row list must be reassembled from the entry
    /// caches; set when an entry is added, updated, toggled, or re-wrapped
    layout_dirty: bool,
}

impl Default for MessageLog {
    fn default() -> Self {
        Self {
            vertical_gap: 1,
            theme: Arc::new(Theme::default()),
            scroll_y: 0,
            follow: true,
            bubbles: Vec::with_capacity(100),
            last_bubbled: 0,
            rendered: Vec::with_capacity(5000),
            expanded: HashSet::new(),
            log_expanded: HashSet::new(),
            last_area: Rect::default(),
            cached_width: 0,
            layout_dirty: true,
        }
    }
}

impl MessageLog {
    /// The area the log was last rendered into
    ///
    /// Recorded every frame the log draws, so an overlay (the forum-search
    /// preview) can anchor on the log's exact bounds, including the dynamic
    /// banner and throbber rows the chat page reserves beneath it.
    pub fn last_area(&self) -> Rect {
        self.last_area
    }

    /// Scroll up by N terminal rows.
    pub fn scroll_up(&mut self, rows: usize) {
        self.follow = false;
        self.scroll_y = self.scroll_y.saturating_sub(rows);
    }

    /// Scroll down by N terminal rows.
    /// You can call this without knowing max scroll; render() will clamp.
    pub fn scroll_down(&mut self, rows: usize) {
        self.scroll_y = self.scroll_y.saturating_add(rows);
        // follow will be re-enabled automatically when render() clamps at bottom
    }

    /// Page up (pass area.height as usize).
    pub fn page_up(&mut self, page_rows: usize) {
        self.scroll_up(page_rows);
    }

    /// Page down (pass area.height as usize).
    pub fn page_down(&mut self, page_rows: usize) {
        self.scroll_down(page_rows);
    }

    /// Jump to bottom and follow new content.
    pub fn end(&mut self) {
        self.follow = true;
    }

    /// Whether the view is currently pinned to bottom.
    pub fn is_following(&self) -> bool {
        self.follow
    }

    /// Clear all messages in our message log
    pub fn clear(&mut self) {
        // clear all entries
        self.bubbles.clear();
        // clear all rendered rows
        self.rendered.clear();
        // reset bubbled to 0
        self.last_bubbled = 0;
        // reset our vertical scroll to 0
        self.scroll_y = 0;
        // drop any expansion state since its entries no longer exist
        self.expanded.clear();
        self.log_expanded.clear();
        // the flat row list must be reassembled once new entries arrive
        self.layout_dirty = true;
    }

    /// If the user is at the bottom of the chat window then make sure to keep new messaged visible
    ///
    /// # Arguments
    ///
    /// * `area` - The area on screen we are rendering content too
    fn follow_messages(&mut self, area: Rect) {
        // get the height of the are we can render content into
        let renderable_height = area.height as usize;
        // determine how far down we can scroll
        let max_scroll = self.rendered.len().saturating_sub(renderable_height);
        // check if we are following new messages
        if self.follow {
            // we are following so set our scroll to the max distance
            self.scroll_y = max_scroll;
        } else {
            // check if we want to start following new messages
            match self.scroll_y.cmp(&max_scroll) {
                // we are not at the bottom so don't do anything
                Ordering::Less => (),
                // we are at the bottom enable follow mode
                Ordering::Equal => self.follow = true,
                // we have scrolled past our max distance
                Ordering::Greater => {
                    // limit our scroll to the max distance
                    self.scroll_y = max_scroll;
                    // enable follow mode
                    self.follow = true
                }
            }
        }
    }

    /// Build message entries for new messages and paint the visible rows
    ///
    /// # Arguments
    ///
    /// * `area` - The area on screen to render entries into
    /// * `messages` - The new (un-folded) messages to turn into entries
    /// * `frame` - The frame to render the visible rows to
    pub fn render_bubbles(&mut self, area: Rect, messages: &[ChatEntry], frame: &mut Frame) {
        // don't render anything if the window is tool small
        if area.width < 10 || area.height < 1 {
            return;
        }
        // remember the area we drew into so a later click can map back to a row
        self.last_area = area;
        // a width change invalidates every entry's wrapped rows, so mark them all
        // dirty; this happens on a terminal resize or when the message board
        // sidebar opens or closes, not on normal redraws
        if area.width != self.cached_width {
            for bubble in &mut self.bubbles {
                bubble.dirty = true;
            }
            self.cached_width = area.width;
            self.layout_dirty = true;
        }
        // fold each new message into the entry list, invalidating only the single
        // entry that actually changed rather than rebuilding the whole conversation
        for msg_kind in messages {
            match MessageBubble::from_msg(msg_kind, &mut self.bubbles) {
                // a new entry was produced: append it (its rows build lazily below)
                EntryFold::New(new_bubble) => {
                    self.bubbles.push(*new_bubble);
                    self.layout_dirty = true;
                }
                // an existing entry was mutated (a tool result merged in); from_msg
                // already marked it dirty, so we only need to re-assemble the layout
                EntryFold::Updated => self.layout_dirty = true,
                // nothing renderable changed
                EntryFold::Ignored => (),
            }
        }
        // rebuild the wrapped rows for any dirty entry; a clean entry keeps its
        // cached rows, so appending a tool result costs one entry, not the chat
        for bubble in &mut self.bubbles {
            if bubble.dirty {
                bubble.rebuild_rows(area, &self.theme, &self.expanded, &self.log_expanded);
                // a re-wrapped entry changed the flat row list too
                self.layout_dirty = true;
            }
        }
        // reassemble the flat visible-row list from the per-entry caches only when
        // the layout actually changed; a plain redraw/scroll reuses the existing list
        if self.layout_dirty {
            self.rendered.clear();
            for bubble in &self.bubbles {
                // copy this entry's cached rows into the flat list
                self.rendered.extend(bubble.rows.iter().cloned());
                // add a gap between each entry
                for _ in 0..self.vertical_gap {
                    self.rendered.push(RenderRow {
                        x: area.x,
                        line: Line::from(""),
                        owner: None,
                    });
                }
            }
            self.layout_dirty = false;
        }
        // render new messages if we are following new messages
        self.follow_messages(area);
        // get the final row we can render
        let end = (self.scroll_y + area.height as usize).min(self.rendered.len());
        // render the visible rows
        for (row_idx, r) in self.rendered[self.scroll_y..end].iter().enumerate() {
            // calculate the y value for where this line will be rendered too
            let y = area.y + row_idx as u16;
            // build the area for where this line will be rendered at
            let line_area = Rect {
                x: r.x,
                y,
                width: area.width.saturating_sub(r.x.saturating_sub(area.x)),
                height: 1,
            };
            // render this line
            frame.render_widget(Paragraph::new(r.line.clone()), line_area);
        }
    }

    /// Render the chat log onto a specific area in the terminal
    pub fn render(&mut self, frame: &mut Frame, area: Rect, messages: &[ChatEntry]) {
        // get the slice of new messages to turn into message entries
        let unbubbled = &messages[self.last_bubbled..];
        // add any new messages to our entry list
        self.render_bubbles(area, unbubbled, frame);
        // update our last folded message index
        self.last_bubbled = messages.len();
    }

    /// Toggle the tool-call entry under a click, if any, expanding/collapsing it
    ///
    /// Maps the clicked terminal cell back to the rendered row that occupies it
    /// (accounting for the current scroll offset) and, when that row belongs to
    /// a tool call, flips the toggle for the part it landed on: a head/detail
    /// row flips the args/result detail in `expanded`, while a live log row
    /// flips the log window size in `log_expanded`. Toggling forces a rebuild
    /// of the affected entry on the next render so it re-wraps with the new
    /// state.
    ///
    /// # Arguments
    ///
    /// * `x` - The column of the click
    /// * `y` - The row of the click
    pub fn handle_click(&mut self, x: u16, y: u16) -> bool {
        // ignore a click outside the area the log was last rendered into
        if x < self.last_area.x
            || x >= self.last_area.x + self.last_area.width
            || y < self.last_area.y
            || y >= self.last_area.y + self.last_area.height
        {
            return false;
        }
        // map the screen row to a rendered-row index using the current scroll
        let row_idx = self.scroll_y + (y - self.last_area.y) as usize;
        // find which tool-call part (if any) owns that row
        let owner = match self.rendered.get(row_idx).and_then(|row| row.owner) {
            Some(owner) => owner,
            // the click landed on a gap or a non-tool entry; nothing to toggle
            None => return false,
        };
        // route the click to the toggle for the part of the entry it landed on
        let id = match owner {
            // a head/detail row flips the args/result detail
            RowOwner::Entry(id) => {
                if !self.expanded.remove(&id) {
                    self.expanded.insert(id);
                }
                id
            }
            // a live log row flips the log window between 5 and 20 lines
            RowOwner::Log(id) => {
                if !self.log_expanded.remove(&id) {
                    self.log_expanded.insert(id);
                }
                id
            }
        };
        // mark only the toggled entry dirty so just it re-wraps on the next render;
        // the rest of the conversation keeps its cached rows
        if let Some(bubble) = self
            .bubbles
            .iter_mut()
            .find(|bubble| bubble.is_tool_call(id))
        {
            bubble.dirty = true;
        }
        true
    }
}

/// The part of a tool-call entry a rendered line belongs to
///
/// A tool call's rows are not all the same click target: its head/detail rows
/// toggle the args/result detail while its live log rows toggle the log
/// window, so each rendered line carries its kind for click routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowKind {
    /// A head or args/result detail row of the entry itself
    Entry,
    /// A row of the entry's live log strip
    Log,
}

/// What a rendered row belongs to, for click routing
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowOwner {
    /// A row of a tool-call entry: a click toggles its args/result detail
    Entry(Uuid),
    /// A row of a tool call's live log strip: a click toggles the log window
    Log(Uuid),
}

/// A line ready to be rendered to the terminal
#[derive(Debug, Clone)]
struct RenderRow {
    /// The x coordinate to start rendering this line at
    x: u16,
    /// The text to render
    line: Line<'static>,
    /// The tool-call part this row belongs to, if any, so a click can toggle it
    owner: Option<RowOwner>,
}

// ---------------- entry rendering ----------------

/// Build a header line naming the agent that produced an entry
///
/// The marker (e.g. `🤖`) is drawn in the content style and the agent name in
/// the theme's accent style so attribution stands out. When no agent is known
/// only the marker is shown.
///
/// # Arguments
///
/// * `marker` - The leading glyph/label for this entry class
/// * `agent` - The display name of the producing agent, if known
/// * `marker_style` - The style for the marker text
/// * `agent_style` - The style for the agent name
fn header_line(
    marker: &str,
    agent: Option<&str>,
    marker_style: Style,
    agent_style: Style,
) -> Line<'static> {
    // start with the class marker in the content style
    let mut spans = vec![Span::styled(marker.to_string(), marker_style)];
    // append the agent name as an accent when one is known
    if let Some(agent) = agent {
        spans.push(Span::styled(format!(" {agent}"), agent_style));
    }
    Line::from(spans)
}

/// Render an agent's response: an attribution header plus its wrapped text
///
/// # Arguments
///
/// * `agent` - The display name of the responding agent, if known
/// * `text` - The response text
/// * `width` - The maximum display width for each line
/// * `theme` - The theme used to style the response
fn render_response(
    agent: Option<&str>,
    text: &str,
    width: usize,
    theme: &Arc<Theme>,
) -> Vec<Line<'static>> {
    // head the response with who produced it
    let mut lines = vec![header_line(
        "🤖",
        agent,
        theme.messages.ai,
        theme.messages.agent,
    )];
    // then the markdown-rendered response text in the prominent AI style
    lines.extend(markdown_lines(text, width, theme.messages.ai));
    lines
}

/// Render an agent's thinking as a prominent, indented, dim/italic block
///
/// The thought text is taken from the tool's `thought` argument (available as
/// soon as the call starts) and falls back to the echoed result.
///
/// # Arguments
///
/// * `agent` - The display name of the thinking agent, if known
/// * `arguments` - The tool arguments carrying the thought
/// * `results` - The echoed result, used if the argument is absent
/// * `width` - The maximum display width for each line
/// * `theme` - The theme used to style the thinking block
fn render_thinking(
    agent: Option<&str>,
    arguments: &Option<Map<String, Value>>,
    results: &[String],
    width: usize,
    theme: &Arc<Theme>,
) -> Vec<Line<'static>> {
    // prefer the thought argument, falling back to the echoed result text
    let thought = arguments
        .as_ref()
        .and_then(|map| map.get("thought"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| results.join("\n"));
    // head the block with a thinking label attributed to the agent
    let mut lines = vec![header_line(
        "  thinking",
        agent,
        theme.messages.thinking,
        theme.messages.thinking,
    )];
    // wrap the thought and indent it so it reads as a nested aside
    let indent = " ".repeat(THINKING_INDENT);
    let inner_width = width.saturating_sub(THINKING_INDENT).max(1);
    for line in wrap_unicode_preserve_ws(&thought, inner_width) {
        lines.push(Line::from(Span::styled(
            format!("{indent}{line}"),
            theme.messages.thinking,
        )));
    }
    lines
}

/// Render a forum post or comment as a prominent, attributed block
///
/// # Arguments
///
/// * `agent` - The display name of the authoring agent, if known
/// * `tool` - The forum tool name (`create_post` or `create_comment`)
/// * `arguments` - The tool arguments carrying the post/comment fields
/// * `width` - The maximum display width for each line
/// * `theme` - The theme used to style the forum block
fn render_forum(
    agent: Option<&str>,
    tool: &str,
    arguments: &Option<Map<String, Value>>,
    width: usize,
    theme: &Arc<Theme>,
) -> Vec<Line<'static>> {
    // pull a string field out of the arguments, or an empty string when absent
    let field = |key: &str| -> String {
        arguments
            .as_ref()
            .and_then(|map| map.get(key))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    // build a header summarizing the post/comment and who wrote it
    let (marker, body) = if tool == "create_post" {
        // a post is referenced by #name and carries a title and body
        let header = format!("📌 #{} — {}", field("name"), field("title"));
        (header, field("body"))
    } else {
        // a comment replies to a parent reference and carries a body
        let header = format!("💬 reply {}", field("parent"));
        (header, field("body"))
    };
    // head the block with the summary, attributed to the authoring agent
    let mut lines = vec![header_line(
        &marker,
        agent,
        theme.messages.forum,
        theme.messages.agent,
    )];
    // then the markdown-rendered body in the prominent forum style
    lines.extend(markdown_lines(&body, width, theme.messages.forum));
    lines
}

/// Render a routine tool call, either collapsed to one line or expanded
///
/// Collapsed (the default) shows a status marker, the agent, the tool name with
/// a hint of its primary argument, and a compact result summary. Expanded adds
/// the pretty-printed arguments, the raw result, and any captured logs beneath.
/// While the call is still running, a live strip of its most recent log lines
/// (tagged [`RowKind::Log`] so a click on it resizes the window) renders under
/// the head line so the user can see a slow tool making progress.
///
/// # Arguments
///
/// * `agent` - The display name of the calling agent, if known
/// * `name` - The tool name
/// * `arguments` - The tool arguments
/// * `results` - The results collected so far
/// * `logs` - The log lines captured for this call so far
/// * `success` - Whether the call succeeded, once its result has arrived
/// * `expanded` - Whether the user has expanded this call's args/result detail
/// * `log_expanded` - Whether the user has enlarged this call's live log window
/// * `width` - The maximum display width for each line
/// * `theme` - The theme used to style the tool call
#[allow(clippy::too_many_arguments)]
fn render_tool_call(
    agent: Option<&str>,
    name: &str,
    arguments: &Option<Map<String, Value>>,
    results: &[String],
    logs: &VecDeque<String>,
    success: Option<bool>,
    expanded: bool,
    log_expanded: bool,
    width: usize,
    theme: &Arc<Theme>,
) -> Vec<(Line<'static>, RowKind)> {
    // pick the status marker and its style from the call's outcome
    let (marker, marker_style) = match success {
        // a finished call is flagged green on success, red on failure
        Some(true) => ("●", theme.messages.tool_ok),
        Some(false) => ("●", theme.messages.tool_err),
        // a call with no result yet is still running
        None => ("○", theme.messages.tool_pending),
    };
    // build the compact one-line summary text (name, arg hint, result summary)
    let expand_marker = if expanded { "▾" } else { "▸" };
    let arg_hint = primary_arg_hint(arguments);
    let summary = format!(
        "{expand_marker} {name}({arg_hint}) · {}",
        result_summary(results)
    );
    // assemble the head line: marker (colored) + agent + summary, all dim grey
    let mut head_spans = vec![
        Span::styled(marker.to_string(), marker_style),
        Span::raw(" "),
    ];
    // attribute the call to its agent when known
    if let Some(agent) = agent {
        head_spans.push(Span::styled(format!("{agent} · "), theme.messages.tool));
    }
    // clip the summary so the collapsed line never wraps past the width
    let used = head_spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum::<usize>();
    let summary = clip_unicode_to_width(&summary, width.saturating_sub(used).max(1));
    head_spans.push(Span::styled(summary, theme.messages.tool));
    let mut lines = vec![(Line::from(head_spans), RowKind::Entry)];
    // when expanded, add the full arguments, raw result, and captured logs
    // beneath the head line
    if expanded {
        let detail = expanded_tool_detail(arguments, results, logs);
        for line in wrap_unicode_preserve_ws(&detail, width) {
            lines.push((
                Line::from(Span::styled(line, theme.messages.tool)),
                RowKind::Entry,
            ));
        }
    }
    // while the call is still running, stream a live strip of its latest log
    // lines so a slow tool (e.g. the initial Ghidra analysis) shows progress
    if success.is_none() && !logs.is_empty() {
        // size the window from the user's toggle: 5 lines, or 20 when enlarged
        let tail = if log_expanded {
            LOG_TAIL_EXPANDED
        } else {
            LOG_TAIL_COLLAPSED
        };
        // clip (never wrap) each line so the strip's height stays fixed once
        // the buffer passes the tail length, keeping follow-scroll steady
        let indent = " ".repeat(LOG_INDENT);
        let inner_width = width.saturating_sub(LOG_INDENT).max(1);
        for line in logs.iter().rev().take(tail).rev() {
            let clipped = clip_unicode_to_width(line, inner_width);
            lines.push((
                Line::from(Span::styled(
                    format!("{indent}{clipped}"),
                    theme.messages.tool_log,
                )),
                RowKind::Log,
            ));
        }
        // close the strip with a footer that doubles as a stable click target
        let action = if log_expanded { "shrink" } else { "enlarge" };
        lines.push((
            Line::from(Span::styled(
                format!("{indent}↳ {} log lines · click to {action}", logs.len()),
                theme.messages.tool_log,
            )),
            RowKind::Log,
        ));
    }
    lines
}

/// Wrap `text` to `width` and style every resulting line uniformly
///
/// # Arguments
///
/// * `text` - The text to wrap
/// * `width` - The maximum display width for each line
/// * `style` - The style to apply to every line
fn styled_block(text: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    // wrap the text then wrap each line in a single styled span
    wrap_unicode_preserve_ws(text, width)
        .into_iter()
        .map(|line| Line::from(Span::styled(line, style)))
        .collect()
}

// ---------------- tool-call formatting ----------------

/// Summarize a tool call's result by size, or its running/empty status
///
/// # Arguments
///
/// * `results` - The results collected for this call so far
fn result_summary(results: &[String]) -> String {
    // summarize the result by size, or show a running status when there is none
    if results.is_empty() {
        // no result has arrived yet, so this call is still running
        "…running".to_string()
    } else {
        // join the collected results and describe their size at a glance
        let joined = results.join("\n");
        let lines = joined.lines().count();
        if lines > 1 {
            // multi-line output reads best as a line count
            format!("{lines} lines")
        } else {
            // single-line output is described by its character count
            format!("{} chars", joined.chars().count())
        }
    }
}

/// Build the expanded detail (args + raw result + captured logs) for a tool call
///
/// # Arguments
///
/// * `arguments` - The arguments the tool was called with, if any
/// * `results` - The results collected for this call
/// * `logs` - The log lines captured for this call, shown when any exist
fn expanded_tool_detail(
    arguments: &Option<Map<String, Value>>,
    results: &[String],
    logs: &VecDeque<String>,
) -> String {
    // pretty-print the arguments as JSON, falling back to an empty object
    let args = match arguments {
        // render the argument object with indentation for readability
        Some(map) => serde_json::to_string_pretty(map).unwrap_or_else(|_| "{}".to_string()),
        // no arguments were supplied
        None => "{}".to_string(),
    };
    // join the raw result text, or note that none has arrived yet
    let result = if results.is_empty() {
        "…running".to_string()
    } else {
        results.join("\n")
    };
    // assemble the args and result into one detail block
    let mut detail = format!("Args:\n{args}\n\nResult:\n{result}");
    // append the captured log tail so a finished call's logs stay reachable
    // after its live strip disappears
    if !logs.is_empty() {
        let joined = logs.iter().cloned().collect::<Vec<_>>().join("\n");
        detail.push_str(&format!(
            "\n\nLogs (last {} captured lines):\n{joined}",
            logs.len()
        ));
    }
    detail
}

/// Pull a short, human-readable hint of a tool call's primary argument
///
/// Prefers a `target` argument (the function/address most Ghidra tools take),
/// then falls back to the first argument. A long value is truncated so the
/// collapsed line stays on one row.
///
/// # Arguments
///
/// * `arguments` - The arguments the tool was called with, if any
fn primary_arg_hint(arguments: &Option<Map<String, Value>>) -> String {
    // without any arguments there is nothing to hint at
    let map = match arguments {
        Some(map) => map,
        None => return String::new(),
    };
    // prefer the common `target` argument, else take whatever comes first
    let value = map.get("target").or_else(|| map.values().next());
    // render the chosen value compactly, unquoting a plain string
    let hint = match value {
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
        None => return String::new(),
    };
    // keep the hint short so it never wraps the collapsed line
    if hint.chars().count() > 40 {
        // truncate on a char boundary and mark the elision
        let truncated: String = hint.chars().take(39).collect();
        format!("{truncated}…")
    } else {
        hint
    }
}

// ---------------- unicode-width-aware clip ----------------

/// Clip a string to at most `width` display columns on a grapheme boundary
///
/// # Arguments
///
/// * `s` - The string to clip
/// * `width` - The maximum display width to keep
fn clip_unicode_to_width(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut w = 0usize;

    for g in s.graphemes(true) {
        let gw = UnicodeWidthStr::width(g);
        if w > 0 && w + gw > width {
            break;
        }
        if w == 0 && gw > width {
            out.push_str(g);
            break;
        }
        out.push_str(g);
        w += gw;
        if w == width {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a tool-call bubble list holding one running call for the tests
    fn tool_call_entries(id: Uuid) -> Vec<MessageBubble> {
        // fold a tool call into an empty entry list to get its bubble
        let call = ChatEntry::ToolCall {
            id,
            agent: Some("ghidra".to_string()),
            name: "initial_analysis".to_string(),
            arguments: None,
        };
        match MessageBubble::from_msg(&call, &mut []) {
            EntryFold::New(bubble) => vec![*bubble],
            _ => panic!("a tool call must fold into a new entry"),
        }
    }

    /// A streamed log line folds onto its call entry and marks it dirty
    #[test]
    fn tool_log_folds_onto_its_call() {
        // build an entry list holding one running tool call
        let id = Uuid::new_v4();
        let mut entries = tool_call_entries(id);
        entries[0].dirty = false;
        // fold a log line for that call into the list
        let log = ChatEntry::ToolLog {
            id,
            line: "[Ghidra stdout] analyzing".to_string(),
        };
        assert!(matches!(
            MessageBubble::from_msg(&log, &mut entries),
            EntryFold::Updated
        ));
        // the line landed in the call's buffer and the entry went stale
        assert!(entries[0].dirty);
        match &entries[0].content {
            EntryKind::Tool { logs, .. } => {
                assert_eq!(logs.len(), 1);
                assert_eq!(logs[0], "[Ghidra stdout] analyzing");
            }
            _ => panic!("the entry must still be a tool call"),
        }
    }

    /// A response renders its markdown styled and wrapped within the width
    #[test]
    fn response_renders_markdown_within_width() {
        // render a markdown-heavy response into a narrow pane
        let theme = Arc::new(Theme::default());
        let text = "# Findings\n\nThe loader uses **xor** with key `0x37`.";
        let lines = render_response(Some("main"), text, 20, &theme);
        // no line may wrap past the pane width
        for line in &lines {
            assert!(line.width() <= 20, "line '{line}' is wider than 20");
        }
        // the markdown emphasis survives into the span styles
        let bold = lines
            .iter()
            .flat_map(|line| &line.spans)
            .find(|span| span.content.trim() == "xor")
            .expect("bold span");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
    }

    /// A log line with no matching call entry is dropped silently
    #[test]
    fn orphan_tool_log_is_ignored() {
        // build an entry list whose only call has a different id
        let mut entries = tool_call_entries(Uuid::new_v4());
        // fold a log line for an unknown call id into the list
        let log = ChatEntry::ToolLog {
            id: Uuid::new_v4(),
            line: "stray line".to_string(),
        };
        assert!(matches!(
            MessageBubble::from_msg(&log, &mut entries),
            EntryFold::Ignored
        ));
    }

    /// The per-call log buffer evicts its oldest line once it hits the cap
    #[test]
    fn log_buffer_evicts_oldest_at_cap() {
        // build an entry list holding one running tool call
        let id = Uuid::new_v4();
        let mut entries = tool_call_entries(id);
        // fold one more line than the buffer holds
        for n in 0..=LOG_BUFFER_CAP {
            let log = ChatEntry::ToolLog {
                id,
                line: format!("line {n}"),
            };
            MessageBubble::from_msg(&log, &mut entries);
        }
        // the buffer stays at the cap with the oldest line evicted
        match &entries[0].content {
            EntryKind::Tool { logs, .. } => {
                assert_eq!(logs.len(), LOG_BUFFER_CAP);
                assert_eq!(logs[0], "line 1");
                assert_eq!(logs[LOG_BUFFER_CAP - 1], format!("line {LOG_BUFFER_CAP}"));
            }
            _ => panic!("the entry must still be a tool call"),
        }
    }

    /// A running call renders a live log strip; a finished call hides it
    #[test]
    fn live_strip_renders_only_while_running() {
        // build a buffer of more lines than the collapsed strip shows
        let theme = Arc::new(Theme::default());
        let logs: VecDeque<String> = (0..10).map(|n| format!("line {n}")).collect();
        // a running call shows the last 5 lines plus the footer, all log rows
        let lines = render_tool_call(
            Some("ghidra"),
            "initial_analysis",
            &None,
            &[],
            &logs,
            None,
            false,
            false,
            80,
            &theme,
        );
        let log_rows: Vec<_> = lines
            .iter()
            .filter(|(_, kind)| *kind == RowKind::Log)
            .collect();
        assert_eq!(log_rows.len(), LOG_TAIL_COLLAPSED + 1);
        // the strip tail ends at the newest line and the footer names the total
        assert!(
            log_rows[LOG_TAIL_COLLAPSED - 1]
                .0
                .to_string()
                .contains("line 9")
        );
        assert!(
            log_rows[LOG_TAIL_COLLAPSED]
                .0
                .to_string()
                .contains("10 log lines")
        );
        // an enlarged window shows up to 20 lines (all 10 here) plus the footer
        let enlarged = render_tool_call(
            Some("ghidra"),
            "initial_analysis",
            &None,
            &[],
            &logs,
            None,
            false,
            true,
            80,
            &theme,
        );
        let enlarged_rows = enlarged
            .iter()
            .filter(|(_, kind)| *kind == RowKind::Log)
            .count();
        assert_eq!(enlarged_rows, 10 + 1);
        // a finished call renders no live strip at all
        let finished = render_tool_call(
            Some("ghidra"),
            "initial_analysis",
            &None,
            &["done".to_string()],
            &logs,
            Some(true),
            false,
            false,
            80,
            &theme,
        );
        assert!(finished.iter().all(|(_, kind)| *kind == RowKind::Entry));
    }

    /// A finished call's expanded detail still carries the captured log tail
    #[test]
    fn expanded_detail_includes_captured_logs() {
        // build a small captured log buffer
        let logs: VecDeque<String> = vec!["line 0".to_string(), "line 1".to_string()].into();
        // the detail block appends the logs section after args and result
        let detail = expanded_tool_detail(&None, &["done".to_string()], &logs);
        assert!(detail.contains("Logs (last 2 captured lines):"));
        assert!(detail.contains("line 1"));
        // with no captured logs the detail block stays as before
        let bare = expanded_tool_detail(&None, &["done".to_string()], &VecDeque::new());
        assert!(!bare.contains("Logs ("));
    }
}
