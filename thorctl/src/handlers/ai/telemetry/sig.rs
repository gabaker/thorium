//! The normalized failure-signature recipe (bundle level 1).
//!
//! A signature is a bounded, multi-line, defanged, number-scrubbed prefix of
//! a *failing* tool result -- enough to cluster breakage across a fleet
//! ("bridge restarted", a short script stack trace) without carrying analysis
//! content. The recipe is normative and versioned: exporters stamp
//! [`super::bundle::SIGNATURE_RECIPE`] onto their bundles, receivers cluster
//! on exact string equality, and every step below is part of the format.
//! Recipe 1 (the original single-line, 90-character form) is what bundles
//! without a stamp carry; recipe 2 signatures only cluster with other
//! recipe-2 signatures, by design.

use std::sync::LazyLock;

use regex::Regex;

/// How many characters of the failing result feed the recipe
const INPUT_CHARS: usize = 4096;

/// The per-line length cap, applied after every rewrite
const LINE_CHARS: usize = 160;

/// How many normalized lines a signature keeps at most
const MAX_LINES: usize = 16;

/// The whole signature's length cap, newlines included
const TOTAL_CHARS: usize = 1500;

/// Whitespace runs inside a line, flattened to single spaces
static WHITESPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+").expect("a valid whitespace regex"));

/// Absolute path tokens, collapsed whole before the number passes
///
/// A path token is a run starting with `/`, `~/`, a Windows drive prefix, or
/// a UNC `\\` -- begun at the line start, after whitespace, or after an
/// opening `` ` ``, `'`, `"`, `(`, `[`, `<`, or `=` (tool errors routinely
/// quote paths, Rust-family tooling in backticks), and ending before
/// whitespace or a closing `` ` ``, `'`, `"`, `)`, `]`, or `>`, so the
/// delimiters survive the collapse. The anchoring keeps a URL's `//` and
/// mid-word slashes from matching; the bare-`/` form additionally needs a
/// second segment (checked in the replacer) so a lone slash never matches
/// either.
static PATH_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?P<lead>^|[\s'"(\[<=`])(?P<tok>(?:[A-Za-z]:[\\/]|\\\\|~/|/)[^\s'")\]>`]*)"#)
        .expect("a valid path regex")
});

/// `0x`-prefixed hex runs, collapsed outright
static HEX_PREFIXED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"0[xX][0-9a-fA-F]+").expect("a valid 0x-hex regex"));

/// Bare hex runs of four-plus characters, collapsed when they carry a digit
/// (an all-letter run like `deadbeef` reads as a word, not an address)
static HEX_RUN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[0-9a-fA-F]{4,}").expect("a valid hex-run regex"));

/// Leftover decimal runs, collapsed last
static DIGITS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[0-9]+").expect("a valid digit-run regex"));

/// Cuts a string at a character count without splitting a code point
///
/// # Arguments
///
/// * `text` - The text to cut
/// * `chars` - The maximum number of characters to keep
fn char_prefix(text: &str, chars: usize) -> &str {
    // find the byte offset of the first character past the cap, if any
    match text.char_indices().nth(chars) {
        Some((cut, _)) => &text[..cut],
        None => text,
    }
}

