//! The text box component for a thorctl chat tui

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

#[derive(Debug, Clone)]
pub struct TextBox {
    /// The title for this text box
    pub title: Option<String>,
    /// The content of this text box
    pub content: String,
    /// The placeholder text for this text box
    pub placeholder: String,
    /// The cursors x axis position within this text box
    pub cursor_x: usize,
    /// The cursors y axis position within this text box
    pub cursor_y: usize,
    /// The cursor position as a CHAR index into the content (never a byte
    /// offset: multi-byte text would panic byte-indexed `String` edits)
    pub cursor_absolute: usize,
    /// Whether the text box is focused or not
    pub is_focused: bool,
    /// Whether the content renders masked (every char shown as `•`), for
    /// secrets; the content itself and the cursor math are untouched
    pub masked: bool,
    /// Vertical scroll offset for the text box
    pub scroll_y: u16,
    /// The area for this text box on screen
    pub area: Option<Rect>,
}

impl Default for TextBox {
    /// Create an empty non focused text box
    fn default() -> Self {
        TextBox {
            title: None,
            content: String::default(),
            placeholder: String::default(),
            cursor_x: 0,
            cursor_y: 0,
            cursor_absolute: 0,
            is_focused: false,
            masked: false,
            scroll_y: 0,
            area: None,
        }
    }
}

impl TextBox {
    /// Create a new home page
    ///
    /// # Arguments
    ///
    /// * `content` - the content to set for this home page
    pub fn new(content: impl Into<String>) -> Self {
        TextBox {
            title: None,
            content: content.into(),
            placeholder: String::default(),
            cursor_x: 0,
            cursor_y: 0,
            cursor_absolute: 0,
            is_focused: true,
            masked: false,
            scroll_y: 0,
            area: None,
        }
    }

    /// Set the title for this text box
    ///
    /// # Arguments
    ///
    /// * `title` - The new title to set
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Set the placeholder text for this text box
    ///
    /// # Arguments
    ///
    /// * `placeholder` - the placeholder to set for this home page
    pub fn placeholder(mut self, placeholder: impl Into<String>) -> Self {
        // insert our placeholder text
        self.placeholder = placeholder.into();
        self
    }

    /// Set this text box to be focused
    pub fn focused(mut self) -> Self {
        self.is_focused = true;
        self
    }

    /// Mask this text box's content at render time (for secrets)
    pub fn masked(mut self) -> Self {
        self.masked = true;
        self
    }

    /// Clear all content in this text box
    pub fn clear(&mut self) {
        // clear the content in this text box
        self.content.clear();
        // reset this text box no matter what
        self.cursor_x = 0;
        self.cursor_y = 0;
        self.cursor_absolute = 0;
    }

    /// Replace this text box's content, moving the cursor to the end
    ///
    /// # Arguments
    ///
    /// * `content` - The new content to set
    pub fn set_content(&mut self, content: String) {
        // move the cursor to the very end of the new content (a char count,
        // never a byte length: multi-byte text would desync the two)
        self.cursor_absolute = content.chars().count();
        // swap the new content in and recompute the line/column pair
        self.content = content;
        self.sync_xy();
    }

    /// The byte offset the char-indexed cursor sits at
    ///
    /// `String` edits are byte-indexed, so every mutation maps the cursor
    /// through this; the end of the content maps to its byte length.
    fn byte_offset(&self) -> usize {
        // map the char index to its byte position, with the end as fallback
        self.content
            .char_indices()
            .nth(self.cursor_absolute)
            .map_or(self.content.len(), |(index, _)| index)
    }

    /// Recompute the cursor's line/column pair from its char index
    ///
    /// One pass over the chars before the cursor replaces the per-edit
    /// line-math every mutation used to hand-maintain.
    fn sync_xy(&mut self) {
        // count lines and the column within the current line up to the cursor
        let mut x = 0;
        let mut y = 0;
        for ch in self.content.chars().take(self.cursor_absolute) {
            if ch == '\n' {
                y += 1;
                x = 0;
            } else {
                x += 1;
            }
        }
        self.cursor_x = x;
        self.cursor_y = y;
    }

    /// Insert a character at the current cursor position
    ///
    /// # Arguments
    ///
    /// * `c` - The character to insert
    fn insert_char(&mut self, c: char) {
        // insert at the cursor's byte position, then advance one char
        self.content.insert(self.byte_offset(), c);
        self.cursor_absolute += 1;
        // recompute the line/column pair for the render-time cursor
        self.sync_xy();
    }

