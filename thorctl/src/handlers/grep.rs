//! Handles grep commands

use chrono::NaiveDateTime;
use http::status::StatusCode;
use owo_colors::OwoColorize;
use std::collections::{BTreeMap, HashMap};
use std::io::{IsTerminal, Write};
use thorium::client::GrepClient;
use thorium::models::{
    GrepDocumentMatches, GrepMatch, GrepMatches, GrepOpts, GrepSource, GrepStats, TruncationReason,
};
use thorium::{Error, Thorium};
use uuid::Uuid;

use super::progress::{Bar, BarKind, MultiBar};
use super::update;
use crate::args::Args;
use crate::args::grep::{Grep, GrepFormat};
use crate::utils;
use crate::utils::repos::validate_repo_url;

/// The corpora a grep search can walk
enum GrepCorpus {
    /// Grep the file corpus
    Files,
    /// Grep the repo corpus
    Repos,
    /// Grep the entity corpus
    Entities,
}

/// The detected kind of a single explicit grep target
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GrepTarget {
    /// This target is a file SHA256
    File,
    /// This target is an entity UUID
    Entity,
    /// This target is a repo URL
    Repo,
}

impl TryFrom<&String> for GrepTarget {
    type Error = thorium::Error;

    /// Detect the kind of a grep target from its shape
    ///
    /// # Arguments
    ///
    /// * `target` - The target to detect the kind of
    fn try_from(target: &String) -> Result<Self, Self::Error> {
        // treat 64 hex characters as a file SHA256
        if utils::is_sha256(target) {
            return Ok(GrepTarget::File);
        }
        // treat anything that parses as a UUID as an entity id
        if Uuid::parse_str(target).is_ok() {
            return Ok(GrepTarget::Entity);
        }
        // anything else must be a valid repo URL
        match validate_repo_url(target) {
            Ok(()) => Ok(GrepTarget::Repo),
            Err(err) => Err(Error::new(format!(
                "'{}' is not a valid SHA256, entity UUID, or repo URL: {}",
                target,
                err.msg().unwrap_or_default()
            ))),
        }
    }
}

/// Resolve the output format and whether to emit ANSI styling
///
/// The format defaults to heading output on an interactive terminal and flat
/// output when piped, exactly like ripgrep's heading behavior; ANSI styling is
/// gated on the terminal independently so a forced format never writes escape
/// codes into a pipe.
///
/// # Arguments
///
/// * `requested` - The format explicitly requested on the command line if any
fn resolve_format(requested: Option<GrepFormat>) -> (GrepFormat, bool) {
    // only emit ANSI styling for an interactive terminal
    let ansi = std::io::stdout().is_terminal();
    // use the requested format or pick one based on the terminal
    let format = requested.unwrap_or(if ansi {
        GrepFormat::Heading
    } else {
        GrepFormat::Flat
    });
    (format, ansi)
}

/// Build the grep options for a single request
///
/// `GrepOpts` is consumed by each request so this is called once per page.
///
/// # Arguments
///
/// * `cmd` - The grep command to build options from
/// * `cursor` - The cursor id to continue an existing search with
/// * `max_matches` - The remaining query wide match budget if one was set
fn build_opts(
    cmd: &Grep,
    cursor: Option<Uuid>,
    max_matches: Option<usize>,
) -> Result<GrepOpts, Error> {
    // start from our pattern and list based options
    let mut opts = GrepOpts::new(&cmd.pattern)
        .groups(cmd.groups.iter().cloned())
        .sources(cmd.sources.iter().copied().map(GrepSource::from))
        .tools(cmd.tools.iter().cloned())
        .limit(cmd.page_size);
    // make this search case insensitive if requested
    if cmd.case_insensitive {
        opts = opts.case_insensitive();
    }
    // allow matches to span lines if requested
    if cmd.multi_line {
        opts = opts.multi_line();
    }
    // set the datetime to start searching at if one was given
    if let Some(start) = &cmd.start {
        opts = opts.start(NaiveDateTime::parse_from_str(start, &cmd.date_fmt)?.and_utc());
    }
    // set the datetime to stop searching at if one was given
    if let Some(end) = &cmd.end {
        opts = opts.end(NaiveDateTime::parse_from_str(end, &cmd.date_fmt)?.and_utc());
    }
    // bound the number of matches if any budget remains
    if let Some(max_matches) = max_matches {
        opts = opts.max_matches(max_matches);
    }
    // bound how long a single page may scan if requested
    if let Some(scan_time) = cmd.scan_time {
        opts = opts.scan_time(scan_time);
    }
    // bound how long a page may scan past its last match if requested
    if let Some(scan_idle) = cmd.scan_idle {
        opts = opts.scan_idle(scan_idle);
    }
    // return hit-less truncated documents if requested
    if cmd.with_truncated {
        opts = opts.with_truncated();
    }
    // continue an existing cursor if we have one
    if let Some(cursor) = cursor {
        opts = opts.cursor(cursor);
    }
    Ok(opts)
}

/// Build an invalid pattern error from an API rejection
///
/// # Arguments
///
/// * `err` - The error the API returned
/// * `cmd` - The grep command that was executed
fn invalid_pattern(err: &Error, cmd: &Grep) -> Error {
    // get the APIs error message if one was set
    let msg = err.msg().unwrap_or_else(|| "invalid request".to_owned());
    // unwrap the APIs json error body if this message is one
    let msg = serde_json::from_str::<serde_json::Value>(&msg)
        .ok()
        .and_then(|body| body["error"].as_str().map(ToOwned::to_owned))
        .unwrap_or(msg);
    // hint at multi line mode when a literal newline was likely rejected
    if !cmd.multi_line && cmd.pattern.contains('\n') {
        Error::new(format!(
            "invalid pattern: {msg} (a literal newline in the pattern requires -U/--multi-line)"
        ))
    } else {
        Error::new(format!("invalid pattern: {msg}"))
    }
}

