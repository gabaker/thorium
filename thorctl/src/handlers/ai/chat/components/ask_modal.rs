//! The ask_analyst question modal: how an agent's question reaches the analyst.
//!
//! When the main agent calls its `ask_analyst` tool mid-turn, the TUI receives
//! a pending ask and opens this modal over the main chat page: the question
//! body on top, the offered options below (the recommended one marked), plus a
//! "type a different answer" row for a free-text reply. Enter answers with the
//! highlighted row, the digit accelerators jump straight to an option, and Esc
//! declines -- the run then takes the recommended default and auto-files a
//! review flag, exactly like an unattended run. The page sends the chosen
//! answer back through the session's analyst link, which wakes the tool future
//! parked inside the agent's turn.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use thorpal::PendingAsk;

use super::TextBox;

/// The modal's dark gray overlay background, matching the summary modal.
const MODAL_BG: Color = Color::Rgb(60, 60, 60);

/// How long after its first render the modal accepts committing keys.
///
/// The modal opens asynchronously from an agent event, so keystrokes the user
/// typed at the prompt can already be queued behind it (and reflex presses
/// land right after it appears). Until this grace passes, Enter, the digit
/// accelerators, `o`, and Esc are swallowed so a keystroke meant for the
/// prompt can never irreversibly answer (or decline) a question the user has
/// not seen; navigation keys stay live throughout.
const ARM_GRACE: std::time::Duration = std::time::Duration::from_millis(400);

/// The ask modal's interaction state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskModalState {
    /// The option rows are shown and own the keyboard.
    Menu,
    /// A free-text answer is being typed into the in-modal text box.
    Typing,
}

/// What a handled key or click asks the chat page to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskOutcome {
    /// The modal handled the input itself; nothing for the page to do.
    None,
    /// Answer with the offered option at this index.
    Pick(usize),
    /// Answer with the typed free text.
    Other(String),
    /// Decline the question: the run takes the recommended default, records
    /// the ask as declined, and auto-files a review flag.
    Decline,
}

/// The question modal for one pending analyst ask.
pub struct AskModal {
    /// The pending ask this modal poses.
    pub ask: PendingAsk,
    /// The modal's current interaction state.
    pub state: AskModalState,
    /// The index of the highlighted row (the options, then the free-text row).
    selected: usize,
    /// The vertical scroll offset of the question body.
    scroll: u16,
    /// The free-text answer box behind the "type a different answer" row.
    pub editor: TextBox,
    /// The rendered rows' hit boxes, for click selection.
    option_rows: Vec<(Rect, usize)>,
    /// When the modal was first rendered, arming its committing keys after
    /// [`ARM_GRACE`]; `None` until the first render.
    armed_at: Option<std::time::Instant>,
}

impl AskModal {
    /// Creates a modal posing a pending ask, highlighting its recommended option
    ///
    /// # Arguments
    ///
    /// * `ask` - The pending ask to pose to the analyst
    pub fn new(ask: PendingAsk) -> Self {
        // open on the recommended option so a bare Enter takes the default
        let selected = ask
            .options
            .iter()
            .position(|option| option.label == ask.recommended)
            .unwrap_or(0);
        AskModal {
            ask,
            state: AskModalState::Menu,
            selected,
            scroll: 0,
            editor: TextBox::default(),
            option_rows: Vec::new(),
            armed_at: None,
        }
    }

    /// The number of selectable rows: every option plus the free-text row
    fn row_count(&self) -> usize {
        self.ask.options.len() + 1
    }

    /// Whether the modal has been visible long enough to accept commits
    ///
    /// False until the modal has actually been rendered and [`ARM_GRACE`] has
    /// passed since, so queued or reflex keystrokes can never answer a
    /// question the user has not seen.
    fn armed(&self) -> bool {
        // armed only once rendered and the grace window has elapsed
        self.armed_at
            .is_some_and(|rendered_at| rendered_at.elapsed() >= ARM_GRACE)
    }

