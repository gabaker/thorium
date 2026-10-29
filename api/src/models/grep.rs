//! Structures related to searching data in Thorium with grep
//!
//! Unlike Elasticsearch based searches, grep based searches read the underlying
//! data out of Scylla/S3 and run a regex over it. That is slower but it can search
//! data that is not indexed at all, like the contents of result files.

use chrono::prelude::*;
use std::collections::HashMap;
use uuid::Uuid;

/// The different sources of data a grep query can search
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum GrepSource {
    /// The inline JSON results for a tool
    Results,
    /// The result files stored in s3 for a tool
    ResultFiles,
    /// The submission info for an item (names, descriptions, and origins)
    Submissions,
    /// The comments on an item
    Comments,
    /// The tags on an item
    Tags,
    /// Any type specific metadata for an item (currently just entities)
    Metadata,
}

impl GrepSource {
    /// Cast our grep source to a str
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            GrepSource::Results => "Results",
            GrepSource::ResultFiles => "ResultFiles",
            GrepSource::Submissions => "Submissions",
            GrepSource::Comments => "Comments",
            GrepSource::Tags => "Tags",
            GrepSource::Metadata => "Metadata",
        }
    }
}

impl std::fmt::Display for GrepSource {
    /// Write our grep source as its serde variant name
    ///
    /// This must stay in sync with serde's variant names since the client uses
    /// it to build `sources[]` query params that `serde_qs` deserializes.
    ///
    /// # Arguments
    ///
    /// * `f` - The formatter to write to
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single match found by a grep query
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct GrepMatch {
    /// The line in the document this match starts on
    pub line: u64,
    /// The offset from the start of the document this match starts at
    pub offset: usize,
    /// How many bytes of the document this match covers
    pub len: usize,
    /// A snippet of the matching line
    pub snippet: String,
}

/// The reason a document was only partially searched
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub enum TruncationReason {
    /// Binary data (a NUL byte) was detected part way through this document
    Binary,
    /// This document was larger then the configured max document size
    MaxDocumentSize,
    /// The query wide match budget was exhausted while searching this document
    MatchBudget,
    /// This document could not be fully searched due to an error (a failed s3
    /// download, a line larger then the max line size, or a search engine error)
    Error,
}

/// The matches found in a single document
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct GrepDocumentMatches {
    /// The matches found in this document
    pub matches: Vec<GrepMatch>,
    /// How many bytes of this document were actually searched
    pub searched: u64,
    /// Why this document was only partially searched if it was
    ///
    /// This is `None` when this document was fully searched.
    pub truncated: Option<TruncationReason>,
}

impl GrepDocumentMatches {
    /// Mark this document as only partially searched if it is not already
    ///
    /// # Arguments
    ///
    /// * `reason` - The reason this document was only partially searched
    pub fn mark_truncated(&mut self, reason: TruncationReason) {
        // keep the first reason since it is where searching actually stopped
        if self.truncated.is_none() {
            self.truncated = Some(reason);
        }
    }
}

/// The matches found in all of the documents for a single sample, repo, or entity
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct GrepMatches {
    /// The key for the item these matches come from
    pub key: String,
    /// The matches found in the inline results for this item by tool, result, and field
    pub results: HashMap<String, HashMap<Uuid, HashMap<String, GrepDocumentMatches>>>,
    /// The matches found in the result files for this item by tool, result, and file name
    pub result_files: HashMap<String, HashMap<Uuid, HashMap<String, GrepDocumentMatches>>>,
    /// The matches found in the submission info for this item by submission and field
    pub submissions: HashMap<Uuid, HashMap<String, GrepDocumentMatches>>,
    /// The matches found in the comments for this item by comment
    pub comments: HashMap<Uuid, GrepDocumentMatches>,
    /// The matches found in the tags for this item by tag key and value
    pub tags: HashMap<String, HashMap<String, GrepDocumentMatches>>,
    /// The matches found in any type specific metadata for this item by field
    pub metadata: HashMap<String, GrepDocumentMatches>,
}

impl GrepMatches {
    /// Create an empty set of matches for a specific item
    ///
    /// # Arguments
    ///
    /// * `key` - The key for the item these matches come from
    #[must_use]
    pub fn new<T: Into<String>>(key: T) -> Self {
        GrepMatches {
            key: key.into(),
            results: HashMap::default(),
            result_files: HashMap::default(),
            submissions: HashMap::default(),
            comments: HashMap::default(),
            tags: HashMap::default(),
            metadata: HashMap::default(),
        }
    }

