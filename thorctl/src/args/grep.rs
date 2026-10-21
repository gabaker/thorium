//! The args for grep searching data in Thorium with Thorctl

use clap::Parser;
use thorium::models::GrepSource;
use uuid::Uuid;

/// The sources of data a grep search can read
///
/// This mirrors [`GrepSource`] so the CLI can derive [`clap::ValueEnum`]
/// without adding CLI derives to the shared API models.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum GrepSourceArg {
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

impl From<GrepSourceArg> for GrepSource {
    /// Convert a CLI grep source to the models grep source
    ///
    /// # Arguments
    ///
    /// * `arg` - The CLI grep source to convert
    fn from(arg: GrepSourceArg) -> Self {
        // map each CLI source onto its model counterpart
        match arg {
            GrepSourceArg::Results => GrepSource::Results,
            GrepSourceArg::ResultFiles => GrepSource::ResultFiles,
            GrepSourceArg::Submissions => GrepSource::Submissions,
            GrepSourceArg::Comments => GrepSource::Comments,
            GrepSourceArg::Tags => GrepSource::Tags,
            GrepSourceArg::Metadata => GrepSource::Metadata,
        }
    }
}

/// The output formats grep matches can be printed in
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum GrepFormat {
    /// Group matches under their item key and document path like ripgrep
    Heading,
    /// Print one self contained `key:source:...:line:snippet` row per match
    Flat,
    /// Print each item's matches as one JSON object per line (NDJSON)
    Json,
}

/// A command to grep data in Thorium
#[derive(Parser, Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct Grep {
    /// The regex pattern to search for
    pub pattern: String,
    /// Specific items to grep instead of searching a whole corpus
    ///     Note: Targets may be file SHA256s, entity UUIDs, and repo URLs mixed
    ///           freely; each target's type is detected from its shape
    #[clap(verbatim_doc_comment)]
    pub targets: Vec<String>,
    /// Grep the repo corpus instead of the file corpus
    #[clap(long, conflicts_with_all = ["targets", "entities"])]
    pub repos: bool,
    /// Grep the entity corpus instead of the file corpus
    #[clap(long, conflicts_with = "targets")]
    pub entities: bool,
    /// Ignore case when matching
    #[clap(short = 'i', long)]
    pub case_insensitive: bool,
    /// Allow the pattern to match across multiple lines
    ///     Note: Multi line searches buffer whole documents into memory making
    ///           them more expensive than single line searches
    #[clap(short = 'U', long, verbatim_doc_comment)]
    pub multi_line: bool,
    /// Any groups to limit this search to
    ///     Note: If no groups are given, the search will include all groups the user is apart of
    #[clap(short, long, value_delimiter = ',', verbatim_doc_comment)]
    pub groups: Vec<String>,
    /// The sources of data to search (defaults to all sources)
    #[clap(long, value_enum, value_delimiter = ',', ignore_case = true)]
    pub sources: Vec<GrepSourceArg>,
    /// Only search results and result files from these tools
    #[clap(long, value_delimiter = ',')]
    pub tools: Vec<String>,
    /// The most recent datetime to start searching at in UTC
    #[clap(short, long, conflicts_with = "targets")]
    pub start: Option<String>,
    /// The oldest datetime to stop searching at in UTC
    #[clap(short, long, conflicts_with = "targets")]
    pub end: Option<String>,
    /// The format string to use when parsing the start/end datetimes
    ///     Example: The format of "2014-5-17T12:34:56" is "%Y-%m-%dT%H:%M:%S"
    ///     (see <https://docs.rs/chrono/latest/chrono/format/strftime>)
    #[clap(long, default_value = "%Y-%m-%dT%H:%M:%S", verbatim_doc_comment)]
    pub date_fmt: String,
    /// The cursor to continue an earlier corpus grep with
    ///     Note: The pattern and scoping options are remembered server side so
    ///           resume with the same corpus flag as the original search
    #[clap(long, conflicts_with = "targets", verbatim_doc_comment)]
    pub cursor: Option<Uuid>,
    /// Stop after at least this many items with matches have been found
    ///     Note: The limit is only checked between pages so a resumed cursor
    ///           never skips matches; the final page may push past the limit
    #[clap(
        short,
        long,
        default_value = "50",
        conflicts_with = "targets",
        verbatim_doc_comment
    )]
    pub limit: usize,
    /// Keep grepping until the entire corpus has been searched
    ///     Note: This can scan many millions of items inadvertently. Be careful!
    #[clap(long, conflicts_with_all = ["limit", "targets"], verbatim_doc_comment)]
    pub no_limit: bool,
    /// The max number of items with matches to return in one request
    #[clap(short, long, default_value = "10", conflicts_with = "targets")]
    pub page_size: usize,
    /// Stop the whole search after this many matches (clamped by the cluster)
    #[clap(long)]
    pub max_matches: Option<usize>,
    /// The max milliseconds one request may scan for (clamped by the cluster)
    #[clap(long, conflicts_with = "targets")]
    pub scan_time: Option<u64>,
    /// The max milliseconds a request keeps scanning after its last new match
    /// (clamped by the cluster)
    #[clap(long, conflicts_with = "targets")]
    pub scan_idle: Option<u64>,
    /// Also show documents that were truncated or errored without any matches
    #[clap(long)]
    pub with_truncated: bool,
    /// The format to print in: heading, flat, or json (one object per item per line)
    ///     Note: Defaults to heading on a terminal and flat when piped or when
    ///           `NO_COLOR` or `TERM=dumb` asks for plain text
    #[clap(long, value_enum, ignore_case = true, verbatim_doc_comment)]
    pub format: Option<GrepFormat>,
}