/// Normalizes one failing tool result into its cluster signature (recipe 2)
///
/// The recipe, in order -- normative, since receivers cluster on equality:
///
/// 1. Take the first 4,096 characters (never splitting a code point).
/// 2. Split into lines on `\r\n`, `\r`, or `\n`.
/// 3. For each line, in order: flatten whitespace runs to one space and trim;
///    skip the line if it is now empty; collapse absolute path tokens to
///    `PATH` (a run starting `/` with a second segment, `~/`, a drive prefix,
///    or `\\`, begun at the line start, after whitespace, or after an opening
///    `` ` ``, `'`, `"`, `(`, `[`, `<`, or `=`, and ending before whitespace
///    or a closing `` ` ``, `'`, `"`, `)`, `]`, or `>`); collapse `0x`-hex
///    runs, then bare hex runs of four-plus characters carrying a digit, then
///    remaining decimal runs, each to `N`; defang; cap the line at 160
///    characters.
/// 4. Drop a line identical to the previous kept line.
/// 5. Keep at most 16 lines joined by `\n`, stopping before a line that would
///    push the total (newlines included) past 1,500 characters -- whole lines
///    only, and the first line always fits.
/// 6. Blank or empty input gives `""`.
///
/// # Arguments
///
/// * `result` - The failing tool result to normalize
pub fn normalize_signature(result: &str) -> String {
    // slice the input down before any allocation-heavy rewriting
    let input = char_prefix(result, INPUT_CHARS);
    // normalize line by line so stack-trace structure survives
    let mut lines: Vec<String> = Vec::new();
    let mut total = 0usize;
    // split on every newline convention; \r\n yields an empty segment that
    // the blank-line skip below drops
    for raw in input.split(['\r', '\n']) {
        // flatten whitespace so indentation never splits a cluster
        let flat = WHITESPACE.replace_all(raw, " ");
        let flat = flat.trim();
        if flat.is_empty() {
            continue;
        }
        // collapse whole path tokens first so a path never half-survives the
        // number passes as scattered segments
        let scrubbed = PATH_TOKEN.replace_all(flat, |caps: &regex::Captures| {
            let lead = &caps["lead"];
            let token = &caps["tok"];
            // a bare /-token must have a second segment to read as a path
            if token.starts_with('/') && !token[1..].contains('/') {
                format!("{lead}{token}")
            } else {
                format!("{lead}PATH")
            }
        });
        // collapse 0x-prefixed hex runs so their letters never read as words
        let scrubbed = HEX_PREFIXED.replace_all(&scrubbed, "N");
        // collapse bare hex runs only when a digit proves they are numbers
        let scrubbed = HEX_RUN.replace_all(&scrubbed, |caps: &regex::Captures| {
            let run = &caps[0];
            if run.bytes().any(|byte| byte.is_ascii_digit()) {
                "N".to_owned()
            } else {
                run.to_owned()
            }
        });
        // collapse whatever decimal runs remain
        let scrubbed = DIGITS.replace_all(&scrubbed, "N");
        // defang surviving URL and IP shapes, then cap the line
        let defanged = thorpal::defang(&scrubbed);
        let line = char_prefix(&defanged, LINE_CHARS).to_owned();
        // drop a line identical to the previous kept line (retry spam)
        if lines.last() == Some(&line) {
            continue;
        }
        // stop before a line that would blow the total budget; the first
        // line always fits since the line cap sits far under the total
        let add = line.chars().count() + usize::from(!lines.is_empty());
        if !lines.is_empty() && total + add > TOTAL_CHARS {
            break;
        }
        total += add;
        lines.push(line);
        if lines.len() >= MAX_LINES {
            break;
        }
    }
    lines.join("\n")
}