    /// Check if any matches were found for this item
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.results.is_empty()
            && self.result_files.is_empty()
            && self.submissions.is_empty()
            && self.comments.is_empty()
            && self.tags.is_empty()
            && self.metadata.is_empty()
    }
}

/// The counts of documents one page of a grep based search scanned
///
/// These counts can overlap: a document that had matches before being cut short
/// is counted under `found` and under `truncated` or `errors`. Fully searched
/// documents with no matches and binary documents where nothing was searched are
/// not counted at all since they can never appear in a response.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct GrepStats {
    /// How many documents had at least one match
    pub found: u64,
    /// How many documents were only partially searched
    ///
    /// This covers binary data, the max document size, and the match budget;
    /// documents that failed with an error are counted in `errors` instead.
    pub truncated: u64,
    /// How many documents failed to be fully searched due to an error
    pub errors: u64,
}

impl GrepStats {
    /// Fold another set of counts into this one
    ///
    /// # Arguments
    ///
    /// * `other` - The counts to fold in
    pub fn merge(&mut self, other: &GrepStats) {
        // sum each bucket since pages just extend the same overlapping counts
        self.found += other.found;
        self.truncated += other.truncated;
        self.errors += other.errors;
    }
}

/// Helps serde default the sources to grep to all of them
fn default_grep_sources() -> Vec<GrepSource> {
    vec![
        GrepSource::Results,
        GrepSource::ResultFiles,
        GrepSource::Submissions,
        GrepSource::Comments,
        GrepSource::Tags,
        GrepSource::Metadata,
    ]
}

/// Helps serde default the number of items to grep in a single page to 10
fn default_grep_limit() -> usize {
    10
}

/// The params for a grep based search
#[derive(Serialize, Deserialize, Debug)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct GrepParams {
    /// The regex pattern to search for
    pub pattern: String,
    /// Whether this search should ignore case or not
    #[serde(default)]
    pub case_insensitive: bool,
    /// Whether this pattern can match across multiple lines
    ///
    /// This forces documents to be buffered in memory up to the configured max
    /// document size, so it is more expensive then a single line search.
    #[serde(default)]
    pub multi_line: bool,
    /// The groups to search data from
    #[serde(default)]
    pub groups: Vec<String>,
    /// When to start searching data at
    #[serde(default = "Utc::now")]
    pub start: DateTime<Utc>,
    /// When to stop searching data at
    pub end: Option<DateTime<Utc>>,
    /// The cursor id to use if one exists
    pub cursor: Option<Uuid>,
    /// The max number of items with matches to return in this response
    #[serde(default = "default_grep_limit")]
    pub limit: usize,
    /// The sources of data to search
    #[serde(default = "default_grep_sources")]
    pub sources: Vec<GrepSource>,
    /// The tools to limit our result searches too
    #[serde(default)]
    pub tools: Vec<String>,
    /// The max number of matches to return for this query
    pub max_matches: Option<usize>,
    /// The max number of milliseconds this response can scan for
    pub scan_time: Option<u64>,
    /// The max number of milliseconds this response keeps scanning after its last new match
    pub scan_idle: Option<u64>,
    /// Whether to return documents that were truncated or errored without any matches
    ///
    /// Hit-less truncated or errored documents are dropped by default so large
    /// corpuses of unsearchable documents never bury actual matches; the per page
    /// metadata counts still report them either way.
    #[serde(default)]
    pub with_truncated: bool,
}

impl Default for GrepParams {
    /// Create default grep params
    fn default() -> Self {
        GrepParams {
            pattern: String::default(),
            case_insensitive: false,
            multi_line: false,
            groups: Vec::default(),
            start: Utc::now(),
            end: None,
            cursor: None,
            limit: default_grep_limit(),
            sources: default_grep_sources(),
            tools: Vec::default(),
            max_matches: None,
            scan_time: None,
            scan_idle: None,
            with_truncated: false,
        }
    }
}

