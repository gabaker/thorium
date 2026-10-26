//! Markdown rendering shared by the chat tui components
//!
//! Agent-written text (chat responses and forum post/comment bodies) is parsed
//! as markdown and word-wrapped into styled rows, so headings, emphasis, lists,
//! and code blocks read clearly instead of as one dense wall of prose. Both the
//! message log and the message board pre-wrap their own text (neither uses
//! ratatui's `Wrap`), so the rows produced here must already fit the pane.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::text_wrap::wrap_words;

/// A piece of one markdown source line: owned text drawn in one resolved style
type Segment = (String, Style);

/// The foreground-only markdown style sheet shared by both chat surfaces
///
/// The message board forces every content span onto its card background, so any
/// background a style sets would be silently dropped there; styling with
/// foreground colors and modifiers only keeps posts in the board and messages
/// in the log looking the same.
#[derive(Clone, Copy, Debug)]
struct ChatStyleSheet;

impl tui_markdown::StyleSheet for ChatStyleSheet {
    /// The style for a markdown heading
    ///
    /// # Arguments
    ///
    /// * `level` - The one-based heading level (1 for `# H1`)
    fn heading(&self, level: u8) -> Style {
        // deeper headings step down from bold underlined to italic
        match level {
            1 => Style::new()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            2 => Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            3 => Style::new()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD | Modifier::ITALIC),
            _ => Style::new()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::ITALIC),
        }
    }

    /// The style for inline code and unhighlighted code blocks
    fn code(&self) -> Style {
        // a foreground accent instead of the default white-on-black so the
        // highlight survives the board's card background
        Style::new().fg(Color::LightYellow)
    }
}

/// Render markdown into styled rows word-wrapped to a pane width
///
/// Unstyled prose keeps the `base` style (the caller's theme color) while
/// markdown styles are patched on top of it. Code block content is hard-wrapped
/// so its indentation survives; everything else wraps at word boundaries. Every
/// row's summed unicode width is at most `width`. Empty text yields no rows.
///
/// # Arguments
///
/// * `markdown` - The markdown text to render
/// * `width` - The number of columns each row may use
/// * `base` - The style for text the markdown leaves unstyled
pub(super) fn markdown_rows(markdown: &str, width: usize, base: Style) -> Vec<Vec<Span<'static>>> {
    // empty text or a zero-width pane adds nothing
    if markdown.trim().is_empty() || width == 0 {
        return Vec::new();
    }
    // normalize carriage returns away so the parser only sees \n breaks
    let cleaned = markdown.replace('\r', "");
    // parse the markdown with our style sheet, wrapping tables to the pane
    let options = tui_markdown::Options::new(ChatStyleSheet)
        .table_width(u16::try_from(width).unwrap_or(u16::MAX));
    let text = tui_markdown::from_str_with_options(&cleaned, &options);
    // flatten each line's style down onto owned per-span segments
    let flat: Vec<Vec<Segment>> = text
        .lines
        .iter()
        .map(|line| flatten_line(line, base))
        .collect();
    // drop blank edges and collapse blank runs the parser leaves behind
    let flat = tidy_lines(flat);
    // wrap each parsed line, hard-wrapping the lines inside code fences
    let mut rows = Vec::with_capacity(flat.len());
    let mut in_fence = false;
    for segments in &flat {
        // fence delimiter lines (```lang) toggle preformatted wrapping
        let fence = segments
            .first()
            .is_some_and(|(text, _)| text.starts_with("```"));
        // wrap this line to the width in the mode the fence state picks
        rows.extend(wrap_segments(segments, width, in_fence || fence));
        // the lines between a pair of fences are code block content
        if fence {
            in_fence = !in_fence;
        }
    }
    // fall back to plain wrapped text if parsing produced nothing to draw
    if rows.is_empty() {
        return wrap_words(markdown.trim_end(), width)
            .into_iter()
            .map(|row| vec![Span::styled(row, base)])
            .collect();
    }
    rows
}