/// Rewrite common corpus grep API errors into actionable messages
///
/// # Arguments
///
/// * `err` - The error to rewrite
/// * `cmd` - The grep command that was executed
/// * `cursor_sent` - Whether this request included a cursor id
fn friendly_grep_error(err: Error, cmd: &Grep, cursor_sent: bool) -> Error {
    // rewrite errors based on their status code if they have one
    match err.status() {
        // bad requests from grep are almost always invalid patterns
        Some(StatusCode::BAD_REQUEST) => invalid_pattern(&err, cmd),
        // not found on a corpus grep means our cursor is unknown or expired
        Some(StatusCode::NOT_FOUND) if cursor_sent => Error::new(
            "unknown or expired cursor (cursors expire and are scoped to the user that created them)",
        ),
        // pass any other errors through unchanged
        _ => err,
    }
}

/// Rewrite common single item grep API errors into actionable messages
///
/// # Arguments
///
/// * `err` - The error to rewrite
/// * `cmd` - The grep command that was executed
/// * `key` - The key for the item that was grepped
fn friendly_key_error(err: Error, cmd: &Grep, key: &str) -> Error {
    // rewrite errors based on their status code if they have one
    match err.status() {
        // bad requests from grep are almost always invalid patterns
        Some(StatusCode::BAD_REQUEST) => invalid_pattern(&err, cmd),
        // not found on a single item grep means the item does not exist
        Some(StatusCode::NOT_FOUND) => Error::new(format!("'{key}' was not found")),
        // pass any other errors through unchanged
        _ => err,
    }
}

/// Collect a map's entries into an ordered map for stable rendering
///
/// # Arguments
///
/// * `map` - The unordered map to sort by key
fn sorted<K: Ord + std::hash::Hash, V>(map: &HashMap<K, V>) -> BTreeMap<&K, &V> {
    // collect references to this map's entries in key order
    map.iter().collect()
}

/// The provenance path and matches for a single flattened grep document
struct DocEntry<'a> {
    /// The source bucket this document came from
    source: &'static str,
    /// The provenance segments under that source
    segments: Vec<String>,
    /// The matches and truncation info for this document
    doc: &'a GrepDocumentMatches,
}

impl DocEntry<'_> {
    /// Build the document path shown in heading output
    ///
    /// The constant `result` field segment is dropped for inline results since
    /// it carries no information; every other source keeps all of its segments.
    fn heading_path(&self) -> String {
        // drop the constant result field segment for inline results
        let segments = if self.source == "results"
            && self.segments.last().map(String::as_str) == Some("result")
        {
            &self.segments[..self.segments.len() - 1]
        } else {
            &self.segments[..]
        };
        // join the source and segments into a single path
        std::iter::once(self.source)
            .chain(segments.iter().map(String::as_str))
            .collect::<Vec<&str>>()
            .join("/")
    }

    /// Build the full provenance path used in flat output
    ///
    /// # Arguments
    ///
    /// * `key` - The key for the item this document came from
    fn flat_path(&self, key: &str) -> String {
        // join the key, source, and all segments into a single path
        std::iter::once(key)
            .chain(std::iter::once(self.source))
            .chain(self.segments.iter().map(String::as_str))
            .collect::<Vec<&str>>()
            .join(":")
    }
}

/// Flatten an item's matches into document entries with a stable order
///
/// Sources are emitted in a fixed order and every map level is sorted by key
/// since the response maps have no inherent order. Segments like tool names,
/// tag keys/values, and file names are untrusted so they are sanitized here,
/// making every provenance path safe to print.
///
/// # Arguments
///
/// * `matches` - The item's matches to flatten
fn flatten(matches: &GrepMatches) -> Vec<DocEntry<'_>> {
    let mut docs = Vec::new();
    // flatten the inline results by tool, result id, and field
    for (tool, results) in sorted(&matches.results) {
        for (result_id, fields) in sorted(results) {
            for (field, doc) in sorted(fields) {
                docs.push(DocEntry {
                    source: "results",
                    segments: vec![sanitize(tool), result_id.to_string(), sanitize(field)],
                    doc,
                });
            }
        }
    }
    // flatten the result files by tool, result id, and file name
    for (tool, results) in sorted(&matches.result_files) {
        for (result_id, files) in sorted(results) {
            for (file, doc) in sorted(files) {
                docs.push(DocEntry {
                    source: "result_files",
                    segments: vec![sanitize(tool), result_id.to_string(), sanitize(file)],
                    doc,
                });
            }
        }
    }
    // flatten the submission info by submission id and field
    for (submission, fields) in sorted(&matches.submissions) {
        for (field, doc) in sorted(fields) {
            docs.push(DocEntry {
                source: "submissions",
                segments: vec![submission.to_string(), sanitize(field)],
                doc,
            });
        }
    }
    // flatten the comments by comment id
    for (comment, doc) in sorted(&matches.comments) {
        docs.push(DocEntry {
            source: "comments",
            segments: vec![comment.to_string()],
            doc,
        });
    }
    // flatten the tags by key and value
    for (key, values) in sorted(&matches.tags) {
        for (value, doc) in sorted(values) {
            docs.push(DocEntry {
                source: "tags",
                segments: vec![sanitize(key), sanitize(value)],
                doc,
            });
        }
    }
    // flatten any type specific metadata by field
    for (field, doc) in sorted(&matches.metadata) {
        docs.push(DocEntry {
            source: "metadata",
            segments: vec![sanitize(field)],
            doc,
        });
    }
    docs
}