    /// Confirms the highlighted row: an option answers, the last row types
    fn confirm_selected(&mut self) -> AskOutcome {
        // the last row opens the free-text answer box instead of answering
        if self.selected == self.ask.options.len() {
            self.editor.is_focused = true;
            self.state = AskModalState::Typing;
            return AskOutcome::None;
        }
        AskOutcome::Pick(self.selected)
    }

    /// Handle a key press while the modal owns the keyboard
    ///
    /// # Arguments
    ///
    /// * `key` - The key event to handle
    pub fn handle_key(&mut self, key: KeyEvent) -> AskOutcome {
        // committing keys stay dead until the modal has been visible for the
        // arming grace; navigation and scrolling work immediately
        let armed = self.armed();
        match self.state {
            // the menu owns navigation, confirmation, and the accelerators
            AskModalState::Menu => match key.code {
                // move the highlighted row up or down, wrapping around
                KeyCode::Up | KeyCode::Char('k') => {
                    self.selected = self.selected.checked_sub(1).unwrap_or(self.row_count() - 1);
                    AskOutcome::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.selected = (self.selected + 1) % self.row_count();
                    AskOutcome::None
                }
                // confirm the highlighted row, once the modal is armed
                KeyCode::Enter if armed => self.confirm_selected(),
                // scroll the question body without moving the selection
                KeyCode::PageUp => {
                    self.scroll = self.scroll.saturating_sub(5);
                    AskOutcome::None
                }
                KeyCode::PageDown => {
                    self.scroll = self.scroll.saturating_add(5);
                    AskOutcome::None
                }
                // decline (take the recommended default and flag it for
                // review), once the modal is armed
                KeyCode::Esc if armed => AskOutcome::Decline,
                // digits jump straight to their option; 'o' opens free text
                KeyCode::Char(c) if armed => {
                    if let Some(digit) = c.to_digit(10) {
                        // a digit outside the offered options does nothing
                        let index = (digit as usize).wrapping_sub(1);
                        if index < self.ask.options.len() {
                            self.selected = index;
                            return self.confirm_selected();
                        }
                        return AskOutcome::None;
                    }
                    if c == 'o' {
                        // jump to and open the free-text answer row
                        self.selected = self.ask.options.len();
                        return self.confirm_selected();
                    }
                    AskOutcome::None
                }
                _ => AskOutcome::None,
            },
            // the free-text box owns every key until the answer is sent
            AskModalState::Typing => match key.code {
                // Enter sends a non-empty answer; an empty box does nothing
                KeyCode::Enter => {
                    let text = self.editor.content.trim().to_owned();
                    if text.is_empty() {
                        return AskOutcome::None;
                    }
                    AskOutcome::Other(text)
                }
                // Esc backs out to the option rows without answering
                KeyCode::Esc => {
                    self.editor.is_focused = false;
                    self.state = AskModalState::Menu;
                    AskOutcome::None
                }
                // everything else edits the answer buffer
                _ => {
                    self.editor.handle_key(key, true);
                    AskOutcome::None
                }
            },
        }
    }

    /// Handle a mouse click, selecting a clicked row
    ///
    /// A click only moves the highlight (Enter confirms), so the sync click
    /// path never answers an ask by accident.
    ///
    /// # Arguments
    ///
    /// * `x` - The column position of the click
    /// * `y` - The row position of the click
    pub fn handle_click(&mut self, x: u16, y: u16) {
        // clicks only mean anything while the option rows are showing
        if self.state != AskModalState::Menu {
            return;
        }
        // a click on a row moves the highlight onto it
        for (rect, index) in &self.option_rows {
            if x >= rect.x && x < rect.x + rect.width && y == rect.y {
                self.selected = *index;
                return;
            }
        }
    }