impl GrepParams {
    /// Check if a specific source of data should be searched
    ///
    /// # Arguments
    ///
    /// * `source` - The source of data to check for
    #[must_use]
    pub fn searches(&self, source: GrepSource) -> bool {
        self.sources.contains(&source)
    }

    /// Check if data in the given groups is in scope for this query
    ///
    /// An empty requested group list keeps the default visibility semantics, where
    /// anything the requesting user can already see is in scope, matching how
    /// result gets treat an empty group list.
    ///
    /// # Arguments
    ///
    /// * `item_groups` - The groups the data being queued is a part of
    #[must_use]
    pub fn groups_allow<'a, I: IntoIterator<Item = &'a String>>(&self, item_groups: I) -> bool {
        // an empty group list keeps the default visibility semantics
        if self.groups.is_empty() {
            return true;
        }
        // otherwise this data must be in at least one requested group
        item_groups
            .into_iter()
            .any(|group| self.groups.contains(group))
    }
}

/// The body of a grep based search
///
/// Only the regex pattern rides in the body since regexes do not survive being
/// crammed into a query string; every other option is a query param set with
/// [`GrepQueryParams`] just like the list routes.
#[derive(Serialize, Deserialize, Debug)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct GrepRequest {
    /// The regex pattern to search for
    pub pattern: String,
}

/// The query params for a grep based search
///
/// These are the query string half of a grep search; the regex pattern itself
/// is sent in the request body as a [`GrepRequest`].
#[derive(Deserialize, Debug)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct GrepQueryParams {
    /// Whether this search should ignore case or not
    #[serde(default)]
    pub case_insensitive: bool,
    /// Whether this pattern can match across multiple lines
    ///
    /// This forces documents to be buffered in memory up to the configured max
    /// document size, so it is more expensive then a single line search.
    #[serde(default)]
    pub multi_line: bool,
    /// The groups to search data from
    #[serde(default)]
    pub groups: Vec<String>,
    /// When to start searching data at
    #[serde(default = "Utc::now")]
    pub start: DateTime<Utc>,
    /// When to stop searching data at
    pub end: Option<DateTime<Utc>>,
    /// The cursor id to use if one exists
    pub cursor: Option<Uuid>,
    /// The max number of items with matches to return in this response
    #[serde(default = "default_grep_limit")]
    pub limit: usize,
    /// The sources of data to search
    #[serde(default = "default_grep_sources")]
    pub sources: Vec<GrepSource>,
    /// The tools to limit our result searches too
    #[serde(default)]
    pub tools: Vec<String>,
    /// The max number of matches to return for this query
    pub max_matches: Option<usize>,
    /// The max number of milliseconds this response can scan for
    pub scan_time: Option<u64>,
    /// The max number of milliseconds this response keeps scanning after its last new match
    pub scan_idle: Option<u64>,
    /// Whether to return documents that were truncated or errored without any matches
    ///
    /// Hit-less truncated or errored documents are dropped by default so large
    /// corpuses of unsearchable documents never bury actual matches; the per page
    /// metadata counts still report them either way.
    #[serde(default)]
    pub with_truncated: bool,
}

impl Default for GrepQueryParams {
    /// Create default grep query params
    fn default() -> Self {
        GrepQueryParams {
            case_insensitive: false,
            multi_line: false,
            groups: Vec::default(),
            start: Utc::now(),
            end: None,
            cursor: None,
            limit: default_grep_limit(),
            sources: default_grep_sources(),
            tools: Vec::default(),
            max_matches: None,
            scan_time: None,
            scan_idle: None,
            with_truncated: false,
        }
    }
}

impl GrepQueryParams {
    /// Combine these query params with a grep request body into full grep params
    ///
    /// # Arguments
    ///
    /// * `request` - The grep request body containing the regex pattern
    #[must_use]
    pub fn into_params(self, request: GrepRequest) -> GrepParams {
        GrepParams {
            pattern: request.pattern,
            case_insensitive: self.case_insensitive,
            multi_line: self.multi_line,
            groups: self.groups,
            start: self.start,
            end: self.end,
            cursor: self.cursor,
            limit: self.limit,
            sources: self.sources,
            tools: self.tools,
            max_matches: self.max_matches,
            scan_time: self.scan_time,
            scan_idle: self.scan_idle,
            with_truncated: self.with_truncated,
        }
    }
}