/// Render markdown into styled lines word-wrapped to a pane width
///
/// This is [`markdown_rows`] packaged as [`Line`]s for the message log. All
/// styling sits on the spans rather than the line, so rows painted as
/// single-height paragraphs keep their styles.
///
/// # Arguments
///
/// * `markdown` - The markdown text to render
/// * `width` - The number of columns each line may use
/// * `base` - The style for text the markdown leaves unstyled
pub(super) fn markdown_lines(markdown: &str, width: usize, base: Style) -> Vec<Line<'static>> {
    // wrap into rows then lift each row of spans into a line
    markdown_rows(markdown, width, base)
        .into_iter()
        .map(Line::from)
        .collect()
}

/// Flatten one parsed line into owned segments over a base style
///
/// # Arguments
///
/// * `line` - The parsed markdown line to flatten
/// * `base` - The style for text the markdown leaves unstyled
fn flatten_line(line: &Line, base: Style) -> Vec<Segment> {
    // patch the line-level style (headings, blockquotes) then the span's own
    // style over the base, so markdown styles win where they are set
    line.spans
        .iter()
        .map(|span| {
            (
                span.content.to_string(),
                base.patch(line.style).patch(span.style),
            )
        })
        .collect()
}

/// Drop blank edge lines and collapse runs of blank lines down to one
///
/// # Arguments
///
/// * `lines` - The flattened lines to tidy
fn tidy_lines(lines: Vec<Vec<Segment>>) -> Vec<Vec<Segment>> {
    // preallocate room for every line
    let mut out = Vec::with_capacity(lines.len());
    // treating the start as blank drops any leading blank lines
    let mut last_blank = true;
    for line in lines {
        // a line is blank when no segment has visible text
        let blank = line.iter().all(|(text, _)| text.trim().is_empty());
        // skip a blank line that follows another blank line
        if blank && last_blank {
            continue;
        }
        last_blank = blank;
        // normalize blank lines so they draw as truly empty rows
        out.push(if blank { Vec::new() } else { line });
    }
    // drop any blank line left at the end
    while out.last().is_some_and(Vec::is_empty) {
        out.pop();
    }
    out
}

/// Wrap one styled line into rows whose summed width fits the pane
///
/// A line that already fits is passed through verbatim, which keeps the
/// spacing of pre-shaped content like table rows intact. An overflowing line
/// is hard-wrapped when preformatted (code) or word-wrapped otherwise.
///
/// # Arguments
///
/// * `segments` - The styled pieces of the line, in display order
/// * `width` - The number of columns each row may use
/// * `preformatted` - Whether to preserve whitespace instead of word-wrapping
fn wrap_segments(
    segments: &[Segment],
    width: usize,
    preformatted: bool,
) -> Vec<Vec<Span<'static>>> {
    // measure the whole line in terminal columns
    let total: usize = segments
        .iter()
        .map(|(text, _)| UnicodeWidthStr::width(text.as_str()))
        .sum();
    // a line that fits is emitted verbatim, preserving all of its spacing
    if total <= width {
        let row = segments
            .iter()
            .filter(|(text, _)| !text.is_empty())
            .map(|(text, style)| Span::styled(text.clone(), *style))
            .collect();
        return vec![row];
    }
    // code lines keep their whitespace; prose rewraps at word boundaries
    if preformatted {
        hard_wrap(segments, width)
    } else {
        pack_tokens(&prose_tokens(segments), width)
    }
}

