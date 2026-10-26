//! The /done summary review modal for a side conversation.
//!
//! When the analyst runs `/done` inside a btw tab, the side conversation is
//! summarized by a one-shot agent and the result is shown in this modal with a
//! visible, selectable action menu -- Accept / Edit / Revise / Skip -- so the
//! flow is discoverable instead of requiring more slash commands. Edit opens
//! the summary in the user's default editor (falling back to an in-modal text
//! box when no editor resolves), and Revise hands the tab's own prompt box
//! back to the analyst to type feedback that regenerates the summary.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use super::TextBox;

/// The modal's visible menu options, in render and selection order.
///
/// Each entry pairs the row's label with its single-key accelerator.
const MENU_OPTIONS: &[(&str, char)] = &[
    ("Accept and send to main chat", 'a'),
    ("Edit the summary (opens $EDITOR)", 'e'),
    ("Revise with feedback", 'r'),
    ("Skip — return nothing", 's'),
];

/// The modal's dark gray overlay background, matching the confirm modal.
const MODAL_BG: Color = Color::Rgb(60, 60, 60);

/// The /done summary review modal's interaction state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryModalState {
    /// Waiting on the summarizer; a progress line shows and Esc backs out.
    Generating,
    /// The summary is shown with the selectable action menu.
    Menu,
    /// The in-modal fallback editor: the summary pane is an editable text box.
    Editing,
    /// Feedback is being typed into the page's prompt box; the modal steps
    /// aside and renders nothing until the revised summary arrives.
    Revising,
}

/// What a handled key or click asks the chat page to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryOutcome {
    /// The modal handled the input itself; nothing for the page to do.
    None,
    /// Accept the summary and send it back to the main conversation.
    Accept,
    /// Edit the summary in the user's default editor.
    EditExternally,
    /// Revise the summary with feedback typed into the page's prompt.
    Revise,
    /// Skip returning anything and close the side conversation.
    Skip,
    /// Dismiss the modal, keeping the side conversation open.
    Dismiss,
}

/// The /done summary review modal for one side conversation.
pub struct SummaryModal {
    /// The side conversation this summary covers.
    pub name: String,
    /// The summary text, doubling as the display buffer (Menu) and the
    /// editable buffer (the Editing fallback).
    pub editor: TextBox,
    /// The modal's current interaction state.
    pub state: SummaryModalState,
    /// The index of the highlighted menu option.
    selected: usize,
    /// The vertical scroll offset of the summary body.
    scroll: u16,
    /// The rendered menu rows' hit boxes, for click selection.
    option_rows: Vec<(Rect, usize)>,
}

impl SummaryModal {
    /// Creates a modal waiting on its first summary generation
    ///
    /// # Arguments
    ///
    /// * `name` - The side conversation being summarized
    pub fn generating(name: impl Into<String>) -> Self {
        // start in the generating state with an empty summary buffer
        SummaryModal {
            name: name.into(),
            editor: TextBox::default(),
            state: SummaryModalState::Generating,
            selected: 0,
            scroll: 0,
            option_rows: Vec::new(),
        }
    }

    /// The current summary text (as generated or edited so far)
    pub fn summary(&self) -> &str {
        // the editor's buffer always holds the latest summary text
        &self.editor.content
    }

    /// Installs a freshly generated (or revised) summary and shows the menu
    ///
    /// # Arguments
    ///
    /// * `summary` - The new summary text to review
    pub fn set_summary(&mut self, summary: String) {
        // swap the new text into the buffer and reset the view
        self.editor.set_content(summary);
        self.editor.is_focused = false;
        self.scroll = 0;
        self.selected = 0;
        self.state = SummaryModalState::Menu;
    }

    /// Maps the highlighted menu option onto its outcome
    fn confirm_selected(&self) -> SummaryOutcome {
        // the option order mirrors MENU_OPTIONS
        match self.selected {
            0 => SummaryOutcome::Accept,
            1 => SummaryOutcome::EditExternally,
            2 => SummaryOutcome::Revise,
            _ => SummaryOutcome::Skip,
        }
    }