/// Replace control characters so untrusted snippets cannot inject terminal escapes
///
/// Snippets come from attacker controlled bytes (malware corpora) so every C0
/// control (including escape), delete, and C1 control is replaced before
/// anything is printed; tabs are kept for readability.
///
/// # Arguments
///
/// * `text` - The untrusted text to sanitize
fn sanitize(text: &str) -> String {
    // replace every control character except tab with the replacement character
    text.chars()
        .map(|c| {
            if c.is_control() && c != '\t' {
                '\u{FFFD}'
            } else {
                c
            }
        })
        .collect()
}

/// Clamp a byte length down to the nearest char boundary in a snippet
///
/// # Arguments
///
/// * `snippet` - The snippet to find a char boundary in
/// * `want` - The byte length to clamp
fn clamp_to_boundary(snippet: &str, want: usize) -> usize {
    // never clamp past the end of the snippet
    let mut end = want.min(snippet.len());
    // walk down to the nearest char boundary
    while end > 0 && !snippet.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// Get the human readable note for a truncation reason
///
/// # Arguments
///
/// * `reason` - The reason a document was only partially searched
fn truncation_note(reason: TruncationReason) -> &'static str {
    // describe each reason a document was cut short
    match reason {
        TruncationReason::Binary => "binary data detected",
        TruncationReason::MaxDocumentSize => "exceeds the max document size",
        TruncationReason::MatchBudget => "match budget hit — raise --max-matches",
        TruncationReason::Error => "a read error cut this document short",
    }
}

/// Check if an item has at least one actual match in any document
///
/// This differs from [`GrepMatches::is_empty`] since hit-less truncated
/// documents returned by `--with-truncated` populate the maps without
/// containing any matches.
///
/// # Arguments
///
/// * `matches` - The item's matches to check
fn has_matches(matches: &GrepMatches) -> bool {
    // check every source for a document with at least one match
    matches
        .results
        .values()
        .flat_map(|results| results.values())
        .flat_map(|fields| fields.values())
        .any(|doc| !doc.matches.is_empty())
        || matches
            .result_files
            .values()
            .flat_map(|results| results.values())
            .flat_map(|files| files.values())
            .any(|doc| !doc.matches.is_empty())
        || matches
            .submissions
            .values()
            .flat_map(|fields| fields.values())
            .any(|doc| !doc.matches.is_empty())
        || matches.comments.values().any(|doc| !doc.matches.is_empty())
        || matches
            .tags
            .values()
            .flat_map(|values| values.values())
            .any(|doc| !doc.matches.is_empty())
        || matches.metadata.values().any(|doc| !doc.matches.is_empty())
}

/// Count every match in an item across all of its documents
///
/// # Arguments
///
/// * `matches` - The item's matches to count
fn count_matches(matches: &GrepMatches) -> usize {
    // sum the matches in every document from every source
    matches
        .results
        .values()
        .flat_map(|results| results.values())
        .flat_map(|fields| fields.values())
        .chain(
            matches
                .result_files
                .values()
                .flat_map(|results| results.values())
                .flat_map(|files| files.values()),
        )
        .chain(
            matches
                .submissions
                .values()
                .flat_map(|fields| fields.values()),
        )
        .chain(matches.comments.values())
        .chain(matches.tags.values().flat_map(|values| values.values()))
        .chain(matches.metadata.values())
        .map(|doc| doc.matches.len())
        .sum()
}

/// Count a page's newly matched items, deduping across page boundaries
///
/// The query wide match budget can split a single item across consecutive
/// pages so the final item of each page is remembered and a continuation of
/// it at the start of the next page is never counted twice.
///
/// # Arguments
///
/// * `data` - The items in this page
/// * `boundary` - The previous page's final item key and whether it was counted
fn count_new_items(data: &[GrepMatches], boundary: &mut Option<(String, bool)>) -> u64 {
    let mut items = 0;
    // walk this page's items in order
    for (idx, matches) in data.iter().enumerate() {
        // check if this item was already counted as the previous page's final item
        let already_counted = idx == 0
            && boundary
                .as_ref()
                .is_some_and(|(key, counted)| *counted && key == &matches.key);
        // count this item if it has matches and was not already counted
        let matched = has_matches(matches);
        if matched && !already_counted {
            items += 1;
        }
        // remember this page's final item for the next boundary check
        if idx == data.len() - 1 {
            boundary.replace((matches.key.clone(), matched || already_counted));
        }
    }
    items
}

/// Check if an error is our output pipe closing
///
/// # Arguments
///
/// * `error` - The error to check
fn is_broken_pipe(error: &Error) -> bool {
    // only io errors can be broken pipes
    match error {
        Error::IO(io) => io.kind() == std::io::ErrorKind::BrokenPipe,
        _ => false,
    }
}

/// Renders grep matches to a writer in the chosen format
struct Renderer {
    /// The output format to render matches in
    format: GrepFormat,
    /// Whether to style output with ANSI colors
    ansi: bool,
}

impl Renderer {
    /// Create a new renderer
    ///
    /// # Arguments
    ///
    /// * `format` - The output format to render matches in
    /// * `ansi` - Whether to style output with ANSI colors
    fn new(format: GrepFormat, ansi: bool) -> Self {
        Renderer { format, ansi }
    }