// the api only methods for grep params
cfg_if::cfg_if! {
    if #[cfg(feature = "api")] {
        use crate::conf::Grep;
        use crate::utils::ApiError;

        impl GrepParams {
            /// Get the end timestamp or get a sane default
            ///
            /// # Arguments
            ///
            /// * `earliest` - The earliest timestamp an item of the searched type can exist at
            pub fn end(&self, earliest: i64) -> Result<DateTime<Utc>, ApiError> {
                match self.end {
                    Some(end) => Ok(end),
                    None => {
                        // default to the earliest an item of this type can exist at
                        match Utc.timestamp_opt(earliest, 0) {
                            chrono::LocalResult::Single(end) => Ok(end),
                            _ => crate::internal_err!(format!(
                                "default earliest timestamp is invalid or ambiguous - {earliest}",
                            )),
                        }
                    }
                }
            }

            /// Clamp our user settable limits to the configured maximums
            ///
            /// # Arguments
            ///
            /// * `conf` - The grep settings for this Thorium cluster
            pub fn clamp(&mut self, conf: &Grep) {
                // never search more items in one page then our config allows, but
                // always allow at least one so a cursor can always make progress
                //
                // max/min instead of clamp since clamp panics if max_items is 0
                self.limit = std::cmp::max(1, std::cmp::min(self.limit, conf.max_items));
                // never return more matches then our config allows
                let max_matches = match self.max_matches {
                    Some(max_matches) => std::cmp::min(max_matches, conf.max_matches),
                    None => conf.max_matches,
                };
                // always allow at least one match so a query can always make progress
                self.max_matches = Some(std::cmp::max(max_matches, 1));
                // never scan longer in one page then our config allows
                let scan_time = match self.scan_time {
                    Some(scan_time) => std::cmp::min(scan_time, conf.max_scan_time),
                    None => conf.max_scan_time,
                };
                // always allow at least one millisecond so the budget is never zero
                self.scan_time = Some(std::cmp::max(scan_time, 1));
                // never idle longer after the last new match then our config allows
                let scan_idle = match self.scan_idle {
                    Some(scan_idle) => std::cmp::min(scan_idle, conf.max_scan_idle),
                    None => conf.max_scan_idle,
                };
                // always allow at least one millisecond so the cutoff is never zero
                self.scan_idle = Some(std::cmp::max(scan_idle, 1));
            }
        }
    }
}

