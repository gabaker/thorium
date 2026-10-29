//! Support for using the engine behind ripgrep to search data in Thorium
//!
//! Documents come from two places: data we already have in memory (inline results,
//! submission info, comments, tags, and entity metadata) and data in s3 (result files).
//! In memory documents are searched with [`Searcher::search_slice`] since they are already
//! resident, while s3 documents are streamed through [`Searcher::search_reader`] so a huge
//! result file is never fully buffered.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use futures::stream::{self, StreamExt};
use grep::matcher::Matcher;
use grep::regex::{RegexMatcher, RegexMatcherBuilder};
use grep::searcher::{
    BinaryDetection, MmapChoice, Searcher, SearcherBuilder, Sink, SinkFinish, SinkMatch,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::AsyncReadExt;
use tokio_util::io::SyncIoBridge;
use tracing::{Level, event, instrument};
use uuid::Uuid;

use super::db;
use super::results::build_result_file_key;
use crate::conf::Grep;
use crate::models::backends::GrepSupport;
use crate::models::{
    ApiCursor, GrepDocumentMatches, GrepMatch, GrepMatches, GrepParams, GrepQueryParams,
    GrepSource, GrepStats, ResultGetParams, TruncationReason, User,
};
use crate::utils::{ApiError, Shared};
use crate::{bad, internal_err};

/// Identifies the document a single worker is searching
#[derive(Debug, Clone)]
pub enum GrepDocument {
    /// A field in the inline JSON results for a tool
    Result {
        /// The tool this result comes from
        tool: String,
        /// The id of the result this document comes from
        id: Uuid,
        /// The name of the field in this result
        field: String,
    },
    /// A result file stored in s3
    ResultFile {
        /// The tool this result file comes from
        tool: String,
        /// The id of the result this file comes from
        id: Uuid,
        /// The name of this result file
        name: String,
    },
    /// A field in an items submission info
    Submission {
        /// The id of the submission this document comes from
        id: Uuid,
        /// The name of the field in this submission
        field: String,
    },
    /// The body of a comment
    Comment {
        /// The id of this comment
        id: Uuid,
    },
    /// The value of a tag
    Tag {
        /// The key for this tag
        key: String,
        /// The value for this tag
        value: String,
    },
    /// A field in an items type specific metadata
    Metadata {
        /// The name of the metadata field
        field: String,
    },
}

impl GrepDocument {
    /// Insert a searched documents matches into its items source specific map
    ///
    /// # Arguments
    ///
    /// * `item` - The matches for the item this document belongs to
    /// * `matches` - The matches found in this document
    fn insert_matches(self, item: &mut GrepMatches, matches: GrepDocumentMatches) {
        // add this documents matches under its source
        match self {
            // this is a field in an inline result
            GrepDocument::Result { tool, id, field } => {
                item.results
                    .entry(tool)
                    .or_default()
                    .entry(id)
                    .or_default()
                    .insert(field, matches);
            }
            // this is a result file in s3
            GrepDocument::ResultFile { tool, id, name } => {
                item.result_files
                    .entry(tool)
                    .or_default()
                    .entry(id)
                    .or_default()
                    .insert(name, matches);
            }
            // this is a field in some submission info
            GrepDocument::Submission { id, field } => {
                item.submissions
                    .entry(id)
                    .or_default()
                    .insert(field, matches);
            }
            // this is the body of a comment
            GrepDocument::Comment { id } => {
                item.comments.insert(id, matches);
            }
            // this is the value of a tag
            GrepDocument::Tag { key, value } => {
                item.tags.entry(key).or_default().insert(value, matches);
            }
            // this is a field in some type specific metadata
            GrepDocument::Metadata { field } => {
                item.metadata.insert(field, matches);
            }
        }
    }
}

/// A stable identity for a single grep document used to resume a search
///
/// This mirrors [`GrepDocument`] without its body so it can be persisted on a
/// grep cursor and matched back up to the same document on a later page.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
pub enum GrepResumeKey {
    /// A field in the inline JSON results for a tool
    Result {
        /// The tool this result comes from
        tool: String,
        /// The id of the result this document comes from
        id: Uuid,
        /// The name of the field in this result
        field: String,
    },
    /// A result file stored in s3
    ResultFile {
        /// The tool this result file comes from
        tool: String,
        /// The id of the result this file comes from
        id: Uuid,
        /// The name of this result file
        name: String,
    },
    /// A field in an items submission info
    Submission {
        /// The id of the submission this document comes from
        id: Uuid,
        /// The name of the field in this submission
        field: String,
    },
    /// The body of a comment
    Comment {
        /// The id of this comment
        id: Uuid,
    },
    /// The value of a tag
    Tag {
        /// The key for this tag
        key: String,
        /// The value for this tag
        value: String,
    },
    /// A field in an items type specific metadata
    Metadata {
        /// The name of the metadata field
        field: String,
    },
}

impl From<&GrepDocument> for GrepResumeKey {
    /// Build the stable identity for a grep document
    ///
    /// # Arguments
    ///
    /// * `doc` - The document to build a stable identity for
    fn from(doc: &GrepDocument) -> Self {
        // map each document onto its identity without its body
        match doc {
            GrepDocument::Result { tool, id, field } => GrepResumeKey::Result {
                tool: tool.clone(),
                id: *id,
                field: field.clone(),
            },
            GrepDocument::ResultFile { tool, id, name } => GrepResumeKey::ResultFile {
                tool: tool.clone(),
                id: *id,
                name: name.clone(),
            },
            GrepDocument::Submission { id, field } => GrepResumeKey::Submission {
                id: *id,
                field: field.clone(),
            },
            GrepDocument::Comment { id } => GrepResumeKey::Comment { id: *id },
            GrepDocument::Tag { key, value } => GrepResumeKey::Tag {
                key: key.clone(),
                value: value.clone(),
            },
            GrepDocument::Metadata { field } => GrepResumeKey::Metadata {
                field: field.clone(),
            },
        }
    }
}

/// How far a single document got before the match budget was exhausted
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrepDocResume {
    /// This document finished on an earlier page so rescanning it could only duplicate matches
    ///
    /// Finished means fully searched or terminally truncated by binary data, the max
    /// document size, or an error.
    Done,
    /// Skip this documents first n matches since they were already returned
    Skip(usize),
}

/// The within item progress of a grep search whose match budget died mid item
///
/// The docs are stored as a list of entries instead of a map because this is
/// persisted as JSON and JSON map keys must be strings.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GrepResume {
    /// The key of the item the match budget was exhausted inside
    pub key: String,
    /// How far each of this items documents got
    pub docs: Vec<(GrepResumeKey, GrepDocResume)>,
}

impl GrepResume {
    /// Consume this resume state and build a map for constant time lookups
    fn into_map(self) -> HashMap<GrepResumeKey, GrepDocResume> {
        self.docs.into_iter().collect()
    }
}

/// The outcome of scanning a single items documents
#[derive(Debug)]
pub struct GrepScan {
    /// The matches found in this items documents
    pub matches: GrepMatches,
    /// The counts of found, truncated, and errored documents in this item
    pub stats: GrepStats,
    /// The progress to resume at if the match budget died inside this item
    pub resume: Option<GrepResume>,
}

/// The s3 buckets a grep document can be streamed from
#[derive(Debug, Clone, Copy)]
pub enum GrepBucket {
    /// The bucket result files are stored in
    Results,
    /// The bucket comment attachments are stored in
    Attachments,
}

/// Where a workers bytes come from
#[derive(Debug)]
enum GrepBody {
    /// Data we already have in memory
    Memory(String),
    /// Data to be streamed out of an s3 bucket
    S3 {
        /// The bucket to stream this document from
        bucket: GrepBucket,
        /// The path to this document in its bucket
        path: String,
    },
}

/// A single document to search and the matches found in it
#[derive(Debug)]
struct GrepSearchWorker {
    /// The document this worker is searching
    doc: GrepDocument,
    /// Where this workers bytes come from
    body: GrepBody,
    /// How many matches this document already returned on earlier pages
    skip: usize,
    /// The matches this worker found
    matches: GrepDocumentMatches,
}