/// Split styled segments into word tokens, dropping the whitespace between them
///
/// A word that straddles a style boundary (e.g. `**bo**ld`) stays one token
/// made of multiple styled pieces, so it is never broken across rows.
///
/// # Arguments
///
/// * `segments` - The styled pieces of the line, in display order
fn prose_tokens(segments: &[Segment]) -> Vec<Vec<Segment>> {
    // the tokens found so far and the token currently being built
    let mut tokens = Vec::new();
    let mut current: Vec<Segment> = Vec::new();
    for (content, style) in segments {
        // the run of non-whitespace in this segment belonging to the token
        let mut piece = String::new();
        for ch in content.chars() {
            if ch.is_whitespace() {
                // whitespace closes the current piece and token
                if !piece.is_empty() {
                    current.push((std::mem::take(&mut piece), *style));
                }
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            } else {
                // extend the current piece with this character
                piece.push(ch);
            }
        }
        // a segment ending mid-word leaves the token open for the next segment
        if !piece.is_empty() {
            current.push((piece, *style));
        }
    }
    // close the trailing token
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Greedily pack word tokens into rows no wider than the pane
///
/// Tokens join with a single space, like [`wrap_words`], and a token wider
/// than a whole row is hard-broken by grapheme rather than overflowing.
///
/// # Arguments
///
/// * `tokens` - The word tokens to pack, each a list of styled pieces
/// * `width` - The number of columns each row may use
fn pack_tokens(tokens: &[Vec<Segment>], width: usize) -> Vec<Vec<Span<'static>>> {
    // the rows packed so far, plus the row being packed and its width
    let mut rows = Vec::new();
    let mut row: Vec<Span<'static>> = Vec::new();
    let mut row_width = 0usize;
    for token in tokens {
        // measure the token in terminal columns across all of its pieces
        let token_width: usize = token
            .iter()
            .map(|(text, _)| UnicodeWidthStr::width(text.as_str()))
            .sum();
        // a token wider than a whole row is hard-broken across rows
        if token_width > width {
            // flush whatever is already on this row first
            if !row.is_empty() {
                rows.push(std::mem::take(&mut row));
                row_width = 0;
            }
            // break the long token, keeping its last piece open for packing
            let mut pieces = hard_wrap(token, width);
            if let Some(last) = pieces.pop() {
                rows.extend(pieces);
                row_width = last.iter().map(Span::width).sum();
                row = last;
            }
            continue;
        }
        // the token plus a joining space must fit on a non-empty row
        let needed = if row.is_empty() {
            token_width
        } else {
            row_width + 1 + token_width
        };
        if needed > width {
            // the token does not fit, so flush this row and start fresh
            rows.push(std::mem::take(&mut row));
            row_width = 0;
        }
        // join with a space styled like the token so underlines stay unbroken
        if !row.is_empty() {
            row.push(Span::styled(" ", token[0].1));
            row_width += 1;
        }
        // add the token's styled pieces to the row
        for (text, style) in token {
            row.push(Span::styled(text.clone(), *style));
        }
        row_width += token_width;
    }
    // always emit at least one (possibly empty) row per source line
    rows.push(row);
    rows
}