// the client only options for a grep search
cfg_if::cfg_if! {
    if #[cfg(feature = "client")] {
        /// A single page of grep matches returned by the API
        ///
        /// Grep searches are sent as POST requests carrying the regex pattern in a JSON
        /// body so they cannot use the normal GET only [`Cursor`](crate::models::Cursor)
        /// helper. Instead each page is returned on its own and the caller continues the
        /// search by passing this page's cursor id back in with [`GrepOpts::cursor`].
        #[derive(Serialize, Deserialize, Debug, Default)]
        pub struct GrepPage {
            /// The id for this cursor if more items still need to be searched
            pub cursor: Option<Uuid>,
            /// The matches found in this page of items
            pub data: Vec<GrepMatches>,
            /// The counts of found, truncated, and errored documents this page scanned
            ///
            /// Defaulted so pages from an older API without metadata still deserialize.
            #[serde(default)]
            pub metadata: Option<GrepStats>,
        }

        impl GrepPage {
            /// Check if all items have been searched
            #[must_use]
            pub fn exhausted(&self) -> bool {
                self.cursor.is_none()
            }
        }

        /// The options for a grep based search
        #[derive(Serialize, Deserialize, Debug)]
        pub struct GrepOpts {
            /// The regex pattern to search for
            pub pattern: String,
            /// Whether this search should ignore case or not
            pub case_insensitive: bool,
            /// Whether this pattern can match across multiple lines
            pub multi_line: bool,
            /// The groups to search data from
            pub groups: Vec<String>,
            /// When to start searching data at
            pub start: Option<DateTime<Utc>>,
            /// When to stop searching data at
            pub end: Option<DateTime<Utc>>,
            /// The cursor id to use if one exists
            pub cursor: Option<Uuid>,
            /// The max number of items with matches to return in a single page
            pub limit: Option<usize>,
            /// The sources of data to search
            pub sources: Vec<GrepSource>,
            /// The tools to limit our result searches too
            pub tools: Vec<String>,
            /// The max number of matches to return for this query
            pub max_matches: Option<usize>,
            /// The max number of milliseconds a single page can scan for
            pub scan_time: Option<u64>,
            /// The max number of milliseconds a page keeps scanning after its last new match
            pub scan_idle: Option<u64>,
            /// Whether to return documents that were truncated or errored without any matches
            ///
            /// Hit-less truncated or errored documents are dropped by default so large
            /// corpuses of unsearchable documents never bury actual matches; the per page
            /// metadata counts still report them either way.
            pub with_truncated: bool,
        }

        impl GrepOpts {
            /// Create a new set of grep options
            ///
            /// # Arguments
            ///
            /// * `pattern` - The regex pattern to search for
            ///
            /// # Examples
            ///
            /// ```
            /// use thorium::models::GrepOpts;
            ///
            /// GrepOpts::new("corn[0-9]+")
            ///     .case_insensitive()
            ///     .tool("CornHarvester")
            ///     .limit(50);
            /// ```
            #[must_use]
            pub fn new<T: Into<String>>(pattern: T) -> Self {
                GrepOpts {
                    pattern: pattern.into(),
                    case_insensitive: false,
                    multi_line: false,
                    groups: Vec::default(),
                    start: None,
                    end: None,
                    cursor: None,
                    limit: None,
                    sources: Vec::default(),
                    tools: Vec::default(),
                    max_matches: None,
                    scan_time: None,
                    scan_idle: None,
                    with_truncated: false,
                }
            }

            /// Make this search case insensitive
            #[must_use]
            pub fn case_insensitive(mut self) -> Self {
                self.case_insensitive = true;
                self
            }

            /// Allow this pattern to match across multiple lines
            #[must_use]
            pub fn multi_line(mut self) -> Self {
                self.multi_line = true;
                self
            }

            /// Restrict this search to a specific group
            ///
            /// # Arguments
            ///
            /// * `group` - The group to search data from
            #[must_use]
            pub fn group<T: Into<String>>(mut self, group: T) -> Self {
                self.groups.push(group.into());
                self
            }

            /// Restrict this search to specific groups
            ///
            /// # Arguments
            ///
            /// * `groups` - The groups to search data from
            #[must_use]
            pub fn groups<T, I>(mut self, groups: I) -> Self
            where
                T: Into<String>,
                I: IntoIterator<Item = T>,
            {
                self.groups.extend(groups.into_iter().map(Into::into));
                self
            }

            /// Restrict this search to start at a specific date
            ///
            /// # Arguments
            ///
            /// * `start` - The date to start searching data at
            #[must_use]
            pub fn start(mut self, start: DateTime<Utc>) -> Self {
                self.start = Some(start);
                self
            }

            /// Restrict this search to end at a specific date
            ///
            /// # Arguments
            ///
            /// * `end` - The date to stop searching data at
            #[must_use]
            pub fn end(mut self, end: DateTime<Utc>) -> Self {
                self.end = Some(end);
                self
            }

            /// Continue an existing grep search cursor
            ///
            /// # Arguments
            ///
            /// * `cursor` - The id of the cursor to continue
            #[must_use]
            pub fn cursor(mut self, cursor: Uuid) -> Self {
                self.cursor = Some(cursor);
                self
            }

            /// Set the max number of items with matches to return in a single page
            ///
            /// # Arguments
            ///
            /// * `limit` - The max number of items with matches to return in a single page
            #[must_use]
            pub fn limit(mut self, limit: usize) -> Self {
                self.limit = Some(limit);
                self
            }

            /// Restrict this search to a single source of data
            ///
            /// If no sources are set then all sources are searched.
            ///
            /// # Arguments
            ///
            /// * `source` - The source of data to search
            #[must_use]
            pub fn source(mut self, source: GrepSource) -> Self {
                self.sources.push(source);
                self
            }

            /// Restrict this search to specific sources of data
            ///
            /// If no sources are set then all sources are searched.
            ///
            /// # Arguments
            ///
            /// * `sources` - The sources of data to search
            #[must_use]
            pub fn sources<I: IntoIterator<Item = GrepSource>>(mut self, sources: I) -> Self {
                self.sources.extend(sources);
                self
            }

            /// Restrict our result searches to a single tool
            ///
            /// # Arguments
            ///
            /// * `tool` - The tool to search results from
            #[must_use]
            pub fn tool<T: Into<String>>(mut self, tool: T) -> Self {
                self.tools.push(tool.into());
                self
            }

            /// Restrict our result searches to specific tools
            ///
            /// # Arguments
            ///
            /// * `tools` - The tools to search results from
            #[must_use]
            pub fn tools<T, I>(mut self, tools: I) -> Self
            where
                T: Into<String>,
                I: IntoIterator<Item = T>,
            {
                self.tools.extend(tools.into_iter().map(Into::into));
                self
            }

            /// Set the max number of matches to return for this query
            ///
            /// # Arguments
            ///
            /// * `max_matches` - The max number of matches to return
            #[must_use]
            pub fn max_matches(mut self, max_matches: usize) -> Self {
                self.max_matches = Some(max_matches);
                self
            }

            /// Set the max number of milliseconds a single page can scan for
            ///
            /// Corpus pages keep scanning until they fill a page of matches, this much
            /// time passes, or the corpus is exhausted; the server clamps this to its
            /// configured maximum.
            ///
            /// # Arguments
            ///
            /// * `scan_time` - The max number of milliseconds to scan for
            #[must_use]
            pub fn scan_time(mut self, scan_time: u64) -> Self {
                self.scan_time = Some(scan_time);
                self
            }

            /// Set the max number of milliseconds a page keeps scanning after its last new match
            ///
            /// This only applies once a page has found at least one item with matches,
            /// so a page that has found nothing keeps scanning until its scan time is
            /// spent; the server clamps this to its configured maximum.
            ///
            /// # Arguments
            ///
            /// * `scan_idle` - The max number of milliseconds to scan past the last new match
            #[must_use]
            pub fn scan_idle(mut self, scan_idle: u64) -> Self {
                self.scan_idle = Some(scan_idle);
                self
            }

            /// Return documents that were truncated or errored without any matches
            ///
            /// These documents are dropped by default so unsearchable documents never
            /// bury actual matches; the page metadata counts them either way.
            #[must_use]
            pub fn with_truncated(mut self) -> Self {
                self.with_truncated = true;
                self
            }
        }

        impl From<GrepOpts> for GrepRequest {
            /// Convert `GrepOpts` into a `GrepRequest` body
            ///
            /// Only the regex pattern rides in the body; all other options are
            /// sent as query params built from the same `GrepOpts`.
            ///
            /// # Arguments
            ///
            /// * `opts` - The grep options to convert
            fn from(opts: GrepOpts) -> Self {
                GrepRequest {
                    pattern: opts.pattern,
                }
            }
        }
    }
}

