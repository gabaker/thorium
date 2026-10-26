//! The clickable tab strip across the top of the chat TUI.
//!
//! The main conversation and every open side conversation (`/btw <name>`) each
//! get a labeled tab. The strip paints into the 3-row band both pages already
//! reserve at the top of their layouts, so adding it changes no page layout.
//! It only renders when more than one tab exists, keeping the common
//! single-conversation case exactly as it was.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::handlers::ai::chat::Tab;

/// The clickable tab strip rendered into the pages' reserved top rows.
#[derive(Default)]
pub struct TabBar {
    /// The rendered labels' hit boxes, each paired with its tab index.
    labels: Vec<(Rect, usize)>,
}

impl TabBar {
    /// Render one bordered label per tab across the strip
    ///
    /// The active tab is highlighted, and a tab whose conversation has a turn
    /// in flight shows a `•` busy marker. Nothing is painted when only one tab
    /// exists.
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render to
    /// * `area` - The reserved 3-row strip to paint the labels into
    /// * `tabs` - Every open tab, in display order
    /// * `active` - The index of the currently focused tab
    pub fn render(&mut self, frame: &mut Frame, area: Rect, tabs: &[Tab], active: usize) {
        // forget the previous frame's hit boxes before rebuilding them
        self.labels.clear();
        // a single tab needs no strip at all
        if tabs.len() < 2 {
            return;
        }
        // lay the labels out left to right, three rows tall each
        let mut x = area.x;
        for (index, tab) in tabs.iter().enumerate() {
            // a busy conversation gets a dot marker beside its label
            let busy = tab.activity().snapshot().busy;
            let marker = if busy { " •" } else { "" };
            let label = format!(" {}{marker} ", tab.label);
            // size this label's box, stopping once the strip is full
            let width = (label.len() as u16 + 2).min(area.width.saturating_sub(x - area.x));
            if width < 4 {
                break;
            }
            let rect = Rect::new(x, area.y, width, area.height.min(3));
            // the active tab is highlighted; the rest stay dim
            let (border, text) = if index == active {
                (
                    Style::default().fg(Color::Cyan),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                (
                    Style::default().fg(Color::DarkGray),
                    Style::default().fg(Color::Gray),
                )
            };
            // draw the bordered label into its box
            let block = Block::default().borders(Borders::ALL).border_style(border);
            let paragraph = Paragraph::new(Line::from(Span::styled(label, text))).block(block);
            frame.render_widget(paragraph, rect);
            // remember the hit box so clicks can select this tab
            self.labels.push((rect, index));
            // advance to the next label's slot
            x += width;
            if x >= area.x + area.width {
                break;
            }
        }
    }

    /// The tab index a click landed on, if any
    ///
    /// # Arguments
    ///
    /// * `x` - The column position of the click
    /// * `y` - The row position of the click
    pub fn hit(&self, x: u16, y: u16) -> Option<usize> {
        // find the label whose box contains the click
        self.labels
            .iter()
            .find(|(rect, _)| {
                x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
            })
            .map(|(_, index)| *index)
    }
}
