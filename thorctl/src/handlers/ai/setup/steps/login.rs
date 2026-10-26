//! The Thorium login page shown when no usable config exists yet

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::handlers::ai::chat::TextBox;

use super::{dim_style, spinner};

/// How many focus positions the page has: three fields plus two buttons
const FOCUS_POSITIONS: usize = 5;

/// The focus position of the log-in button
const FOCUS_LOGIN: usize = 3;

/// The focus position of the skip button
const FOCUS_SKIP: usize = 4;

/// What a key press on the login page asks the app to do
pub enum LoginOutcome {
    /// Nothing; the page consumed the key itself
    None,
    /// Log into Thorium with these credentials
    Submit {
        /// The Thorium API url to log into
        url: String,
        /// The username to authenticate as
        username: String,
        /// The password to authenticate with
        password: String,
    },
    /// Skip the login and write placeholder keys
    Skip,
    /// Go back one step
    Back,
}

/// The login page's input state
pub struct LoginStep {
    /// The Thorium API url field
    url: TextBox,
    /// The username field
    username: TextBox,
    /// The password field, masked at render time
    password: TextBox,
    /// Which field or button holds focus
    focus: usize,
    /// The last login failure, shown inline until the next attempt
    pub error: Option<String>,
}

impl LoginStep {
    /// Create the login page with empty fields
    pub fn new() -> Self {
        LoginStep {
            url: TextBox::default()
                .title("Thorium API url")
                .placeholder("https://thorium.example.com"),
            username: TextBox::default().title("Username"),
            password: TextBox::default().title("Password").masked(),
            focus: 0,
            error: None,
        }
    }

    /// Handle a key press on the login page
    ///
    /// # Arguments
    ///
    /// * `key` - The key event to handle
    pub fn handle_key(&mut self, key: KeyEvent) -> LoginOutcome {
        // escape always backs out of the page (intercepted before the boxes,
        // whose own esc handling would just unfocus)
        if key.code == KeyCode::Esc {
            return LoginOutcome::Back;
        }
        // control chords never type into these fields: ctrl+c quits globally,
        // and the ctrl+s/ctrl+r chords other steps teach must not leak a
        // literal char (or a ctrl+j newline) into a masked password
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return LoginOutcome::None;
        }
        // tab/arrows walk the fields and buttons in order with wraparound
        match key.code {
            KeyCode::Tab | KeyCode::Down => {
                self.focus = (self.focus + 1) % FOCUS_POSITIONS;
                return LoginOutcome::None;
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.focus = (self.focus + FOCUS_POSITIONS - 1) % FOCUS_POSITIONS;
                return LoginOutcome::None;
            }
            _ => {}
        }
        // the buttons only react to enter; stray typing is ignored on them
        if self.focus >= FOCUS_LOGIN {
            if key.code == KeyCode::Enter {
                if self.focus == FOCUS_SKIP {
                    return LoginOutcome::Skip;
                }
                return self.submit();
            }
            return LoginOutcome::None;
        }
        // everything else routes into the focused field; a submitted (enter on
        // a non-empty box) field advances focus toward the login button
        let submitted = self.focused_box().handle_key(key, false);
        if submitted {
            self.focus += 1;
        }
        LoginOutcome::None
    }

    /// Insert pasted text into the focused field
    ///
    /// # Arguments
    ///
    /// * `text` - The pasted text to insert
    pub fn paste(&mut self, text: &str) {
        // pastes only make sense into the fields, never the buttons; strip
        // the trailing newline terminals append to copied lines, which no
        // url, username, or password legitimately ends with
        if self.focus < FOCUS_LOGIN {
            self.focused_box()
                .insert_str(text.trim_end_matches(['\r', '\n']));
        }
    }

    /// The field box currently holding focus
    fn focused_box(&mut self) -> &mut TextBox {
        match self.focus {
            0 => &mut self.url,
            1 => &mut self.username,
            _ => &mut self.password,
        }
    }

    /// Validate the fields and build the submit outcome
    fn submit(&mut self) -> LoginOutcome {
        // every field is required to attempt a login
        let url = self.url.content.trim().to_string();
        let username = self.username.content.trim().to_string();
        let password = self.password.content.clone();
        if url.is_empty() || username.is_empty() || password.is_empty() {
            self.error = Some("the url, username, and password are all required".to_string());
            return LoginOutcome::None;
        }
        // a url without a scheme cannot build a client; catch the typo here
        if !url.starts_with("http://") && !url.starts_with("https://") {
            self.error = Some("the API url must begin with http:// or https://".to_string());
            return LoginOutcome::None;
        }
        self.error = None;
        LoginOutcome::Submit {
            url,
            username,
            password,
        }
    }

    /// Render the login page
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render into
    /// * `area` - The body area to render in
    /// * `logging_in` - Whether a login attempt is in flight
    pub fn render(&mut self, frame: &mut Frame, area: Rect, logging_in: bool) {
        // carve the page into an intro, the three fields, buttons, and status
        let [intro, url, username, password, buttons, status] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(2),
            Constraint::Min(0),
        ])
        .areas(area);
        // explain why the page is here at all
        frame.render_widget(
            Paragraph::new(vec![Line::styled(
                "No usable thorctl config was found, so the wizard can log into \
                    Thorium and create one. Skipping writes an AI-only config; run \
                    `thorctl login` later for Thorium access.",
                dim_style(),
            )])
            .wrap(ratatui::widgets::Wrap { trim: false }),
            intro,
        );
        // only the focused field shows a cursor
        self.url.is_focused = self.focus == 0;
        self.username.is_focused = self.focus == 1;
        self.password.is_focused = self.focus == 2;
        self.url.render(frame, url);
        self.username.render(frame, username);
        self.password.render(frame, password);
        // the two buttons highlight by focus
        let button = |label: &str, focused: bool| {
            if focused {
                Span::styled(
                    format!("[ {label} ]"),
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(format!("[ {label} ]"), Style::default().fg(Color::Gray))
            }
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                button("Log in", self.focus == FOCUS_LOGIN),
                Span::raw("   "),
                button("Skip for now", self.focus == FOCUS_SKIP),
            ])),
            buttons,
        );
        // the status strip carries the in-flight spinner or the last failure
        let line = if logging_in {
            Line::styled(
                format!("{} logging into Thorium…", spinner()),
                Style::default().fg(Color::Yellow),
            )
        } else if let Some(error) = &self.error {
            Line::styled(error.clone(), Style::default().fg(Color::Red))
        } else {
            Line::raw("")
        };
        frame.render_widget(
            Paragraph::new(line).wrap(ratatui::widgets::Wrap { trim: false }),
            status,
        );
    }
}
