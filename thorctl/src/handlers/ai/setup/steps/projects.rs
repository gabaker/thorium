//! The Ghidra projects directory page

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

use crate::handlers::ai::chat::TextBox;

use super::dim_style;

/// What a key press on the projects page asks the app to do
pub enum ProjectsOutcome {
    /// Nothing; the page consumed the key itself
    None,
    /// Validate and use this projects directory
    Submit(String),
    /// Go back one step
    Back,
}

/// The projects page's input state
pub struct ProjectsStep {
    /// The projects directory box
    boxed: TextBox,
    /// The last validation error, shown inline until the next attempt
    pub error: Option<String>,
}

impl ProjectsStep {
    /// Create the projects page prefilled with the effective directory
    ///
    /// # Arguments
    ///
    /// * `prefill` - The configured projects dir, or the shared default
    pub fn new(prefill: String) -> Self {
        let mut boxed = TextBox::default().title("Ghidra projects directory");
        boxed.set_content(prefill);
        ProjectsStep { boxed, error: None }
    }

    /// Handle a key press on the projects page
    ///
    /// # Arguments
    ///
    /// * `key` - The key event to handle
    pub fn handle_key(&mut self, key: KeyEvent) -> ProjectsOutcome {
        // escape backs out of the page
        if key.code == KeyCode::Esc {
            return ProjectsOutcome::Back;
        }
        // control chords never type into the path box (ctrl+j would embed a
        // newline mid-path; ctrl+c quits globally)
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return ProjectsOutcome::None;
        }
        // route the key into the box; enter submits a non-empty path
        if self.boxed.handle_key(key, false) {
            return ProjectsOutcome::Submit(self.boxed.content.trim().to_string());
        }
        ProjectsOutcome::None
    }

    /// Insert pasted text into the projects box
    ///
    /// # Arguments
    ///
    /// * `text` - The pasted text to insert
    pub fn paste(&mut self, text: &str) {
        self.boxed.insert_str(text.trim_end_matches(['\r', '\n']));
    }

    /// Render the projects page
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The body area to render in
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        // carve an intro, the path box, and an error strip
        let [intro, boxed, status] = Layout::vertical([
            Constraint::Length(4),
            Constraint::Length(3),
            Constraint::Min(0),
        ])
        .areas(area);
        frame.render_widget(
            Paragraph::new(vec![Line::styled(
                "ThorPal creates and reopens Ghidra projects under this directory. \
                    Ghidra 12.1+ rejects any path component starting with a dot, so \
                    something like ~/ghidra-projects works and ~/.thorium/projects \
                    does not. The directory is created on first use.",
                dim_style(),
            )])
            .wrap(Wrap { trim: false }),
            intro,
        );
        self.boxed.is_focused = true;
        self.boxed.render(frame, boxed);
        if let Some(error) = &self.error {
            frame.render_widget(
                Paragraph::new(Line::styled(error.clone(), Style::default().fg(Color::Red)))
                    .wrap(Wrap { trim: false }),
                status,
            );
        }
    }
}