    /// Handle a key press while the modal owns the keyboard
    ///
    /// # Arguments
    ///
    /// * `key` - The key event to handle
    pub fn handle_key(&mut self, key: KeyEvent) -> SummaryOutcome {
        match self.state {
            // while generating only Esc does anything: it backs out to the
            // chat (the summary still arrives and reopens the menu)
            SummaryModalState::Generating => match key.code {
                KeyCode::Esc => SummaryOutcome::Dismiss,
                _ => SummaryOutcome::None,
            },
            // the menu owns navigation, confirmation, and the accelerators
            SummaryModalState::Menu => match key.code {
                // move the highlighted option up or down, wrapping around
                KeyCode::Up | KeyCode::Char('k') => {
                    self.selected = self
                        .selected
                        .checked_sub(1)
                        .unwrap_or(MENU_OPTIONS.len() - 1);
                    SummaryOutcome::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.selected = (self.selected + 1) % MENU_OPTIONS.len();
                    SummaryOutcome::None
                }
                // confirm the highlighted option
                KeyCode::Enter => self.confirm_selected(),
                // scroll the summary body without moving the selection
                KeyCode::PageUp => {
                    self.scroll = self.scroll.saturating_sub(5);
                    SummaryOutcome::None
                }
                KeyCode::PageDown => {
                    self.scroll = self.scroll.saturating_add(5);
                    SummaryOutcome::None
                }
                // back out to the side chat, keeping the side open
                KeyCode::Esc => SummaryOutcome::Dismiss,
                // the single-key accelerators jump straight to their outcome
                KeyCode::Char(c) => match MENU_OPTIONS.iter().position(|(_, accel)| *accel == c) {
                    Some(index) => {
                        self.selected = index;
                        self.confirm_selected()
                    }
                    None => SummaryOutcome::None,
                },
                _ => SummaryOutcome::None,
            },
            // the fallback editor owns every key until the edit finishes
            SummaryModalState::Editing => match key.code {
                // Enter or Esc finishes the edit and returns to the menu
                KeyCode::Enter | KeyCode::Esc => {
                    self.editor.is_focused = false;
                    self.state = SummaryModalState::Menu;
                    SummaryOutcome::None
                }
                // everything else edits the buffer (Ctrl+J inserts a newline)
                _ => {
                    self.editor.handle_key(key, true);
                    SummaryOutcome::None
                }
            },
            // while revising the page's prompt owns the keyboard, not the modal
            SummaryModalState::Revising => SummaryOutcome::None,
        }
    }

    /// Handle a mouse click, selecting a clicked menu row
    ///
    /// A click only moves the highlight (Enter confirms), so the sync click
    /// path never has to dispatch the async worker requests a confirmation
    /// can trigger.
    ///
    /// # Arguments
    ///
    /// * `x` - The column position of the click
    /// * `y` - The row position of the click
    pub fn handle_click(&mut self, x: u16, y: u16) {
        // clicks only mean anything while the menu is showing
        if self.state != SummaryModalState::Menu {
            return;
        }
        // a click on a menu row moves the highlight onto it
        for (rect, index) in &self.option_rows {
            if x >= rect.x && x < rect.x + rect.width && y == rect.y {
                self.selected = *index;
                return;
            }
        }
    }

    /// Puts the modal into the fallback in-modal editing state
    ///
    /// Used when no external editor could run; the summary pane becomes an
    /// editable text box pre-filled with the current summary.
    pub fn edit_inline(&mut self) {
        // hand the buffer the keyboard and switch states
        self.editor.is_focused = true;
        self.state = SummaryModalState::Editing;
    }