impl GrepSearchWorker {
    /// Search this workers document and record any matches
    ///
    /// Errors that are specific to a single document (an oversized line, a failed s3
    /// download) are logged and treated as a skipped document rather then failing the
    /// entire query.
    ///
    /// # Arguments
    ///
    /// * `matcher` - The matcher to search with
    /// * `remaining` - The query wide budget of matches still left
    /// * `limits` - The configured limits for grep searches
    /// * `multi_line` - Whether this pattern can match across multiple lines
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "GrepSearchWorker::scan", skip_all, err(Debug))]
    async fn scan(
        mut self,
        matcher: &RegexMatcher,
        remaining: &Arc<AtomicUsize>,
        limits: &Grep,
        multi_line: bool,
        shared: &Shared,
    ) -> Result<Self, ApiError> {
        // if our query wide match budget is already exhausted then don't search anything
        if remaining.load(Ordering::Relaxed) == 0 {
            // flag this document as only partially searched since we never got to it
            self.matches.mark_truncated(TruncationReason::MatchBudget);
            return Ok(self);
        }
        // every document is capped at one byte past this limit so a document that is
        // exactly at the limit can be told apart from one that was actually truncated
        let max = limits.max_document_size.as_u64();
        // take ownership of this documents body so a blocking task can own its bytes
        let body = std::mem::replace(&mut self.body, GrepBody::Memory(String::new()));
        // search this documents body
        match body {
            // this document is already in memory so search it in place
            GrepBody::Memory(body) => {
                // build the pieces our blocking task needs to own
                let mut sink = GrepSink::new(matcher.clone(), remaining.clone(), limits, self.skip);
                let mut searcher = build_searcher(limits, multi_line, true);
                let matcher = matcher.clone();
                // search this document on a blocking thread so we never stall the runtime
                let outcome = tokio::task::spawn_blocking(move || {
                    // search our document, capped the same way a streamed document is
                    let result = searcher.search_slice(
                        &matcher,
                        cap_document(body.as_bytes(), max),
                        &mut sink,
                    );
                    // hand back our sink either way so partial matches are not lost
                    (result, sink)
                })
                .await?;
                // unpack our search result and sink
                let (result, sink) = outcome;
                // keep this documents matches and decide why it was cut short if it was
                self.matches = classify_search(result, sink, max);
            }
            // this document is in s3 so stream it through our searcher
            GrepBody::S3 { bucket, path } => {
                // get the client for the bucket this document is in
                let client = match bucket {
                    GrepBucket::Results => &shared.s3.results,
                    GrepBucket::Attachments => &shared.s3.attachments,
                };
                // start downloading this document
                let stream = match client.download(&path).await {
                    Ok(stream) => stream,
                    Err(error) => {
                        // log that we failed to download this document but continue on
                        event!(
                            Level::WARN,
                            msg = "Failed to download document",
                            error = error.msg().unwrap_or_default()
                        );
                        // flag this document as only partially searched due to an error
                        self.matches.mark_truncated(TruncationReason::Error);
                        return Ok(self);
                    }
                };
                // cap how many bytes can ever cross into our searcher
                let reader = stream.into_async_read().take(max + 1);
                // bridge our async reader into a blocking one our searcher can use
                let bridge = SyncIoBridge::new(reader);
                // build the pieces our blocking task needs to own
                let mut sink = GrepSink::new(matcher.clone(), remaining.clone(), limits, self.skip);
                let mut searcher = build_searcher(limits, multi_line, false);
                let matcher = matcher.clone();
                // search this document on a blocking thread so we never stall the runtime
                let outcome = tokio::task::spawn_blocking(move || {
                    // search our streamed document
                    let result = searcher.search_reader(&matcher, bridge, &mut sink);
                    // hand back our sink either way so partial matches are not lost
                    (result, sink)
                })
                .await?;
                // unpack our search result and sink
                let (result, sink) = outcome;
                // keep this documents matches and decide why it was cut short if it was
                self.matches = classify_search(result, sink, max);
            }
        }
        Ok(self)
    }
}

/// Cap a document to the same byte budget a streamed document gets
///
/// One byte past the limit is kept so a document that is exactly at the limit can
/// be told apart from one that was actually truncated, mirroring the capped reader
/// used for streamed documents.
///
/// # Arguments
///
/// * `body` - The bytes of the document to cap
/// * `max` - The max number of bytes of a document to search
fn cap_document(body: &[u8], max: u64) -> &[u8] {
    // keep one byte past our limit so truncation is detectable
    let keep = usize::try_from(max.saturating_add(1)).unwrap_or(usize::MAX);
    &body[..std::cmp::min(body.len(), keep)]
}

/// Classify the outcome of searching a single document
///
/// The sink is always drained into the returned matches, even when the search
/// errored, so partial matches that already claimed from the match budget are
/// never lost.
///
/// # Arguments
///
/// * `result` - The outcome of the search
/// * `sink` - The sink the search collected matches in
/// * `max` - The max number of bytes of a document to search
fn classify_search(
    result: Result<(), std::io::Error>,
    sink: GrepSink,
    max: u64,
) -> GrepDocumentMatches {
    // always keep the matches our sink collected so partial matches are not lost
    let mut matches = sink.into_matches();
    // check if we failed to search this entire document
    if let Err(error) = result {
        // log that we only partially searched this document but continue on
        event!(
            Level::WARN,
            msg = "Only partially searched document",
            error = error.to_string()
        );
        // flag this document as only partially searched due to an error
        //
        // an earlier truncation reason like binary data always wins over this one
        matches.mark_truncated(TruncationReason::Error);
    } else if matches.searched > max {
        // we read past our limit so this document was too large
        matches.mark_truncated(TruncationReason::MaxDocumentSize);
    }
    matches
}

/// Build a searcher for a single document
///
/// # Arguments
///
/// * `limits` - The configured limits for grep searches
/// * `multi_line` - Whether this pattern can match across multiple lines
/// * `in_memory` - Whether this document is already resident in memory
fn build_searcher(limits: &Grep, multi_line: bool, in_memory: bool) -> Searcher {
    // build our base searcher
    let mut builder = SearcherBuilder::new();
    // always compute line numbers and never memory map anything
    builder
        .line_number(true)
        .multi_line(multi_line)
        .memory_map(MmapChoice::never())
        // abandon a document as soon as we see a NUL so we don't scan binary blobs
        .binary_detection(BinaryDetection::quit(0));
    // pick the heap limit for this document
    //
    // multi line searches buffer the whole document on the heap, so they get the full
    // document budget. single line searches only ever hold one line in their roll
    // buffer, so they get the much smaller line budget.
    let heap_limit = if in_memory {
        // in memory documents are already resident so a heap limit would just reject them
        None
    } else if multi_line {
        // grep's multi line fill loop errors the moment its buffer fills to the heap
        // limit, before the read that would observe EOF, so the limit must sit two
        // bytes past the cap: one for the truncation sentinel byte our capped reader
        // feeds and one so that capped stream still sees EOF instead of a full buffer
        Some(
            usize::try_from(limits.max_document_size.as_u64().saturating_add(2))
                .unwrap_or(usize::MAX),
        )
    } else {
        Some(usize::try_from(limits.max_line_size.as_u64()).unwrap_or(usize::MAX))
    };
    builder.heap_limit(heap_limit);
    builder.build()
}

/// Collects the matches found in a single document
struct GrepSink {
    /// The matcher to find the exact bounds of each match with
    matcher: RegexMatcher,
    /// The query wide budget of matches still left
    remaining: Arc<AtomicUsize>,
    /// How many already returned matches to skip before recording any
    ///
    /// Skipped matches never claim from the match budget so a resumed search
    /// always makes progress through a document.
    skip: usize,
    /// The max number of bytes to keep in a single snippet
    max_snippet_size: usize,
    /// The matches found so far
    matches: GrepDocumentMatches,
}

impl GrepSink {
    /// Create a new sink for a single document
    ///
    /// # Arguments
    ///
    /// * `matcher` - The matcher to find the exact bounds of each match with
    /// * `remaining` - The query wide budget of matches still left
    /// * `limits` - The configured limits for grep searches
    /// * `skip` - The number of already returned matches to skip
    fn new(matcher: RegexMatcher, remaining: Arc<AtomicUsize>, limits: &Grep, skip: usize) -> Self {
        GrepSink {
            matcher,
            remaining,
            skip,
            max_snippet_size: limits.max_snippet_size,
            matches: GrepDocumentMatches::default(),
        }
    }

    /// Consume this sink and return the matches it collected
    fn into_matches(self) -> GrepDocumentMatches {
        self.matches
    }

    /// Claim one match from the query wide budget
    ///
    /// Returns false if the budget was already exhausted, which means our caller should
    /// stop searching.
    fn claim(&self) -> bool {
        // try to decrement our budget without ever wrapping below 0
        self.remaining
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
    }
}

impl Sink for GrepSink {
    /// Sinks report their errors as io errors
    type Error = std::io::Error;

