//! The shortcut menu in thorctl analyze chat

use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default)]
pub struct ShortcutMenuBuilder {
    ///  The different shortcut options
    pub options: BTreeMap<char, String>,
    /// The overlay's title, when a consumer overrides the chat default
    pub title: Option<String>,
}

impl ShortcutMenuBuilder {
    /// Set the overlay's title (builder style)
    ///
    /// The default stays the chat home page's `" Home Shortcuts "`, so the
    /// other TUIs reusing this menu name themselves without touching chat.
    ///
    /// # Arguments
    ///
    /// * `title` - The title to render on the overlay's top border
    pub fn title(mut self, title: impl Into<String>) -> Self {
        // record the override; build falls back to the chat default
        self.title = Some(title.into());
        self
    }

    /// Add a new short to our shortcut menu
    ///
    /// # Arguments
    ///
    /// * `key` - The key to add
    /// * `description` - The description of what this shortcut does
    pub fn add(mut self, key: char, description: impl Into<String>) -> Self {
        // add this new shortcut
        self.options.insert(key, description.into());
        self
    }

    /// Compile all of our options as rendered lines
    pub fn build(self) -> ShortcutMenu {
        // track the longest line in this menu
        let mut longest = 0;
        // preallocate a list to add our renderable lines into
        let mut renderable = Vec::with_capacity(self.options.len());
        // render all of our options
        for (key, description) in self.options {
            // build this renderable line
            let line = Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    key.to_string(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("    "),
                Span::raw(description),
            ]);
            // get the width of this line
            let width = line.width();
            // check if this line is our longest line so far
            if width > longest {
                // this is our new longest line set it
                longest = width;
            }
            // add this renderable line for this option
            renderable.push(line);
        }
        // keep the chat home page's historical title by default
        let title = self.title.unwrap_or_else(|| " Home Shortcuts ".to_owned());
        // the box must fit the title too, not just the widest option row, so
        // a short menu under a long title never clips it
        longest = longest.max(title.chars().count());
        // build our shortcut meny
        ShortcutMenu {
            options: renderable,
            longest: longest as u16,
            active: false,
            title,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ShortcutMenu {
    ///  The different shortcut options rendered as lines
    pub options: Vec<Line<'static>>,
    /// The longest line in this menu
    pub longest: u16,
    /// Whether this menu is visible  and shortcut mode is active
    pub active: bool,
    /// The title rendered on the overlay's top border
    pub title: String,
}

impl ShortcutMenu {
    /// Get a shortcut menu builder
    pub fn builder() -> ShortcutMenuBuilder {
        ShortcutMenuBuilder {
            options: BTreeMap::default(),
            title: None,
        }
    }

    /// Invert whether this menu is active or not
    pub fn toggle(&mut self) {
        self.active = !self.active;
    }

    /// Render the help overlay to the frame
    ///
    /// Displays a floating window in the bottom right corner with
    /// available keyboard shortcuts.
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render to
    pub fn render(&self, frame: &mut Frame) {
        // only render the menu if its active
        if self.active {
            // get the full terminal renderable area
            let area = frame.area();
            // the menu is its option rows plus the top and bottom border rows
            let height = self.options.len() as u16 + 2;
            // Calculate position in bottom right corner with some padding
            let x = area.width.saturating_sub(self.longest + 3);
            let y = area.height.saturating_sub(height);
            // build the area to render this menu into
            let overlay_area = Rect::new(x, y, self.longest + 3, height);
            // Create the overlay block with medium dark gray background
            let block = Block::default()
                .title(self.title.as_str())
                .title_alignment(Alignment::Center)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Gray))
                // set the background color to a dark gray
                .style(Style::default().bg(Color::Rgb(60, 60, 60)));
            // wrap our renderable options in a paragraph
            let paragraph = Paragraph::new(self.options.clone())
                .block(block)
                // set the background color to a dark gray
                .style(Style::default().fg(Color::White).bg(Color::Rgb(60, 60, 60)));
            // Clear the area first (important for overlays), then render the widget
            frame.render_widget(Clear, overlay_area);
            frame.render_widget(paragraph, overlay_area);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// A consumer-set title renders instead of the chat default
    #[test]
    fn custom_titles_render_on_the_overlay() {
        // a menu titled for another TUI keeps its own name on the border
        let mut menu = ShortcutMenu::builder()
            .title(" Explore Shortcuts ")
            .add('h', "help")
            .build();
        menu.toggle();
        let text = draw(&menu, 60, 12);
        assert!(text.contains("Explore Shortcuts"), "got {text:?}");
        assert!(!text.contains("Home Shortcuts"));
        // an untitled menu keeps the chat home page's historical title
        let mut plain = ShortcutMenu::builder().add('h', "help").build();
        plain.toggle();
        let text = draw(&plain, 60, 12);
        assert!(text.contains("Home Shortcuts"), "got {text:?}");
    }

    /// Draws the menu on one frame and returns the buffer's visible text
    fn draw(menu: &ShortcutMenu, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| menu.render(frame)).expect("frame");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    /// Every entry stays on screen as the menu grows past its original three
    #[test]
    fn menu_fits_all_entries_on_screen() {
        // build the six-entry menu the chat page registers
        let mut menu = ShortcutMenu::builder()
            .add('h', "Go home")
            .add('m', "Message board")
            .add('g', "Traversal graph")
            .add('s', "Search forum")
            .add('t', "Timeline")
            .add('u', "Usage")
            .build();
        // the menu only renders while shortcut mode is active
        menu.toggle();
        // the last entries must land inside the frame, not past its bottom edge
        let content = draw(&menu, 80, 24);
        assert!(content.contains("Search forum"));
        assert!(content.contains("Usage"));
    }
}