#[cfg(all(test, feature = "api"))]
mod tests {
    use super::*;

    /// Make sure a query string built the way the client builds it round-trips
    ///
    /// This pins the coupling between [`GrepSource`]'s `Display` impl and its serde
    /// variant names since the client relies on them matching.
    #[test]
    fn grep_query_params_round_trip() {
        // build a start timestamp to round trip
        let start = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
        // build a query string using the same Display based encoding as the client,
        // percent encoding the '+' in the offset like reqwest does on the wire
        let query = format!(
            "case_insensitive=true&groups[]=plants&start={start}&limit=5\
             &sources[]={first}&sources[]={second}&tools[]=CornHarvester\
             &scan_time=120&scan_idle=30&with_truncated=true",
            start = start.to_rfc3339().replace('+', "%2B"),
            first = GrepSource::ResultFiles,
            second = GrepSource::Tags,
        );
        // deserialize the query string with the same config as the extractor
        let params: GrepQueryParams = serde_qs::Config::new()
            .max_depth(5)
            .deserialize_str(&query)
            .unwrap();
        // make sure every field round-tripped
        assert!(params.case_insensitive);
        assert_eq!(params.groups, vec!["plants".to_owned()]);
        assert_eq!(params.start, start);
        assert_eq!(params.limit, 5);
        assert_eq!(
            params.sources,
            vec![GrepSource::ResultFiles, GrepSource::Tags]
        );
        assert_eq!(params.tools, vec!["CornHarvester".to_owned()]);
        assert_eq!(params.scan_time, Some(120));
        assert_eq!(params.scan_idle, Some(30));
        assert!(params.with_truncated);
    }