/// A signature's one-line summary: its first line plus the extra-line count
///
/// Every list renderer shows this form; the full multi-line signature lives
/// in detail pop-outs.
///
/// # Arguments
///
/// * `sig` - The normalized signature to summarize
pub fn sig_summary(sig: &str) -> (&str, usize) {
    // the first line is the summary; count whatever follows it
    let mut lines = sig.lines();
    let first = lines.next().unwrap_or("");
    (first, lines.count())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The number-scrubbing passes collapse in their documented order
    #[test]
    fn signatures_collapse_numbers_in_order() {
        // table of (failing result, expected signature); the single-line
        // cases are recipe-1 outputs that must stay identical under recipe 2
        let cases = [
            // 0x-prefixed runs collapse even when letters-only after the 0x
            ("read at 0xDEADBEEF failed", "read at N failed"),
            // bare hex runs with a digit collapse; pure words survive
            ("handle 4f2c01 vs deadbeef", "handle N vs deadbeef"),
            // leftover decimals collapse, however short
            (
                "script error: line 42: unknown fn 'f7'",
                "script error: line N: unknown fn 'fN'",
            ),
            // intra-line whitespace runs flatten to single spaces and trim
            ("  bridge \t restarted   twice  ", "bridge restarted twice"),
        ];
        for (input, expected) in cases {
            assert_eq!(normalize_signature(input), expected, "for input {input:?}");
        }
    }

    /// Line structure survives: every newline convention, blanks dropped,
    /// consecutive duplicates collapsed
    #[test]
    fn signatures_keep_line_structure() {
        // CR, CRLF, and LF all split; blanks vanish; and the number collapse
        // makes the step lines identical, so they dedupe to one
        let input = "script error\r\n\r\n  at step 1\rat step 2\nat step 2\nat step 3";
        assert_eq!(normalize_signature(input), "script error\nat step N");
        // non-consecutive repeats stay: only runs collapse
        let sig = normalize_signature("alpha\nstep 1\nbeta\nstep 2");
        assert_eq!(sig, "alpha\nstep N\nbeta\nstep N");
    }

    /// Absolute path tokens collapse whole, in every spelling
    #[test]
    fn signatures_collapse_path_tokens() {
        let cases = [
            // unix absolute paths need a second segment
            ("open /home/user/sample.bin failed", "open PATH failed"),
            // a lone slash-token is not a path
            ("div a/b vs /x", "div a/b vs /x"),
            // home-relative, drive, and UNC spellings always collapse
            ("read ~/sessions/x.json", "read PATH"),
            (r"load C:\Users\a\mal.dll", "load PATH"),
            (r"copy \\share\drop\x", "copy PATH"),
            // quoted and bracketed paths collapse with their delimiters kept,
            // so an install location never splits clusters across machines
            (
                "install: '/opt/tool/v1_PUBLIC' missing",
                "install: 'PATH' missing",
            ),
            (r#"open "/var/lib/db.bin" denied"#, r#"open "PATH" denied"#),
            // backtick quoting (the Rust-family tooling shape) collapses too
            ("couldn't read `/home/user/x.rs`", "couldn't read `PATH`"),
            ("stat (/usr/lib/x.so) failed", "stat (PATH) failed"),
            ("cfg=/etc/tool/conf.yml bad key", "cfg=PATH bad key"),
            // a URL's // still never reads as a path (`:` is no boundary)
            (
                "fetch http://host/payload failed",
                "fetch hxxp[://]host/payload failed",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(normalize_signature(input), expected, "for input {input:?}");
        }
    }

    /// Surviving URL and IP shapes defang after the number scrubbing
    #[test]
    fn signatures_defang_surviving_shapes() {
        let sig = normalize_signature("fetch http://192.168.1.10/payload failed");
        // the scheme breaks and the collapsed authority stays bracketed
        assert!(sig.contains("hxxp[://]"), "got {sig:?}");
        assert!(!sig.contains("http://"), "got {sig:?}");
        assert!(
            !sig.contains("192"),
            "the address digits must collapse: {sig:?}"
        );
    }

    /// The caps hold: per line, per signature, and at char boundaries
    #[test]
    fn signatures_cap_lines_and_totals() {
        // a long single line caps at the per-line budget, char-safe
        let long_line = "é".repeat(400);
        let sig = normalize_signature(&long_line);
        assert_eq!(sig.chars().count(), 160);
        // many lines cap at the line budget
        let many = (0..40).map(|i| format!("line {i}x")).collect::<Vec<_>>();
        let sig = normalize_signature(&many.join("\n"));
        assert!(
            sig.lines().count() <= 16,
            "got {} lines",
            sig.lines().count()
        );
        // long lines cap at the total budget with whole lines only
        let wide = (0..20)
            .map(|i| format!("{} {}", "y".repeat(150), char::from(b'a' + i)))
            .collect::<Vec<_>>();
        let sig = normalize_signature(&wide.join("\n"));
        assert!(
            sig.chars().count() <= 1500,
            "got {} chars",
            sig.chars().count()
        );
        assert!(sig.lines().all(|line| line.chars().count() <= 160));
        // the first line always fits, however tight the budget math
        assert!(!sig.is_empty());
    }

    /// The empty and whitespace-only results normalize to the empty signature
    #[test]
    fn signatures_tolerate_empty_results() {
        assert_eq!(normalize_signature(""), "");
        assert_eq!(normalize_signature(" \n\t \r\n "), "");
    }

    /// The summary form is the first line plus the count of further lines
    #[test]
    fn summaries_split_first_line_and_count() {
        assert_eq!(sig_summary("one line"), ("one line", 0));
        assert_eq!(sig_summary("top\nmid\nend"), ("top", 2));
        assert_eq!(sig_summary(""), ("", 0));
    }
}