    /// Record all of the matches on a single matching line
    ///
    /// # Arguments
    ///
    /// * `_searcher` - The searcher that found this match
    /// * `mat` - The matching line
    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        // get the line this match starts on if we have one
        let line = mat.line_number().unwrap_or_default();
        // get where this matching line starts in the document
        let line_offset = usize::try_from(mat.absolute_byte_offset()).unwrap_or(usize::MAX);
        // whether we still have budget left to keep searching
        let mut keep_going = true;
        // the matches we found on this line
        //
        // these are collected into a local first because our matcher borrows us while it
        // is iterating, so we cannot push into our own matches until it is done
        let mut found_here = Vec::new();
        // copy our skip count to a local since our matcher borrows us while iterating
        let mut skip = self.skip;
        // a matching line can contain several matches so find each of them
        let found = self.matcher.find_iter(mat.bytes(), |found| {
            // skip matches that were already returned on an earlier page of this cursor
            if skip > 0 {
                // never claim budget or record a match we already returned
                skip -= 1;
                return true;
            }
            // claim a match from our query wide budget
            if !self.claim() {
                // our budget is exhausted so stop searching
                keep_going = false;
                return false;
            }
            // build a snippet of this match truncated to our max snippet size
            let snippet = build_snippet(mat.bytes(), found.start(), self.max_snippet_size);
            // a multi line search can coalesce matches on adjacent lines into one
            // block, so count the newlines before this match to find its own line
            #[allow(
                clippy::naive_bytecount,
                reason = "snippet prefixes are short, and a bytecount crate would be a new dependency"
            )]
            let line = line
                + mat.bytes()[..found.start()]
                    .iter()
                    .filter(|byte| **byte == b'\n')
                    .count() as u64;
            // record this match
            found_here.push(GrepMatch {
                line,
                offset: line_offset + found.start(),
                len: found.end() - found.start(),
                snippet,
            });
            true
        });
        // save how many matches we still need to skip
        self.skip = skip;
        // save the matches we found on this line
        self.matches.matches.append(&mut found_here);
        // if our budget ran out then this document was only partially searched
        if !keep_going {
            self.matches.mark_truncated(TruncationReason::MatchBudget);
        }
        // surface any error our matcher hit while finding matches
        match found {
            Ok(()) => Ok(keep_going),
            Err(error) => Err(std::io::Error::other(error.to_string())),
        }
    }

    /// Record that binary data was found and stop searching this document
    ///
    /// # Arguments
    ///
    /// * `_searcher` - The searcher that found this binary data
    /// * `_offset` - The offset the binary data was found at
    fn binary_data(&mut self, _searcher: &Searcher, _offset: u64) -> Result<bool, Self::Error> {
        // binary data means we abandoned this document part way through
        self.matches.mark_truncated(TruncationReason::Binary);
        Ok(false)
    }

    /// Record how much of this document we actually searched
    ///
    /// # Arguments
    ///
    /// * `_searcher` - The searcher that searched this document
    /// * `finish` - The info about the completed search
    fn finish(&mut self, _searcher: &Searcher, finish: &SinkFinish) -> Result<(), Self::Error> {
        // save how many bytes of this document we searched
        self.matches.searched = finish.byte_count();
        // if binary data was detected then this document was only partially searched
        if finish.binary_byte_offset().is_some() {
            self.matches.mark_truncated(TruncationReason::Binary);
        }
        Ok(())
    }
}

/// Build a snippet of a match bounded by our max snippet size
///
/// The snippet is cut on a character boundary so it is always valid UTF-8 and any
/// non UTF-8 bytes are replaced rather then rejected.
///
/// # Arguments
///
/// * `bytes` - The bytes of the matching line
/// * `start` - Where in those bytes this match starts
/// * `max` - The max number of bytes to keep
fn build_snippet(bytes: &[u8], start: usize, max: usize) -> String {
    // start our snippet at the start of this match so the match itself is always visible
    let slice = &bytes[std::cmp::min(start, bytes.len())..];
    // convert this slice to a string, replacing any invalid UTF-8
    let mut snippet = String::from_utf8_lossy(slice).into_owned();
    // trim any trailing line terminator so snippets are single lines
    while snippet.ends_with('\n') || snippet.ends_with('\r') {
        snippet.pop();
    }
    // truncate our snippet on a character boundary if its too long
    if snippet.len() > max {
        // find the closest character boundary at or below our max
        let boundary = snippet.floor_char_boundary(max);
        snippet.truncate(boundary);
    }
    snippet
}

/// Build the regex matcher for a grep query
///
/// The matcher always lets `^` and `$` anchor to lines instead of the whole
/// document, matching ripgrep; whether the pattern may actually match across
/// lines is decided separately by the searcher's multi line setting.
///
/// Beware the name collision: the builder's `multi_line` below is the regex
/// `(?m)` flag (where `^`/`$` anchor) and is always on, while our `multi_line`
/// argument means "the pattern may match across lines" and only drives the
/// line terminator choice here plus the searcher built in [`build_searcher`].
///
/// # Arguments
///
/// * `pattern` - The regex pattern to compile
/// * `case_insensitive` - Whether this matcher should ignore case
/// * `multi_line` - Whether this pattern can match across multiple lines
/// * `limits` - The configured limits for grep searches
pub(crate) fn build_matcher(
    pattern: &str,
    case_insensitive: bool,
    multi_line: bool,
    limits: &Grep,
) -> Result<RegexMatcher, ApiError> {
    // build the matcher to search our documents with
    let mut builder = RegexMatcherBuilder::new();
    builder
        .case_insensitive(case_insensitive)
        // this is the regex (?m) flag, NOT our multi_line argument: it only makes
        // ^/$ anchor to lines instead of the whole document, matching ripgrep, and
        // must be on even for single line searches or they could only ever anchor
        // to the start/end of the whole document; whether the pattern may actually
        // MATCH across lines is our multi_line argument, applied below and in the
        // searcher
        .multi_line(true)
        // bound how large a compiled regex and its DFA cache can get
        .size_limit(usize::try_from(limits.regex_size_limit.as_u64()).unwrap_or(usize::MAX))
        .dfa_size_limit(usize::try_from(limits.dfa_size_limit.as_u64()).unwrap_or(usize::MAX));
    // only set a line terminator for single line searches
    //
    // setting one enables line oriented optimizations but also rejects any pattern
    // containing a literal newline, which is exactly what a multi line search needs.
    if !multi_line {
        builder.line_terminator(Some(b'\n'));
    }
    // a bad pattern is user input so return a 400 instead of a 500
    match builder.build(pattern) {
        Ok(matcher) => Ok(matcher),
        Err(error) => bad!(format!("invalid grep pattern - {error}")),
    }
}

/// Searches documents for a single item with the engine behind ripgrep
pub struct GrepSearcher {
    /// The matcher to search all of our documents with
    matcher: RegexMatcher,
    /// The documents queued to be searched
    queued: Vec<GrepSearchWorker>,
    /// The query wide budget of matches still left
    remaining: Arc<AtomicUsize>,
    /// Whether this pattern can match across multiple lines
    multi_line: bool,
    /// Whether hit-less truncated or errored documents are kept in responses
    with_truncated: bool,
    /// How far each document got on earlier pages if we are resuming an item
    resume: HashMap<GrepResumeKey, GrepDocResume>,
    /// The configured limits for grep searches
    limits: Grep,
}

impl GrepSearcher {
    /// Create a new searcher for a grep query
    ///
    /// # Arguments
    ///
    /// * `params` - The params for this grep query
    /// * `matcher` - The compiled matcher to search documents with
    /// * `remaining` - The query wide budget of matches still left
    /// * `resume` - The progress to resume at if we already searched part of this item
    /// * `shared` - Shared Thorium objects
    pub fn new(
        params: &GrepParams,
        matcher: &RegexMatcher,
        remaining: Arc<AtomicUsize>,
        resume: Option<GrepResume>,
        shared: &Shared,
    ) -> Self {
        // get the configured limits for grep searches
        let limits = shared.config.thorium.grep.clone();
        GrepSearcher {
            // matchers are cheaply clonable so each item can share one compile
            matcher: matcher.clone(),
            queued: Vec::with_capacity(limits.concurrency),
            remaining,
            multi_line: params.multi_line,
            with_truncated: params.with_truncated,
            resume: resume.map_or_default(GrepResume::into_map),
            limits,
        }
    }

    /// Get the matches to skip for a document or None if it should not be queued at all
    ///
    /// # Arguments
    ///
    /// * `doc` - The document that is about to be queued
    fn resume_skip(&self, doc: &GrepDocument) -> Option<usize> {
        match self.resume.get(&GrepResumeKey::from(doc)) {
            // this document already finished on an earlier page so don't queue it
            Some(GrepDocResume::Done) => None,
            // skip the matches this document already returned
            Some(GrepDocResume::Skip(skip)) => Some(*skip),
            // this document has never been searched
            None => Some(0),
        }
    }

    /// Queue a document we already have in memory to be searched
    ///
    /// # Arguments
    ///
    /// * `doc` - The document to search
    /// * `body` - The body of the document to search
    pub fn add_memory(&mut self, doc: GrepDocument, body: String) {
        // skip this document entirely if it already finished on an earlier page
        let Some(skip) = self.resume_skip(&doc) else {
            return;
        };
        // build a worker for this in memory document
        let worker = GrepSearchWorker {
            doc,
            body: GrepBody::Memory(body),
            skip,
            matches: GrepDocumentMatches::default(),
        };
        // queue this worker to be searched
        self.queued.push(worker);
    }

