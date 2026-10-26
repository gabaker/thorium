//! Unicode-width-aware text wrapping shared by the chat tui components

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Convert a string into multiple lines that automatically wrap text
///
/// Lines are broken at the exact column they overflow, so whitespace is kept
/// verbatim; this suits preformatted content like tool-call dumps.
///
/// # Arguments
///
/// * `input` - The string to line wrap
/// * `max_width` - The maxiumum allowable width for each line
pub(super) fn wrap_unicode_preserve_ws(input: &str, max_width: usize) -> Vec<String> {
    // short circuit if our max width is 0
    if max_width == 0 {
        return vec![String::new()];
    }
    // preallocate a vec for our line wrapped strings
    let mut out = Vec::with_capacity(5);
    // iterate over our string split by new lines
    for split in input.split('\n') {
        // preallocate a new to add our unicode chars too
        let mut line = String::new();
        // start with a length of 0
        let mut line_width = 0usize;
        // iterate over the graphemes in this line
        for grapheme in split.graphemes(true) {
            // get width of this grapheme
            let grapheme_width = UnicodeWidthStr::width(grapheme);
            // check if this grapheme fits on our current line
            if line_width > 0 && line_width + grapheme_width > max_width {
                // get our old line
                let full_line = std::mem::take(&mut line);
                // add this full line to our output
                out.push(full_line);
                // reset our width to zero for our new line
                line_width = 0;
            }
            // add this grapheme to our current line
            line.push_str(grapheme);
            // increment our width
            line_width += grapheme_width;
        }
        // add any remaining lines
        out.push(line);
    }
    out
}

/// Word-wrap a string into lines no wider than `max_width` columns
///
/// Words are packed greedily and explicit new lines are kept, so prose reads
/// naturally in a narrow pane. A single word wider than `max_width` (e.g. a long
/// hex run) is hard-broken rather than overflowing the line.
///
/// # Arguments
///
/// * `input` - The string to word wrap
/// * `max_width` - The maximum allowable width for each line
pub(super) fn wrap_words(input: &str, max_width: usize) -> Vec<String> {
    // short circuit if our max width is 0
    if max_width == 0 {
        return vec![String::new()];
    }
    // preallocate a vec for our wrapped lines
    let mut out = Vec::with_capacity(5);
    // wrap each source line on its own so explicit new lines survive
    for split in input.split('\n') {
        // the line currently being packed and its display width
        let mut line = String::new();
        let mut line_width = 0usize;
        for word in split.split_whitespace() {
            // measure the word in terminal columns
            let word_width = UnicodeWidthStr::width(word);
            // a word wider than a whole line is hard-broken across lines
            if word_width > max_width {
                // flush whatever is already on this line first
                if !line.is_empty() {
                    out.push(std::mem::take(&mut line));
                }
                // break the long word, keeping its last piece open for packing
                let mut pieces = wrap_unicode_preserve_ws(word, max_width);
                line = pieces.pop().unwrap_or_default();
                line_width = UnicodeWidthStr::width(line.as_str());
                out.extend(pieces);
                continue;
            }
            // the word plus a joining space must fit on a non-empty line
            let needed = if line.is_empty() {
                word_width
            } else {
                line_width + 1 + word_width
            };
            if needed > max_width {
                // the word does not fit, so flush this line and start fresh
                out.push(std::mem::take(&mut line));
                line_width = 0;
            }
            // join with a space unless this starts the line
            if !line.is_empty() {
                line.push(' ');
                line_width += 1;
            }
            // add the word to the line
            line.push_str(word);
            line_width += word_width;
        }
        // always emit at least one (possibly empty) line per source line
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Words pack greedily up to the width and explicit new lines survive
    #[test]
    fn wrap_words_packs_words_and_keeps_newlines() {
        // words are packed greedily up to the width
        assert_eq!(
            wrap_words("the quick brown fox", 9),
            vec!["the quick", "brown fox"]
        );
        // explicit new lines are preserved, including blank ones
        assert_eq!(wrap_words("a\n\nb", 10), vec!["a", "", "b"]);
    }

    /// A word wider than the line is broken across lines instead of overflowing
    #[test]
    fn wrap_words_hard_breaks_overlong_words() {
        // a word wider than the line is split rather than overflowing
        assert_eq!(
            wrap_words("x 0123456789 y", 4),
            vec!["x", "0123", "4567", "89 y"]
        );
    }
}
