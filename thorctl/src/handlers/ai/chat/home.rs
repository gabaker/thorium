//! The home page for the Thorium chat tui

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};

use super::AppEvent;
use super::components::{ActivitySnapshot, Banner, ShortcutMenu, TextBox, status_bar};

pub struct Home {
    /// The Thorium banner
    banner: Banner,
    /// The text box for our prompt
    pub prompt: TextBox,
    /// The shortcut menu for this page
    pub shortcuts: ShortcutMenu,
    /// The content area between the tab band and the prompt, as last rendered
    content_area: Rect,
}

impl Default for Home {
    fn default() -> Self {
        // build the banner for our home page
        let banner = Banner::new("Thorium");
        // build our promp
        let prompt = TextBox::default()
            .title("Prompt")
            .placeholder("How can Thorium help?")
            .focused();
        // build the shortcut menu for this page
        let shortcuts = ShortcutMenu::builder().build();
        // build our home page
        Home {
            banner,
            prompt,
            shortcuts,
            content_area: Rect::default(),
        }
    }
}

impl Home {
    /// Create a new home page
    ///
    /// # Arguments
    ///
    /// * `prompt` - the prompt to set for this home page
    pub fn new(prompt: impl Into<String>) -> Self {
        // start with a default home page
        let mut home = Home::default();
        // update our prompt
        home.prompt.placeholder = prompt.into();
        // return our new home page
        home
    }

    /// Insert pasted text into the prompt when it has the keyboard
    ///
    /// # Arguments
    ///
    /// * `text` - The pasted text to insert
    pub fn handle_paste(&mut self, text: &str) {
        // paste only ever edits the focused prompt; it never submits
        if self.prompt.is_focused {
            self.prompt.insert_str(text);
        }
    }

    /// Handle key input for this text box
    ///
    /// # Arguments
    ///
    /// * `code` - The key code that was pressed
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<AppEvent> {
        // if our prompt is focused then send the key to the prompt
        if self.prompt.is_focused {
            // send this input to our prompt text box and check if we
            // are consuming this prompt box and changing to a chat page
            // pass this input to our prompt text box
            if self.prompt.handle_key(key, false) {
                // tell our tab we are changing our active page to a chat page
                return Some(AppEvent::HomeToChat);
            }
        } else {
            // handle this key
            match (key.code, self.shortcuts.active) {
                // toggle shortcut mode off and on
                (KeyCode::Char(' '), _) => self.shortcuts.toggle(),
                // toggle our shortcut mode off
                (KeyCode::Esc, true) => self.shortcuts.toggle(),
                // hand the keyboard to the prompt box for typing
                (KeyCode::Char('i'), false) => self.prompt.is_focused = true,
                // open or close the message board
                (KeyCode::Char('m'), true) => {
                    // a shortcut was picked, so hide the shortcut menu
                    self.shortcuts.toggle();
                    return Some(AppEvent::ToggleMessageBoard);
                }
                // open or close the traversal graph overlay
                (KeyCode::Char('g'), true) => {
                    // a shortcut was picked, so hide the shortcut menu
                    self.shortcuts.toggle();
                    return Some(AppEvent::ToggleTraversal);
                }
                // open the message board with its forum search ready to type into
                (KeyCode::Char('s'), true) => {
                    // a shortcut was picked, so hide the shortcut menu
                    self.shortcuts.toggle();
                    return Some(AppEvent::OpenForumSearch);
                }
                // open or close the usage (agent stats) overlay
                (KeyCode::Char('u'), true) => {
                    // a shortcut was picked, so hide the shortcut menu
                    self.shortcuts.toggle();
                    return Some(AppEvent::ToggleAgentStats);
                }
                // open or close the gantt timeline overlay
                (KeyCode::Char('t'), true) => {
                    // a shortcut was picked, so hide the shortcut menu
                    self.shortcuts.toggle();
                    return Some(AppEvent::ToggleGantt);
                }
                _ => (),
            }
        }
        None
    }

    /// Handle a mouse click event
    ///
    /// # Arguments
    ///
    /// * `x` - The column position of the click
    /// * `y` - The row position of the click
    pub fn handle_click(&mut self, x: u16, y: u16) {
        // right now the only thing clickable is the prompt box
        self.prompt.handle_click(x, y);
    }

    /// The content area between the tab band and the prompt, as last rendered
    ///
    /// The tab anchors the forum-search preview on this rect when a search
    /// runs while the home page is showing.
    pub fn content_area(&self) -> Rect {
        self.content_area
    }

    /// Render the home tab content
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render to
    /// * `area` - The area to render this page in
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        // split the area into four vertical chunks: tabs, content, query input, and status bar
        let chunks = Layout::vertical([
            Constraint::Length(3), // tabs
            Constraint::Min(0),    // content
            Constraint::Length(8), // query input
            Constraint::Length(2), // status bar
        ])
        .split(area);
        // remember the content area so overlays can anchor on it
        self.content_area = chunks[1];
        // render a banner
        self.banner.render(frame, chunks[1]);
        // render our prompt
        self.prompt.render(frame, chunks[2]);
        // render our status bar; the home page tracks no run usage, so pass a
        // default snapshot (which suppresses the token readout) and no cost
        status_bar::render(
            frame,
            chunks[3],
            self.prompt.is_focused,
            &ActivitySnapshot::default(),
            None,
            None,
        );
        // render our shortcut menu if its active
        self.shortcuts.render(frame);
    }
}