    /// Render the modal centered over the page
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render to
    /// * `area` - The page area to center the modal within
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        // the first render starts the arming grace for the committing keys
        if self.armed_at.is_none() {
            self.armed_at = Some(std::time::Instant::now());
        }
        // size the modal to most of the page so long questions stay readable;
        // max-then-min (never clamp) so a tiny pane shrinks the modal instead
        // of inverting the clamp bounds and panicking in raw mode
        let width = (area.width * 4 / 5).max(20).min(area.width);
        let height = (area.height * 4 / 5).max(8).min(area.height);
        // center the modal within the page
        let x = area.x + area.width.saturating_sub(width) / 2;
        let y = area.y + area.height.saturating_sub(height) / 2;
        let overlay = Rect::new(x, y, width, height);
        // build the modal block with the overlay gray background
        let block = Block::default()
            .title(format!(" Analyst question — from {} ", self.ask.agent))
            .title_alignment(Alignment::Center)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .style(Style::default().bg(MODAL_BG));
        // clear the overlay first so the page never bleeds through
        frame.render_widget(Clear, overlay);
        let inner = block.inner(overlay);
        frame.render_widget(block, overlay);
        // split the modal into the question body, the rows, and a footer; the
        // typing state swaps the rows for the free-text answer box
        let rows_height = match self.state {
            AskModalState::Menu => self.row_count() as u16 + 1,
            AskModalState::Typing => 3,
        };
        let chunks = Layout::vertical([
            Constraint::Min(1),              // question body
            Constraint::Length(rows_height), // option rows / answer box
            Constraint::Length(1),           // footer key hints
        ])
        .split(inner);
        // the question body renders read-only with scroll in both states
        let body = Paragraph::new(self.ask.question.clone())
            .style(Style::default().fg(Color::White).bg(MODAL_BG))
            .wrap(Wrap { trim: false })
            .scroll((self.scroll, 0));
        frame.render_widget(body, chunks[0]);
        // render the selectable rows, remembering their hit boxes
        self.option_rows.clear();
        match self.state {
            AskModalState::Menu => {
                for index in 0..self.row_count() {
                    // the first row sits one line below the question body
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
                    // highlight the selected row like the summary modal does
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
                    // an option row names its label (and marks the default);
                    // the final row offers the free-text answer instead
                    let line = match self.ask.options.get(index) {
                        Some(option) => {
                            let marker_suffix = if option.label == self.ask.recommended {
                                " (recommended)"
                            } else {
                                ""
                            };
                            Line::from(vec![
                                Span::styled(
                                    format!(
                                        "{marker}{}. {}{marker_suffix}",
                                        index + 1,
                                        option.label
                                    ),
                                    style,
                                ),
                                Span::styled(
                                    format!("  — {}", option.description),
                                    Style::default().fg(Color::DarkGray).bg(MODAL_BG),
                                ),
                            ])
                        }
                        None => Line::from(vec![Span::styled(
                            format!("{marker}o. Type a different answer…"),
                            style,
                        )]),
                    };
                    frame.render_widget(Paragraph::new(line), row);
                    self.option_rows.push((row, index));
                }
            }
            AskModalState::Typing => {
                // the free-text answer box takes the rows area
                self.editor.render(frame, chunks[1]);
            }
        }
        // render the footer key hints for the current state
        let hints = match self.state {
            AskModalState::Menu => {
                " ↑/↓ or j/k select · enter answer · 1-4 jump · o type your own · esc take \
                 default & flag for review "
            }
            AskModalState::Typing => " enter send answer · esc back to options ",
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

    impl AskModal {
        /// Arms the modal immediately, standing in for a first render in tests
        fn armed_for_tests(mut self) -> Self {
            // back-date the first render so the arming grace has elapsed
            self.armed_at = std::time::Instant::now()
                .checked_sub(ARM_GRACE)
                .or(Some(std::time::Instant::now()));
            self
        }
    }

    /// Builds the two-option pending ask the tests pose
    fn pending() -> PendingAsk {
        PendingAsk {
            id: 7,
            agent: "main".to_owned(),
            question: "Which indicators should the analysis prioritize?".to_owned(),
            options: vec![
                thorpal::AskOption {
                    label: "static".to_owned(),
                    description: "static indicators first".to_owned(),
                },
                thorpal::AskOption {
                    label: "dynamic".to_owned(),
                    description: "dynamic indicators first".to_owned(),
                },
            ],
            recommended: "static".to_owned(),
        }
    }

    /// An unarmed modal swallows every committing key but still navigates
    #[test]
    fn unarmed_modal_swallows_commits() {
        // a freshly opened (never-rendered) modal is not armed yet
        let mut modal = AskModal::new(pending());
        // queued or reflex commits are swallowed: no answer, no decline
        assert_eq!(modal.handle_key(press(KeyCode::Enter)), AskOutcome::None);
        assert_eq!(
            modal.handle_key(press(KeyCode::Char('2'))),
            AskOutcome::None
        );
        assert_eq!(
            modal.handle_key(press(KeyCode::Char('o'))),
            AskOutcome::None
        );
        assert_eq!(modal.handle_key(press(KeyCode::Esc)), AskOutcome::None);
        assert_eq!(modal.state, AskModalState::Menu);
        // navigation still works while unarmed
        assert_eq!(modal.handle_key(press(KeyCode::Down)), AskOutcome::None);
    }

    /// The modal opens on the recommended option and Enter answers with it
    #[test]
    fn opens_on_the_recommended_option() {
        let mut modal = AskModal::new(pending()).armed_for_tests();
        // "static" is the recommended first option, so Enter picks index 0
        assert_eq!(modal.handle_key(press(KeyCode::Enter)), AskOutcome::Pick(0));
    }

    /// Selection wraps across the options and the free-text row
    #[test]
    fn selection_wraps_and_confirms() {
        let mut modal = AskModal::new(pending()).armed_for_tests();
        // up from the first row wraps to the free-text row, which opens typing
        assert_eq!(modal.handle_key(press(KeyCode::Up)), AskOutcome::None);
        assert_eq!(modal.handle_key(press(KeyCode::Enter)), AskOutcome::None);
        assert_eq!(modal.state, AskModalState::Typing);
        // back out: down from the free-text row wraps to the first option,
        // and one more step lands on the second option
        assert_eq!(modal.handle_key(press(KeyCode::Esc)), AskOutcome::None);
        assert_eq!(modal.state, AskModalState::Menu);
        assert_eq!(modal.handle_key(press(KeyCode::Down)), AskOutcome::None);
        assert_eq!(modal.handle_key(press(KeyCode::Down)), AskOutcome::None);
        assert_eq!(modal.handle_key(press(KeyCode::Enter)), AskOutcome::Pick(1));
    }

    /// Digit accelerators answer directly; out-of-range digits do nothing
    #[test]
    fn digit_accelerators_answer_directly() {
        let mut modal = AskModal::new(pending()).armed_for_tests();
        assert_eq!(
            modal.handle_key(press(KeyCode::Char('2'))),
            AskOutcome::Pick(1)
        );
        assert_eq!(
            modal.handle_key(press(KeyCode::Char('9'))),
            AskOutcome::None
        );
        assert_eq!(
            modal.handle_key(press(KeyCode::Char('1'))),
            AskOutcome::Pick(0)
        );
    }

    /// Esc on the menu declines, taking the recommended default
    #[test]
    fn esc_declines() {
        let mut modal = AskModal::new(pending()).armed_for_tests();
        assert_eq!(modal.handle_key(press(KeyCode::Esc)), AskOutcome::Decline);
    }

    /// The free-text flow types an answer and submits it with Enter
    #[test]
    fn free_text_answers_round_trip() {
        let mut modal = AskModal::new(pending()).armed_for_tests();
        // 'o' jumps to and opens the free-text answer box
        assert_eq!(
            modal.handle_key(press(KeyCode::Char('o'))),
            AskOutcome::None
        );
        assert_eq!(modal.state, AskModalState::Typing);
        // an empty submit does nothing
        assert_eq!(modal.handle_key(press(KeyCode::Enter)), AskOutcome::None);
        // type an answer and send it
        assert_eq!(
            modal.handle_key(press(KeyCode::Char('h'))),
            AskOutcome::None
        );
        assert_eq!(
            modal.handle_key(press(KeyCode::Char('i'))),
            AskOutcome::None
        );
        assert_eq!(
            modal.handle_key(press(KeyCode::Enter)),
            AskOutcome::Other("hi".to_owned())
        );
    }
}
