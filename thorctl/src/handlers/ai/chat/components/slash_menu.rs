//! The slash-command autocomplete popup for the chat prompt.
//!
//! While the user types a command name (a prompt starting with `/` and no
//! whitespace yet), this popup floats directly above the prompt listing the
//! matching commands. Up/Down move the selection, Tab/Enter accept it, and Esc
//! dismisses the popup until the prompt's content changes again.

use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

use crate::handlers::ai::chat::commands::{SLASH_COMMANDS, SlashCommand};

/// The slash-command autocomplete popup anchored above the chat prompt
#[derive(Default)]
pub struct SlashMenu {
    /// The commands matching the currently typed fragment
    matches: Vec<&'static SlashCommand>,
    /// The index of the highlighted match
    selected: usize,
    /// Whether the popup is currently visible
    pub active: bool,
    /// The prompt content the user dismissed the popup for, so Esc sticks
    /// until the content changes
    dismissed_for: Option<String>,
}

impl SlashMenu {
    /// Recompute this popup's matches and visibility from the prompt's content
    ///
    /// The popup is visible while a command name is being typed: the content
    /// starts with `/`, no whitespace follows the slash yet (arguments hide
    /// it), the user has not dismissed the popup for this exact content, and
    /// at least one command name matches the typed fragment by prefix.
    ///
    /// # Arguments
    ///
    /// * `content` - The prompt's current content
    pub fn update(&mut self, content: &str) {
        // the popup only applies while a command name is being typed
        let typing_name = content
            .strip_prefix('/')
            .is_some_and(|rest| !rest.contains(char::is_whitespace));
        // an Esc dismissal sticks until the content changes
        let dismissed = self.dismissed_for.as_deref() == Some(content);
        if !typing_name || dismissed {
            // hide the popup and drop any stale matches
            self.active = false;
            self.matches.clear();
            return;
        }
        // the typed fragment is everything after the slash
        let fragment = &content[1..];
        // keep the commands whose names start with the typed fragment
        self.matches = SLASH_COMMANDS
            .iter()
            .filter(|cmd| cmd.name.starts_with(fragment))
            .collect();
        // clamp the selection in case the match list shrank
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
        // only show the popup while something matches
        self.active = !self.matches.is_empty();
    }

    /// Move the selection up one match, wrapping at the top
    pub fn move_up(&mut self) {
        // wrap to the bottom when moving up from the first match
        if self.selected == 0 {
            self.selected = self.matches.len().saturating_sub(1);
        } else {
            self.selected -= 1;
        }
    }

    /// Move the selection down one match, wrapping at the bottom
    pub fn move_down(&mut self) {
        // wrap to the top when moving down from the last match
        self.selected = (self.selected + 1) % self.matches.len().max(1);
    }

    /// The currently highlighted command
    pub fn selection(&self) -> &'static SlashCommand {
        // the selection is always clamped to the match list by update
        self.matches[self.selected]
    }

    /// Dismiss this popup until the prompt's content changes
    ///
    /// # Arguments
    ///
    /// * `content` - The prompt content the popup was dismissed for
    pub fn dismiss(&mut self, content: &str) {
        // remember the content so update keeps the popup hidden for it
        self.dismissed_for = Some(content.to_owned());
        self.active = false;
    }

    /// Render the popup floating directly above the prompt
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render to
    /// * `prompt_area` - The prompt's on-screen area to anchor above
    pub fn render(&self, frame: &mut Frame, prompt_area: Rect) {
        // only render the popup while its active
        if !self.active {
            return;
        }
        // render each match as a usage + description line
        let mut longest = 0;
        let mut lines = Vec::with_capacity(self.matches.len());
        for (index, cmd) in self.matches.iter().enumerate() {
            // highlight the selected match so the keyboard focus is visible
            let base = if index == self.selected {
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            // build this match's line with its usage and description
            let line = Line::from(vec![
                Span::styled(" ", base),
                Span::styled(
                    cmd.usage,
                    base.fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ),
                Span::styled("    ", base),
                Span::styled(cmd.description, base),
                Span::styled(" ", base),
            ]);
            // track the widest line so the popup fits its content
            longest = longest.max(line.width());
            lines.push(line);
        }
        // size the popup to its content, clamped to the frame's bounds
        let width = (longest as u16 + 2).min(frame.area().width);
        let height = (self.matches.len() as u16 + 2).min(prompt_area.y);
        // anchor the popup directly above the prompt, inset like its border
        let x = prompt_area.x + 1;
        let y = prompt_area.y.saturating_sub(height);
        let overlay_area = Rect::new(x, y, width, height);
        // build the popup block with the overlay gray background
        let block = Block::default()
            .title(" Commands ")
            .title_alignment(Alignment::Center)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Gray))
            // set the background color to a dark gray
            .style(Style::default().bg(Color::Rgb(60, 60, 60)));
        // wrap the match lines in a paragraph
        let paragraph = Paragraph::new(lines)
            .block(block)
            // set the background color to a dark gray
            .style(Style::default().fg(Color::White).bg(Color::Rgb(60, 60, 60)));
        // Clear the area first (important for overlays), then render the widget
        frame.render_widget(Clear, overlay_area);
        frame.render_widget(paragraph, overlay_area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare slash shows every command
    #[test]
    fn slash_shows_all_commands() {
        // update with just a slash typed
        let mut menu = SlashMenu::default();
        menu.update("/");
        assert!(menu.active);
        assert_eq!(menu.matches.len(), SLASH_COMMANDS.len());
    }

    /// A prefix filters the matches and a mismatch hides the popup
    #[test]
    fn prefix_filters_matches() {
        let mut menu = SlashMenu::default();
        // a matching prefix keeps the popup open on the report command
        menu.update("/rep");
        assert!(menu.active);
        assert_eq!(menu.selection().name, "report");
        // a mismatched prefix hides the popup entirely
        menu.update("/zzz");
        assert!(!menu.active);
    }

    /// Typing past the command name into arguments hides the popup
    #[test]
    fn arguments_hide_the_popup() {
        let mut menu = SlashMenu::default();
        // the popup shows while the name is being typed
        menu.update("/report");
        assert!(menu.active);
        // the trailing space marks the start of arguments
        menu.update("/report ");
        assert!(!menu.active);
    }

    /// A non-command prompt never shows the popup
    #[test]
    fn plain_text_never_shows_the_popup() {
        let mut menu = SlashMenu::default();
        // a normal chat message should leave the popup hidden
        menu.update("what does this binary do?");
        assert!(!menu.active);
    }

    /// A dismissal sticks until the prompt's content changes
    #[test]
    fn dismissal_sticks_until_content_changes() {
        let mut menu = SlashMenu::default();
        // open the popup and then dismiss it for this exact content
        menu.update("/rep");
        menu.dismiss("/rep");
        assert!(!menu.active);
        // updating with the same content keeps the popup hidden
        menu.update("/rep");
        assert!(!menu.active);
        // changing the content shows the popup again
        menu.update("/repo");
        assert!(menu.active);
    }
}