    /// Queue a document in s3 to be streamed and searched
    ///
    /// # Arguments
    ///
    /// * `doc` - The document to search
    /// * `bucket` - The bucket this document is in
    /// * `path` - The path to this document in its bucket
    pub fn add_s3<P: Into<String>>(&mut self, doc: GrepDocument, bucket: GrepBucket, path: P) {
        // skip this document entirely if it already finished on an earlier page
        let Some(skip) = self.resume_skip(&doc) else {
            return;
        };
        // build a worker for this s3 document
        let worker = GrepSearchWorker {
            doc,
            body: GrepBody::S3 {
                bucket,
                path: path.into(),
            },
            skip,
            matches: GrepDocumentMatches::default(),
        };
        // queue this worker to be searched
        self.queued.push(worker);
    }

    /// Check if any documents were queued to be searched
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queued.is_empty()
    }

    /// Search all of our queued documents and fold their matches into one map
    ///
    /// Only a bounded number of documents are searched at once so a single item with
    /// thousands of result files cannot swamp the API.
    ///
    /// # Arguments
    ///
    /// * `key` - The key for the item we are searching
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "GrepSearcher::scan", skip_all, err(Debug))]
    pub async fn scan(self, key: &str, shared: &Shared) -> Result<GrepScan, ApiError> {
        // destructure ourselves so our workers can borrow our matcher
        let GrepSearcher {
            matcher,
            queued,
            remaining,
            multi_line,
            with_truncated,
            resume,
            limits,
        } = self;
        // search our documents at most concurrency at a time, but always at least
        // one at a time since a concurrency of 0 would stall this stream forever
        let workers = stream::iter(queued)
            .map(|worker| worker.scan(&matcher, &remaining, &limits, multi_line, shared))
            .buffer_unordered(std::cmp::max(1, limits.concurrency))
            .collect::<Vec<Result<GrepSearchWorker, ApiError>>>()
            .await;
        // propagate any error that was not specific to a single document
        let workers = workers
            .into_iter()
            .collect::<Result<Vec<GrepSearchWorker>, ApiError>>()?;
        // build the progress to resume at if our match budget died inside this item
        //
        // this must happen before the fold below since the fold prunes documents from
        // the response that a resume must still never rescan.
        let resume = build_resume(key, resume, &workers);
        // build the matches for this item
        let mut matches = GrepMatches::new(key);
        // the counts of found, truncated, and errored documents in this item
        let mut stats = GrepStats::default();
        // fold each workers matches into the right source specific map
        for worker in workers {
            fold_worker(&mut matches, &mut stats, with_truncated, worker);
        }
        Ok(GrepScan {
            matches,
            stats,
            resume,
        })
    }
}

/// Fold one scanned workers document into its items matches and stats
///
/// Documents that can never carry useful info (fully searched with no matches, or
/// binary documents where nothing was searched) are dropped without being counted.
/// Hit-less truncated or errored documents are always counted but are only kept in
/// the response when `with_truncated` asks for them.
///
/// # Arguments
///
/// * `matches` - The matches for the item this document belongs to
/// * `stats` - The counts of found, truncated, and errored documents for this item
/// * `with_truncated` - Whether hit-less truncated or errored documents are kept
/// * `worker` - The scanned worker to fold in
fn fold_worker(
    matches: &mut GrepMatches,
    stats: &mut GrepStats,
    with_truncated: bool,
    worker: GrepSearchWorker,
) {
    // skip any documents that had no matches and were fully searched
    if worker.matches.matches.is_empty() && worker.matches.truncated.is_none() {
        return;
    }
    // skip binary documents where nothing was searched since they can never
    // match any text query and would just add noise to every response
    if worker.matches.matches.is_empty()
        && worker.matches.searched == 0
        && worker.matches.truncated == Some(TruncationReason::Binary)
    {
        return;
    }
    // count any documents that had actual matches
    if !worker.matches.matches.is_empty() {
        stats.found += 1;
    }
    // count any documents that were cut short, even ones that also had matches
    match worker.matches.truncated {
        // this document failed to be fully searched due to an error
        Some(TruncationReason::Error) => stats.errors += 1,
        // this document was only partially searched
        Some(_) => stats.truncated += 1,
        // this document was fully searched
        None => (),
    }
    // unless they were asked for, drop documents that were truncated or
    // errored without any matches so unsearchable documents never bury
    // actual matches; the stats above still count them
    if !with_truncated && worker.matches.matches.is_empty() {
        return;
    }
    // add this documents matches under its source
    worker.doc.insert_matches(matches, worker.matches);
}

/// Build the resume state for an item if the match budget died inside it
///
/// Returns None when no document was cut off by the match budget, which means this
/// item either completed or only stopped for reasons a rescan cannot fix.
///
/// # Arguments
///
/// * `key` - The key for the item that was searched
/// * `prior` - The resume state this scan started from
/// * `workers` - Every worker that searched one of this items documents
fn build_resume(
    key: &str,
    prior: HashMap<GrepResumeKey, GrepDocResume>,
    workers: &[GrepSearchWorker],
) -> Option<GrepResume> {
    // if no document was cut off by the match budget then there is nothing to resume
    if !workers
        .iter()
        .any(|worker| worker.matches.truncated == Some(TruncationReason::MatchBudget))
    {
        return None;
    }
    // carry forward documents that finished on an earlier page and so were never
    // requeued, so a later page can still never rescan them
    let mut docs = prior
        .into_iter()
        .filter(|(_, state)| matches!(state, GrepDocResume::Done))
        .collect::<HashMap<GrepResumeKey, GrepDocResume>>();
    // record how far every worker got this page
    for worker in workers {
        // decide this documents resume state
        let state = match worker.matches.truncated {
            // the budget died inside this document so skip everything already returned
            //
            // errors are also retried since they can be transient (a failed s3
            // download); skipping already returned matches keeps a retry duplicate
            // free, and a deterministic error cannot loop forever because resume
            // state only persists while some document is budget truncated.
            Some(TruncationReason::MatchBudget | TruncationReason::Error) => {
                GrepDocResume::Skip(worker.skip + worker.matches.matches.len())
            }
            // fully searched or terminally truncated documents must never be rescanned
            _ => GrepDocResume::Done,
        };
        docs.insert(GrepResumeKey::from(&worker.doc), state);
    }
    // convert our map back to a list of entries so it can be persisted as JSON
    let docs = docs.into_iter().collect();
    Some(GrepResume {
        key: key.to_owned(),
        docs,
    })
}

/// Queue the results and result files for an item to be searched
///
/// This is identical for every greppable type so it lives here instead of in
/// [`GrepSupport`].
///
/// # Arguments
///
/// * `searcher` - The searcher to queue documents in
/// * `key` - The key of the item to queue results for
/// * `item` - The item to queue results for
/// * `user` - The user that is grepping this item
/// * `params` - The params for this grep query
/// * `shared` - Shared Thorium objects
#[instrument(name = "grep::queue_results", skip_all, err(Debug))]
async fn queue_results<T: GrepSupport>(
    searcher: &mut GrepSearcher,
    key: &str,
    item: &T,
    user: &User,
    params: &GrepParams,
    shared: &Shared,
) -> Result<(), ApiError> {
    // if we aren't searching results or result files then there is nothing to queue
    if !params.searches(GrepSource::Results) && !params.searches(GrepSource::ResultFiles) {
        return Ok(());
    }
    // build the params for getting this items results
    let result_params = ResultGetParams {
        hidden: false,
        tools: params.tools.clone(),
        groups: params.groups.clone(),
    };
    // get this items results, which authorizes the groups we are reading from
    let outputs = item
        .greppable_results(key, user, result_params, shared)
        .await?;
    // queue each tools results
    for (tool, results) in outputs.results {
        for output in results {
            // queue this results inline JSON if we are searching it
            if params.searches(GrepSource::Results) {
                // serialize this result back to JSON so we can grep all of its fields at once
                if let Ok(body) = serde_json::to_string(&output.result) {
                    let doc = GrepDocument::Result {
                        tool: tool.clone(),
                        id: output.id,
                        field: "result".to_owned(),
                    };
                    searcher.add_memory(doc, body);
                }
            }
            // queue this results files if we are searching them
            if params.searches(GrepSource::ResultFiles) {
                for name in &output.files {
                    // build the path to this result file in s3, skipping any stored name that
                    // could escape its result's prefix
                    let Ok(path) = build_result_file_key(&output.id, Path::new(name)) else {
                        continue;
                    };
                    // queue this result file to be streamed and searched
                    let doc = GrepDocument::ResultFile {
                        tool: tool.clone(),
                        id: output.id,
                        name: name.clone(),
                    };
                    searcher.add_s3(doc, GrepBucket::Results, path);
                }
            }
        }
    }
    Ok(())
}