    /// Render one item's matches in the chosen format
    ///
    /// # Arguments
    ///
    /// * `matches` - The item's matches to render
    /// * `out` - The writer to render to
    fn render(&self, matches: &GrepMatches, out: &mut dyn Write) -> Result<(), Error> {
        // dispatch to the right format
        match self.format {
            GrepFormat::Heading => self.render_heading(matches, out),
            GrepFormat::Flat => self.render_flat(matches, out),
            GrepFormat::Json => Self::render_json(matches, out),
        }
    }

    /// Build a snippet row with the matching bytes highlighted
    ///
    /// Snippets start at the match itself so the first `len` bytes are the
    /// match. The split happens before sanitization since sanitizing changes
    /// byte lengths, and the boundary is clamped so multi byte chars are never
    /// split.
    ///
    /// # Arguments
    ///
    /// * `grep_match` - The match to build a snippet row for
    fn render_snippet(&self, grep_match: &GrepMatch) -> String {
        // find how many bytes of the snippet the match itself covers
        let end = clamp_to_boundary(&grep_match.snippet, grep_match.len);
        // split the raw snippet before sanitizing shifts byte offsets
        let (hit, rest) = grep_match.snippet.split_at(end);
        // neutralize control characters in both halves
        let hit = sanitize(hit);
        let rest = sanitize(rest);
        // highlight the matching bytes when styling is enabled
        if self.ansi {
            format!("{}{rest}", hit.bright_red().bold())
        } else {
            format!("{hit}{rest}")
        }
    }

    /// Render one item's matches grouped under headings like ripgrep
    ///
    /// # Arguments
    ///
    /// * `matches` - The item's matches to render
    /// * `out` - The writer to render to
    fn render_heading(&self, matches: &GrepMatches, out: &mut dyn Write) -> Result<(), Error> {
        // sanitize this item's key since repo urls are not fully trusted
        let key = sanitize(&matches.key);
        // print this item's key as the top level heading
        if self.ansi {
            writeln!(out, "{}", key.bright_magenta())?;
        } else {
            writeln!(out, "{key}")?;
        }
        // walk this item's documents in a stable order
        for entry in flatten(matches) {
            // print this document's provenance path
            if self.ansi {
                writeln!(out, "  {}", entry.heading_path().bright_blue())?;
            } else {
                writeln!(out, "  {}", entry.heading_path())?;
            }
            // print each match in this document
            for grep_match in &entry.doc.matches {
                // build this match's highlighted snippet row
                let snippet = self.render_snippet(grep_match);
                if self.ansi {
                    writeln!(out, "    {}:{snippet}", grep_match.line.bright_green())?;
                } else {
                    writeln!(out, "    {}:{snippet}", grep_match.line)?;
                }
            }
            // note documents that were only partially searched
            if let Some(reason) = entry.doc.truncated {
                // build the truncation note for this document
                let note = format!("[truncated: {}]", truncation_note(reason));
                if self.ansi {
                    writeln!(out, "    {}", note.dimmed())?;
                } else {
                    writeln!(out, "    {note}")?;
                }
            }
        }
        // separate items with a blank line
        writeln!(out)?;
        Ok(())
    }

    /// Render one item's matches as self contained provenance rows
    ///
    /// Truncation notes go to stderr so stdout stays purely match rows for
    /// pipelines.
    ///
    /// # Arguments
    ///
    /// * `matches` - The item's matches to render
    /// * `out` - The writer to render to
    fn render_flat(&self, matches: &GrepMatches, out: &mut dyn Write) -> Result<(), Error> {
        // sanitize this item's key since repo urls are not fully trusted
        let key = sanitize(&matches.key);
        // walk this item's documents in a stable order
        for entry in flatten(matches) {
            // build this document's full provenance path
            let path = entry.flat_path(&key);
            // print each match as its own self contained row
            for grep_match in &entry.doc.matches {
                // build this match's highlighted snippet row
                let snippet = self.render_snippet(grep_match);
                if self.ansi {
                    writeln!(
                        out,
                        "{}:{}:{snippet}",
                        path.bright_magenta(),
                        grep_match.line.bright_green()
                    )?;
                } else {
                    writeln!(out, "{path}:{}:{snippet}", grep_match.line)?;
                }
            }
            // send truncation notes to stderr to keep stdout machine readable
            if let Some(reason) = entry.doc.truncated {
                eprintln!("{path} [truncated: {}]", truncation_note(reason));
            }
        }
        Ok(())
    }

    /// Render one item's matches as a single sanitized JSON object line
    ///
    /// Snippets are free text so they are sanitized in every format; serde
    /// only escapes C0 controls so raw C1 controls could otherwise still
    /// reach a terminal through `cat`. Map keys (tools, tags, file names)
    /// keep their exact bytes in JSON so consumers can re-query them.
    ///
    /// # Arguments
    ///
    /// * `matches` - The item's matches to render
    /// * `out` - The writer to render to
    fn render_json(matches: &GrepMatches, out: &mut dyn Write) -> Result<(), Error> {
        // clone this item so we can sanitize snippets without touching the original
        let mut clean = matches.clone();
        // sanitize the inline result snippets
        for doc in clean
            .results
            .values_mut()
            .flat_map(|results| results.values_mut())
            .flat_map(|fields| fields.values_mut())
        {
            sanitize_doc(doc);
        }
        // sanitize the result file snippets
        for doc in clean
            .result_files
            .values_mut()
            .flat_map(|results| results.values_mut())
            .flat_map(|files| files.values_mut())
        {
            sanitize_doc(doc);
        }
        // sanitize the submission snippets
        for doc in clean
            .submissions
            .values_mut()
            .flat_map(|fields| fields.values_mut())
        {
            sanitize_doc(doc);
        }
        // sanitize the comment snippets
        for doc in clean.comments.values_mut() {
            sanitize_doc(doc);
        }
        // sanitize the tag snippets
        for doc in clean
            .tags
            .values_mut()
            .flat_map(|values| values.values_mut())
        {
            sanitize_doc(doc);
        }
        // sanitize the metadata snippets
        for doc in clean.metadata.values_mut() {
            sanitize_doc(doc);
        }
        // write this item as one JSON object per line
        writeln!(out, "{}", serde_json::to_string(&clean)?)?;
        Ok(())
    }
}