    /// Insert a newline at the current cursor position
    fn insert_newline(&mut self) {
        // a newline is just a char insert; sync_xy lands the cursor on the
        // fresh line's first column
        self.insert_char('\n');
    }

    /// Insert a whole string at the current cursor position (e.g. a paste)
    ///
    /// Line endings are normalized to `\n`, so a CRLF paste never embeds bare
    /// carriage returns, and the cursor lands after the inserted text.
    ///
    /// # Arguments
    ///
    /// * `text` - The text to insert
    pub fn insert_str(&mut self, text: &str) {
        // normalize CRLF and stray CR line endings to plain newlines
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        // splice the whole text in at the cursor's byte position
        self.content.insert_str(self.byte_offset(), &normalized);
        // advance the cursor past what was inserted (chars, not bytes)
        self.cursor_absolute += normalized.chars().count();
        // recompute the line/column pair for the render-time cursor
        self.sync_xy();
    }

    /// Delete the character before the cursor (backspace)
    fn delete_char_before(&mut self) {
        // nothing to delete at the very start
        if self.cursor_absolute == 0 {
            return;
        }
        // step the cursor back one char, then remove the char it now sits on
        self.cursor_absolute -= 1;
        self.content.remove(self.byte_offset());
        // recompute the line/column pair (a removed newline joins two lines)
        self.sync_xy();
    }

    /// Delete the character at the cursor (delete)
    fn delete_char_at(&mut self) {
        // nothing to delete at the very end
        if self.cursor_absolute >= self.content.chars().count() {
            return;
        }
        // remove the char under the cursor; the cursor itself stays put
        self.content.remove(self.byte_offset());
        // recompute the line/column pair (a removed newline joins two lines)
        self.sync_xy();
    }

    /// Move the cursor left
    ///
    /// Crosses newlines, so every char of a multi-line paste stays reachable.
    fn move_cursor_left(&mut self) {
        // we can only move our cursor if we are not at the very start
        if self.cursor_absolute > 0 {
            // step back one char and recompute the line/column pair (a step
            // across a newline lands at the end of the previous line)
            self.cursor_absolute -= 1;
            self.sync_xy();
        }
    }

    /// Move the cursor right
    ///
    /// Crosses newlines, so every char of a multi-line paste stays reachable.
    fn move_cursor_right(&mut self) {
        // we can't move our cursor past the end of our content
        if self.cursor_absolute < self.content.chars().count() {
            // step forward one char and recompute the line/column pair (a
            // step across a newline lands at the next line's first column)
            self.cursor_absolute += 1;
            self.sync_xy();
        }
    }

    /// Move our cursor to the start of the text box
    fn move_cursor_to_start(&mut self) {
        // reset all of our cursor values to 0
        self.cursor_x = 0;
        self.cursor_y = 0;
        self.cursor_absolute = 0;
    }

    /// Move our cursor to the end of this line
    fn move_cursor_to_end_of_line(&mut self) {
        // find how many characters to get to the end of the current line
        let to_end = self
            .content
            .chars()
            .skip(self.cursor_absolute)
            .take_while(|c| c != &'\n')
            .count();
        // increment our cursor values
        self.cursor_x += to_end;
        self.cursor_absolute += to_end;
    }