    /// Make sure an empty query string falls back to the serde defaults
    #[test]
    fn grep_query_params_defaults() {
        // deserialize an empty query string with the same config as the extractor
        let params: GrepQueryParams = serde_qs::Config::new()
            .max_depth(5)
            .deserialize_str("")
            .unwrap();
        // make sure the serde defaults were applied
        assert!(!params.case_insensitive);
        assert!(!params.multi_line);
        assert_eq!(params.groups, [] as [String; 0]);
        assert!(params.cursor.is_none());
        assert_eq!(params.limit, default_grep_limit());
        assert_eq!(params.sources, default_grep_sources());
        assert_eq!(params.tools, [] as [String; 0]);
        assert!(params.max_matches.is_none());
        assert!(params.scan_time.is_none());
        assert!(params.scan_idle.is_none());
        assert!(!params.with_truncated);
    }

    /// Make sure merging grep stats sums each bucket
    #[test]
    fn grep_stats_merge_sums_each_bucket() {
        // build a set of counts to merge into
        let mut stats = GrepStats {
            found: 1,
            truncated: 2,
            errors: 3,
        };
        // merge another set of counts into it
        stats.merge(&GrepStats {
            found: 10,
            truncated: 20,
            errors: 30,
        });
        // make sure each bucket was summed
        assert_eq!(
            stats,
            GrepStats {
                found: 11,
                truncated: 22,
                errors: 33,
            }
        );
    }

    /// Make sure grep pages deserialize with and without metadata
    ///
    /// Pages from an older API without metadata must still deserialize so a newer
    /// client can talk to an older cluster.
    #[cfg(feature = "client")]
    #[test]
    fn grep_page_metadata_is_optional() {
        // deserialize a page that carries metadata
        let page: GrepPage = serde_json::from_str(
            r#"{"data": [], "metadata": {"found": 1, "truncated": 2, "errors": 3}}"#,
        )
        .unwrap();
        // make sure the metadata round-tripped
        assert_eq!(
            page.metadata,
            Some(GrepStats {
                found: 1,
                truncated: 2,
                errors: 3,
            })
        );
        // deserialize a page from an older API without metadata
        let page: GrepPage = serde_json::from_str(r#"{"data": []}"#).unwrap();
        // make sure the missing metadata was defaulted
        assert!(page.metadata.is_none());
    }

    /// Make sure the end timestamp defaults to the given earliest timestamp
    #[cfg(feature = "api")]
    #[test]
    fn end_defaults_to_the_given_earliest() {
        // an unset end should default to the earliest timestamp we are given
        let params = GrepParams::default();
        let end = params.end(1_262_332_800).unwrap();
        assert_eq!(end, Utc.timestamp_opt(1_262_332_800, 0).unwrap());
        // an explicitly set end should always win over the default
        let explicit = Utc.with_ymd_and_hms(2024, 1, 2, 3, 4, 5).unwrap();
        let params = GrepParams {
            end: Some(explicit),
            ..GrepParams::default()
        };
        assert_eq!(params.end(1_262_332_800).unwrap(), explicit);
    }

    /// Make sure group scoping gates data by the requested groups
    #[test]
    fn groups_allow_gates_by_requested_groups() {
        // a query with no requested groups keeps the default visibility semantics
        let params = GrepParams::default();
        assert!(params.groups_allow(&vec!["corn".to_owned()]));
        assert!(params.groups_allow(&Vec::new()));
        // a query with requested groups only allows data in one of those groups
        let params = GrepParams {
            groups: vec!["corn".to_owned()],
            ..GrepParams::default()
        };
        assert!(params.groups_allow(&vec!["corn".to_owned(), "apples".to_owned()]));
        assert!(!params.groups_allow(&vec!["apples".to_owned()]));
        assert!(!params.groups_allow(&Vec::new()));
    }
}