/// Sanitize every snippet in a single document
///
/// # Arguments
///
/// * `doc` - The document to sanitize the snippets of
fn sanitize_doc(doc: &mut GrepDocumentMatches) {
    // sanitize each match's snippet in place
    for grep_match in &mut doc.matches {
        grep_match.snippet = sanitize(&grep_match.snippet);
    }
}

/// Render one page of grep matches to stdout
///
/// # Arguments
///
/// * `data` - The items with matches to render
/// * `renderer` - The renderer to print matches with
fn render_page(data: &[GrepMatches], renderer: &Renderer) -> Result<(), Error> {
    // lock stdout once for this whole page
    let mut stdout = std::io::stdout().lock();
    // render every item in this page
    for matches in data {
        renderer.render(matches, &mut stdout)?;
    }
    Ok(())
}

/// Print the run summary to stderr unless quiet mode is set
///
/// # Arguments
///
/// * `stats` - The merged stats across all pages
/// * `items` - The number of items with at least one match
/// * `pages` - The number of pages walked or 0 for single item greps
/// * `resume` - The cursor to resume with if we stopped early
/// * `cmd` - The grep command that was executed
/// * `quiet` - Whether to suppress the summary
fn print_footer(
    stats: &GrepStats,
    items: u64,
    pages: u64,
    resume: Option<Uuid>,
    cmd: &Grep,
    quiet: bool,
) {
    // print nothing in quiet mode
    if quiet {
        return;
    }
    // only corpus greps know how many pages were walked
    let pages_part = if pages > 0 {
        format!(" across {pages} pages")
    } else {
        String::new()
    };
    // summarize what this grep found
    eprintln!(
        "grep: {items} matched items{pages_part} · docs: {} matched · {} truncated · {} errors",
        stats.found, stats.truncated, stats.errors
    );
    // hint at hit-less truncated documents being hidden
    if stats.truncated > 0 && !cmd.with_truncated {
        eprintln!("grep: re-run with --with-truncated to also list hit-less truncated documents");
    }
    // hand back a resume cursor when we stopped early
    if let Some(cursor) = resume {
        eprintln!("grep: search stopped early; resume with --cursor {cursor}");
    }
}

/// Fold a single item's documents into locally computed stats
///
/// Single item greps return no page metadata so the same buckets the server
/// counts are computed client side.
///
/// # Arguments
///
/// * `matches` - The item's matches to count
/// * `stats` - The stats to fold this item's counts into
fn fold_local_stats(matches: &GrepMatches, stats: &mut GrepStats) {
    // walk every document in this item
    for entry in flatten(matches) {
        // count documents with at least one match
        if !entry.doc.matches.is_empty() {
            stats.found += 1;
        }
        // count partially searched documents in the same buckets as the server
        match entry.doc.truncated {
            Some(TruncationReason::Error) => stats.errors += 1,
            Some(_) => stats.truncated += 1,
            None => (),
        }
    }
}

/// Walk a corpus-wide grep cursor, rendering matches as pages arrive
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `args` - The arguments passed to Thorctl
/// * `cmd` - The grep command to execute
/// * `corpus` - The corpus to grep
/// * `renderer` - The renderer to print matches with
async fn grep_corpus(
    thorium: &Thorium,
    args: &Args,
    cmd: &Grep,
    corpus: GrepCorpus,
    renderer: &Renderer,
) -> Result<(), Error> {
    // build a progress spinner that is a no-op in quiet mode
    let multi = MultiBar::new(args.quiet);
    let bar: Bar = multi.add("grep", BarKind::Timer);
    bar.set_message("searching");
    // track merged stats and counts across pages
    let mut stats = GrepStats::default();
    let mut items: u64 = 0;
    let mut pages: u64 = 0;
    // track the previous page's final item so budget split items count once
    let mut boundary = None;
    // track how much of the query wide match budget remains if one was set
    let mut remaining = cmd.max_matches;
    // seed our cursor from the command line when resuming an earlier search
    let mut cursor = cmd.cursor;
    // the cursor to resume with if we stop early
    let mut resume = None;
    // page through this corpus until the cursor is exhausted
    loop {
        // build the options for this page with any remaining match budget
        let opts = build_opts(cmd, cursor, remaining)?;
        // grep the next page from the right corpus
        let page = match corpus {
            GrepCorpus::Files => thorium.files.grep(opts).await,
            GrepCorpus::Repos => thorium.repos.grep(opts).await,
            GrepCorpus::Entities => thorium.entities.grep(opts).await,
        };
        // make common grep API errors actionable before bailing
        let page = page.map_err(|err| friendly_grep_error(err, cmd, cursor.is_some()))?;
        pages += 1;
        // fold this page's stats into our running totals
        if let Some(metadata) = &page.metadata {
            stats.merge(metadata);
        }
        // count this page's newly matched items
        items += count_new_items(&page.data, &mut boundary);
        // spend this page's matches against any query wide match budget
        if let Some(budget) = remaining {
            // the server returns at most the budget we sent so this never overspends
            let spent: usize = page.data.iter().map(count_matches).sum();
            remaining = Some(budget.saturating_sub(spent));
        }
        // render this page while the spinner is suspended to avoid interleaving
        if let Err(error) = bar.suspend(|| render_page(&page.data, renderer)) {
            // stop quietly when our output pipe closes like grep does
            if is_broken_pipe(&error) {
                resume = page.cursor;
                break;
            }
            return Err(error);
        }
        // update the spinner with our progress
        bar.set_message(format!(
            "page {pages} · {items} matched items · {} docs matched",
            stats.found
        ));
        // continue to the next page, stop at a spent budget, or finish
        match page.cursor {
            Some(next) => {
                // stop between pages once we have found enough items
                if !cmd.no_limit && items >= cmd.limit as u64 {
                    resume = Some(next);
                    break;
                }
                // stop between pages once the match budget is spent
                if remaining == Some(0) {
                    resume = Some(next);
                    break;
                }
                cursor = Some(next);
            }
            // an absent cursor means the whole corpus has been searched
            None => break,
        }
    }
    // clear the spinner before printing the summary
    bar.finish_and_clear();
    // summarize what this grep found
    print_footer(&stats, items, pages, resume, cmd, args.quiet);
    // exit with a failure like grep itself when nothing matched
    if items == 0 {
        return Err(Error::new("no matches found"));
    }
    Ok(())
}