/// Hard-wrap styled segments at the exact columns they overflow
///
/// Whitespace is kept verbatim and multi-width graphemes are never split, so
/// this suits preformatted content like code block lines; it is the styled
/// counterpart of [`super::text_wrap::wrap_unicode_preserve_ws`].
///
/// # Arguments
///
/// * `segments` - The styled pieces to wrap, in display order
/// * `width` - The number of columns each row may use
fn hard_wrap(segments: &[Segment], width: usize) -> Vec<Vec<Span<'static>>> {
    // the rows built so far, plus the row and same-style run being built
    let mut rows = Vec::new();
    let mut row: Vec<Span<'static>> = Vec::new();
    let mut row_width = 0usize;
    let mut buf = String::new();
    let mut buf_style = Style::default();
    for (content, style) in segments {
        // a style change closes the run buffered under the previous style
        if *style != buf_style && !buf.is_empty() {
            row.push(Span::styled(std::mem::take(&mut buf), buf_style));
        }
        buf_style = *style;
        // walk the graphemes so multi-width characters are never split
        for grapheme in content.graphemes(true) {
            // get the width of this grapheme
            let grapheme_width = UnicodeWidthStr::width(grapheme);
            // check if this grapheme fits on the current row
            if row_width > 0 && row_width + grapheme_width > width {
                // flush the buffered run and finish this row
                if !buf.is_empty() {
                    row.push(Span::styled(std::mem::take(&mut buf), buf_style));
                }
                rows.push(std::mem::take(&mut row));
                row_width = 0;
            }
            // add this grapheme to the run and count its width
            buf.push_str(grapheme);
            row_width += grapheme_width;
        }
    }
    // flush the trailing run and row
    if !buf.is_empty() {
        row.push(Span::styled(buf, buf_style));
    }
    rows.push(row);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sum the display width of every span on a row
    ///
    /// # Arguments
    ///
    /// * `row` - The row to measure
    fn row_width(row: &[Span<'static>]) -> usize {
        row.iter().map(Span::width).sum()
    }

    /// Join a row's spans back into plain text
    ///
    /// # Arguments
    ///
    /// * `row` - The row to flatten
    fn row_text(row: &[Span<'static>]) -> String {
        row.iter().map(|span| span.content.as_ref()).collect()
    }

    /// A markdown sample exercising headings, emphasis, lists, code, and hashes
    const SAMPLE: &str = "# Findings\n\nThe loader uses **xor** with key `0x37` to \
        decode its strings.\n\n- sha256 09b1b708bba619b50f5fcbb47a6bbac3b4a7cdee2f78ab10f7bbc0e0e1f1b1cc\n\
        - contacts evil.example.com\n\n```rust\nfn decode(buf: &mut [u8]) {\n    \
        for byte in buf.iter_mut() { *byte ^= 0x37; }\n}\n```\n\n> verify before trusting";

    /// Every row fits the requested width at narrow and wide panes
    #[test]
    fn rows_never_exceed_the_width() {
        // check the invariant across narrow, cramped, and comfortable widths
        for width in [8usize, 20, 46] {
            for row in markdown_rows(SAMPLE, width, Style::default()) {
                assert!(
                    row_width(&row) <= width,
                    "row '{}' is wider than {width}",
                    row_text(&row)
                );
            }
        }
    }

    /// Markdown emphasis survives into the span styles over the base style
    #[test]
    fn styles_are_preserved_over_the_base() {
        let base = Style::default().fg(Color::Rgb(220, 220, 220));
        let rows = markdown_rows("plain **bold** text", 40, base);
        // everything fits on one row
        assert_eq!(rows.len(), 1);
        // the bold word keeps the bold modifier on top of the base color
        let bold = rows[0]
            .iter()
            .find(|span| span.content.trim() == "bold")
            .expect("bold span");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(bold.style.fg, base.fg);
        // the plain words keep the base color without added emphasis
        let plain = rows[0]
            .iter()
            .find(|span| span.content.trim() == "plain")
            .expect("plain span");
        assert_eq!(plain.style.fg, base.fg);
        assert!(!plain.style.add_modifier.contains(Modifier::BOLD));
    }

    /// Empty and whitespace-only text yields no rows, like `text_rows`
    #[test]
    fn empty_text_yields_no_rows() {
        assert!(markdown_rows("", 20, Style::default()).is_empty());
        assert!(markdown_rows("  \n \t ", 20, Style::default()).is_empty());
    }

    /// A word wider than the pane is hard-broken and keeps its style
    #[test]
    fn overlong_words_break_and_keep_their_style() {
        let hash = "09b1b708bba619b50f5fcbb47a6bbac3";
        let rows = markdown_rows(&format!("**{hash}**"), 10, Style::default());
        // the hash spreads over several rows, none wider than the pane
        assert!(rows.len() > 1);
        for row in &rows {
            assert!(row_width(row) <= 10);
            // every piece of the broken word stays bold
            for span in row {
                assert!(span.style.add_modifier.contains(Modifier::BOLD));
            }
        }
        // nothing of the hash is lost across the break
        let joined: String = rows.iter().map(|row| row_text(row)).collect();
        assert_eq!(joined, hash);
    }

    /// Code block lines hard-wrap so their indentation survives
    #[test]
    fn code_blocks_keep_their_indentation() {
        let rows = markdown_rows("```\n    indented line\n```", 40, Style::default());
        // the content line sits between the two fence lines, indentation intact
        let content = rows
            .iter()
            .map(|row| row_text(row))
            .find(|text| text.contains("indented"))
            .expect("code content row");
        assert!(content.starts_with("    "));
    }

    /// Blank edges are trimmed and interior blank runs collapse to one row
    #[test]
    fn blank_lines_are_tidied() {
        let rows = markdown_rows("one\n\ntwo", 20, Style::default());
        let texts: Vec<String> = rows.iter().map(|row| row_text(row)).collect();
        // the two paragraphs stay separated by exactly one blank row
        assert_eq!(texts, vec!["one", "", "two"]);
    }
}
