//! The Themes used by thorctl AI chat
use ratatui::prelude::*;

/// The styles for the different classes of message log entry
///
/// A flat, borderless feed keeps meaningful content (AI responses, agent
/// thinking, and forum posts/comments) prominent while pushing routine tool
/// calls into a dim, compact grey line. Every style is foreground-only -- no
/// backgrounds or borders -- for a modern, uncluttered look.
#[derive(Debug)]
pub struct MessageTheme {
    /// The style for a user's own messages
    pub user: Style,
    /// The style for an agent's assistant text (prominent)
    pub ai: Style,
    /// The style for the agent-name label attached to an entry
    pub agent: Style,
    /// The style for an agent's thinking text (dim, italic)
    pub thinking: Style,
    /// The style for a forum post/comment body (prominent)
    pub forum: Style,
    /// The style for a routine tool call's compact grey line
    pub tool: Style,
    /// The style for the status marker of a tool call that succeeded
    pub tool_ok: Style,
    /// The style for the status marker of a tool call that failed
    pub tool_err: Style,
    /// The style for the status marker of a tool call still running
    pub tool_pending: Style,
    /// The style for a running tool call's live log lines (dim, italic)
    pub tool_log: Style,
    /// The style for an error entry (prominent, red)
    pub error: Style,
}

/// Set a default theme
impl Default for MessageTheme {
    /// Set a default theme
    fn default() -> Self {
        // a user's own messages read in plain, bright text
        let user = Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD);
        // an agent's assistant text is the main content, so keep it bright
        let ai = Style::default().fg(Color::White);
        // the agent-name label is a cyan accent so attribution stands out
        let agent = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        // thinking is secondary reasoning, shown dim and italic
        let thinking = Style::default()
            .fg(Color::Gray)
            .add_modifier(Modifier::ITALIC);
        // forum posts/comments are findings worth highlighting
        let forum = Style::default().fg(Color::LightYellow);
        // routine tool calls are de-emphasized to a dark grey
        let tool = Style::default().fg(Color::DarkGray);
        // green/red status markers flag a call's success or failure at a glance
        let tool_ok = Style::default().fg(Color::Green);
        let tool_err = Style::default().fg(Color::Red);
        // a still-running call gets a neutral dim marker
        let tool_pending = Style::default().fg(Color::DarkGray);
        // live log lines read as subordinate telemetry under the tool line
        let tool_log = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC);
        // an error is surfaced prominently in red
        let error = Style::default().fg(Color::Red).add_modifier(Modifier::BOLD);
        // Build the message theme
        MessageTheme {
            user,
            ai,
            agent,
            thinking,
            forum,
            tool,
            tool_ok,
            tool_err,
            tool_pending,
            tool_log,
            error,
        }
    }
}

/// The styles for the traversal graph view
#[derive(Debug)]
pub struct GraphTheme {
    /// The per-agent cursor colors, assigned by palette slot modulo the length
    pub agent_palette: [Color; 8],
    /// The style for a node only discovered from a result, never visited
    pub node_discovered: Style,
    /// The style for a node an agent visited with any function-scoped tool
    pub node_visited: Style,
    /// The style for a node whose code was read (decompiled/disassembled)
    pub node_examined: Style,
    /// The style for the mutation marker on a node an agent modified
    pub node_mutated: Style,
    /// The style for a node whose only visit failed (likely a bogus target)
    pub node_unresolved: Style,
    /// The style for a call edge between two known functions
    pub edge: Color,
    /// The style for a call edge both of whose endpoints were visited
    pub edge_traversed: Color,
    /// The visit-frequency heat ramp, coolest to hottest
    pub heat_ramp: [Style; 5],
}

/// Set a default graph theme
impl Default for GraphTheme {
    /// Set a default graph theme
    fn default() -> Self {
        // one distinct cursor color per agent, cycling for large swarms
        let agent_palette = [
            Color::Cyan,
            Color::Yellow,
            Color::Green,
            Color::Magenta,
            Color::Blue,
            Color::LightRed,
            Color::LightGreen,
            Color::LightMagenta,
        ];
        // discovered-only nodes read as faint context around the real work
        let node_discovered = Style::default().fg(Color::DarkGray);
        // visited nodes are the main content, so keep them bright
        let node_visited = Style::default().fg(Color::White);
        // examined nodes stand out as actually-read code
        let node_examined = Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD);
        // the mutation marker flags functions an agent renamed/retyped
        let node_mutated = Style::default().fg(Color::Yellow);
        // an unresolved target is a visible dead end
        let node_unresolved = Style::default().fg(Color::Red);
        // edges are context, so they stay dim unless both ends were visited
        let edge = Color::DarkGray;
        let edge_traversed = Color::Gray;
        // the heat ramp runs dim -> white -> yellow -> light red -> red+bold
        let heat_ramp = [
            Style::default().fg(Color::DarkGray),
            Style::default().fg(Color::White),
            Style::default().fg(Color::Yellow),
            Style::default().fg(Color::LightRed),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ];
        GraphTheme {
            agent_palette,
            node_discovered,
            node_visited,
            node_examined,
            node_mutated,
            node_unresolved,
            edge,
            edge_traversed,
            heat_ramp,
        }
    }
}

/// A theme for thorctl AI chat
#[derive(Debug)]
pub struct Theme {
    /// The styles for message log entries
    pub messages: MessageTheme,
    /// The styles for the traversal graph view
    pub graph: GraphTheme,
}

/// Set a default theme
impl Default for Theme {
    fn default() -> Self {
        Theme {
            messages: MessageTheme::default(),
            graph: GraphTheme::default(),
        }
    }
}