/// Grep each explicit target in order
///
/// # Arguments
///
/// * `thorium` - A Thorium client
/// * `args` - The arguments passed to Thorctl
/// * `cmd` - The grep command to execute
/// * `renderer` - The renderer to print matches with
async fn grep_targets(
    thorium: &Thorium,
    args: &Args,
    cmd: &Grep,
    renderer: &Renderer,
) -> Result<(), Error> {
    // detect every target's kind before sending any requests
    let targets = cmd
        .targets
        .iter()
        .map(|target| GrepTarget::try_from(target).map(|kind| (kind, target)))
        .collect::<Result<Vec<(GrepTarget, &String)>, Error>>()?;
    // track locally computed stats across targets
    let mut stats = GrepStats::default();
    let mut items: u64 = 0;
    // lock stdout once for all targets
    let mut stdout = std::io::stdout().lock();
    // grep each target in the order it was given
    for (kind, target) in targets {
        // build the options for this target
        let opts = build_opts(cmd, None, cmd.max_matches)?;
        // grep this one item from the right corpus
        let matches = match kind {
            GrepTarget::File => thorium.files.grep_key(target, opts).await,
            GrepTarget::Entity => thorium.entities.grep_key(target, opts).await,
            GrepTarget::Repo => thorium.repos.grep_key(target, opts).await,
        };
        // make common grep API errors actionable before bailing
        let matches = matches.map_err(|err| friendly_key_error(err, cmd, target))?;
        // fold this item's documents into our local stats
        fold_local_stats(&matches, &mut stats);
        // count this item if any of its documents matched
        if has_matches(&matches) {
            items += 1;
        }
        // render this item unless it came back entirely empty
        if !matches.is_empty() {
            // stop quietly when our output pipe closes like grep does
            if let Err(error) = renderer.render(&matches, &mut stdout) {
                if is_broken_pipe(&error) {
                    break;
                }
                return Err(error);
            }
        }
    }
    // summarize what this grep found
    print_footer(&stats, items, 0, None, cmd, args.quiet);
    // exit with a failure like grep itself when nothing matched
    if items == 0 {
        return Err(Error::new("no matches found"));
    }
    Ok(())
}