/// Grep all of the documents for a single item
///
/// # Arguments
///
/// * `user` - The user that is grepping this item
/// * `key` - The key of the item to grep
/// * `params` - The params for this grep query
/// * `matcher` - The compiled matcher to search documents with
/// * `remaining` - The query wide budget of matches still left
/// * `resume` - The progress to resume at if we already searched part of this item
/// * `shared` - Shared Thorium objects
#[instrument(
    name = "grep::grep_item",
    skip(user, params, matcher, remaining, resume, shared),
    err(Debug)
)]
pub(crate) async fn grep_item<T: GrepSupport>(
    user: &User,
    key: &str,
    params: &GrepParams,
    matcher: &RegexMatcher,
    remaining: Arc<AtomicUsize>,
    resume: Option<GrepResume>,
    shared: &Shared,
) -> Result<GrepScan, ApiError> {
    // get this item, which authorizes that this user can see it
    let item = T::get_greppable(user, key, shared).await?;
    // build the searcher for this items documents
    let mut searcher = GrepSearcher::new(params, matcher, remaining, resume, shared);
    // queue this items results and result files
    queue_results::<T>(&mut searcher, key, &item, user, params, shared).await?;
    // queue this items own documents
    item.queue_documents(&mut searcher, params);
    // search all of this items documents
    searcher.scan(key, shared).await
}

/// Grep all of the documents for a single item
///
/// The single item route has no cursor, so matches past the match budget are
/// truncated rather then resumable; raising `max_matches` or narrowing the
/// sources is the way to see more of them.
///
/// # Arguments
///
/// * `user` - The user that is grepping this item
/// * `key` - The key of the item to grep
/// * `params` - The params for this grep query
/// * `shared` - Shared Thorium objects
#[instrument(name = "grep::grep_one", skip(user, params, shared), err(Debug))]
pub async fn grep_one<T: GrepSupport>(
    user: &User,
    key: &str,
    mut params: GrepParams,
    shared: &Shared,
) -> Result<GrepMatches, ApiError> {
    // clamp any user settable limits to our configured maximums
    params.clamp(&shared.config.thorium.grep);
    // compile this queries pattern once up front so a bad pattern always 400s
    let matcher = build_matcher(
        &params.pattern,
        params.case_insensitive,
        params.multi_line,
        &shared.config.thorium.grep,
    )?;
    // only validate explicitly requested groups; an empty list must stay empty so
    // getting results falls back to the default group intersection semantics
    if !params.groups.is_empty() {
        // authorize the groups to grep data from
        user.authorize_groups(&mut params.groups, shared).await?;
    }
    // build the query wide budget of matches this query can return
    let remaining = match params.max_matches {
        Some(max_matches) => Arc::new(AtomicUsize::new(max_matches)),
        None => return internal_err!("grep params were not clamped".to_owned()),
    };
    // grep this single item, dropping the resume state since this route has no cursor
    grep_item::<T>(user, key, &params, &matcher, remaining, None, shared)
        .await
        .map(|scan| scan.matches)
}

/// Grep all of the documents for every item this user can see
///
/// # Arguments
///
/// * `user` - The user that is grepping these items
/// * `params` - The params for this grep query
/// * `shared` - Shared Thorium objects
#[instrument(name = "grep::grep_many", skip_all, err(Debug))]
pub async fn grep_many<T: GrepSupport>(
    user: &User,
    mut params: GrepParams,
    shared: &Shared,
) -> Result<ApiCursor<GrepMatches>, ApiError> {
    // clamp any user settable limits to our configured maximums
    params.clamp(&shared.config.thorium.grep);
    // keep the groups the user actually requested so result gets keep their
    // default group semantics when no groups were specified
    let requested_groups = params.groups.clone();
    // authorize the groups to grep data from
    user.authorize_groups(&mut params.groups, shared).await?;
    // grep a page of items
    db::grep::grep::<T>(user, params, requested_groups, shared).await
}

impl<S> FromRequestParts<S> for GrepQueryParams
where
    S: Send + Sync,
{
    /// Rejections from extracting grep query params are api errors
    type Rejection = ApiError;

    /// Extract grep query params from a requests query string
    ///
    /// # Arguments
    ///
    /// * `parts` - The request parts to extract our query params from
    /// * `_state` - The shared state for this router
    #[allow(
        clippy::unused_async_trait_impl,
        reason = "axum's FromRequestParts trait defines this method as async"
    )]
    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // try to extract our query
        if let Some(query) = parts.uri.query() {
            // try to deserialize our query string
            Ok(serde_qs::Config::new()
                .max_depth(5)
                .deserialize_str(query)?)
        } else {
            Ok(Self::default())
        }
    }
}

