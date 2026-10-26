//! Terminal colors for any displayable value that switch off when they would only add noise
//!
//! The methods mirror the ones thorctl uses from `owo_colors`, but they paint through the
//! `colored` crate, which prints plain text unless stdout is a terminal and honors `NO_COLOR`
//! and `CLICOLOR`. Output sent to a pipe or a file therefore never carries escape sequences.

use colored::{Color, ColoredString, Colorize};
use std::fmt::Display;

/// Colors for anything that can be displayed
pub trait Paint: Display {
    /// Paint the value bright red
    fn bright_red(&self) -> ColoredString {
        self.to_string().as_str().bright_red()
    }

    /// Paint the value bright green
    fn bright_green(&self) -> ColoredString {
        self.to_string().as_str().bright_green()
    }

    /// Paint the value bright yellow
    fn bright_yellow(&self) -> ColoredString {
        self.to_string().as_str().bright_yellow()
    }

    /// Paint the value bright blue
    fn bright_blue(&self) -> ColoredString {
        self.to_string().as_str().bright_blue()
    }

    /// Print the value in bold
    fn bold(&self) -> ColoredString {
        self.to_string().as_str().bold()
    }

    /// Paint the value in a color of choice
    ///
    /// # Arguments
    ///
    /// * `color` - The color to paint with
    fn color(&self, color: Color) -> ColoredString {
        self.to_string().as_str().color(color)
    }
}

impl<T: Display> Paint for T {}

#[cfg(test)]
mod tests {
    use super::Paint;

    /// Painting keeps the text of the value, whatever the terminal decides about color
    #[test]
    fn painting_keeps_the_text() {
        // a number and a string both display as themselves once the escapes are removed
        let plain = |text: String| -> String {
            let mut out = String::new();
            let mut chars = text.chars();
            while let Some(c) = chars.next() {
                if c == '\u{1b}' {
                    for end in chars.by_ref() {
                        if end == 'm' {
                            break;
                        }
                    }
                } else {
                    out.push(c);
                }
            }
            out
        };
        assert_eq!(plain(Paint::bright_green(&409).to_string()), "409");
        assert_eq!(plain(Paint::bold(&String::from("ok")).to_string()), "ok");
    }

    /// Without a terminal on stdout (as under `cargo test`) nothing is painted
    #[test]
    fn a_pipe_gets_plain_text() {
        // the test harness captures stdout, so `colored` must decide not to color
        colored::control::unset_override();
        assert_eq!(Paint::bright_red(&"x").to_string(), "x");
    }
}