    /// Render the modal centered over the page
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render to
    /// * `area` - The page area to center the modal within
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        // while revising the page's prompt is the interaction surface, so the
        // modal renders nothing at all
        if self.state == SummaryModalState::Revising {
            return;
        }
        // size the modal to most of the page so long summaries stay readable
        let width = (area.width * 4 / 5).clamp(20, area.width);
        let height = (area.height * 4 / 5).clamp(8, area.height);
        // center the modal within the page
        let x = area.x + area.width.saturating_sub(width) / 2;
        let y = area.y + area.height.saturating_sub(height) / 2;
        let overlay = Rect::new(x, y, width, height);
        // build the modal block with the overlay gray background
        let block = Block::default()
            .title(format!(" Side conversation summary — btw:{} ", self.name))
            .title_alignment(Alignment::Center)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Gray))
            .style(Style::default().bg(MODAL_BG));
        // clear the overlay first so the page never bleeds through
        frame.render_widget(Clear, overlay);
        let inner = block.inner(overlay);
        frame.render_widget(block, overlay);
        // a still-generating summary is just a progress line
        if self.state == SummaryModalState::Generating {
            let progress = Paragraph::new("Summarizing this side conversation… (esc hides)")
                .style(Style::default().fg(Color::DarkGray).bg(MODAL_BG));
            frame.render_widget(progress, inner);
            return;
        }
        // split the modal into the summary body, the menu, and a footer
        let menu_height = if self.state == SummaryModalState::Menu {
            MENU_OPTIONS.len() as u16 + 1
        } else {
            0
        };
        let chunks = Layout::vertical([
            Constraint::Min(1),              // summary body
            Constraint::Length(menu_height), // action menu (hidden while editing)
            Constraint::Length(1),           // footer key hints
        ])
        .split(inner);
        // the editing state renders the editable text box as the body
        if self.state == SummaryModalState::Editing {
            self.editor.render(frame, chunks[0]);
        } else {
            // the menu state renders the summary read-only with scroll
            let body = Paragraph::new(self.editor.content.clone())
                .style(Style::default().fg(Color::White).bg(MODAL_BG))
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0));
            frame.render_widget(body, chunks[0]);
        }
        // render the selectable menu rows, remembering their hit boxes
        self.option_rows.clear();
        if self.state == SummaryModalState::Menu {
            for (index, (label, accel)) in MENU_OPTIONS.iter().enumerate() {
                // the first menu row sits one line below the summary body
                let row = Rect::new(
                    chunks[1].x,
                    chunks[1].y + 1 + index as u16,
                    chunks[1].width,
                    1,
                );
                // skip rows squeezed out of a tiny terminal
                if row.y >= chunks[1].y + chunks[1].height {
                    break;
                }
                // highlight the selected row and dim the accelerator hint
                let (marker, style) = if index == self.selected {
                    (
                        "▸ ",
                        Style::default()
                            .fg(Color::Yellow)
                            .bg(MODAL_BG)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    ("  ", Style::default().fg(Color::White).bg(MODAL_BG))
                };
                let line = Line::from(vec![
                    Span::styled(format!("{marker}{label}"), style),
                    Span::styled(
                        format!("  ({accel})"),
                        Style::default().fg(Color::DarkGray).bg(MODAL_BG),
                    ),
                ]);
                frame.render_widget(Paragraph::new(line), row);
                self.option_rows.push((row, index));
            }
        }
        // render the footer key hints for the current state
        let hints = match self.state {
            SummaryModalState::Editing => " enter done · ctrl+j newline · esc done ",
            _ => " ↑/↓ or j/k select · enter confirm · pgup/pgdn scroll · esc back to chat ",
        };
        let footer = Paragraph::new(hints)
            .style(Style::default().fg(Color::DarkGray).bg(MODAL_BG))
            .alignment(Alignment::Center);
        frame.render_widget(footer, chunks[2]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};

    /// Builds a plain key press event for the state-machine tests
    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    /// The menu wraps its selection and confirms the highlighted outcome
    #[test]
    fn menu_selection_wraps_and_confirms() {
        // install a summary so the modal is in the menu state
        let mut modal = SummaryModal::generating("crypto");
        modal.set_summary("found the xor loop".to_owned());
        assert_eq!(modal.state, SummaryModalState::Menu);
        // moving up from the first option wraps to the last (skip)
        assert_eq!(modal.handle_key(press(KeyCode::Up)), SummaryOutcome::None);
        assert_eq!(
            modal.handle_key(press(KeyCode::Enter)),
            SummaryOutcome::Skip
        );
        // moving back down wraps to the first option (accept)
        assert_eq!(modal.handle_key(press(KeyCode::Down)), SummaryOutcome::None);
        assert_eq!(
            modal.handle_key(press(KeyCode::Enter)),
            SummaryOutcome::Accept
        );
        // esc backs out to the chat without picking anything
        assert_eq!(
            modal.handle_key(press(KeyCode::Esc)),
            SummaryOutcome::Dismiss
        );
    }

    /// The accelerators jump straight to their outcome from any selection
    #[test]
    fn menu_accelerators_confirm_directly() {
        let mut modal = SummaryModal::generating("crypto");
        modal.set_summary("found the xor loop".to_owned());
        // each accelerator confirms its own option
        assert_eq!(
            modal.handle_key(press(KeyCode::Char('e'))),
            SummaryOutcome::EditExternally
        );
        assert_eq!(
            modal.handle_key(press(KeyCode::Char('r'))),
            SummaryOutcome::Revise
        );
        assert_eq!(
            modal.handle_key(press(KeyCode::Char('s'))),
            SummaryOutcome::Skip
        );
        assert_eq!(
            modal.handle_key(press(KeyCode::Char('a'))),
            SummaryOutcome::Accept
        );
        // an unmapped character does nothing
        assert_eq!(
            modal.handle_key(press(KeyCode::Char('z'))),
            SummaryOutcome::None
        );
    }

    /// The inline editor edits the buffer and returns to the menu on enter
    #[test]
    fn inline_editing_round_trips() {
        let mut modal = SummaryModal::generating("crypto");
        modal.set_summary("summary".to_owned());
        // enter the fallback editing state and type a character
        modal.edit_inline();
        assert_eq!(modal.state, SummaryModalState::Editing);
        assert_eq!(
            modal.handle_key(press(KeyCode::Char('!'))),
            SummaryOutcome::None
        );
        assert_eq!(modal.summary(), "summary!");
        // enter finishes the edit and shows the menu again
        assert_eq!(
            modal.handle_key(press(KeyCode::Enter)),
            SummaryOutcome::None
        );
        assert_eq!(modal.state, SummaryModalState::Menu);
    }

    /// While generating only esc does anything
    #[test]
    fn generating_swallows_keys() {
        let mut modal = SummaryModal::generating("crypto");
        // ordinary keys are swallowed while the summarizer works
        assert_eq!(
            modal.handle_key(press(KeyCode::Char('a'))),
            SummaryOutcome::None
        );
        assert_eq!(
            modal.handle_key(press(KeyCode::Enter)),
            SummaryOutcome::None
        );
        // esc backs out to the chat
        assert_eq!(
            modal.handle_key(press(KeyCode::Esc)),
            SummaryOutcome::Dismiss
        );
    }
}