/// Collect the snippets of every match in a single document
///
/// This is only used by our tests to make asserting on matches less verbose.
///
/// # Arguments
///
/// * `matches` - The document matches to collect snippets from
#[cfg(test)]
fn snippets(matches: &GrepDocumentMatches) -> Vec<&str> {
    // borrow each match's snippet in order
    matches
        .matches
        .iter()
        .map(|found| found.snippet.as_str())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the grep limits to test with
    ///
    /// # Arguments
    ///
    /// * `max_matches` - The max number of matches a query can return
    fn limits(max_matches: usize) -> Grep {
        Grep {
            max_matches,
            ..Grep::default()
        }
    }

    /// Build a matcher to test with
    ///
    /// # Arguments
    ///
    /// * `pattern` - The pattern to build a matcher for
    /// * `case_insensitive` - Whether this matcher should ignore case
    /// * `multi_line` - Whether this matcher can match across lines
    /// * `limits` - The limits to build this matcher with
    fn matcher(
        pattern: &str,
        case_insensitive: bool,
        multi_line: bool,
        limits: &Grep,
    ) -> Result<RegexMatcher, ApiError> {
        // delegate to the real builder so tests can never drift from the api
        build_matcher(pattern, case_insensitive, multi_line, limits)
    }

    /// Search an in memory document and return the matches found in it
    ///
    /// # Arguments
    ///
    /// * `pattern` - The pattern to search for
    /// * `body` - The document to search
    /// * `limits` - The limits to search with
    fn search_memory(pattern: &str, body: &str, limits: &Grep) -> GrepDocumentMatches {
        // search this document without skipping any matches
        let (matches, _) = search_memory_skip(pattern, body, limits, 0);
        matches
    }

    /// Search an in memory document skipping already returned matches
    ///
    /// Also returns the budget left after the search so tests can assert skipped
    /// matches never claim from it.
    ///
    /// # Arguments
    ///
    /// * `pattern` - The pattern to search for
    /// * `body` - The document to search
    /// * `limits` - The limits to search with
    /// * `skip` - The number of already returned matches to skip
    fn search_memory_skip(
        pattern: &str,
        body: &str,
        limits: &Grep,
        skip: usize,
    ) -> (GrepDocumentMatches, usize) {
        // build our matcher and sink
        let matcher = matcher(pattern, false, false, limits).unwrap();
        let remaining = Arc::new(AtomicUsize::new(limits.max_matches));
        let mut sink = GrepSink::new(matcher.clone(), remaining.clone(), limits, skip);
        // search this document
        let mut searcher = build_searcher(limits, false, true);
        searcher
            .search_slice(&matcher, body.as_bytes(), &mut sink)
            .unwrap();
        (sink.into_matches(), remaining.load(Ordering::Relaxed))
    }

    /// Search a streamed document and return the matches found in it
    ///
    /// This takes the same `search_reader` path our s3 documents take, just over an in
    /// process reader instead of an s3 stream so no backend is needed.
    ///
    /// # Arguments
    ///
    /// * `pattern` - The pattern to search for
    /// * `body` - The document to search
    /// * `limits` - The limits to search with
    /// * `multi_line` - Whether this pattern can match across lines
    fn search_stream(
        pattern: &str,
        body: &[u8],
        limits: &Grep,
        multi_line: bool,
    ) -> (Result<(), std::io::Error>, GrepDocumentMatches) {
        // build our matcher and sink
        let matcher = matcher(pattern, false, multi_line, limits).unwrap();
        let remaining = Arc::new(AtomicUsize::new(limits.max_matches));
        let mut sink = GrepSink::new(matcher.clone(), remaining, limits, 0);
        // cap this document the same way we cap an s3 stream
        let max = usize::try_from(limits.max_document_size.as_u64()).unwrap();
        let capped = &body[..std::cmp::min(body.len(), max + 1)];
        // search this document through the reader path
        let mut searcher = build_searcher(limits, multi_line, false);
        let result = searcher.search_reader(&matcher, std::io::Cursor::new(capped), &mut sink);
        (result, sink.into_matches())
    }

    /// A single match reports its line, offset, length, and snippet
    #[test]
    fn single_match_reports_line_offset_and_len() {
        // search a document with a single match on its second line
        let limits = limits(100);
        let matches = search_memory("corn", "apples\ncorn here\n", &limits);
        // we should have found exactly one match
        assert_eq!(matches.matches.len(), 1);
        // that match should be on line 2 at the start of that line
        let found = &matches.matches[0];
        assert_eq!(found.line, 2);
        assert_eq!(found.offset, 7);
        assert_eq!(found.len, 4);
        assert_eq!(found.snippet, "corn here");
        // and this document was fully searched
        assert!(matches.truncated.is_none());
    }

    /// Every match on one line is reported with its own offset
    #[test]
    fn several_matches_on_one_line_are_all_reported() {
        // search a document with three matches on a single line
        let limits = limits(100);
        let matches = search_memory("corn", "corn corn corn\n", &limits);
        // we should have found all three matches
        assert_eq!(matches.matches.len(), 3);
        // each match should have its own offset on the same line
        assert_eq!(
            matches
                .matches
                .iter()
                .map(|found| (found.line, found.offset))
                .collect::<Vec<(u64, usize)>>(),
            vec![(1, 0), (1, 5), (1, 10)]
        );
    }

    /// Case insensitive searches match any case
    #[test]
    fn case_insensitive_matches_any_case() {
        let limits = limits(100);
        // a case sensitive search should not match a different case
        let sensitive = matcher("corn", false, false, &limits).unwrap();
        let remaining = Arc::new(AtomicUsize::new(limits.max_matches));
        let mut sink = GrepSink::new(sensitive.clone(), remaining, &limits, 0);
        let mut searcher = build_searcher(&limits, false, true);
        searcher
            .search_slice(&sensitive, b"CORN\n", &mut sink)
            .unwrap();
        assert_eq!(sink.into_matches().matches, [] as [GrepMatch; 0]);
        // but a case insensitive search should
        let insensitive = matcher("corn", true, false, &limits).unwrap();
        let remaining = Arc::new(AtomicUsize::new(limits.max_matches));
        let mut sink = GrepSink::new(insensitive.clone(), remaining, &limits, 0);
        let mut searcher = build_searcher(&limits, false, true);
        searcher
            .search_slice(&insensitive, b"CORN\n", &mut sink)
            .unwrap();
        assert_eq!(sink.into_matches().matches.len(), 1);
    }

    /// Dollar anchors match at the end of every line
    #[test]
    fn dollar_anchors_match_at_line_ends() {
        // a dollar anchor should match at the end of every line, not just the document
        let limits = limits(100);
        let matches = search_memory("corn$", "corn\nmore corn\nnothing here\n", &limits);
        // both lines ending in corn should match
        assert_eq!(matches.matches.len(), 2);
        assert_eq!(
            matches
                .matches
                .iter()
                .map(|found| found.line)
                .collect::<Vec<u64>>(),
            vec![1, 2]
        );
    }

    /// Caret anchors match at the start of every line
    #[test]
    fn caret_anchors_match_at_line_starts() {
        // a caret anchor should match at the start of every line, not just the document
        let limits = limits(100);
        let matches = search_memory("^corn", "apples\ncorn here\n", &limits);
        // only the line starting with corn should match
        assert_eq!(matches.matches.len(), 1);
        assert_eq!(matches.matches[0].line, 2);
        assert_eq!(matches.matches[0].offset, 7);
    }

    /// Multi line searches accept and match patterns spanning lines
    #[test]
    fn multi_line_matches_across_lines() {
        let limits = limits(100);
        // a single line search can never match across a newline so the pattern is rejected
        //
        // this is the same behavior as ripgrep without '-U', and it surfaces as a 400
        assert!(matcher("corn\napples", false, false, &limits).is_err());
        // but a multi line search should accept it and match across the newline
        let (result, matches) = search_stream("corn\napples", b"corn\napples\n", &limits, true);
        result.unwrap();
        assert_eq!(matches.matches.len(), 1);
        assert_eq!(matches.matches[0].line, 1);
    }

    /// Matches in a coalesced multi line block report their own lines
    #[test]
    fn multi_line_blocks_report_per_match_lines() {
        // search a document whose adjacent matching lines coalesce into one block
        let limits = limits(100);
        let (result, matches) = search_stream("corn", b"corn\ncorn\ncorn\n", &limits, true);
        result.unwrap();
        // every match should report the line it actually starts on
        assert_eq!(
            matches
                .matches
                .iter()
                .map(|found| (found.line, found.offset))
                .collect::<Vec<(u64, usize)>>(),
            vec![(1, 0), (2, 5), (3, 10)]
        );
    }

    /// Snippets are truncated to the max snippet size
    #[test]
    fn snippets_are_truncated_to_the_max_snippet_size() {
        // search a document with a very long matching line
        let mut limits = limits(100);
        limits.max_snippet_size = 8;
        let body = format!("corn{}\n", "a".repeat(1024));
        let matches = search_memory("corn", &body, &limits);
        // our snippet should be cut down to our max snippet size
        assert_eq!(matches.matches.len(), 1);
        assert_eq!(matches.matches[0].snippet.len(), 8);
        assert_eq!(matches.matches[0].snippet, "cornaaaa");
    }

    /// The match budget halts a search at exactly the cap
    #[test]
    fn max_matches_halts_early_and_returns_exactly_the_cap() {
        // search a document with far more matches then our cap allows
        let limits = limits(3);
        let body = "corn\n".repeat(100);
        let matches = search_memory("corn", &body, &limits);
        // we should get exactly our cap worth of matches
        assert_eq!(matches.matches.len(), 3);
        // and this document should be flagged as truncated by our match budget
        assert_eq!(matches.truncated, Some(TruncationReason::MatchBudget));
    }

    /// The match budget is shared across documents
    #[test]
    fn max_matches_is_shared_across_documents() {
        // build a budget that only allows two matches across all documents
        let limits = limits(2);
        let matcher = matcher("corn", false, false, &limits).unwrap();
        let remaining = Arc::new(AtomicUsize::new(limits.max_matches));
        // search our first document, which uses up the entire budget
        let mut first = GrepSink::new(matcher.clone(), remaining.clone(), &limits, 0);
        let mut searcher = build_searcher(&limits, false, true);
        searcher
            .search_slice(&matcher, b"corn\ncorn\ncorn\n", &mut first)
            .unwrap();
        assert_eq!(first.into_matches().matches.len(), 2);
        // our second document should get nothing since the budget is exhausted
        let mut second = GrepSink::new(matcher.clone(), remaining.clone(), &limits, 0);
        let mut searcher = build_searcher(&limits, false, true);
        searcher
            .search_slice(&matcher, b"corn\n", &mut second)
            .unwrap();
        let second = second.into_matches();
        assert_eq!(second.matches, [] as [GrepMatch; 0]);
        assert_eq!(second.truncated, Some(TruncationReason::MatchBudget));
        // and our budget should be fully drained
        assert_eq!(remaining.load(Ordering::Relaxed), 0);
    }

    /// Oversized documents are searched up to the limit and flagged
    #[test]
    fn oversized_documents_are_searched_up_to_the_limit_and_flagged() {
        // build a document larger then our max document size with a match in its prefix
        let mut limits = limits(100);
        limits.max_document_size = bytesize::ByteSize::b(64);
        let mut body = b"corn\n".to_vec();
        body.extend(std::iter::repeat_n(b'a', 4096));
        body.push(b'\n');
        // search it through the same capped reader path our s3 documents take
        let (result, matches) = search_stream("corn", &body, &limits, false);
        // this should not error, we just stop reading at our limit
        result.unwrap();
        // we should still get the match from the prefix we did search
        assert_eq!(matches.matches.len(), 1);
        assert_eq!(matches.matches[0].line, 1);
        // and we should have searched no more then one byte past our limit
        assert!(matches.searched <= limits.max_document_size.as_u64() + 1);
    }

    /// Oversized lines surface as a skipped document
    #[test]
    fn oversized_lines_are_reported_as_a_skipped_document() {
        // build a document with a single line larger then our max line size
        let mut limits = limits(100);
        limits.max_line_size = bytesize::ByteSize::b(64);
        let mut body = b"corn".to_vec();
        body.extend(std::iter::repeat_n(b'a', 4096));
        body.push(b'\n');
        // search it through our streamed path
        let (result, _matches) = search_stream("corn", &body, &limits, false);
        // grep should refuse to grow its roll buffer past our heap limit, which we
        // surface to the user as a skipped document rather then a 500
        assert!(result.is_err());
    }

    /// Multi line streams buffer the capped prefix and search it cleanly
    #[test]
    fn multi_line_streams_stay_inside_the_max_document_size() {
        // build a multi line document larger then our max document size
        let mut limits = limits(100);
        limits.max_document_size = bytesize::ByteSize::b(64);
        let mut body = b"corn\n".to_vec();
        body.extend(std::iter::repeat_n(b'a', 4096));
        body.push(b'\n');
        // search it with multi line mode on, which buffers onto the heap
        let (result, matches) = search_stream("corn", &body, &limits, true);
        // the capped prefix must buffer and search cleanly instead of dying on the
        // heap limit, so oversized documents report their real truncation reason
        result.unwrap();
        // the match in the prefix we did search should still be reported
        assert_eq!(matches.matches.len(), 1);
        // and we should have searched exactly one byte past our limit, which is
        // what classify_search flags as a document that was too large
        assert_eq!(matches.searched, limits.max_document_size.as_u64() + 1);
    }

    /// Errored searches keep the partial matches their sink collected
    #[test]
    fn classify_search_keeps_partial_matches_on_error() {
        // collect a match into a sink first
        let limits = limits(100);
        let matcher = matcher("corn", false, false, &limits).unwrap();
        let remaining = Arc::new(AtomicUsize::new(limits.max_matches));
        let mut sink = GrepSink::new(matcher.clone(), remaining.clone(), &limits, 0);
        let mut searcher = build_searcher(&limits, false, true);
        searcher
            .search_slice(&matcher, b"corn\n", &mut sink)
            .unwrap();
        // classifying an errored search must keep the matches the sink collected
        let classified = classify_search(Err(std::io::Error::other("corn error")), sink, 1024);
        assert_eq!(classified.matches.len(), 1);
        assert_eq!(classified.truncated, Some(TruncationReason::Error));
        // and the budget those matches claimed maps to matches we actually return
        assert_eq!(remaining.load(Ordering::Relaxed), limits.max_matches - 1);
    }

    /// Reads past the document size limit are flagged as truncated
    #[test]
    fn classify_search_flags_reads_past_the_limit() {
        // search a document that is one byte past a tiny document size limit
        let limits = limits(100);
        let oversized = matcher("corn", false, false, &limits).unwrap();
        let remaining = Arc::new(AtomicUsize::new(limits.max_matches));
        let mut sink = GrepSink::new(oversized.clone(), remaining, &limits, 0);
        let mut searcher = build_searcher(&limits, false, true);
        searcher
            .search_slice(&oversized, b"corn\n", &mut sink)
            .unwrap();
        // a clean search that read past the limit is a truncated document
        let matches = classify_search(Ok(()), sink, 4);
        assert_eq!(matches.truncated, Some(TruncationReason::MaxDocumentSize));
        // but binary data always wins over the document size since it came first
        let binary = matcher("corn", false, false, &limits).unwrap();
        let remaining = Arc::new(AtomicUsize::new(limits.max_matches));
        let mut sink = GrepSink::new(binary.clone(), remaining, &limits, 0);
        let mut searcher = build_searcher(&limits, false, true);
        searcher
            .search_slice(&binary, b"corn\0here\n", &mut sink)
            .unwrap();
        let matches = classify_search(Ok(()), sink, 1);
        assert_eq!(matches.truncated, Some(TruncationReason::Binary));
    }

    /// In memory documents are capped at the max document size
    #[test]
    fn in_memory_documents_are_capped_at_the_document_size() {
        // build an in memory document larger then our max document size
        let mut limits = limits(100);
        limits.max_document_size = bytesize::ByteSize::b(64);
        let max = limits.max_document_size.as_u64();
        let mut body = String::from("corn\n");
        body.push_str(&"a".repeat(4096));
        body.push('\n');
        // search just the capped prefix the same way the search worker does
        let matcher = matcher("corn", false, false, &limits).unwrap();
        let remaining = Arc::new(AtomicUsize::new(limits.max_matches));
        let mut sink = GrepSink::new(matcher.clone(), remaining, &limits, 0);
        let mut searcher = build_searcher(&limits, false, true);
        let result = searcher.search_slice(&matcher, cap_document(body.as_bytes(), max), &mut sink);
        let classified = classify_search(result, sink, max);
        // the match in the prefix should be found and the document flagged as truncated
        assert_eq!(classified.matches.len(), 1);
        assert_eq!(
            classified.truncated,
            Some(TruncationReason::MaxDocumentSize)
        );
        // and we should have searched exactly one byte past the limit
        assert_eq!(classified.searched, max + 1);
    }

    /// Binary content abandons a document and flags it
    #[test]
    fn binary_content_quits_early_and_is_flagged() {
        // build a document with a match, an embedded NUL, and then another match
        let limits = limits(100);
        let body = b"corn\nbinary\0here\ncorn\n";
        // search it through our streamed path
        let (result, matches) = search_stream("corn", body, &limits, false);
        // abandoning a binary document is not an error, we just stop searching it
        result.unwrap();
        // this document should be flagged as truncated by binary data
        assert_eq!(matches.truncated, Some(TruncationReason::Binary));
        // and we should never report the match that sits after the NUL
        //
        // the line buffered reader scans each buffer for binary data before searching it,
        // so a NUL this early abandons the document before any line is reported
        assert!(matches.matches.iter().all(|found| found.line < 3));
    }

    /// Pathological patterns are rejected by the configured size limits
    #[test]
    fn pathological_patterns_are_rejected_by_our_size_limits() {
        // build limits with a tiny regex size limit
        let mut limits = limits(100);
        limits.regex_size_limit = bytesize::ByteSize::b(16);
        limits.dfa_size_limit = bytesize::ByteSize::b(16);
        // a pattern that compiles to more then our size limit should be rejected
        let built = matcher(
            "[a-zA-Z0-9]{64}corn(apples|oranges|pears)+",
            false,
            false,
            &limits,
        );
        assert!(built.is_err());
    }

    /// Invalid patterns are rejected at compile time
    #[test]
    fn invalid_patterns_are_rejected() {
        // an unbalanced group is not a valid regex
        let limits = limits(100);
        assert!(matcher("corn(", false, false, &limits).is_err());
    }

    /// The sources list gates which sources are searched
    #[test]
    fn searches_gates_each_source() {
        // by default every source should be searched
        let params = GrepParams::default();
        for source in [
            GrepSource::Results,
            GrepSource::ResultFiles,
            GrepSource::Submissions,
            GrepSource::Comments,
            GrepSource::Tags,
            GrepSource::Metadata,
        ] {
            assert!(params.searches(source));
        }
        // restricting to just submissions should gate out everything else
        let params = GrepParams {
            sources: vec![GrepSource::Submissions],
            ..GrepParams::default()
        };
        assert!(params.searches(GrepSource::Submissions));
        assert!(!params.searches(GrepSource::Results));
        assert!(!params.searches(GrepSource::ResultFiles));
        assert!(!params.searches(GrepSource::Comments));
        assert!(!params.searches(GrepSource::Tags));
        assert!(!params.searches(GrepSource::Metadata));
    }

    /// Clamping bounds every user settable limit
    #[test]
    fn clamp_bounds_user_settable_limits() {
        // build a config with small maximums
        let conf = Grep {
            max_matches: 10,
            max_items: 5,
            max_scan_time: 500,
            max_scan_idle: 50,
            ..Grep::default()
        };
        // a request asking for more then we allow should be clamped down
        let mut params = GrepParams {
            limit: 1000,
            max_matches: Some(1000),
            scan_time: Some(100_000),
            scan_idle: Some(100_000),
            ..GrepParams::default()
        };
        params.clamp(&conf);
        assert_eq!(params.limit, 5);
        assert_eq!(params.max_matches, Some(10));
        assert_eq!(params.scan_time, Some(500));
        assert_eq!(params.scan_idle, Some(50));
        // a request asking for less then we allow should be left alone
        let mut params = GrepParams {
            limit: 2,
            max_matches: Some(3),
            scan_time: Some(20),
            scan_idle: Some(5),
            ..GrepParams::default()
        };
        params.clamp(&conf);
        assert_eq!(params.limit, 2);
        assert_eq!(params.max_matches, Some(3));
        assert_eq!(params.scan_time, Some(20));
        assert_eq!(params.scan_idle, Some(5));
        // a request that sets no match limit should get our configured maximum
        let mut params = GrepParams::default();
        params.clamp(&conf);
        assert_eq!(params.max_matches, Some(10));
        // a request that sets no time budgets should get our configured maximums
        assert_eq!(params.scan_time, Some(500));
        assert_eq!(params.scan_idle, Some(50));
        // a request asking for zero items should be floored so cursors always progress
        let mut params = GrepParams {
            limit: 0,
            ..GrepParams::default()
        };
        params.clamp(&conf);
        assert_eq!(params.limit, 1);
        // zero time budgets should be floored so they are never empty
        let mut params = GrepParams {
            scan_time: Some(0),
            scan_idle: Some(0),
            ..GrepParams::default()
        };
        params.clamp(&conf);
        assert_eq!(params.scan_time, Some(1));
        assert_eq!(params.scan_idle, Some(1));
        // even a nonsense config allowing zero items should still allow one
        let conf = Grep {
            max_items: 0,
            ..Grep::default()
        };
        let mut params = GrepParams::default();
        params.clamp(&conf);
        assert_eq!(params.limit, 1);
    }

    /// The snippet test helper returns every snippet in order
    #[test]
    fn snippet_helper_returns_each_snippet() {
        // search a document with two matches
        let limits = limits(100);
        let matches = search_memory("corn", "corn\ncorn here\n", &limits);
        // our helper should hand back both snippets in order
        assert_eq!(snippets(&matches), vec!["corn", "corn here"]);
    }

    /// Build a worker that looks like it was already scanned for resume tests
    ///
    /// # Arguments
    ///
    /// * `doc` - The document this worker searched
    /// * `skip` - The matches this worker skipped before recording any
    /// * `found` - The number of matches this worker recorded
    /// * `truncated` - Why this workers document was only partially searched
    fn scanned_worker(
        doc: GrepDocument,
        skip: usize,
        found: usize,
        truncated: Option<TruncationReason>,
    ) -> GrepSearchWorker {
        // build the matches this worker found
        let matches = (0..found)
            .map(|offset| GrepMatch {
                line: 1,
                offset,
                len: 1,
                snippet: String::new(),
            })
            .collect();
        GrepSearchWorker {
            doc,
            body: GrepBody::Memory(String::new()),
            skip,
            matches: GrepDocumentMatches {
                matches,
                searched: 0,
                truncated,
            },
        }
    }

    /// Skip mode skips matches already returned on earlier pages
    #[test]
    fn skip_mode_skips_already_returned_matches() {
        // search a document with three matches, skipping the first two
        let limits = limits(100);
        let (matches, remaining) = search_memory_skip("corn", "corn corn corn\n", &limits, 2);
        // only the third match should be recorded
        assert_eq!(matches.matches.len(), 1);
        assert_eq!(matches.matches[0].offset, 10);
        // skipped matches should never claim from our budget
        assert_eq!(remaining, 99);
        // and this document should still count as fully searched
        assert!(matches.truncated.is_none());
    }

    /// Skipped matches never claim from the match budget
    #[test]
    fn skip_mode_never_claims_budget_for_skipped_matches() {
        // a budget of one is enough to return the one unseen match in this document
        let limits = limits(1);
        let (matches, remaining) = search_memory_skip("corn", "corn corn corn\n", &limits, 2);
        assert_eq!(matches.matches.len(), 1);
        assert_eq!(matches.matches[0].offset, 10);
        assert_eq!(remaining, 0);
        assert!(matches.truncated.is_none());
        // but a fourth match should still be cut off by the exhausted budget
        let (matches, remaining) = search_memory_skip("corn", "corn corn corn corn\n", &limits, 2);
        assert_eq!(matches.matches.len(), 1);
        assert_eq!(matches.matches[0].offset, 10);
        assert_eq!(remaining, 0);
        assert_eq!(matches.truncated, Some(TruncationReason::MatchBudget));
    }

    /// Skipping more matches then exist is harmless
    #[test]
    fn skip_mode_tolerates_more_skip_than_matches() {
        // skip more matches then this document has
        let limits = limits(100);
        let (matches, remaining) = search_memory_skip("corn", "corn\n", &limits, 5);
        // nothing should be recorded and nothing should be claimed
        assert_eq!(matches.matches, [] as [GrepMatch; 0]);
        assert_eq!(remaining, 100);
        assert!(matches.truncated.is_none());
    }

    /// Resume state only exists after budget truncation
    #[test]
    fn build_resume_is_none_without_budget_truncation() {
        // build workers that completed or stopped for reasons a rescan cannot fix
        let workers = vec![
            scanned_worker(GrepDocument::Comment { id: Uuid::new_v4() }, 0, 3, None),
            scanned_worker(
                GrepDocument::Tag {
                    key: "plant".to_owned(),
                    value: "corn".to_owned(),
                },
                0,
                0,
                Some(TruncationReason::Binary),
            ),
            scanned_worker(
                GrepDocument::Metadata {
                    field: "earliest_seen".to_owned(),
                },
                0,
                1,
                Some(TruncationReason::Error),
            ),
        ];
        // no document was cut off by the match budget so there is nothing to resume
        assert!(build_resume("corn", HashMap::default(), &workers).is_none());
    }

    /// Resume state maps each document outcome onto skip or done
    #[test]
    fn build_resume_maps_each_outcome() {
        // build one worker for each outcome a scan can have
        let done = GrepDocument::Comment { id: Uuid::new_v4() };
        let cut_off = GrepDocument::Metadata {
            field: "earliest_seen".to_owned(),
        };
        let never_started = GrepDocument::Tag {
            key: "plant".to_owned(),
            value: "corn".to_owned(),
        };
        let binary = GrepDocument::Submission {
            id: Uuid::new_v4(),
            field: "description".to_owned(),
        };
        let errored = GrepDocument::Comment { id: Uuid::new_v4() };
        let workers = vec![
            scanned_worker(done.clone(), 0, 3, None),
            scanned_worker(cut_off.clone(), 2, 3, Some(TruncationReason::MatchBudget)),
            scanned_worker(
                never_started.clone(),
                4,
                0,
                Some(TruncationReason::MatchBudget),
            ),
            scanned_worker(binary.clone(), 0, 1, Some(TruncationReason::Binary)),
            scanned_worker(errored.clone(), 1, 2, Some(TruncationReason::Error)),
        ];
        // the match budget died in this item so we should get resume state
        let resume = build_resume("corn", HashMap::default(), &workers).unwrap();
        assert_eq!(resume.key, "corn");
        let map = resume.into_map();
        // completed documents must never be rescanned
        assert_eq!(
            map.get(&GrepResumeKey::from(&done)),
            Some(&GrepDocResume::Done)
        );
        // cut off documents skip everything already returned
        assert_eq!(
            map.get(&GrepResumeKey::from(&cut_off)),
            Some(&GrepDocResume::Skip(5))
        );
        // never started documents make no progress but lose nothing
        assert_eq!(
            map.get(&GrepResumeKey::from(&never_started)),
            Some(&GrepDocResume::Skip(4))
        );
        // terminally truncated documents must never be rescanned either
        assert_eq!(
            map.get(&GrepResumeKey::from(&binary)),
            Some(&GrepDocResume::Done)
        );
        // errored documents are retried, skipping everything they already returned
        assert_eq!(
            map.get(&GrepResumeKey::from(&errored)),
            Some(&GrepDocResume::Skip(3))
        );
    }

    /// Resume state carries forward documents that already finished
    #[test]
    fn build_resume_carries_prior_done_docs() {
        // build prior resume state with a finished document that was not requeued
        let finished = GrepDocument::Comment { id: Uuid::new_v4() };
        let stale = GrepDocument::Metadata {
            field: "stale".to_owned(),
        };
        let prior = HashMap::from([
            (GrepResumeKey::from(&finished), GrepDocResume::Done),
            (GrepResumeKey::from(&stale), GrepDocResume::Skip(2)),
        ]);
        // cut off another document this page
        let cut_off = GrepDocument::Tag {
            key: "plant".to_owned(),
            value: "corn".to_owned(),
        };
        let workers = vec![scanned_worker(
            cut_off.clone(),
            0,
            1,
            Some(TruncationReason::MatchBudget),
        )];
        let map = build_resume("corn", prior, &workers).unwrap().into_map();
        // the finished document should survive so it is never rescanned
        assert_eq!(
            map.get(&GrepResumeKey::from(&finished)),
            Some(&GrepDocResume::Done)
        );
        // stale skip states are rebuilt from their workers, not carried forward
        assert!(!map.contains_key(&GrepResumeKey::from(&stale)));
        assert_eq!(
            map.get(&GrepResumeKey::from(&cut_off)),
            Some(&GrepDocResume::Skip(1))
        );
    }

    /// Resume keys identify every document kind distinctly
    #[test]
    fn resume_key_identifies_every_document_kind() {
        // build one of each kind of document
        let id = Uuid::new_v4();
        let docs = [
            GrepDocument::Result {
                tool: "corn".to_owned(),
                id,
                field: "result".to_owned(),
            },
            GrepDocument::ResultFile {
                tool: "corn".to_owned(),
                id,
                name: "peeled".to_owned(),
            },
            GrepDocument::Submission {
                id,
                field: "description".to_owned(),
            },
            GrepDocument::Comment { id },
            GrepDocument::Tag {
                key: "plant".to_owned(),
                value: "corn".to_owned(),
            },
            GrepDocument::Metadata {
                field: "earliest_seen".to_owned(),
            },
        ];
        // every kind of document should map onto its own distinct identity
        let keys = docs
            .iter()
            .map(GrepResumeKey::from)
            .collect::<std::collections::HashSet<GrepResumeKey>>();
        assert_eq!(keys.len(), docs.len());
    }

    /// Resume state round trips through JSON
    #[test]
    fn grep_resume_serde_round_trips() {
        // build resume state with one of each document state
        let resume = GrepResume {
            key: "corn".to_owned(),
            docs: vec![
                (
                    GrepResumeKey::Comment { id: Uuid::new_v4() },
                    GrepDocResume::Done,
                ),
                (
                    GrepResumeKey::Tag {
                        key: "plant".to_owned(),
                        value: "corn".to_owned(),
                    },
                    GrepDocResume::Skip(3),
                ),
            ],
        };
        // round trip our resume state through the same JSON path our cursors use
        let serial = serde_json::to_string(&resume).unwrap();
        let rebuilt: GrepResume = serde_json::from_str(&serial).unwrap();
        assert_eq!(rebuilt.key, resume.key);
        assert_eq!(rebuilt.docs, resume.docs);
    }
}