/// Handle the grep command
///
/// # Arguments
///
/// * `args` - The arguments passed to Thorctl
/// * `cmd` - The grep command to execute
pub async fn handle(args: &Args, cmd: &Grep) -> Result<(), Error> {
    // load our config and instance our client
    let (conf, thorium) = utils::get_client(args).await?;
    // warn about insecure connections if not set to skip
    if !conf.skip_insecure_warning.unwrap_or_default() {
        utils::warn_insecure_conf(&conf)?;
    }
    // check if we need to update
    if !args.skip_update && !conf.skip_update.unwrap_or_default() {
        update::ask_update(&thorium).await?;
    }
    // resolve the output format and whether to style it
    let (format, ansi) = resolve_format(cmd.format);
    // build the renderer for that format
    let renderer = Renderer::new(format, ansi);
    // grep explicit targets or walk a whole corpus
    if cmd.targets.is_empty() {
        // pick the corpus to walk
        let corpus = if cmd.entities {
            GrepCorpus::Entities
        } else if cmd.repos {
            GrepCorpus::Repos
        } else {
            GrepCorpus::Files
        };
        grep_corpus(&thorium, args, cmd, corpus, &renderer).await
    } else {
        grep_targets(&thorium, args, cmd, &renderer).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a document with a single match
    ///
    /// # Arguments
    ///
    /// * `line` - The line this match starts on
    /// * `len` - How many bytes this match covers
    /// * `snippet` - The snippet for this match
    fn doc(line: u64, len: usize, snippet: &str) -> GrepDocumentMatches {
        GrepDocumentMatches {
            matches: vec![GrepMatch {
                line,
                offset: 0,
                len,
                snippet: snippet.to_owned(),
            }],
            searched: snippet.len() as u64,
            truncated: None,
        }
    }

    /// Build a fixture with every source populated
    fn fixture() -> GrepMatches {
        // use fixed uuids so sorting is predictable
        let uuid1 = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
        let uuid2 = Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap();
        let uuid3 = Uuid::parse_str("33333333-3333-3333-3333-333333333333").unwrap();
        // build an item keyed by a fake sha256
        let mut matches = GrepMatches::new("a".repeat(64));
        // add inline results for two tools to check sorting
        matches
            .results
            .entry("zeta".to_owned())
            .or_default()
            .entry(uuid1)
            .or_default()
            .insert("result".to_owned(), doc(12, 7, "corn123 germinated"));
        matches
            .results
            .entry("alpha".to_owned())
            .or_default()
            .entry(uuid2)
            .or_default()
            .insert("result".to_owned(), doc(3, 6, "corn42"));
        // add a result file
        matches
            .result_files
            .entry("mid".to_owned())
            .or_default()
            .entry(uuid1)
            .or_default()
            .insert("strings.txt".to_owned(), doc(88, 6, "corn42 failed"));
        // add a submission field
        matches
            .submissions
            .entry(uuid3)
            .or_default()
            .insert("description".to_owned(), doc(3, 5, "corn9 pulled"));
        // add a comment
        matches.comments.insert(uuid1, doc(1, 4, "corny comment"));
        // add a tag
        matches
            .tags
            .entry("plant".to_owned())
            .or_default()
            .insert("corn9000".to_owned(), doc(1, 8, "corn9000"));
        // add entity metadata
        matches
            .metadata
            .insert("name".to_owned(), doc(2, 4, "corned"));
        matches
    }

    /// Render a fixture to a string in the requested format
    ///
    /// # Arguments
    ///
    /// * `matches` - The matches to render
    /// * `format` - The format to render in
    /// * `ansi` - Whether to style the output
    fn render(matches: &GrepMatches, format: GrepFormat, ansi: bool) -> String {
        // render into an in-memory buffer
        let mut buffer = Vec::new();
        Renderer::new(format, ansi)
            .render(matches, &mut buffer)
            .unwrap();
        String::from_utf8(buffer).unwrap()
    }

    /// Heading output orders sources in a fixed order and sorts the keys within each source
    #[test]
    fn heading_orders_sources_and_sorts_keys() {
        // render the full fixture without styling
        let rendered = render(&fixture(), GrepFormat::Heading, false);
        // build the expected output with fixed source order and sorted keys
        let expected = format!(
            "{}\n\
             \x20 results/alpha/22222222-2222-2222-2222-222222222222\n\
             \x20   3:corn42\n\
             \x20 results/zeta/11111111-1111-1111-1111-111111111111\n\
             \x20   12:corn123 germinated\n\
             \x20 result_files/mid/11111111-1111-1111-1111-111111111111/strings.txt\n\
             \x20   88:corn42 failed\n\
             \x20 submissions/33333333-3333-3333-3333-333333333333/description\n\
             \x20   3:corn9 pulled\n\
             \x20 comments/11111111-1111-1111-1111-111111111111\n\
             \x20   1:corny comment\n\
             \x20 tags/plant/corn9000\n\
             \x20   1:corn9000\n\
             \x20 metadata/name\n\
             \x20   2:corned\n\n",
            "a".repeat(64)
        );
        assert_eq!(rendered, expected);
    }

    /// A highlight that ends inside a multi byte char is clamped to the char boundary
    #[test]
    fn heading_highlight_clamps_to_char_boundary() {
        // build a match whose length lands in the middle of a multi byte char
        let grep_match = GrepMatch {
            line: 1,
            offset: 0,
            len: 2,
            snippet: "héllo".to_owned(),
        };
        // render the snippet with styling enabled
        let rendered = Renderer::new(GrepFormat::Heading, true).render_snippet(&grep_match);
        // the highlight must cover exactly the first full char and nothing more
        assert_eq!(rendered, "\u{1b}[1m\u{1b}[91mh\u{1b}[39m\u{1b}[0méllo");
    }

    /// Sanitizing a snippet replaces C0, C1, and newline controls but keeps tabs
    #[test]
    fn sanitize_neutralizes_escapes() {
        // sanitize a snippet with C0, C1, and newline controls plus a tab
        let cleaned = sanitize("evil\u{1b}[31mred\u{9b}X\nnext\tok");
        assert_eq!(cleaned, "evil\u{fffd}[31mred\u{fffd}X\u{fffd}next\tok");
    }

    /// Flat rows carry every provenance level of their document
    #[test]
    fn flat_rows_carry_full_provenance() {
        // render the full fixture as flat rows
        let rendered = render(&fixture(), GrepFormat::Flat, false);
        // flat rows keep every provenance level including the result field
        let key = "a".repeat(64);
        assert!(rendered.contains(&format!(
            "{key}:results:zeta:11111111-1111-1111-1111-111111111111:result:12:corn123 germinated\n"
        )));
        assert!(rendered.contains(&format!(
            "{key}:result_files:mid:11111111-1111-1111-1111-111111111111:strings.txt:88:corn42 failed\n"
        )));
        assert!(rendered.contains(&format!("{key}:tags:plant:corn9000:1:corn9000\n")));
    }

    /// A truncated document without matches prints only its truncation note
    #[test]
    fn hitless_truncated_doc_prints_note_only() {
        // build an item with one hit-less truncated document
        let mut matches = GrepMatches::new("b".repeat(64));
        matches.metadata.insert(
            "name".to_owned(),
            GrepDocumentMatches {
                matches: Vec::new(),
                searched: 100,
                truncated: Some(TruncationReason::MatchBudget),
            },
        );
        // heading output shows the path and the note only
        let rendered = render(&matches, GrepFormat::Heading, false);
        assert!(rendered.contains("  metadata/name\n"));
        assert!(rendered.contains("    [truncated: match budget hit — raise --max-matches]\n"));
        // flat output sends the note to stderr leaving stdout empty
        let rendered = render(&matches, GrepFormat::Flat, false);
        assert_eq!(rendered, "");
    }

    /// Escape sequences in provenance paths are neutralized in heading and flat output
    #[test]
    fn provenance_paths_are_sanitized() {
        // build an item with an escape injection attempt in a tag value
        let mut matches = GrepMatches::new("d".repeat(64));
        matches
            .tags
            .entry("plant".to_owned())
            .or_default()
            .insert("evil\u{1b}[31mcorn".to_owned(), doc(1, 4, "corny"));
        // the heading path must have the escape neutralized
        let rendered = render(&matches, GrepFormat::Heading, false);
        assert!(rendered.contains("  tags/plant/evil\u{fffd}[31mcorn\n"));
        // the flat path must have the escape neutralized too
        let rendered = render(&matches, GrepFormat::Flat, false);
        assert!(rendered.contains(":tags:plant:evil\u{fffd}[31mcorn:"));
    }

    /// JSON output is one line per item with sanitized snippets
    #[test]
    fn json_sanitizes_snippets() {
        // build an item with an escape injection attempt in its snippet
        let mut matches = GrepMatches::new("c".repeat(64));
        matches
            .metadata
            .insert("name".to_owned(), doc(1, 4, "evil\u{1b}[31mred"));
        // render the item as NDJSON
        let rendered = render(&matches, GrepFormat::Json, false);
        // the output must be exactly one line of JSON
        assert_eq!(rendered.matches('\n').count(), 1);
        // parse the line back and check the snippet was sanitized
        let parsed: serde_json::Value = serde_json::from_str(rendered.trim_end()).unwrap();
        let snippet = parsed["metadata"]["name"]["matches"][0]["snippet"]
            .as_str()
            .unwrap();
        assert!(snippet.contains('\u{fffd}'));
        assert!(!snippet.contains('\u{1b}'));
    }

    /// Counting matches sums the matches from every source
    #[test]
    fn count_matches_sums_every_source() {
        // the fixture has one match in each of its seven documents
        assert_eq!(count_matches(&fixture()), 7);
    }

    /// An item the match budget splits across pages is counted once
    #[test]
    fn budget_split_items_count_once_across_pages() {
        // build two pages where the match budget split one item across them
        let mut split = GrepMatches::new("e".repeat(64));
        split.metadata.insert("name".to_owned(), doc(1, 4, "corn1"));
        let mut rest = GrepMatches::new("e".repeat(64));
        rest.metadata.insert("rule".to_owned(), doc(9, 4, "corn2"));
        let mut fresh = GrepMatches::new("f".repeat(64));
        fresh.metadata.insert("name".to_owned(), doc(2, 4, "corn3"));
        // the first page ends with the split item
        let mut boundary = None;
        assert_eq!(count_new_items(&[split], &mut boundary), 1);
        // the second page starts with the same item which must not recount
        assert_eq!(count_new_items(&[rest, fresh], &mut boundary), 1);
    }

    /// A split item without matches in its first slice is counted when its matches arrive
    #[test]
    fn hitless_boundary_item_counts_when_its_matches_arrive() {
        // build a split item whose first slice had no matches
        let mut hitless = GrepMatches::new("e".repeat(64));
        hitless.metadata.insert(
            "name".to_owned(),
            GrepDocumentMatches {
                matches: Vec::new(),
                searched: 10,
                truncated: Some(TruncationReason::MatchBudget),
            },
        );
        let mut rest = GrepMatches::new("e".repeat(64));
        rest.metadata.insert("rule".to_owned(), doc(9, 4, "corn2"));
        // the hit-less first slice must not count
        let mut boundary = None;
        assert_eq!(count_new_items(&[hitless], &mut boundary), 0);
        // the slice that actually carries its matches must count once
        assert_eq!(count_new_items(&[rest], &mut boundary), 1);
    }

    /// Grep targets are inferred as files, entities, or repos from their shape
    #[test]
    fn target_inference_three_ways() {
        // 64 hex chars are a file sha256
        let sha256 = "a".repeat(64);
        assert_eq!(GrepTarget::try_from(&sha256).unwrap(), GrepTarget::File);
        // hyphenated and simple uuids are entities
        let hyphenated = "6ba7b810-9dad-11d1-80b4-00c04fd430c8".to_owned();
        assert_eq!(
            GrepTarget::try_from(&hyphenated).unwrap(),
            GrepTarget::Entity
        );
        let simple = "6ba7b8109dad11d180b400c04fd430c8".to_owned();
        assert_eq!(GrepTarget::try_from(&simple).unwrap(), GrepTarget::Entity);
        // anything else must be a valid repo url
        let repo = "github.com/corn/stalker".to_owned();
        assert_eq!(GrepTarget::try_from(&repo).unwrap(), GrepTarget::Repo);
        // garbage is rejected with an error naming all three shapes
        let garbage = "nope".to_owned();
        let error = GrepTarget::try_from(&garbage).unwrap_err();
        assert!(
            error
                .msg()
                .unwrap()
                .contains("is not a valid SHA256, entity UUID, or repo URL")
        );
    }

    /// An explicitly requested output format is always used
    #[test]
    fn explicit_format_is_used() {
        // an explicitly requested format is always used
        let (format, _) = resolve_format(Some(GrepFormat::Json));
        assert_eq!(format, GrepFormat::Json);
        let (format, _) = resolve_format(Some(GrepFormat::Heading));
        assert_eq!(format, GrepFormat::Heading);
    }
}