    /// Handle key input for this text box
    ///
    /// # Arguments
    ///
    /// * `code` - The key code that was pressed
    pub fn handle_key(&mut self, key: KeyEvent, allow_empty: bool) -> bool {
        //// reset our content if we have placeholder text
        //self.clear_placeholder();
        // handle this key event
        match key.code {
            // Delete character before cursor
            KeyCode::Backspace => self.delete_char_before(),
            // Delete character at cursor
            KeyCode::Delete => self.delete_char_at(),
            // Move cursor left
            KeyCode::Left => self.move_cursor_left(),
            // Move cursor right
            KeyCode::Right => self.move_cursor_right(),
            // Move cursor to start
            KeyCode::Home => self.move_cursor_to_start(),
            // Move cursor to end
            KeyCode::End => self.move_cursor_to_end_of_line(),
            // Add a new line to this text box
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.insert_newline();
            }
            // Type characters
            KeyCode::Char(c) => self.insert_char(c),
            // return that a user is finished with this text box
            KeyCode::Enter => {
                // if we don't want to allow empty text boxes then make sure content
                // is not empty
                if allow_empty {
                    return true;
                } else {
                    return !self.content.is_empty();
                }
            }
            // unfocus on this prompt
            KeyCode::Esc => self.is_focused = false,
            _ => {}
        }
        // all key codes other then enter are not stating this text box is finished
        false
    }

    /// Handle a mouse click event
    ///
    /// If this method returns true then this click was in this text boxes area.
    ///
    /// # Arguments
    ///
    /// * `x` - The column position of the click
    /// * `y` - The row position of the click
    pub fn handle_click(&mut self, x: u16, y: u16) -> bool {
        // we should always have an area as long as this text box has been rendered
        if let Some(area) = self.area {
            // Check if the click was on this text box
            if x >= area.x && x < area.x + area.width && y >= area.y && y < area.y + area.height {
                self.is_focused = true;
                return true;
            }
        }
        // Click was outside the query box, unfocus it
        self.is_focused = false;
        false
    }

    /// Consume this text box and replace with some placeholder text
    ///
    /// If this text box contains no text then None will be returned.
    pub fn consume(&mut self) -> Option<String> {
        // consume and replace our old text
        let consumed = std::mem::take(&mut self.content);
        // reset this text box no matter what
        self.cursor_x = 0;
        self.cursor_y = 0;
        self.cursor_absolute = 0;
        // only return our consumed text if it wasn't empty
        if consumed.is_empty() {
            None
        } else {
            Some(consumed)
        }
    }

    /// Render the text box to the frame
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame to render to
    /// * `area` - The area to render the input in
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        // get the right content to display; masked boxes show one bullet per
        // char (same char count, so the cursor math needs no special casing)
        let content = if self.content.is_empty() {
            self.placeholder.clone()
        } else if self.masked {
            self.content
                .chars()
                .map(|c| if c == '\n' { '\n' } else { '•' })
                .collect()
        } else {
            self.content.clone()
        };
        // if this is placeholder text then make the text grey
        let style = if self.content.is_empty() {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::White)
        };
        // the block to render in this input widgeth
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan));
        // if we have a title then also set that
        let block = match &self.title {
            // set our title in this block
            Some(title) => block.title(format!(" {title} ")).title_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            None => block,
        };
        // build the input widget
        let input = Paragraph::new(content)
            .style(style)
            .block(block)
            .wrap(Wrap { trim: false });
        // render the input
        //frame.render_widget(&self.textarea, area);
        frame.render_widget(input, area);
        // show cursor when focused
        if self.is_focused {
            // calculate cursor position within the input area
            // account for the border (1 char) on the left
            let cursor_x = area.x + 1 + self.cursor_x as u16;
            let cursor_y = area.y + 1 + self.cursor_y as u16;
            // only show the cursor while it's within the visible area on BOTH
            // axes: a multi-line paste can push the logical line past the
            // box's fixed height, and an unclamped y would paint the terminal
            // cursor over whatever widget sits below
            if cursor_x < area.x + area.width - 1 && cursor_y < area.y + area.height - 1 {
                frame.set_cursor_position((cursor_x, cursor_y));
            }
        }
        // update this clickable area for this text box
        self.area = Some(area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Multi-byte chars survive typing, backspace, and delete without panics
    ///
    /// This is the exact user crash: pasting text with `’` (3 bytes in UTF-8)
    /// desynced the byte-indexed insert from the per-char cursor, so the NEXT
    /// insert panicked on `is_char_boundary`.
    #[test]
    fn multibyte_insert_backspace_delete_round_trip() {
        let mut text_box = TextBox::default();
        // type around a 3-byte quote and a 4-byte emoji
        for ch in "Ghidra’s 🦀 code".chars() {
            text_box.insert_char(ch);
        }
        assert_eq!(text_box.content, "Ghidra’s 🦀 code");
        assert_eq!(text_box.cursor_absolute, 15, "the cursor counts chars");
        // backspace erases char by char straight through the wide chars
        for _ in 0..15 {
            text_box.delete_char_before();
        }
        assert_eq!(text_box.content, "");
        assert_eq!(text_box.cursor_absolute, 0);
        // delete-at from the front chews through wide chars the same way
        text_box.insert_str("’🦀x");
        text_box.move_cursor_to_start();
        text_box.delete_char_at();
        text_box.delete_char_at();
        assert_eq!(text_box.content, "x");
    }

    /// Delete-at removes the char under the cursor without moving the cursor
    #[test]
    fn delete_at_removes_under_cursor_without_moving() {
        let mut text_box = TextBox::default();
        text_box.insert_str("abc");
        // park the cursor on 'b' and delete it
        text_box.move_cursor_to_start();
        text_box.move_cursor_right();
        text_box.delete_char_at();
        assert_eq!(text_box.content, "ac");
        // the cursor stayed where it was, now over 'c'
        assert_eq!(text_box.cursor_absolute, 1);
        assert_eq!(text_box.cursor_x, 1);
        // deleting at the end is a no-op
        text_box.move_cursor_right();
        text_box.delete_char_at();
        assert_eq!(text_box.content, "ac");
    }

    /// Arrows, home, and end navigate multi-byte lines by chars
    #[test]
    fn arrows_home_end_navigate_multibyte_lines() {
        let mut text_box = TextBox::default();
        text_box.insert_str("a’b");
        // walk left over the quote and back to the end of the line
        text_box.move_cursor_left();
        text_box.move_cursor_left();
        assert_eq!(text_box.cursor_absolute, 1);
        text_box.move_cursor_to_end_of_line();
        assert_eq!(text_box.cursor_absolute, 3);
        assert_eq!(text_box.cursor_x, 3);
        // home resets everything; right steps one char at a time
        text_box.move_cursor_to_start();
        text_box.move_cursor_right();
        assert_eq!(text_box.cursor_absolute, 1);
        // typing at the cursor splices mid-string on a char boundary
        text_box.insert_char('x');
        assert_eq!(text_box.content, "ax’b");
        // arrows cross newlines, so every pasted line stays reachable
        let mut text_box = TextBox::default();
        text_box.insert_str("ab\ncd");
        text_box.move_cursor_to_start();
        for _ in 0..3 {
            text_box.move_cursor_right();
        }
        assert_eq!((text_box.cursor_y, text_box.cursor_x), (1, 0));
        text_box.move_cursor_left();
        assert_eq!((text_box.cursor_y, text_box.cursor_x), (0, 2));
    }

    /// Pasted strings normalize CRLF/CR and place the cursor after the text
    #[test]
    fn insert_str_normalizes_crlf_and_places_the_cursor() {
        let mut text_box = TextBox::default();
        // a multi-line paste with CRLF and a stray CR
        text_box.insert_str("one\r\ntwo\rthree");
        assert_eq!(text_box.content, "one\ntwo\nthree");
        // the cursor landed at the very end, on the third line
        assert_eq!(text_box.cursor_absolute, 13);
        assert_eq!(text_box.cursor_y, 2);
        assert_eq!(text_box.cursor_x, 5);
    }

    /// set_content counts the cursor in chars, never bytes
    #[test]
    fn set_content_counts_chars_not_bytes() {
        let mut text_box = TextBox::default();
        text_box.set_content("l’né\nx".to_owned());
        // 4 chars on line one, a newline, then 1 char on line two
        assert_eq!(text_box.cursor_absolute, 6);
        assert_eq!(text_box.cursor_y, 1);
        assert_eq!(text_box.cursor_x, 1);
        // typing right after a set_content must not panic mid-char
        text_box.insert_char('!');
        assert_eq!(text_box.content, "l’né\nx!");
    }

    /// A paste into the middle preserves both sides and the line math
    #[test]
    fn paste_into_middle_preserves_both_sides() {
        let mut text_box = TextBox::default();
        text_box.insert_str("head tail");
        // park the cursor after "head " and paste a multi-line snippet
        text_box.move_cursor_to_start();
        for _ in 0..5 {
            text_box.move_cursor_right();
        }
        text_box.insert_str("mid\nline ");
        assert_eq!(text_box.content, "head mid\nline tail");
        // the cursor sits right after the pasted text, before "tail"
        assert_eq!(text_box.cursor_y, 1);
        assert_eq!(text_box.cursor_x, 5);
        // and backspacing over "line " plus the newline joins the lines
        for _ in 0..6 {
            text_box.delete_char_before();
        }
        assert_eq!(text_box.content, "head midtail");
        assert_eq!(text_box.cursor_y, 0);
        assert_eq!(text_box.cursor_x, 8);
    }
}
