//! Cursors for data in Thorium
use ::grep::regex::RegexMatcher;
use bb8_redis::redis::cmd;
use chrono::prelude::*;
use elasticsearch::SearchParts;
use futures::stream::{self, StreamExt, TryStreamExt};
use scylla::client::pager::TypedRowStream;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::cmp::{Ord, Ordering};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt::Debug;
use std::hash::Hash;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};
use tracing::{Level, event, instrument};
use uuid::Uuid;

use axum::http::StatusCode;

use super::elastic::{self, ElasticResponse};
use super::keys::{cursors, tags};
use crate::models::backends::GrepSupport;
use crate::models::backends::grep::{self, GrepResume, GrepScan};
use crate::models::schema::{
    Bucket, CursorLimit, ExistsStatement, GroupedCursorStatements, PagedTimeCursorStatements,
    QueryError, RowResult, SimpleCursorStatements, TimeCursorStatements,
};
use crate::models::{
    ApiCursor, CursorMetadataSupport, ElasticDoc, TagCounts, TagKeyCounts, TagListRow, TagMap, User,
};
use crate::models::{ElasticSearchParams, GrepMatches, GrepParams, GrepSource, GrepStats, TagType};
use crate::utils::{ApiError, Shared};
use crate::{
    bad, conn, deserialize, internal_err, internal_err_unwrapped, log_scylla_err, not_found, query,
    serialize,
};

/// The different kinds of cursors
pub enum CursorKind {
    /// A group based cursor in Scylla
    Scylla,
    /// A very basic non bucketed cursor in Scylla
    SimpleScylla,
    /// A group-based cursor in Scylla with no concept of time bucketing
    GroupedScylla,
    /// A tags counting cursor
    TagsCount,
    /// A cursor based on data in Elastic
    Elastic,
    /// A cursor for a grep based search
    Grep,
    /// A Tree of data in Thorium
    Tree,
    /// A cursor based on data in redis
    Redis,
}

impl CursorKind {
    /// Get our cursor kind as a str
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            CursorKind::Scylla => "Scylla",
            CursorKind::SimpleScylla => "SimpleScylla",
            CursorKind::GroupedScylla => "GroupedScylla",
            CursorKind::TagsCount => "TagsCount",
            CursorKind::Elastic => "Elastic",
            CursorKind::Grep => "Grep",
            CursorKind::Tree => "Tree",
            CursorKind::Redis => "Redis",
        }
    }
}

/// The tie type of a time cursor's statements
type TieOf<D> = <<D as ScyllaCursorSupport>::Statements as TimeCursorStatements>::Tie;

/// The tie scope type of a time cursor's statements
type ScopeOf<D> = <<D as ScyllaCursorSupport>::Statements as TimeCursorStatements>::TieScope;

/// The data to retain throughout this cursors life
#[derive(Serialize, Deserialize, Debug)]
#[serde(bound = "")]
pub struct ScyllaCursorRetain<D: ScyllaCursorSupport> {
    /// The start timestamp for this cursor
    pub start: DateTime<Utc>,
    /// The end timestamp for this cursor
    pub end: DateTime<Utc>,
    /// The tag based filters to use with this cursor
    pub extra_filter: D::ExtraFilters,
    /// The values to group the rows from this cursor by
    pub group_by: Vec<D::GroupBy>,
    /// The first unconsumed tie of each partition a page ended inside of, at the start
    /// timestamp
    pub ties: Vec<(ScopeOf<D>, TieOf<D>)>,
    /// Tags data to retain if this is a tags cursor
    pub tags_retain: Option<TagsRetain>,
    /// The values already returned by this cursor when deduping, retained so that duplicates
    /// on later pages are also deduped
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    pub dedupe_set: HashSet<String>,
}

/// Tag data to retain throughout a cursor's life if it's
/// a tags cursor
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TagsRetain {
    /// Whether this cursor should crawl the tags DB or not
    pub tags: HashMap<String, Vec<String>>,
    /// The type of tags we're crawling
    pub tag_type: TagType,
    /// The total number of tags we are searching
    pub tags_required: usize,
    /// Whether we're searching on case-insensitive tags
    pub case_insensitive: bool,
    /// Any ties in past iterations in a tag based cursor
    pub ties: HashMap<String, String>,
}

impl<D: ScyllaCursorSupport> ScyllaCursorRetain<D> {
    /// Narrow this cursor to the groups a request may list, dropping the saved ties of every
    /// other group's partitions so they are never read
    ///
    /// Returns whether any saved group is left.
    ///
    /// # Arguments
    ///
    /// * `allowed` - The groups the request may list
    fn narrow(&mut self, allowed: &HashSet<D::GroupBy>) -> bool {
        // keep only the saved groups this request may still list
        self.group_by.retain(|group| allowed.contains(group));
        // drop the ties of partitions in any other group
        self.ties
            .retain(|(scope, _)| allowed.contains(D::Statements::scope_group(scope)));
        // tag ties are kept by the group's name, so drop those of any other group too
        if let Some(tags_retain) = &mut self.tags_retain {
            let names: HashSet<String> = allowed.iter().map(ToString::to_string).collect();
            tags_retain.ties.retain(|group, _| names.contains(group));
        }
        !self.group_by.is_empty()
    }
}

/// The core logic all cursors must implement
pub trait CursorCore: Debug + Serialize + for<'a> Deserialize<'a> {
    /// The params to build this cursor form
    type Params;

    /// The extra info to filter with
    type ExtraFilters: Debug + Serialize + for<'a> Deserialize<'a> + Send + Sync;

    /// The type of data to group our rows by
    type GroupBy: std::fmt::Display
        + Debug
        + Clone
        + Hash
        + Eq
        + PartialEq
        + Serialize
        + for<'a> Deserialize<'a>
        + Send
        + Sync;

    /// Get the partition size for this cursor
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    fn partition_size(shared: &Shared) -> u16;

    /// Get our cursor id from params
    ///
    /// # Arguments
    ///
    /// * `params` - The params to use to build this cursor
    fn get_id(params: &Self::Params) -> Option<Uuid>;

    /// Get our start and end timestamps
    ///
    /// # Arguments
    ///
    /// * `params` - The params to use to build this cursor
    /// * `shared` - Shared Thorium objects
    fn get_start_end(
        params: &Self::Params,
        shared: &Shared,
    ) -> Result<(DateTime<Utc>, DateTime<Utc>), ApiError>;

    /// Get any values to group rows in this cursor by from our params
    ///
    /// # Arguments
    ///
    /// * `params` - The params to use to build this cursor
    fn get_group_by(params: &mut Self::Params) -> Vec<Self::GroupBy>;

    /// Get our extra filters from our params
    ///
    /// # Arguments
    ///
    /// * `params` - The params to use to build this cursor
    fn get_extra_filters(params: &mut Self::Params) -> Self::ExtraFilters;

    /// Get our tag filters from our params
    ///
    /// # Arguments
    ///
    /// * `params` - The params to use to build this cursor
    fn get_tag_filters(
        _params: &mut Self::Params,
    ) -> Option<(TagType, HashMap<String, Vec<String>>)> {
        None
    }

    /// Get whether the matching on tags should be case-insensitive
    ///
    /// # Arguments
    ///
    /// `params` - The params to use to build this cursor
    fn get_tags_case_insensitive(_params: &Self::Params) -> bool {
        // return false by default
        false
    }

    /// Get the max number of rows to return, checked against the most a listing allows
    ///
    /// # Arguments
    ///
    /// * `params` - The params to use to build this cursor
    fn get_limit(params: &Self::Params) -> Result<CursorLimit, ApiError>;

    /// Determines if a new item is a duplicate or not
    ///
    /// # Arguments
    ///
    /// * `set` - The current set of deduped data
    fn dedupe_item(&self, dedupe_set: &mut HashSet<String>) -> bool;

    /// Get the tag clustering key for a row without the timestamp, as the tags table stores it
    fn get_tag_clustering_key(&self) -> String {
        unimplemented!("This type does not support tags");
    }
}

/// Abstracts cursor logic across different tables and data types
pub trait ScyllaCursorSupport: CursorCore {
    /// The intermediate list row to use
    type IntermediateRow: Into<Self>
        + for<'frame, 'metadata> scylla::deserialize::row::DeserializeRow<'frame, 'metadata>;

    /// The statements this cursor pages with
    type Statements: TimeCursorStatements<
            Extra = Self::ExtraFilters,
            Group = Self::GroupBy,
            Row = Self::IntermediateRow,
        >;

    /// The unique key for this cursors row
    type UniqueType<'a>: Debug + PartialEq;

    /// Get the statements this cursor pages with
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    fn statements(shared: &Shared) -> &Self::Statements;

    /// Add an item to our tag tie breaker map, keeping any tie a group already has
    ///
    /// # Arguments
    ///
    /// * `ties` - Our current ties
    fn add_tag_tie(&self, _ties: &mut HashMap<String, String>) {
        unimplemented!("This type does not support tags");
    }

    /// Get the timestamp for this item
    fn get_timestamp(&self) -> DateTime<Utc>;

    /// Get the unique key for this intermediate row
    ///
    /// # Arguments
    ///
    /// * `intermediate` - The intermediate row to get a unique key for
    fn get_intermediate_unique_key<'a>(
        intermediate: &'a Self::IntermediateRow,
    ) -> Self::UniqueType<'a>;

    /// Get the unique key for this row
    fn get_unique_key<'a>(&'a self) -> Self::UniqueType<'a>;

    /// Add a group to a specific returned line
    fn add_group_to_line(&mut self, _group: String) {
        unimplemented!("This type does not support tags");
    }

    /// Add a group to a specific returned line
    ///
    /// # Arguments
    ///
    /// * `intermediate` - The intermediate row to get a unique key for
    fn add_intermediate_to_line(&mut self, intermediate: Self::IntermediateRow);

    /// Convert a tag list row into our list line
    ///
    /// # Arguments
    ///
    /// * `row` - The tag row to convert
    fn from_tag_row(_row: TagListRow) -> Self {
        unimplemented!("Doesn't support tags");
    }

    /// Build all of the keys needs to retrieve census data
    ///
    /// # Arguments
    ///
    /// * `groups` - The values to group our rows by
    /// * `extra` - The extra values required to list this data
    /// * `year` - The year we are looking for census data for
    /// * `bucket` - The newest bucket to look for census data from
    /// * `keys` - The vec to add our census stream keys too
    /// * `shared` - Shared Thorium objects
    fn census_keys<'a>(
        groups: &'a [Self::GroupBy],
        extra: &Self::ExtraFilters,
        year: i32,
        bucket: Bucket,
        keys: &mut Vec<(&'a Self::GroupBy, String, Bucket)>,
        shared: &Shared,
    );

    /// Sort the tied rows of a tags cursor by the clustering key the tags table stores them by
    ///
    /// Only tags cursors call this, since `get_tag_clustering_key` is unimplemented for types
    /// that don't support tags.
    ///
    /// # Arguments
    ///
    /// * `tied` - The lines sharing one timestamp
    fn sort_tied_rows_by_tag_clustering_key(tied: &mut VecDeque<SortedLine<Self>>)
    where
        Self: Sized,
    {
        // sort the lines by the clustering key the tags table stores them by
        tied.make_contiguous()
            .sort_by_cached_key(|sorted| sorted.line.get_tag_clustering_key());
    }

    /// Convert tag rows into a sorted Vec of self for tag based queries
    ///
    /// # Arguments
    ///
    /// * `mapping` - The mapping of items and their tag data to sort
    /// * `tags` - Whether this a tags cursor not not
    /// * `sorted` - The map to store sorted data in
    /// * `mapped_count` - The currently available number of rows we have sorted
    fn sort_tags(
        mapping: &mut HashMap<String, TagMapping>,
        tags: &HashMap<String, Vec<String>>,
        sorted: &mut BTreeMap<DateTime<Utc>, VecDeque<SortedLine<Self>>>,
        mapped_count: &mut usize,
    ) -> Result<(), ApiError>
    where
        Self: Sized,
    {
        // count the number of key/value pairs we are trying to match
        let required = tags.values().map(|values| values.len()).sum::<usize>();
        // go through this list in order
        for (_, mapped) in mapping.drain() {
            // skip any mappings where we don't have the required number of tags
            if mapped.tags.len() == required {
                // turn our this items casts into a stream
                let mut row_stream = mapped.rows.into_iter();
                // build the sample list line from the first row
                let mut line = Self::from_tag_row(row_stream.next().unwrap());
                // add all the rest of our row to this line
                for row in row_stream {
                    line.add_group_to_line(row.group);
                }
                // get this lines timestamp
                let timestamp = line.get_timestamp();
                // get an entry to the list for this timestamp
                let entry = sorted.entry(timestamp).or_default();
                // add our new line, whose ties the tags cursor tracks itself
                entry.push_back(SortedLine {
                    line,
                    tie: None,
                    scopes: Vec::new(),
                });
                // increment our mapped count
                *mapped_count += 1;
            }
        }
        Ok(())
    }
}

/// A line a time cursor read but hasn't returned yet, with the tie and partitions of its rows
#[derive(Debug)]
pub struct SortedLine<D: ScyllaCursorSupport> {
    /// The line
    line: D,
    /// The tie its rows share, or `None` for a line built from tag rows
    tie: Option<TieOf<D>>,
    /// The partitions its rows were read from
    scopes: Vec<ScopeOf<D>>,
}

impl<D: ScyllaCursorSupport> SortedLine<D> {
    /// The first of this line's partitions in key order, which orders lines sharing a tie
    fn first_scope(&self) -> Option<&ScopeOf<D>> {
        self.scopes
            .iter()
            .min_by(|left, right| D::Statements::cmp_scopes(left, right))
    }

    /// Compare two lines sharing a timestamp in the order a page consumes them: by their tie
    /// in the order the tie queries read it, then by their first partition
    ///
    /// # Arguments
    ///
    /// * `other` - The line to compare against
    fn cmp_consumed(&self, other: &Self) -> Ordering {
        // lines built from tag rows have no tie and are ordered by the tags cursor
        let ties = match (&self.tie, &other.tie) {
            (Some(mine), Some(theirs)) => D::Statements::cmp_ties(mine, theirs),
            _ => Ordering::Equal,
        };
        // lines sharing a tie, such as a branch and a tag of one name, go by partition
        ties.then_with(|| match (self.first_scope(), other.first_scope()) {
            (Some(mine), Some(theirs)) => D::Statements::cmp_scopes(mine, theirs),
            _ => Ordering::Equal,
        })
    }
}

/// A row containing a count of objects within a partition
pub struct CursorCountRow {
    /// The group this row is for
    pub group: Option<String>,
    /// The year this count is from
    pub year: Option<i32>,
    /// The bucket this count is from
    pub bucket: Option<i32>,
    /// The number of objects in this count
    pub count: i64,
}

/// A count of objects from a group for a specific year/bucket combo
#[derive(Debug)]
pub struct CursorCount {
    /// The year this count is from
    pub year: i32,
    /// The bucket this count is from
    pub bucket: i32,
    /// The number of objects in this count
    pub count: i64,
}

/// The rows of one tagged item and the tag keys and values they matched
#[derive(Default, Debug)]
pub struct TagMapping {
    /// They key/values this data has
    tags: HashSet<(String, String)>,
    /// The rows for this tag
    rows: Vec<TagListRow>,
}

impl TagMapping {
    /// Add a row to this mapping
    ///
    /// # Arguments
    ///
    /// * `key` - The key to add
    /// * `value` - The value to add
    /// * `row` - The row to add
    pub fn add(&mut self, key: &str, value: &str, row: TagListRow) {
        // add our new key/value
        self.tags.insert((key.to_string(), value.to_string()));
        // add this row
        self.rows.push(row);
    }
}

/// Read every row of some tag queries into a mapping from each item to its rows
///
/// # Arguments
///
/// * `queries` - Each query's key, value, and rows
/// * `mapping` - The mapping to add the rows to
async fn parse_tag_queries<'a>(
    queries: Vec<(&'a str, &'a str, TypedRowStream<TagListRow>)>,
    mapping: &mut HashMap<String, TagMapping>,
) -> Result<(), ApiError> {
    // crawl over our queries and deserialize them
    for (key, value, mut typed_stream) in queries {
        // cast our rows to typed values
        while let Some(row) = typed_stream.next().await {
            // check if we failed to deserialize this row
            let cast = row?;
            // get an entry to this rows info in our tag mapping map
            let entry: &mut TagMapping = mapping.entry(cast.item.clone()).or_default();
            // add this tag row to our mapping
            entry.add(key, value, cast);
        }
    }
    Ok(())
}

/// Run some cursor statements 50 at a time, keeping the results of all of them
///
/// # Arguments
///
/// * `futures` - The statements to run
async fn run_all<T, F>(futures: Vec<F>) -> Result<Vec<T>, ApiError>
where
    F: std::future::Future<Output = Result<T, QueryError>>,
{
    // run the statements 50 at a time and raise the first error
    let results = stream::iter(futures)
        .buffer_unordered(50)
        .collect::<Vec<Result<T, QueryError>>>()
        .await
        .into_iter()
        .collect::<Result<Vec<T>, QueryError>>()?;
    Ok(results)
}

/// The statements of a tag listing, which page through the tags table or its case-insensitive
/// view
trait TagCursorStatements:
    PagedTimeCursorStatements<
        Extra = (TagType, String, String),
        Group = String,
        Row = TagListRow,
        Tie = String,
    >
{
}

impl<S> TagCursorStatements for S where
    S: PagedTimeCursorStatements<
            Extra = (TagType, String, String),
            Group = String,
            Row = TagListRow,
            Tie = String,
        >
{
}

/// A cursor for a listing group permisisoned data within scylla
#[derive(Debug)]
pub struct ScyllaCursor<D>
where
    for<'de> D: Deserialize<'de> + Debug,
    D: Serialize,
    D: ScyllaCursorSupport,
    D: Debug,
{
    /// The Id for this cursor
    pub id: Uuid,
    /// The username of the user this cursor is scoped to
    pub username: String,
    /// The cursor settings/data to retain across cursor iterations
    pub retain: ScyllaCursorRetain<D>,
    /// The year that we last got data from
    pub year: i32,
    /// The bucket that we last got data from
    pub bucket: Bucket,
    /// The final year we are going to get data from
    pub end_year: i32,
    /// The final bucket we will get data from
    pub end_bucket: Bucket,
    /// The partition size to use
    partition_size: u16,
    /// The max number of items to return at once
    pub limit: CursorLimit,
    /// Whether to dedupe this data or not
    pub dedupe: bool,
    /// The data this cursor has retrieved
    pub data: Vec<D>,
    /// The internal sorted data to return to the user
    sorted: BTreeMap<DateTime<Utc>, VecDeque<SortedLine<D>>>,
    /// The currently available number of rows we have sorted
    pub mapped: usize,
    /// whether this cursor has been exhausted or not
    pub buckets_exhausted: bool,
    /// Whether this is a new cursor or it already existed and is being continued
    pub new: bool,
}

/// lowercase our tags if needed
fn maybe_lower_tags<D: ScyllaCursorSupport>(
    params: &D::Params,
    tags: HashMap<String, Vec<String>>,
) -> (bool, HashMap<String, Vec<String>>) {
    // check if this is a case insensitive filter
    if D::get_tags_case_insensitive(params) {
        // this is case insensitive so shift all lowercasable chars to their lowercase variants
        // pre allocate a map for our lowercased tags
        let mut lowered: HashMap<String, Vec<String>> = HashMap::with_capacity(tags.len());
        // step over each tag key and its values and lowercase them if possible
        for (key, values) in &tags {
            // lowercase our key
            let lower_key = key.to_lowercase();
            // get an entry to this newly lowercased tag key
            let entry = lowered.entry(lower_key).or_default();
            // build an iter of lowercased values
            let lower_values = values.iter().map(|value| value.to_lowercase());
            // extend this lowercased key with its lowercased values
            entry.extend(lower_values);
        }
        // return our lowercased tags
        (true, lowered)
    } else {
        // this is not case insensitive so just return our current tags
        (false, tags)
    }
}

/// Build the filters of each tag key and value a tag listing matches
///
/// # Arguments
///
/// * `tags_retain` - Tag data retained for tags cursors
fn tag_filters(tags_retain: &TagsRetain) -> Vec<(&str, &str, (TagType, String, String))> {
    tags_retain
        .tags
        .iter()
        .flat_map(|(key, values)| {
            values.iter().map(move |value| {
                (
                    key.as_str(),
                    value.as_str(),
                    (tags_retain.tag_type, key.clone(), value.clone()),
                )
            })
        })
        .collect()
}

impl<D> ScyllaCursor<D>
where
    for<'de> D: Deserialize<'de> + Debug + std::marker::Send,
    D: Serialize,
    D: ScyllaCursorSupport,
    D: Debug,
{
    /// Get the retain info for tags if this is a tags cursor
    ///
    /// # Arguments
    ///
    /// * `params` - The params to build this cursor from
    fn get_tags_retain(params: &mut D::Params) -> Option<TagsRetain> {
        // get our tag filters
        if let Some((tag_type, tags)) = D::get_tag_filters(params) {
            // lowercase our tags if necesary
            let (case_insensitive, tags) = maybe_lower_tags::<D>(params, tags);
            // calculate the total number of tags required for this query
            let tags_required = tags.values().map(Vec::len).sum();
            Some(TagsRetain {
                tags,
                tag_type,
                tags_required,
                case_insensitive,
                ties: HashMap::new(),
            })
        } else {
            None
        }
    }

    /// Build a cursor over the window between its start and end timestamps
    ///
    /// # Arguments
    ///
    /// * `id` - The cursor's id
    /// * `username` - The username of the user this cursor is scoped to
    /// * `retain` - The data the cursor keeps between pages
    /// * `limit` - The most rows a page returns
    /// * `dedupe` - Whether to dedupe items or not
    /// * `new` - Whether this cursor is new rather than continued
    /// * `shared` - Shared Thorium objects
    fn build(
        id: Uuid,
        username: &str,
        retain: ScyllaCursorRetain<D>,
        limit: CursorLimit,
        dedupe: bool,
        new: bool,
        shared: &Shared,
    ) -> Result<Self, ApiError> {
        // tag based cursors have a different partition size
        let partition_size = match &retain.tags_retain {
            Some(tags_retain) => {
                shared
                    .config
                    .thorium
                    .tags
                    .map_type(&tags_retain.tag_type)
                    .partition_size
            }
            None => D::partition_size(shared),
        };
        // get the years and buckets this cursor starts and ends in
        let year = retain.start.year();
        let end_year = retain.end.year();
        let bucket = Bucket::of(retain.start, year, partition_size)?;
        let end_bucket = Bucket::of(retain.end, end_year, partition_size)?;
        Ok(ScyllaCursor {
            id,
            username: username.to_owned(),
            retain,
            year,
            bucket,
            end_year,
            end_bucket,
            partition_size,
            limit,
            dedupe,
            data: Vec::default(),
            sorted: BTreeMap::default(),
            mapped: 0,
            buckets_exhausted: false,
            new,
        })
    }

    /// Create a new cursor from params, with its extra filters if they're given separately
    ///
    /// # Arguments
    ///
    /// * `params` - The params to build this cursor from
    /// * `extra_filter` - The extra filters to use, or `None` to take them from the params
    /// * `username` - The username of the user this cursor is scoped to
    /// * `dedupe` - Whether to dedupe items or not
    /// * `shared` - Shared Thorium objects
    fn create(
        mut params: D::Params,
        extra_filter: Option<D::ExtraFilters>,
        username: &str,
        dedupe: bool,
        shared: &Shared,
    ) -> Result<Self, ApiError> {
        // get the start/end timestamps from our cursor
        let (start, end) = D::get_start_end(&params, shared)?;
        // make sure that start is not before end
        if start < end {
            return bad!("You cannot start before you end!".to_owned());
        }
        // get the retain info for tags if this a tag based cursor
        let tags_retain = Self::get_tags_retain(&mut params);
        // get our extra filters if they weren't given
        let extra_filter = match extra_filter {
            Some(extra_filter) => extra_filter,
            None => D::get_extra_filters(&mut params),
        };
        // get our group restrictions
        let group_by = D::get_group_by(&mut params);
        // build the inital data to retain across cursor iterations
        let retain = ScyllaCursorRetain {
            start,
            end,
            extra_filter,
            group_by,
            ties: Vec::new(),
            tags_retain,
            dedupe_set: HashSet::default(),
        };
        let limit = D::get_limit(&params)?;
        Self::build(
            Uuid::new_v4(),
            username,
            retain,
            limit,
            dedupe,
            true,
            shared,
        )
    }

    /// Create a new cursor object from just params
    ///
    /// A limit outside the range a listing allows is refused.
    ///
    /// # Arguments
    ///
    /// * `params` - The params to build this cursor from
    /// * `username` - The username of the user this cursor is scoped to
    /// * `dedupe` - Whether to dedupe items or not
    /// * `shared` - Shared Thorium objects
    #[instrument(
        name = "ScyllaCursor::from_params",
        skip(params, dedupe, shared),
        err(Debug)
    )]
    pub async fn from_params(
        params: D::Params,
        username: &str,
        dedupe: bool,
        shared: &Shared,
    ) -> Result<ScyllaCursor<D>, ApiError> {
        // if we have a cursor id then try to get our cursor from the DB
        match D::get_id(&params) {
            Some(id) => ScyllaCursor::get(id, username, params, dedupe, shared).await,
            None => Self::create(params, None, username, dedupe, shared),
        }
    }

    /// Create a new cursor object from params with a set extra fields
    ///
    /// # Arguments
    ///
    /// * `params` - The params to build this cursor from
    /// * `extra_filter` - The extra filters to use in this cursor
    /// * `username` - The username of the user this cursor is scoped to
    /// * `dedupe` - Whether to dedupe items or not
    /// * `shared` - Shared Thorium objects
    #[instrument(
        name = "ScyllaCursor::from_params_extra",
        skip(params, extra_filter, shared),
        err(Debug)
    )]
    pub async fn from_params_extra(
        params: D::Params,
        extra_filter: D::ExtraFilters,
        username: &str,
        dedupe: bool,
        shared: &Shared,
    ) -> Result<ScyllaCursor<D>, ApiError> {
        // if we have a cursor id then try to get our cursor from the DB
        match D::get_id(&params) {
            Some(id) => ScyllaCursor::get(id, username, params, dedupe, shared).await,
            None => Self::create(params, Some(extra_filter), username, dedupe, shared),
        }
    }

    /// Rebuild a cursor from retained data without touching redis
    ///
    /// This is for cursors that embed another cursors retained state, like grep
    /// cursors, so the embedded cursor can be rebuilt and persisted as part of its
    /// owner instead of under its own redis key.
    ///
    /// The rebuilt cursor resumes over only the saved groups the params may still list,
    /// as continuing a saved cursor does, and is not found when none are left. A limit
    /// outside the range a listing allows is refused.
    ///
    /// # Arguments
    ///
    /// * `retain` - The retained data to rebuild this cursor from
    /// * `params` - The params authorized for this page, giving its groups and limit
    /// * `username` - The username of the user this cursor is scoped to
    /// * `dedupe` - Whether to dedupe items or not
    /// * `shared` - Shared Thorium objects
    pub fn from_retain(
        mut retain: ScyllaCursorRetain<D>,
        mut params: D::Params,
        username: &str,
        dedupe: bool,
        shared: &Shared,
    ) -> Result<ScyllaCursor<D>, ApiError> {
        // get the groups this page was authorized to list, which a scoped token, a deleted
        // group, or a user's removal from a group can make narrower than the saved groups
        let allowed: HashSet<D::GroupBy> = D::get_group_by(&mut params).into_iter().collect();
        // resume over the saved groups still allowed, and answer as for an unknown cursor
        // when none are left
        if !retain.narrow(&allowed) {
            return not_found!("Cursor doesn't exist".to_owned());
        }
        // check the limit the same way the list routes do
        let limit = D::get_limit(&params)?;
        // rebuild our cursor with an id that is inert since it is never saved
        Self::build(
            Uuid::new_v4(),
            username,
            retain,
            limit,
            dedupe,
            false,
            shared,
        )
    }

    /// Gets a cursors data from Redis
    ///
    /// # Arguments
    ///
    /// * `cursor_id` - The uuid of the cursor to retrieve if one is known
    /// * `username` - The username of the user this cursor is scoped to
    /// * `params` - The params to build this cursor from
    /// * `dedupe` - Whether to dedupe items or not
    /// * `shared` - Shared thorium objects
    #[instrument(name = "ScyllaCursor::get", skip(params, shared), err(Debug))]
    pub async fn get(
        cursor_id: Uuid,
        username: &str,
        mut params: D::Params,
        dedupe: bool,
        shared: &Shared,
    ) -> Result<ScyllaCursor<D>, ApiError> {
        // build the key to our cursor data in redis scoped to the creating user
        let key = cursors::data(CursorKind::Scylla, username, &cursor_id, shared);
        // get our cursor from redis
        let data: Option<String> = query!(cmd("get").arg(key), shared).await?;
        // check if we got any cursor data
        match data {
            Some(data) => {
                // try to deseruialize this cursors retained data
                let mut retain: ScyllaCursorRetain<D> = deserialize!(&data);
                // get the groups this request was authorized to list, which a scoped token, a
                // deleted group, or a user's removal from a group can make narrower than the
                // groups saved with it
                let allowed: HashSet<D::GroupBy> =
                    D::get_group_by(&mut params).into_iter().collect();
                // resume over the saved groups still allowed, and answer as for an unknown id
                // when none are left
                if !retain.narrow(&allowed) {
                    return not_found!(format!("Cursor {} doesn't exist", cursor_id));
                }
                // rebuild our cursor
                let limit = D::get_limit(&params)?;
                Self::build(cursor_id, username, retain, limit, dedupe, false, shared)
            }
            None => not_found!(format!("Cursor {} doesn't exist", cursor_id)),
        }
    }

    /// Cast the rows of some statements and sort them by timestamp, merging rows of the same
    /// item into one line
    ///
    /// # Arguments
    ///
    /// * `results` - The rows each statement read
    fn sort(&mut self, results: Vec<Vec<RowResult<D::IntermediateRow>>>) -> Result<(), ApiError> {
        // crawl over each statement's rows
        for row in results.into_iter().flatten() {
            // raise any errors from casting
            let cast = row?;
            // get the timestamp, tie, and partition of this row
            let timestamp = D::Statements::row_time(&cast);
            let tie = D::Statements::row_tie(&cast);
            let scope = D::Statements::row_scope(&cast);
            // get an entry to the list for this timestamp
            let entry = self.sorted.entry(timestamp).or_default();
            // check if this item has already been added to this list
            let inter_unique = D::get_intermediate_unique_key(&cast);
            let pos = entry
                .iter()
                .position(|item| inter_unique == item.line.get_unique_key());
            // drop our reference to our interemdiate unique value
            drop(inter_unique);
            match pos {
                // add this row to the line it belongs to, noting its partition
                Some(pos) => {
                    let sorted = &mut entry[pos];
                    sorted.line.add_intermediate_to_line(cast);
                    if !sorted.scopes.contains(&scope) {
                        sorted.scopes.push(scope);
                    }
                }
                // cast our row into a new line
                None => {
                    entry.push_back(SortedLine {
                        line: cast.into(),
                        tie: Some(tie),
                        scopes: vec![scope],
                    });
                    // increment our mapped count
                    self.mapped += 1;
                }
            }
        }
        Ok(())
    }

    /// Check if we have any ties queries
    ///
    /// # Arguments
    ///
    /// * `limit` - The max number of rows to return to the user plus 1
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "ScyllaCursor::query_ties", skip(self, shared), err(Debug))]
    async fn query_ties(
        &mut self,
        limit: CursorLimit,
        shared: &Shared,
    ) -> Result<Vec<Vec<RowResult<D::IntermediateRow>>>, ApiError> {
        // these ties are read now, so only ties left over from this page remain
        let ties = std::mem::take(&mut self.retain.ties);
        let statements = D::statements(shared);
        // read each tied partition from its tie onward at our start timestamp
        let futures = ties
            .iter()
            .map(|(scope, tie)| {
                statements.ties(
                    &shared.scylla.session,
                    &self.retain.extra_filter,
                    scope,
                    self.year,
                    self.bucket,
                    self.retain.start,
                    tie,
                    limit,
                )
            })
            .collect::<Vec<_>>();
        run_all(futures).await
    }

    /// Find the buckets that have been confirmed to contain data
    ///
    /// # Arguments
    ///
    /// * `bucket_limit` - The most buckets to return for each census stream
    /// * `stream_keys` - the stream keys to look for data in
    /// * `found` - The buckets that we found that contain data
    /// * `shared` - Shared Thorium objects
    async fn find_buckets<'a>(
        &self,
        bucket_limit: u32,
        stream_keys: &mut Vec<(&'a D::GroupBy, String, Bucket)>,
        found: &mut HashMap<&'a D::GroupBy, Vec<Bucket>>,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // stop at our end bucket in our end year, or the first bucket of other years
        let end = if self.year == self.end_year {
            self.end_bucket
        } else {
            Bucket::new(0)
        };
        // build a redis pipeline to get the newest buckets with data from each stream
        let mut pipe = redis::pipe();
        for (_, stream_key, newest) in stream_keys.iter() {
            pipe.cmd("zrange")
                .arg(stream_key)
                .arg(newest.get())
                .arg(end.get())
                .arg("byscore")
                .arg("limit")
                .arg(0)
                .arg(bucket_limit)
                .arg("rev");
        }
        // execute our queries
        let bucket_strings: Vec<Vec<String>> = pipe.query_async(conn!(shared)).await?;
        // parse each stream's buckets, refusing any that isn't a bucket number
        let parsed = bucket_strings
            .iter()
            .map(|strings| {
                strings
                    .iter()
                    .map(|val| Bucket::from_stored(val))
                    .collect::<Result<Vec<Bucket>, _>>()
            })
            .collect::<Result<Vec<Vec<Bucket>>, _>>()?;
        // a stream that returned its limit may have unread buckets just below its oldest one,
        // so no stream reads below the newest such bucket until those buckets are read; a page
        // then fills from the newest rows of every group
        let floor = parsed
            .iter()
            .filter(|buckets| buckets.len() >= bucket_limit as usize)
            .filter_map(|buckets| buckets.last().copied())
            .max();
        // keep only the streams that still have buckets to read
        let streams = std::mem::take(stream_keys);
        for (mut buckets, (group, stream_key, _)) in parsed.into_iter().zip(streams) {
            // skip any stream that has no buckets left
            let Some(oldest) = buckets.last().copied() else {
                continue;
            };
            // continue below the oldest bucket read now, unless it's the year's first bucket
            let read_to = floor.map_or(oldest, |floor| oldest.max(floor));
            if let Some(next) = read_to.prev() {
                stream_keys.push((group, stream_key, next));
            }
            // leave the buckets below the floor for a later read
            buckets.retain(|bucket| *bucket >= read_to);
            // add these buckets to those of any other census stream of this group
            if !buckets.is_empty() {
                found.entry(group).or_default().extend(buckets);
            }
        }
        Ok(())
    }

    /// Crawl partitions and pull data from them
    ///
    /// # Arguments
    ///
    /// * `limit` - The max number of rows to return to the user plus 1
    /// * `bucket_limit` - The most buckets one pull reads
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "ScyllaCursor::query", skip(self, shared), err(Debug))]
    async fn query(
        &mut self,
        limit: CursorLimit,
        bucket_limit: u32,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // take our groups so the census keys can borrow them while we update our position
        let group_by = std::mem::take(&mut self.retain.group_by);
        let result = self
            .query_groups(&group_by, limit, bucket_limit, shared)
            .await;
        // put our groups back
        self.retain.group_by = group_by;
        result
    }

    /// Crawl the partitions of some groups and pull data from them
    ///
    /// # Arguments
    ///
    /// * `group_by` - The groups to crawl
    /// * `limit` - The max number of rows to return to the user plus 1
    /// * `bucket_limit` - The most buckets one pull reads
    /// * `shared` - Shared Thorium objects
    async fn query_groups(
        &mut self,
        group_by: &[D::GroupBy],
        limit: CursorLimit,
        bucket_limit: u32,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        let statements = D::statements(shared);
        // build the keys for each census stream we are going to crawl this year
        let mut stream_keys = Vec::with_capacity(group_by.len());
        D::census_keys(
            group_by,
            &self.retain.extra_filter,
            self.year,
            self.bucket,
            &mut stream_keys,
            shared,
        );
        // keep a map of the buckets we have found by group
        let mut found = HashMap::with_capacity(group_by.len());
        // loop until we have enough data to return
        loop {
            // get the next buckets that contain data
            self.find_buckets(bucket_limit, &mut stream_keys, &mut found, shared)
                .await?;
            // if we found no buckets then check if we have exhausted this cursor
            if found.is_empty() {
                // if we are in the final year then this cursor is exhausted
                if self.year <= self.end_year {
                    self.buckets_exhausted = true;
                    return Ok(());
                }
                // there is no more data in this year, so move to the end of the year before
                self.year = self.year.saturating_sub(1);
                self.bucket = Bucket::last_of_year(self.year, self.partition_size)?;
                stream_keys.clear();
                D::census_keys(
                    group_by,
                    &self.retain.extra_filter,
                    self.year,
                    self.bucket,
                    &mut stream_keys,
                    shared,
                );
                continue;
            }
            // a group's census streams can share buckets, so pull each bucket once
            let windows = found
                .drain()
                .map(|(group, mut buckets)| {
                    buckets.sort_unstable_by(|left, right| right.cmp(left));
                    buckets.dedup();
                    (group, buckets)
                })
                .collect::<Vec<_>>();
            // pull no more buckets at once than keeps the statement within its limits
            let mut futures = Vec::with_capacity(windows.len());
            for (group, buckets) in &windows {
                for chunk in buckets.chunks(bucket_limit.max(1) as usize) {
                    futures.push(statements.pull(
                        &shared.scylla.session,
                        &self.retain.extra_filter,
                        group,
                        self.year,
                        chunk,
                        self.retain.start,
                        self.retain.end,
                        limit,
                    ));
                }
            }
            let results = run_all(futures).await?;
            // cast and sort the rows we just retrieved
            self.sort(results)?;
            // if we found enough data for this page of our cursor then break out
            if self.mapped >= self.limit.get() || self.exhausted_time() {
                break;
            }
        }
        Ok(())
    }

    /// Filter down ordered lists of buckets to their intersecting ranges and the bucket to
    /// continue from
    ///
    /// Returns the buckets every tag has data in, and the bucket below the newest of the
    /// oldest buckets each stream found, or `None` when the year's first bucket was reached.
    ///
    /// # Arguments
    ///
    /// * `group_by` - The groups being listed
    /// * `tags` - The tags to find bucket intersections for
    /// * `tags_required` - The number of tags required
    /// * `pre_filter` - The unfiltered buckets for our tags
    /// * `possible` - Whether an intersection is still possible
    #[instrument(name = "ScyllaCursor::filter_bucket_intersection", skip_all)]
    fn filter_bucket_intersection(
        group_by: &[D::GroupBy],
        tags: &HashMap<String, Vec<String>>,
        tags_required: usize,
        pre_filter: Vec<Vec<Bucket>>,
        possible: &mut bool,
    ) -> (Vec<Bucket>, Option<Bucket>) {
        // get the first and last bucket that we know contains data
        let mut first = Bucket::new(0);
        let mut last = Bucket::new(0);
        // build a map of our buckets by tag key/value pair
        let mut map: HashMap<(&String, &String), BTreeSet<Bucket>> =
            HashMap::with_capacity(tags_required);
        // step over our bucket data in the same order we retrieved it
        let mut bucket_stream = pre_filter.into_iter();
        for (key, values) in tags {
            // get all the buckets that contain data for each value
            for value in values {
                // check for census info for each group
                for _ in group_by {
                    // get this values buckets
                    let buckets = bucket_stream.next().unwrap_or_default();
                    // skip any empty bucket lists
                    if let (Some(local_first), Some(local_last)) = (buckets.first(), buckets.last())
                    {
                        // move the first and last buckets with data up
                        first = first.max(*local_first);
                        last = last.max(*local_last);
                        // add the buckets for this group
                        map.entry((key, value)).or_default().extend(buckets);
                    }
                }
            }
        }
        // continue below the newest of the oldest buckets each stream found
        let next_bucket = last.prev();
        // check if we didn't find data for every tag key/value pair
        if map.len() < tags_required {
            // it's no longer possible to have an intersection this year
            *possible = false;
            return (Vec::new(), next_bucket);
        }
        // count how many key/value pairs have data in each bucket of the overlapping range
        let mut in_range: HashMap<Bucket, usize> = HashMap::with_capacity(1000);
        for buckets in map.values() {
            for bucket in buckets {
                if *bucket <= first && *bucket >= last {
                    *in_range.entry(*bucket).or_default() += 1;
                }
            }
        }
        // keep the buckets every key/value pair has data in
        let intersection = in_range
            .into_iter()
            .filter(|(_, count)| *count == tags_required)
            .map(|(bucket, _)| bucket)
            .collect();
        (intersection, next_bucket)
    }

    /// Finds the next chunk of buckets that contain data for some tags
    ///
    /// # Arguments
    ///
    /// * `group_by` - The groups being listed
    /// * `tags_retain` - Tag data retained for tags cursors
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "ScyllaCursor::tags_find_buckets", skip_all, err(Debug))]
    async fn tags_find_buckets(
        &self,
        group_by: &[D::GroupBy],
        tags_retain: &TagsRetain,
        shared: &Shared,
    ) -> Result<Vec<Bucket>, ApiError> {
        // stop at our end bucket in our end year, or the first bucket of other years
        let end = if self.year == self.end_year {
            self.end_bucket
        } else {
            Bucket::new(0)
        };
        // track the bucket to read from next
        let mut next_bucket = self.bucket;
        // track whether an intersection is possible this year
        let mut possible = true;
        // decide on which census stream function to call based on if we're case insensitive
        let census_stream_fn = if tags_retain.case_insensitive {
            tags::census_stream_case_insensitive
        } else {
            tags::census_stream
        };
        // build a census stream key for each tag key, value, and group
        let mut stream_keys = Vec::with_capacity(10);
        for (key, values) in &tags_retain.tags {
            for value in values {
                for group in group_by {
                    let stream_key = census_stream_fn(
                        tags_retain.tag_type,
                        group,
                        key,
                        value,
                        self.year,
                        shared,
                    );
                    stream_keys.push(stream_key);
                }
            }
        }
        // loop until we have exhausted this years buckets
        loop {
            // build a redis pipeline to get all of the valid buckets for these tags
            let mut pipe = redis::pipe();
            for stream_key in &stream_keys {
                pipe.cmd("zrange")
                    .arg(stream_key)
                    .arg(next_bucket.get())
                    .arg(end.get())
                    .arg("byscore")
                    .arg("limit")
                    .arg(0)
                    .arg(100)
                    .arg("rev");
            }
            // execute our queries
            let bucket_strings: Vec<Vec<String>> = pipe.query_async(conn!(shared)).await?;
            // parse each stream's buckets, refusing any that isn't a bucket number
            let pre_intersection = bucket_strings
                .iter()
                .map(|buckets| {
                    buckets
                        .iter()
                        .map(|val| Bucket::from_stored(val))
                        .collect::<Result<Vec<Bucket>, _>>()
                })
                .collect::<Result<Vec<Vec<Bucket>>, _>>()?;
            // get the intersection of all buckets that all tags are in
            let (mut intersection, next) = Self::filter_bucket_intersection(
                group_by,
                &tags_retain.tags,
                tags_retain.tags_required,
                pre_intersection,
                &mut possible,
            );
            // return any intersecting buckets, or none once no intersection is left this year
            if !intersection.is_empty() || !possible {
                intersection.sort_unstable();
                return Ok(intersection);
            }
            // continue below the buckets we just checked, if any are left this year
            match next {
                Some(next) => next_bucket = next,
                None => return Ok(Vec::new()),
            }
        }
    }

    /// Check if this cursor has been exhausted
    pub fn exhausted(&self) -> bool {
        // check if we have any more mapped values and our buckets are exhausted
        self.buckets_exhausted && self.mapped == 0
    }

    /// Check if this cursor mapped the full bounds of its data
    fn exhausted_time(&self) -> bool {
        // check if our year/bucket are at or past our end year/bucket
        self.year <= self.end_year && self.bucket <= self.end_bucket
    }

    /// Record the first unconsumed tie of every partition a line left at the page boundary
    /// was read from
    ///
    /// # Arguments
    ///
    /// * `remaining` - The lines left at the page boundary, in consumption order
    fn record_ties(&mut self, remaining: &VecDeque<SortedLine<D>>) {
        for sorted in remaining {
            // lines built from tag rows are tracked by the tags cursor
            let Some(tie) = &sorted.tie else {
                continue;
            };
            // keep the tie of an earlier line in each partition, since lines are consumed in
            // order
            for scope in &sorted.scopes {
                if !self.retain.ties.iter().any(|(tied, _)| tied == scope) {
                    self.retain.ties.push((scope.clone(), tie.clone()));
                }
            }
        }
    }

    /// Consume our sorted data in order to fill our user facing data buffer
    ///
    /// Returns whether a full page was filled.
    ///
    /// # Arguments
    ///
    /// * `tag_ties` - The ties of a tags cursor, or `None` for a general cursor
    fn consume_sorted(&mut self, mut tag_ties: Option<&mut HashMap<String, String>>) -> bool {
        let limit = self.limit.get();
        // keep looping until we have enough data to return or no more sorted data
        loop {
            // keep consuming until our user facing data buffer is full
            'outer: while self.data.len() < limit {
                // keep popping data from the end of our btree
                match self.sorted.pop_last() {
                    // we have a list of data to return
                    Some((timestamp, mut item)) => {
                        // if we have multiple rows with the same timestamp then disambiguate them
                        if tag_ties.is_some() && item.len() > 1 {
                            // sort our tied rows by the tags table's clustering key
                            D::sort_tied_rows_by_tag_clustering_key(&mut item);
                        } else if item.len() > 1 {
                            // consume tied rows in the order their tie queries read them
                            item.make_contiguous().sort_by(SortedLine::cmp_consumed);
                        }
                        // consume only enough data to fill our data
                        while self.data.len() < limit {
                            match item.pop_front() {
                                Some(sorted) => {
                                    // consume this mapped value
                                    self.mapped -= 1;
                                    // only dedupe if we are configured too
                                    if self.dedupe
                                        && !sorted.line.dedupe_item(&mut self.retain.dedupe_set)
                                    {
                                        // this is a duplicate item so skip it
                                        continue;
                                    }
                                    // add this data to our user facing vec
                                    self.data.push(sorted.line);
                                }
                                None => continue 'outer,
                            }
                        }
                        // record a tie for every group or partition in our remaining rows
                        match &mut tag_ties {
                            Some(tag_ties) => {
                                for tied in &item {
                                    tied.line.add_tag_tie(tag_ties);
                                }
                            }
                            None => self.record_ties(&item),
                        }
                        // this item still has more data so readd it
                        self.sorted.insert(timestamp, item);
                        // we have all the data that we need
                        break;
                    }
                    // we have no more data so break
                    None => {
                        // update our start timestamp if we found some data
                        if let Some(last) = self.data.last() {
                            self.retain.start = last.get_timestamp();
                        }
                        return false;
                    }
                }
            }
            // check if we have enough data to try and return yet
            if self.data.len() >= limit {
                // update our start timestamp if we found some data
                if let Some(last) = self.data.last() {
                    self.retain.start = last.get_timestamp();
                }
                return true;
            }
        }
    }

    /// Gets the next page of queries for this cursor for data for general queries
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "ScyllaCursor::next_general", skip_all, err(Debug))]
    async fn next_general(&mut self, shared: &Shared) -> Result<(), ApiError> {
        // get the limit + 1 of data for each group so we can check if we end on any ties
        let limit = self.limit.plus_one();
        // get the number of buckets to check at once
        let bucket_limit = D::Statements::bucket_limit(&self.retain.extra_filter)?;
        // get data from any previously tied queries
        let tied_queries = self.query_ties(limit, shared).await?;
        // if we had any queries based on ties then consume them
        if !tied_queries.is_empty() {
            // cast and sort the rows we just retrieved
            self.sort(tied_queries)?;
            // consume our sorted data and check if we have enough data to return
            if self.consume_sorted(None) {
                // we have enough data so return
                return Ok(());
            }
        }
        // determine which partitions have data
        self.query(limit, bucket_limit, shared).await?;
        // consume enough of our sorted data
        self.consume_sorted(None);
        Ok(())
    }

    /// Read the rows tied at our start timestamp from each group's tie onward, for every key
    /// and value
    ///
    /// # Arguments
    ///
    /// * `statements` - The tag listing's statements
    /// * `tags_retain` - Tag data retained for tags cursors
    /// * `mapping` - The mapping of items to their tag rows
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "ScyllaCursor::tag_ties", skip_all, err(Debug))]
    async fn tag_ties<S: TagCursorStatements>(
        &self,
        statements: &S,
        tags_retain: &mut TagsRetain,
        mapping: &mut HashMap<String, TagMapping>,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        let filters = tag_filters(tags_retain);
        // query each tie for each tag key and value
        let mut futures = Vec::with_capacity(tags_retain.ties.len() * filters.len());
        for (group, breaker) in &tags_retain.ties {
            for (key, value, extra) in &filters {
                let query = statements.ties(
                    &shared.scylla.session,
                    extra,
                    group,
                    self.year,
                    self.bucket,
                    self.retain.start,
                    breaker,
                );
                futures.push(async move { Ok::<_, QueryError>((*key, *value, query.await?)) });
            }
        }
        // execute our futures 50 at a time
        let queries = run_all(futures).await?;
        // build our tag mapping object
        parse_tag_queries(queries, mapping).await?;
        // clear our tag ties
        tags_retain.ties.clear();
        Ok(())
    }

    /// Read the rows of the next buckets every tag has data in until a page is full
    ///
    /// # Arguments
    ///
    /// * `statements` - The tag listing's statements
    /// * `group_by` - The groups being listed
    /// * `tags_retain` - Tag data retained for tags cursors
    /// * `mapping` - The mapping of items to their tag rows
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "ScyllaCursor::tag_query", skip_all, err(Debug))]
    async fn tag_query<S: TagCursorStatements>(
        &mut self,
        statements: &S,
        group_by: &[D::GroupBy],
        tags_retain: &mut TagsRetain,
        mapping: &mut HashMap<String, TagMapping>,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // the tags tables name each group as text
        let groups = group_by
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<String>>();
        // loop until we have found enough data to return
        loop {
            // get the next 100 buckets that contain data
            let buckets = self
                .tags_find_buckets(group_by, tags_retain, shared)
                .await?;
            // query each tag key and value in each group, 100 buckets at a time
            let filters = tag_filters(tags_retain);
            let mut futures = Vec::with_capacity(filters.len() * groups.len());
            for (key, value, extra) in &filters {
                for group in &groups {
                    for chunk in buckets.chunks(100) {
                        let query = statements.pull(
                            &shared.scylla.session,
                            extra,
                            group,
                            self.year,
                            chunk,
                            self.retain.start,
                            self.retain.end,
                        );
                        futures
                            .push(async move { Ok::<_, QueryError>((*key, *value, query.await?)) });
                    }
                }
            }
            // execute our futures 50 at a time
            let queries = run_all(futures).await?;
            // build our tag mapping object
            parse_tag_queries(queries, mapping).await?;
            // only retain mappings that matched on all of the required tags
            mapping.retain(|_, mapped| mapped.tags.len() == tags_retain.tags_required);
            // add our mapped data to our sorted items
            D::sort_tags(
                mapping,
                &tags_retain.tags,
                &mut self.sorted,
                &mut self.mapped,
            )?;
            // consume our sorted data and return if needed
            self.consume_sorted(Some(&mut tags_retain.ties));
            // if we have enough data to return then return
            if self.mapped >= self.limit.get() {
                break;
            }
            // update our bucket counter correctly
            match (self.year.cmp(&self.end_year), buckets.first()) {
                // we have more buckets this year to query so continue below the oldest one
                (Ordering::Greater | Ordering::Equal, Some(first)) => match first.prev() {
                    Some(bucket) => self.bucket = bucket,
                    // the year's first bucket was read, so move to the year before
                    None if self.year > self.end_year => {
                        self.year = self.year.saturating_sub(1);
                        self.bucket = Bucket::last_of_year(self.year, self.partition_size)?;
                    }
                    // the end year's first bucket was read, so this cursor is exhausted
                    None => {
                        self.bucket = self.end_bucket;
                        self.buckets_exhausted = true;
                        break;
                    }
                },
                // we have more years to query so decrement our year and update our bucket
                (Ordering::Greater, None) => {
                    // decrement our year
                    self.year = self.year.saturating_sub(1);
                    // reset our bucket to the last possible partition of the year
                    self.bucket = Bucket::last_of_year(self.year, self.partition_size)?;
                }
                // we have reached our end bucket and end year
                (Ordering::Equal | Ordering::Less, None) => {
                    self.bucket = self.end_bucket;
                    // set this cursor to be exhausted
                    self.buckets_exhausted = true;
                    break;
                }
                // we have gone past our end year bounds somehow
                (Ordering::Less, Some(_)) => {
                    event!(
                        Level::ERROR,
                        msg = "Gone past end year bound?",
                        year = self.year,
                        end_year = self.end_year,
                        bucket = self.bucket.get(),
                        end_bucket = self.end_bucket.get()
                    );
                    // return an error to the user
                    return internal_err!("Cursor Failure".to_owned());
                }
            }
        }
        Ok(())
    }

    /// Gets the next page of a tag listing through the statements of its tags table or view
    ///
    /// # Arguments
    ///
    /// * `statements` - The tag listing's statements
    /// * `tags_retain` - Tag data retained for tags cursors
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "ScyllaCursor::next_tags", skip_all, err(Debug))]
    async fn next_tags<S: TagCursorStatements>(
        &mut self,
        statements: &S,
        mut tags_retain: TagsRetain,
        shared: &Shared,
    ) -> Result<TagsRetain, ApiError> {
        // size our mapping for every tag of every row we may return
        let capacity = tags_retain.tags_required.saturating_mul(self.limit.get());
        // get the group by this cursor will use, putting it back when we're done
        let group_by = std::mem::take(&mut self.retain.group_by);
        // keep a mapping of the tags data we retrieved
        let mut mapping = HashMap::with_capacity(capacity);
        // read any tie data, then query for more if we don't have a page yet
        let result = async {
            if !tags_retain.ties.is_empty() {
                self.tag_ties(statements, &mut tags_retain, &mut mapping, shared)
                    .await?;
            }
            // add our mapped data to our sorted items
            D::sort_tags(
                &mut mapping,
                &tags_retain.tags,
                &mut self.sorted,
                &mut self.mapped,
            )?;
            // consume our sorted data
            self.consume_sorted(Some(&mut tags_retain.ties));
            // only query for more data if we don't have enough to return
            if self.mapped < self.limit.get() {
                self.tag_query(
                    statements,
                    &group_by,
                    &mut tags_retain,
                    &mut mapping,
                    shared,
                )
                .await?;
            }
            Ok::<(), ApiError>(())
        }
        .await;
        // put our group by back
        self.retain.group_by = group_by;
        result.map(|()| tags_retain)
    }

    /// Gets the next page of data for this cursor
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "ScyllaCursor::next", skip_all, err(Debug))]
    pub async fn next(&mut self, shared: &Shared) -> Result<(), ApiError> {
        // if we have more already sorted rows then return those first
        if self.mapped > 0 {
            // consume our remaining sorted values
            self.consume_sorted(None);
            // if we have enough data then return
            if self.data.len() >= self.limit.get() {
                return Ok(());
            }
        }
        // crawl over the tags table if we have tag filters
        match self.retain.tags_retain.take() {
            // we have tags to filter on, as written or in any case
            Some(tags_retain) => {
                let queries = &shared.scylla.queries;
                let tags_retain = if tags_retain.case_insensitive {
                    let statements = &queries.tags_case_insensitive.list;
                    self.next_tags(statements, tags_retain, shared).await?
                } else {
                    self.next_tags(&queries.tags.list, tags_retain, shared)
                        .await?
                };
                // restore our retained tags data
                self.retain.tags_retain = Some(tags_retain);
                Ok(())
            }
            // we don't any any tags to filter on
            None => self.next_general(shared).await,
        }
    }

    /// Saves this cursor to Redis
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "ScyllaCursor::save", skip_all, err(Debug))]
    pub async fn save(&self, shared: &Shared) -> Result<(), ApiError> {
        // serialize our retained data
        let data = serialize!(&self.retain);
        // build the key to save this cursor data too scoped to the creating user
        let key = cursors::data(CursorKind::Scylla, &self.username, &self.id, shared);
        // get how long to hold onto this cursor
        let ttl = scylla_cursor_save_expire(self.new);
        // save this cursors data to redis
        let _: () = query!(cmd("set").arg(key).arg(data).arg("EX").arg(ttl), shared).await?;
        Ok(())
    }
}

/// Get how many seconds [`ScyllaCursor::save`] holds a cursor in redis for
///
/// A new cursor is only held for 8 hours so abandoned new cursors don't linger in redis, and a
/// cursor that is being paged is held for 7 days.
///
/// # Arguments
///
/// * `new` - Whether this is a newly created cursor
fn scylla_cursor_save_expire(new: bool) -> u64 {
    // if this is a new cursor only hold onto it for 8 hours
    // if its an existing cursor that is being paged then hold onto it for 7 days
    if new { 28_800 } else { 604_800 }
}

impl<D> From<ScyllaCursor<D>> for ApiCursor<D>
where
    for<'de> D: Deserialize<'de> + Debug + std::marker::Send,
    D: Serialize,
    D: ScyllaCursorSupport,
    D: CursorMetadataSupport,
    D: Debug,
{
    /// convert this scylla cursor to a user facing cursor
    fn from(scylla_cursor: ScyllaCursor<D>) -> Self {
        // if our cursor is exhausted then don't include a cursor id
        let id = if scylla_cursor.exhausted() {
            None
        } else {
            Some(scylla_cursor.id)
        };
        // build our cursor object
        ApiCursor {
            cursor: id,
            data: scylla_cursor.data,
            metadata: None,
        }
    }
}

/// The data retained for the lifetime of this scylla simple cursor
#[derive(Serialize, Deserialize, Debug)]
pub struct SimpleScyllaCursorRetain<T> {
    /// The partitions we are crawling for data
    partitions: Vec<String>,
    /// Our current position in our partitions vec
    index: usize,
    /// The clustering key to start listing values after next time
    tie: Option<T>,
}

/// The tie type of a simple cursor's statements
type SimpleTieOf<D> = <<D as SimpleCursorExt>::Statements as SimpleCursorStatements>::Tie;

/// Abstracts simple cursor logic across different tables and data types
pub trait SimpleCursorExt: Sized {
    /// The statements this cursor pages with
    type Statements: SimpleCursorStatements<Row = Self, Partition = String>;

    /// Get the statements this cursor pages with
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    fn statements(shared: &Shared) -> &Self::Statements;
}

/// A simple non group permissioned cursor
#[derive(Debug)]
pub struct SimpleScyllaCursor<D>
where
    for<'de> D: Deserialize<'de>,
    D: Serialize,
    D: SimpleCursorExt,
{
    /// The ID for this cursor
    pub id: Uuid,
    /// The username of the user this cursor is scoped to
    pub username: String,
    /// The dta that is retained thoughout this cursors life
    pub retain: SimpleScyllaCursorRetain<SimpleTieOf<D>>,
    /// The max number of items to return at once
    pub limit: CursorLimit,
    /// Whether this cursor is exhausted or not
    pub exhausted: bool,
    /// Whether this cursor has data in redis or not
    pub in_redis: bool,
    /// The data to return
    pub data: Vec<D>,
}

impl<D> SimpleScyllaCursor<D>
where
    for<'de> D: Deserialize<'de>,
    D: Serialize,
    D: SimpleCursorExt,
{
    /// Create a new simple cursor object
    ///
    /// # Arguments
    ///
    /// * `partitions` - The partitions to list data from
    /// * `username` - The username of the user this cursor is scoped to
    /// * `limit` - The max number of items to return at once
    #[must_use]
    pub fn new(partitions: Vec<String>, username: &str, limit: CursorLimit) -> Self {
        // create an initial simple scylla cursor
        let retain = SimpleScyllaCursorRetain {
            partitions,
            index: 0,
            tie: None,
        };
        // build our simple cursor
        SimpleScyllaCursor {
            id: Uuid::new_v4(),
            username: username.to_owned(),
            retain,
            limit,
            exhausted: false,
            in_redis: false,
            data: Vec::with_capacity(limit.get()),
        }
    }

    /// Set the max number of items this cursor should return at once
    ///
    /// # Arguments
    ///
    /// * `limit` - The max number of items that should be returned at once
    #[must_use]
    pub fn limit(mut self, limit: CursorLimit) -> Self {
        // update the limit for this cursor
        self.limit = limit;
        self
    }

    /// Gets a simple cursors info from Redis
    ///
    /// # Arguments
    ///
    /// * `cursor_id` - The id of the cursor to retrieve
    /// * `username` - The username of the user this cursor is scoped to
    /// * `limit` - The max number of items to return at once
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "SimpleScyllaCursor::get", skip(shared), err(Debug))]
    pub async fn get(
        cursor_id: Uuid,
        username: &str,
        limit: CursorLimit,
        shared: &Shared,
    ) -> Result<SimpleScyllaCursor<D>, ApiError> {
        // build the key to our cursor data in redis scoped to the creating user
        let key = cursors::data(CursorKind::SimpleScylla, username, &cursor_id, shared);
        // get our cursor from redis
        let data: Option<String> = query!(cmd("get").arg(key), shared).await?;
        // check if we got any cursor data
        match data {
            Some(data) => {
                // deserialize our retained data
                let retain = deserialize!(&data);
                let cursor = SimpleScyllaCursor {
                    id: cursor_id,
                    username: username.to_owned(),
                    retain,
                    limit,
                    exhausted: false,
                    in_redis: true,
                    data: Vec::with_capacity(limit.get()),
                };
                Ok(cursor)
            }
            // we didn't find any cursor data
            None => not_found!(format!("Cursor {} doesn't exist", cursor_id)),
        }
    }

    /// Get the next page of this cursors data
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "SimpleScyllaCursor::next", skip_all, err(Debug))]
    pub async fn next(&mut self, shared: &Shared) -> Result<(), ApiError> {
        // if we have no partitions then exhaust this cursor and return empty
        if self.retain.partitions.is_empty() {
            self.exhausted = true;
            return Ok(());
        }
        let statements = D::statements(shared);
        // loop until we find enough data or exhaust our partitions, reading the rows left of
        // our limit each time, which is never 0
        while let Some(limit) = self.limit.remaining(self.data.len()) {
            let remaining = limit.get();
            let partition = &self.retain.partitions[self.retain.index];
            // query scylla for the rows after our tie, or the first rows of this partition
            let rows = match &self.retain.tie {
                Some(tie) => {
                    statements
                        .after(&shared.scylla.session, partition, tie, limit)
                        .await?
                }
                None => {
                    statements
                        .first(&shared.scylla.session, partition, limit)
                        .await?
                }
            };
            // get the number of rows we read
            let cnt = rows.len();
            // cast our rows into the correct objects and log any errors
            self.data
                .extend(rows.into_iter().filter_map(|res| log_scylla_err!(res)));
            // getting fewer rows than our limit means this partition is exhausted
            if cnt < remaining {
                // check if we are at the last index
                if self.retain.index == self.retain.partitions.len() - 1 {
                    // we are at the last index so just return what we have
                    self.exhausted = true;
                    break;
                }
                // we have more partitions to get data from so continue on
                self.retain.index += 1;
                // reset our tie value
                self.retain.tie = None;
            }
            // if we have all the data we need then return
            if self.data.len() == self.limit.get() {
                // set our tie value if we have any data
                if let Some(last) = self.data.last() {
                    self.retain.tie = Some(D::Statements::row_tie(last));
                }
                break;
            }
        }
        Ok(())
    }

    /// Saves this cursor to Redis
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "SimpleScyllaCursor::save", skip_all, err(Debug))]
    pub async fn save(&self, shared: &Shared) -> Result<(), ApiError> {
        // build the key to save this cursor data too scoped to the creating user
        let key = cursors::data(CursorKind::SimpleScylla, &self.username, &self.id, shared);
        // either save or delete this cursor based on whether its exhausted or not
        if self.exhausted {
            // only delete if this cursor was actually written to redis
            if self.in_redis {
                // delete this cursor from redis
                let _: () = query!(cmd("del").arg(key), shared).await?;
            }
        } else {
            // serialize our paritions
            let data = serialize!(&self.retain);
            // save this cursors data to redis
            let _: () = query!(
                cmd("set").arg(key).arg(data).arg("EX").arg(2_628_000),
                shared
            )
            .await?;
        }
        Ok(())
    }
}

impl<D> From<SimpleScyllaCursor<D>> for ApiCursor<D>
where
    for<'de> D: Deserialize<'de>,
    D: Serialize,
    D: SimpleCursorExt,
    D: CursorMetadataSupport,
{
    /// convert this simple scylla cursor to a user facing cursor
    fn from(scylla_cursor: SimpleScyllaCursor<D>) -> Self {
        // if our cursor is exhausted then don't include a cursor id
        let id = if scylla_cursor.exhausted {
            None
        } else {
            Some(scylla_cursor.id)
        };
        // build our cursor object
        ApiCursor {
            cursor: id,
            data: scylla_cursor.data,
            metadata: None,
        }
    }
}

/// The core features required for implementing a Scylla cursor crawling grouped rows
///
/// The fundamental difference compared to [`ScyllaCursorSupport`] is this cursor has
/// no concept of time or bucketing by time
pub trait GroupedScyllaCursorSupport: Sized {
    /// The params to build this cursor form
    type Params;

    /// Any extra info to filter with
    type ExtraFilters: Clone + Debug + Serialize + for<'a> Deserialize<'a>;

    /// The intermediary component type casted from a Scylla row that is used to build `Self`
    type RowType: Into<Self>
        + for<'frame, 'metadata> scylla::deserialize::row::DeserializeRow<'frame, 'metadata>;

    /// The type of data our rows are grouped by (AKA the partition key)
    type GroupBy: Hash + Eq + Debug + Serialize + for<'a> Deserialize<'a>;

    /// The type `Self` is sorted by in Scylla (AKA the clustering key);
    /// the resulting list of `Self` will be returned ordered by this type
    type SortBy: Ord + Debug + Serialize + for<'a> Deserialize<'a>;

    /// The type used to break ties when the same instance of `SortBy` is found in multiple
    /// groups, but they are unique entities (i.e. the two entities have the same name, but
    /// none of the same groups)
    type SortTieBreaker: Eq;

    /// The statements this cursor pages with
    type Statements: GroupedCursorStatements<
            Row = Self::RowType,
            Group = Self::GroupBy,
            SortBy = Self::SortBy,
            TieBreaker = Self::SortTieBreaker,
        >;

    /// Get the statements this cursor pages with
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    fn statements(shared: &Shared) -> &Self::Statements;

    /// Get our cursor id from params
    ///
    /// # Arguments
    ///
    /// * `params` - The params to use to build this cursor
    fn get_id(params: &mut Self::Params) -> Option<Uuid>;

    /// Get the groups to query from our params
    ///
    /// # Arguments
    ///
    /// * `params` - The params to use to build this cursor
    fn get_groups(params: &mut Self::Params) -> HashSet<Self::GroupBy>;

    /// Get our extra filters from our params
    ///
    /// # Arguments
    ///
    /// * `params` - The params to use to build this cursor
    fn get_extra_filters(params: &mut Self::Params) -> Self::ExtraFilters;

    /// Get our the max number of rows to return
    ///
    /// # Arguments
    ///
    /// * `params` - The params to use to build this cursor
    fn get_limit(params: &Self::Params) -> Result<CursorLimit, ApiError>;

    /// Return the value we're sorting by from `self`
    fn get_sort_by(&self) -> Self::SortBy;

    /// Get the tie-breaking value from `self`
    fn get_tie_breaker(&self) -> &Self::SortTieBreaker;

    /// Convert `self` to a tie to re-retrieve later
    fn to_tie(self) -> (Self::SortBy, Vec<Self::GroupBy>);

    /// Add a row to `self`, probably by just adding the row's group
    ///
    /// # Arguments
    ///
    /// * `row` - The component row to add
    fn add_row(&mut self, row: Self::RowType);
}

/// The data to retain throughout this cursors life
#[derive(Serialize, Deserialize, Debug)]
pub struct GroupedScyllaCursorRetain<D: GroupedScyllaCursorSupport> {
    /// The tag based filters to use with this cursor
    pub extra_filter: D::ExtraFilters,
    /// The values to group the rows from this cursor by
    pub group_by: HashSet<D::GroupBy>,
    /// Any ties in past iterations of this cursor that would be skipped
    /// if we continued from our current clustering key
    pub ties: Vec<(D::SortBy, Vec<D::GroupBy>)>,
    /// The current sort value we left off at last query
    pub current_sort_by: Option<D::SortBy>,
}

impl<D: GroupedScyllaCursorSupport> GroupedScyllaCursorRetain<D> {
    /// Narrow this cursor to the groups a request may list, dropping every other group from the
    /// saved ties so they are never read
    ///
    /// Returns whether any saved group is left.
    ///
    /// # Arguments
    ///
    /// * `allowed` - The groups the request may list
    fn narrow(&mut self, allowed: &HashSet<D::GroupBy>) -> bool {
        // keep only the saved groups this request may still list
        self.group_by.retain(|group| allowed.contains(group));
        // drop any other group from each tie, and the ties left with no group
        for (_, groups) in &mut self.ties {
            groups.retain(|group| allowed.contains(group));
        }
        self.ties.retain(|(_, groups)| !groups.is_empty());
        !self.group_by.is_empty()
    }
}

/// A cursor for a listing group permisisoned data within scylla
#[derive(Debug)]
pub struct GroupedScyllaCursor<D>
where
    for<'de> D: Deserialize<'de> + Debug,
    D: Serialize,
    D: GroupedScyllaCursorSupport,
    D: Debug,
{
    /// The Id for this cursor
    pub id: Uuid,
    /// The username of the user this cursor is scoped to
    pub username: String,
    /// The cursor settings/data to retain across cursor iterations
    pub retain: GroupedScyllaCursorRetain<D>,
    /// The max number of items to return at once
    pub limit: CursorLimit,
    /// Whether this cursor has data in redis or not
    pub in_redis: bool,
    /// The data this cursor has retrieved
    pub data: Vec<D>,
    /// Whether this is a new cursor or it already existed and is being continued
    pub new: bool,
}

impl<D> GroupedScyllaCursor<D>
where
    for<'de> D: Deserialize<'de> + Debug,
    D: Serialize,
    D: GroupedScyllaCursorSupport,
    D: Debug,
{
    /// Create a new cursor object from just params
    ///
    /// # Arguments
    ///
    /// * `params` - The params to build this cursor from
    /// * `username` - The username of the user this cursor is scoped to
    /// * `dedupe` - Whether to dedupe items or not
    /// * `shared` - Shared Thorium objects
    #[instrument(
        name = "GroupedScyllaCursor::from_params",
        skip(params, dedupe, shared),
        err(Debug)
    )]
    pub async fn from_params(
        mut params: D::Params,
        username: &str,
        dedupe: bool,
        shared: &Shared,
    ) -> Result<GroupedScyllaCursor<D>, ApiError> {
        // get our extra filters unless we're continuing a cursor
        match D::get_id(&mut params) {
            Some(id) => GroupedScyllaCursor::get(id, username, params, dedupe, shared).await,
            None => {
                let extra_filter = D::get_extra_filters(&mut params);
                Self::create(params, extra_filter, username)
            }
        }
    }

    /// Create a new cursor object from params with a set extra fields
    ///
    /// # Arguments
    ///
    /// * `params` - The params to build this cursor from
    /// * `extra_filter` - The extra filters to use in this cursor
    /// * `username` - The username of the user this cursor is scoped to
    /// * `dedupe` - Whether to dedupe items or not
    /// * `shared` - Shared Thorium objects
    #[instrument(
        name = "GroupedScyllaCursor::from_params_extra",
        skip(params, extra_filter, shared),
        err(Debug)
    )]
    pub async fn from_params_extra(
        mut params: D::Params,
        extra_filter: D::ExtraFilters,
        username: &str,
        dedupe: bool,
        shared: &Shared,
    ) -> Result<GroupedScyllaCursor<D>, ApiError> {
        // if we have a cursor id then try to get our cursor from the DB
        match D::get_id(&mut params) {
            Some(id) => GroupedScyllaCursor::get(id, username, params, dedupe, shared).await,
            None => Self::create(params, extra_filter, username),
        }
    }

    /// Create a new cursor from params and its extra filters
    ///
    /// # Arguments
    ///
    /// * `params` - The params to build this cursor from
    /// * `extra_filter` - The extra filters to use in this cursor
    /// * `username` - The username of the user this cursor is scoped to
    fn create(
        mut params: D::Params,
        extra_filter: D::ExtraFilters,
        username: &str,
    ) -> Result<Self, ApiError> {
        // get our group restrictions
        let groups = D::get_groups(&mut params);
        // build the inital data to retain across cursor iterations
        let retain = GroupedScyllaCursorRetain {
            extra_filter,
            group_by: groups,
            ties: Vec::default(),
            current_sort_by: None,
        };
        // build our cursor
        Ok(GroupedScyllaCursor {
            id: Uuid::new_v4(),
            username: username.to_owned(),
            retain,
            limit: D::get_limit(&params)?,
            in_redis: false,
            data: Vec::default(),
            new: true,
        })
    }

    /// Gets a cursors data from scylla
    ///
    /// # Arguments
    ///
    /// * `cursor_id` - The uuid of the cursor to retrieve if one is known
    /// * `username` - The username of the user this cursor is scoped to
    /// * `params` - The params to build this cursor from
    /// * `dedupe` - Whether to dedupe items or not
    /// * `shared` - Shared thorium objects
    #[instrument(name = "GroupedScyllaCursor::get", skip(params, shared), err(Debug))]
    pub async fn get(
        cursor_id: Uuid,
        username: &str,
        mut params: D::Params,
        dedupe: bool,
        shared: &Shared,
    ) -> Result<GroupedScyllaCursor<D>, ApiError> {
        // build the key to our cursor data in redis scoped to the creating user
        let key = cursors::data(CursorKind::GroupedScylla, username, &cursor_id, shared);
        // get our cursor from redis
        let data: Option<String> = query!(cmd("get").arg(key), shared).await?;
        // check if we got any cursor data
        match data {
            Some(data) => {
                // deserialize our retained data
                let mut retain: GroupedScyllaCursorRetain<D> = deserialize!(&data);
                // get the groups this request was authorized to list, which a scoped token, a
                // deleted group, or a user's removal from a group can make narrower than the
                // groups saved with it
                let allowed = D::get_groups(&mut params);
                // resume over the saved groups still allowed, and answer as for an unknown id
                // when none are left
                if !retain.narrow(&allowed) {
                    return not_found!(format!(
                        "cursor '{}' was not found; maybe it expired?",
                        cursor_id
                    ));
                }
                // build and return our cursor
                let cursor = GroupedScyllaCursor {
                    id: cursor_id,
                    username: username.to_owned(),
                    retain,
                    limit: D::get_limit(&params)?,
                    in_redis: true,
                    data: Vec::default(),
                    new: false,
                };
                Ok(cursor)
            }
            // we didn't find any cursor data
            None => not_found!(format!(
                "cursor '{}' was not found; maybe it expired?",
                cursor_id
            )),
        }
    }

    /// Check if we have any ties queries
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    #[instrument(
        name = "GroupedScyllaCursor::query_ties",
        skip(self, shared),
        err(Debug)
    )]
    async fn query_ties(
        &self,
        shared: &Shared,
    ) -> Result<Vec<Vec<RowResult<D::RowType>>>, ApiError> {
        let statements = D::statements(shared);
        // get an extra row to check for more ties
        let limit = self.limit.plus_one();
        // read each tied sort value in each of its groups
        let mut futures = Vec::new();
        for (sort_by, groups) in &self.retain.ties {
            for group in groups {
                futures.push(statements.ties(&shared.scylla.session, group, sort_by, limit));
            }
        }
        run_all(futures).await
    }

    /// Remove any groups from our list that didn't return any data
    ///
    /// # Arguments
    ///
    /// * `rows` - The casted rows we retrieved last query
    fn prune_exhausted_groups(&mut self, rows: &[D::RowType]) {
        // collect the groups that returned rows
        let mut non_empty_groups: HashSet<D::GroupBy> = HashSet::new();
        for row in rows {
            non_empty_groups.insert(D::Statements::row_group(row));
        }
        // retain only groups that are not empty
        self.retain
            .group_by
            .retain(|group| non_empty_groups.contains(group));
    }

    /// Build a sorted map of the sort value to a list of unique entities with that
    /// sort value, where each entity is composed of one or more rows
    ///
    /// Entities' uniqueness is determined using their [`GroupedScyllaCursorSupport::SortTieBreaker`]
    ///
    /// # Arguments
    ///
    /// * rows - The rows to build the entities from
    fn build_sorted_map(rows: Vec<D::RowType>) -> BTreeMap<D::SortBy, VecDeque<D>> {
        let mut map: BTreeMap<D::SortBy, VecDeque<D>> = BTreeMap::new();
        for row in rows {
            // get the list of entities by their sort name
            let entity_list = map.entry(D::Statements::row_sort(&row)).or_default();
            // see if we've seen this entity before using our tie breakers
            if let Some(entity) = entity_list
                .iter_mut()
                .find(|n| n.get_tie_breaker() == D::Statements::row_tie_breaker(&row))
            {
                // if we've already seen this policy, add the row to it
                entity.add_row(row);
            } else {
                // otherwise, cast the row to the entity and add it to our list
                entity_list.push_back(row.into());
            }
        }
        map
    }

    /// Cast the query results to their row type
    ///
    /// # Arguments
    ///
    /// * `results` - The rows each statement read
    #[instrument(name = "GroupedScyllaCursor::cast_rows", skip_all, err(Debug))]
    fn cast_rows(results: Vec<Vec<RowResult<D::RowType>>>) -> Result<Vec<D::RowType>, ApiError> {
        // raise any errors from casting
        let casts = results
            .into_iter()
            .flatten()
            .collect::<Result<Vec<D::RowType>, _>>()?;
        Ok(casts)
    }

    /// Crawl partitions and pull data from them, starting after `current_sort_by` if we
    /// left off somewhere
    ///
    /// # Arguments
    ///
    /// * `current_sort_by` - The sort value we left off at, if any
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "GroupedScyllaCursor::query", skip(self, shared), err(Debug))]
    async fn query(
        &self,
        current_sort_by: Option<&D::SortBy>,
        shared: &Shared,
    ) -> Result<Vec<Vec<RowResult<D::RowType>>>, ApiError> {
        let statements = D::statements(shared);
        // get an extra row to check for exhausted rows and ties
        let limit = self.limit.plus_one();
        // query each group from where we left off, or from its start
        let mut futures = Vec::with_capacity(self.retain.group_by.len());
        for group in &self.retain.group_by {
            let session = &shared.scylla.session;
            futures.push(async move {
                match current_sort_by {
                    Some(after) => statements.pull_more(session, group, after, limit).await,
                    None => statements.pull(session, group, limit).await,
                }
            });
        }
        run_all(futures).await
    }

    /// Consume our sorted data map in order to fill our user facing data buffer
    ///
    /// # Arguments
    ///
    /// * `sorted_map` - The data sorted by the `SortBy` value mapped to a list of
    ///   unique entities containing that `SortBy` value
    fn consume_sorted(&mut self, mut sorted_map: BTreeMap<D::SortBy, VecDeque<D>>) {
        let limit = self.limit.get();
        // keep consuming until our user facing data buffer is full or we run out of data
        while self.data.len() < limit {
            let Some((_sort_by, mut list)) = sorted_map.pop_first() else {
                break;
            };
            // keep consuming from each list until our user facing data buffer is full or we
            // run out of data
            while self.data.len() < limit {
                match list.pop_front() {
                    Some(item) => self.data.push(item),
                    None => break,
                }
            }
            if !list.is_empty() {
                // if we still have data left, we must have had ties;
                // save those ties to our cursor to get next iteration
                self.retain.ties = list.into_iter().map(D::to_tie).collect();
            }
        }
    }

    /// Consume the rows returned by our tie queries into our user facing data buffer
    ///
    /// Once our ties have been queried they are cleared so later pages don't query and return the
    /// same tied entities again; consuming the tied rows may record a new leftover tie. Returns
    /// whether the tied rows filled our data buffer.
    ///
    /// # Arguments
    ///
    /// * `rows` - The rows returned by our tie queries
    fn consume_tied_rows(&mut self, rows: Vec<D::RowType>) -> bool {
        // sort the tied rows into entities by their sort value
        let sorted_map = Self::build_sorted_map(rows);
        // our ties have been queried so clear them before any new leftover tie is recorded
        self.retain.ties.clear();
        // consume the sorted data and add it to our user-facing data buffer
        self.consume_sorted(sorted_map);
        // see if our ties returned all we needed
        self.data.len() >= self.limit.get()
    }

    /// Gets the next page of queries for this cursor for data for general queries
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "GroupedScyllaCursor::next", skip_all, err(Debug))]
    pub async fn next(&mut self, shared: &Shared) -> Result<(), ApiError> {
        // get data from any previously tied queries
        let tied_query_results = self.query_ties(shared).await?;
        // cast the rows we just retrieved
        let rows = Self::cast_rows(tied_query_results)?;
        // consume our tied rows and return early if they returned all we needed
        if self.consume_tied_rows(rows) {
            return Ok(());
        }
        // retrieve data, continuing from where we left off if this is a subsequent search
        let query_results = self
            .query(self.retain.current_sort_by.as_ref(), shared)
            .await?;
        // cast the rows we just retrieved to the component row type
        let rows = Self::cast_rows(query_results)?;
        // prune any groups that returned no rows
        self.prune_exhausted_groups(&rows);
        // build a sorted map of values
        let sorted_map = Self::build_sorted_map(rows);
        // consume the sorted data and add it to our user-facing data buffer
        self.consume_sorted(sorted_map);
        // save the current sort by we left off at
        self.retain.current_sort_by = self.data.last().map(D::get_sort_by);
        Ok(())
    }

    /// Saves this cursor to Scylla
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "GroupedScyllaCursor::save", skip_all, err(Debug))]
    pub async fn save(&self, shared: &Shared) -> Result<(), ApiError> {
        // build the key to save this cursor data too scoped to the creating user
        let key = cursors::data(CursorKind::GroupedScylla, &self.username, &self.id, shared);
        // either save or delete this cursor based on whether its exhausted or not
        if self.exhausted() {
            // only delete if this cursor was actually written to redis
            if self.in_redis {
                // delete this cursor from redis
                let _: () = query!(cmd("del").arg(key), shared).await?;
            }
        } else {
            // serialize our paritions
            let data = serialize!(&self.retain);
            // if this is a new cursor only hold onto it for 8 hours
            // if its an existing cursor that is being paged then hold onto it for 7 days
            let ttl = if self.new { 28_800 } else { 604_800 };
            // save this cursors data to redis
            let _: () = query!(cmd("set").arg(key).arg(data).arg("EX").arg(ttl), shared).await?;
        }
        Ok(())
    }

    /// Check whether this cursor is exhausted
    fn exhausted(&self) -> bool {
        // if none of our groups return data anymore, the cursor is exhausted
        self.retain.group_by.is_empty()
    }
}

impl<D> From<GroupedScyllaCursor<D>> for ApiCursor<D>
where
    for<'de> D: Deserialize<'de> + Debug,
    D: Serialize,
    D: GroupedScyllaCursorSupport,
    D: CursorMetadataSupport,
    D: Debug,
{
    /// convert this scylla cursor to a user facing cursor
    fn from(scylla_cursor: GroupedScyllaCursor<D>) -> Self {
        // if our cursor is exhausted then don't include a cursor id
        let id = if scylla_cursor.exhausted() {
            None
        } else {
            Some(scylla_cursor.id)
        };
        // build our cursor object
        ApiCursor {
            cursor: id,
            data: scylla_cursor.data,
            metadata: None,
        }
    }
}

/// A cursor that determines if some data exists or not
pub struct ExistsCursor {
    /// The timestamp to start getting more data for this cursor at
    pub start: DateTime<Utc>,
    /// The year we are last got data from
    pub year: i32,
    /// The bucket chunk we last got data from
    pub bucket: Bucket,
    /// The final date that our cursor should get data from
    pub end: DateTime<Utc>,
    /// The final year we are going to get data from
    pub end_year: i32,
    /// The final bucket we will get data from
    pub end_bucket: Bucket,
    /// The size of the partitions in seconds
    pub partition_size: u16,
}

impl ExistsCursor {
    /// Create a new [`ExistsCursor`]
    ///
    /// # Arguments
    ///
    /// * `start` - The timestamp to start checking for data at
    /// * `end` - The timestamp to stop checking for data at
    /// * `partition_size` - The partition size to use
    pub fn new(
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        partition_size: u16,
    ) -> Result<Self, ApiError> {
        // get the years this cursor should start and end in
        let year = start.year();
        let end_year = end.year();
        // build our exists cursor
        let cursor = ExistsCursor {
            start,
            year,
            bucket: Bucket::of(start, year, partition_size)?,
            end,
            end_year,
            // the walk stops at the end timestamp's bucket in the end year
            end_bucket: Bucket::of(end, end_year, partition_size)?,
            partition_size,
        };
        Ok(cursor)
    }

    /// Get the next window of partitions [`ExistsCursor::exists`] checks and advance past it
    ///
    /// The walk never depends on query results, so advancing before the window is queried
    /// behaves the same as advancing after. Returns the year and buckets to query and whether
    /// this is the final window. The walk stops at `end_bucket` in `end_year`.
    fn next_window(&mut self) -> Result<(i32, Vec<Bucket>, bool), ApiError> {
        // get the year this window is in
        let year = self.year;
        // check if we are in the final year to list and stop at the correct bucket
        let lowest = Bucket::new(self.bucket.get().saturating_sub(99));
        let end = if self.year == self.end_year {
            std::cmp::max(lowest, self.end_bucket)
        } else {
            lowest
        };
        // get the buckets in this window
        let buckets = (end.get()..=self.bucket.get())
            .map(Bucket::new)
            .collect::<Vec<Bucket>>();
        // update our bucket counter correctly
        let done = match (end.prev(), self.year.cmp(&self.end_year)) {
            // we have more buckets this year to query so just update our bucket
            (Some(below), Ordering::Greater) => {
                self.bucket = below;
                false
            }
            (Some(below), Ordering::Equal) if end > self.end_bucket => {
                self.bucket = below;
                false
            }
            // we have more years to query so decrement our year and update our bucket
            (None, Ordering::Greater) => {
                // decrement our year
                self.year = self.year.saturating_sub(1);
                // reset our bucket to the last possible partition of the year
                self.bucket = Bucket::last_of_year(self.year, self.partition_size)?;
                false
            }
            // were done querying so update our bucket
            (below, _) => {
                self.bucket = below.unwrap_or(end);
                true
            }
        };
        Ok((year, buckets, done))
    }

    /// Check if any rows exist across all partitions
    ///
    /// # Arguments
    ///
    /// * `statement` - The statement checking a window of buckets for the key
    /// * `key` - The key to use when checking for the existence of rows
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "ExistsCursor::exists", skip_all, err(Debug))]
    pub async fn exists<S: ExistsStatement>(
        mut self,
        statement: &S,
        key: &S::Key,
        shared: &Shared,
    ) -> Result<bool, ApiError> {
        // walk every window of buckets to check before checking any
        let mut windows = Vec::with_capacity(100);
        loop {
            // get the next window of partitions to check and advance our cursor
            let (year, buckets, done) = self.next_window()?;
            windows.push((year, buckets));
            // stop once we have our final window
            if done {
                break;
            }
        }
        // check the windows 100 at a time, 50 at once, stopping at the first that has data
        for chunk in windows.chunks(100) {
            // build this chunk's checks before streaming them
            let futures = chunk
                .iter()
                .map(|(year, buckets)| statement.any(&shared.scylla.session, *year, buckets, key))
                .collect::<Vec<_>>();
            let mut stream = stream::iter(futures).buffer_unordered(50);
            // check all partitons for data
            while let Some(found) = stream.next().await {
                // check if any rows were returned
                if found? {
                    // log that we found data
                    event!(Level::INFO, exists = true);
                    return Ok(true);
                }
            }
        }
        // log that we did not find data
        event!(Level::INFO, exists = false);
        Ok(false)
    }
}
pub trait TagCountCursorSupport: ScyllaCursorSupport + Send {
    /// The type for our data with tags
    type Details;

    /// Get or create a new backing cursor for the data whose tags we are counting
    ///
    /// # Arguments
    ///
    /// * `params` - The params used to get our backing cursor
    /// * `extra` - Any extra params needed to get our backing cursor
    /// * `username` - The username of the user this cursor is scoped to
    /// * `shared` - Shared Thorium objects
    #[allow(async_fn_in_trait)]
    async fn backing_cursor(
        params: <Self as CursorCore>::Params,
        _extra: <Self as CursorCore>::ExtraFilters,
        username: &str,
        shared: &Shared,
    ) -> Result<ScyllaCursor<Self>, ApiError> {
        // get our backing cursor scoped to the creating user
        ScyllaCursor::from_params(params, username, true, shared).await
    }

    /// Get the details for our backing cursor
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is calling this cursor
    /// * `backing` - The backing cursor to get details from
    /// * `shared` - Shared Thorium objects
    #[allow(async_fn_in_trait)]
    async fn get_details(
        user: &User,
        backing: &mut ScyllaCursor<Self>,
        shared: &Shared,
    ) -> Result<Vec<Self::Details>, ApiError>;

    /// Extract the tags from a single item returned by this cursor
    ///
    /// # Arguments
    ///
    /// * `item` - The item to extract tags from
    fn tags(item: Self::Details) -> TagMap;
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TagCountCursorRetain {
    /// The total number of items that were counted
    pub total: u64,
    /// The specific counts for each Tag
    pub tags: HashMap<String, TagKeyCounts>,
}

impl TagCountCursorRetain {
    /// Create a new ``TagCounts`` object with some default capacity
    ///
    /// # Arguments
    ///
    /// * `capacity` - The capacity to set
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        TagCountCursorRetain {
            total: 0,
            tags: HashMap::with_capacity(capacity),
        }
    }

    /// Add some tags to our count
    ///
    /// # Arguments
    ///
    /// * `tags` - The tags to add to our count
    pub fn add_tags(&mut self, tags: TagMap) {
        // increment our total count
        self.total += 1;
        // step over these tags keys
        for (key, value_map) in tags {
            // get an entry to this keys count
            let key_entry = self.tags.entry(key).or_default();
            // count each of our values
            for (value, _) in value_map {
                // get an entry to this counts map
                let value_entry = key_entry.values.entry(value).or_default();
                // increment this key/value pairs count
                *value_entry += 1;
                // increment the total count for this key
                key_entry.total += 1;
            }
        }
    }
}

pub struct ScyllaTagCountCursor<D: TagCountCursorSupport + Send> {
    /// The id for our tag count cursor
    pub id: Uuid,
    /// The username of the user this cursor is scoped to
    pub username: String,
    /// The info to retain across tag counts
    pub retain: TagCountCursorRetain,
    /// The cursor backing our counts
    backing: ScyllaCursor<D>,
    /// Whether this is a new cursor or it already existed and is being continued
    pub new: bool,
}

impl<D: TagCountCursorSupport + Send> ScyllaTagCountCursor<D> {
    /// Get or create a new tag count cursor
    ///
    /// # Arguments
    ///
    /// * `params` - The params for counting tags
    /// * `extra` - Any extra params needed to count tags
    /// * `username` - The username of the user this cursor is scoped to
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "ScyllaTagCountCursor::from_params", skip_all, err(Debug))]
    pub async fn from_params(
        params: <D as CursorCore>::Params,
        extra: <D as CursorCore>::ExtraFilters,
        username: &str,
        shared: &Shared,
    ) -> Result<Self, ApiError> {
        // get our cursor id if we have one
        let (id, retain, new) = match <D as CursorCore>::get_id(&params) {
            Some(id) => {
                // build the key to our cursor data in redis scoped to the creating user
                let key = cursors::data(CursorKind::TagsCount, username, &id, shared);
                // get our cursor from redis
                let data: Option<String> = query!(cmd("get").arg(key), shared).await?;
                // if we didn't get any data then return a 404
                match data {
                    // deserialize our cursor data
                    Some(data) => (id, deserialize!(&data), false),
                    // this cursor data is missing, so answer as its backing cursor does for an
                    // id that isn't found under this user
                    None => return not_found!(format!("Cursor {id} doesn't exist")),
                }
            }
            None => (
                Uuid::new_v4(),
                TagCountCursorRetain::with_capacity(1000),
                true,
            ),
        };
        // get our backing cursor
        let mut backing = D::backing_cursor(params, extra, username, shared).await?;
        // override our backing cursors id to be the same as our tags count cursor
        backing.id = id;
        // scope our backing cursor to the same user as our tags count cursor
        backing.username = username.to_owned();
        // build our internal tag count cursor
        let internal = ScyllaTagCountCursor {
            id,
            username: username.to_owned(),
            retain,
            backing,
            new,
        };
        Ok(internal)
    }

    /// Count the next page of data
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is counting tags and objects
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "ScyllaTagCountCursor::next", skip_all, err(Debug))]
    pub async fn next(&mut self, user: &User, shared: &Shared) -> Result<(), ApiError> {
        // get the next page of data for this cursor
        self.backing.next(shared).await?;
        // get the details for the current page of data
        let details = D::get_details(user, &mut self.backing, shared).await?;
        // count the tags for each item
        for item in details {
            // get the tags for this item
            let tags = D::tags(item);
            // add the tags to our count
            self.retain.add_tags(tags);
        }
        Ok(())
    }

    /// Save this tag count cursor to redis
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "ScyllaTagCountCursor::save", skip_all, err(Debug))]
    pub async fn save(&self, shared: &Shared) -> Result<(), ApiError> {
        // save our backing cursor
        self.backing.save(shared).await?;
        // serialize our cursor info
        let serialized = serialize!(&self.retain);
        // build the key to save this cursor data too scoped to the creating user
        let key = cursors::data(CursorKind::TagsCount, &self.username, &self.id, shared);
        // if this is a new cursor only hold onto it for 8 hours
        // if its an existing cursor that is being paged then hold onto it for 7 days
        let ttl = if self.new { 28_800 } else { 604_800 };
        // save this cursors data to redis
        let _: () = query!(
            cmd("set").arg(key).arg(serialized).arg("EX").arg(ttl),
            shared
        )
        .await?;
        Ok(())
    }

    /// Check if our count cursor has exhausted its backing cursor
    pub fn exhausted(&self) -> bool {
        self.backing.exhausted()
    }
}

impl<D: TagCountCursorSupport> From<ScyllaTagCountCursor<D>> for TagCounts {
    /// Convert an ``ScyllaTagCountCursor`` to a user facing ``TagCounts``
    ///
    /// # Arguments
    ///
    /// * `cursor` - The internal tag count cursor to convert
    fn from(cursor: ScyllaTagCountCursor<D>) -> Self {
        // if our cursor is not exhausted then return a cursor id
        let cursor_id = if cursor.exhausted() {
            None
        } else {
            Some(cursor.id)
        };
        // build our user facing tag count cursor
        TagCounts {
            cursor: cursor_id,
            total: cursor.retain.total,
            tags: cursor.retain.tags,
        }
    }
}

/// The data to retain throughout this cursors life
#[derive(Serialize, Deserialize, Debug)]
pub struct ElasticCursorRetain {
    /// The timestamp to start listing from for this cursor
    pub start: DateTime<Utc>,
    /// The timestamp to stop listing at for this cursor
    pub end: DateTime<Utc>,
    /// The serialized groups this cursor is searching in
    pub groups: Vec<String>,
    /// The query to send
    pub query: String,
    /// The point in time info for this cursor
    pub pit: String,
    /// The search after values for this cursor
    pub search_after: Vec<i64>,
}

/// A cursor for data in elastic
#[derive(Debug)]
pub struct ElasticCursor {
    /// The id for this cursor
    pub id: Uuid,
    /// The username of the user this cursor is scoped to
    pub username: String,
    /// The info to retain throughout this cursors lifetime
    pub retain: ElasticCursorRetain,
    /// The max number of items to return at once
    pub limit: i64,
    /// The data this cursor has retrieved
    pub data: Vec<ElasticDoc>,
    /// Whether this is a new cursor or it already existed and is being continued
    pub new: bool,
}

/// Build the tenant-isolation group filter for an elastic search
///
/// The group filter is the ONLY tenant boundary on the search path, so it must never collapse to a
/// query that matches every document. In Elasticsearch a `bool` query with zero `should` clauses is
/// rewritten to `match_all`, so a caller that reaches this helper with an empty group list would
/// otherwise search every tenant's documents. This helper is therefore safe by
/// construction: it rejects an empty group list rather than emitting an all-matching filter.
///
/// # Arguments
///
/// * `groups` - The groups this search is scoped to
fn build_group_filter(groups: &[String]) -> Result<serde_json::Value, ApiError> {
    // an empty group list must never produce an empty `should` array, which Elasticsearch
    // rewrites to `match_all` and would leak every tenant's documents. Reject it here so the
    // tenant-isolation filter can never degrade into an all-matching query.
    if groups.is_empty() {
        // refuse to build a filter that would match all documents
        return internal_err!(
            "Refusing to build an elastic group filter with no groups".to_owned()
        );
    }
    // build one exact-match clause per group so only documents in these groups can match
    let group_filters = groups
        .iter()
        .map(|group| {
            // match this group exactly with no fuzziness or synonym expansion
            serde_json::json!({
                "match": {
                    "group": {
                        "query": group,
                        "minimum_should_match": "100%",
                        "fuzziness": "0",
                        "fuzzy_transpositions": false,
                        "auto_generate_synonyms_phrase_query": false
                    }
                }
            })
        })
        .collect::<serde_json::Value>();
    Ok(group_filters)
}

/// Build the `query_string` clause for a user supplied elastic search query
///
/// The raw user query is embedded as the `must` clause of the search. Elasticsearch's `query_string`
/// defaults leave leading wildcards, regex syntax, and unbounded boolean clause chaining enabled
/// across all fields, so a single crafted query can force per-shard term-dictionary enumeration and
/// exhaust the shared cluster's search threadpools. This helper centralizes the clause so
/// the cost-bounding options can be enforced in one place.
///
/// # Arguments
///
/// * `query` - The raw user supplied query string
fn build_query_string_clause(query: &str) -> serde_json::Value {
    // disable leading wildcards so a single query cannot force per-shard term-dictionary
    // enumeration, and bound the automaton expansion of regex/fuzzy terms so a crafted query cannot
    // exhaust the shared cluster's search threadpools
    serde_json::json!({
        "query_string": {
            "query": query,
            // reject leading `*`/`?` wildcards that scan the entire term dictionary
            "allow_leading_wildcard": false,
            // cap the states a single regex/fuzzy term can expand into
            "max_determinized_states": 10_000,
        }
    })
}

impl ElasticCursor {
    /// Create or get an elastic cursor based on search params
    ///
    /// # Arguments
    ///
    /// * `params` - The elastic search params to use
    /// * `username` - The username of the user this cursor is scoped to
    /// * `shared` - Shared Thorium objects
    pub async fn from_params(
        mut params: ElasticSearchParams,
        username: &str,
        shared: &Shared,
    ) -> Result<Self, ApiError> {
        // if a cursor was specified then get an existing cursor
        if let Some(id) = params.cursor.take() {
            // get an existing cursor
            ElasticCursor::get(id, username, params, shared).await
        } else {
            // we don't have an existing cursor so make a new one
            ElasticCursor::new(params, username, shared).await
        }
    }

    /// Create a new elastic cursor
    ///
    /// # Arguments
    ///
    /// * `params` - The elastic search params to use
    /// * `username` - The username of the user this cursor is scoped to
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "ElasticCursor::new", skip(shared), err(Debug))]
    async fn new(
        params: ElasticSearchParams,
        username: &str,
        shared: &Shared,
    ) -> Result<ElasticCursor, ApiError> {
        // get the names of our indexes
        let indexes = params
            .indexes
            .iter()
            .map(|index| index.full_name(&shared.config.elastic))
            .collect::<Vec<&str>>();
        // get a new point in time id for this search
        let pit = elastic::PointInTime::new(&indexes, shared).await?.id;
        // build an intial cursor retained data struct
        let retain = ElasticCursorRetain {
            start: params.start,
            end: params.end(shared)?,
            groups: params.groups,
            query: params.query,
            pit,
            search_after: Vec::default(),
        };
        // build a new elastic cursor
        let cursor = ElasticCursor {
            id: Uuid::new_v4(),
            username: username.to_owned(),
            retain,
            limit: i64::from(params.limit),
            data: Vec::default(),
            new: true,
        };
        Ok(cursor)
    }

    /// Gets a cursors data for elastic
    ///
    /// # Arguments
    ///
    /// * `id` - The ID of the cursor to get
    /// * `username` - The username of the user this cursor is scoped to
    /// * `params` - The elastic search params to use
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "ElasticCursor::get", skip(shared), err(Debug))]
    pub async fn get(
        id: Uuid,
        username: &str,
        params: ElasticSearchParams,
        shared: &Shared,
    ) -> Result<ElasticCursor, ApiError> {
        // build the key to our cursor data in redis scoped to the creating user
        let key = cursors::data(CursorKind::Elastic, username, &id, shared);
        // get our cursor from redis
        let data: Option<String> = query!(cmd("get").arg(key), shared).await?;
        // check if we got any cursor data
        match data {
            Some(data) => {
                // try to deserialize our cursor data
                let retain = deserialize!(&data);
                // build our cursor
                let cursor = ElasticCursor {
                    id,
                    username: username.to_owned(),
                    retain,
                    limit: params.limit.into(),
                    data: Vec::default(),
                    new: false,
                };
                Ok(cursor)
            }
            None => not_found!(format!("cursor {} was not found, perhaps it expired?", id)),
        }
    }

    //// Get the first page of data from elastic
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "ElasticCursor::next", skip_all, fields(query = self.retain.query), err(Debug))]
    pub async fn next(&mut self, shared: &Shared) -> Result<(), ApiError> {
        // if we have no groups then short circuit and do nothing
        if self.retain.groups.is_empty() {
            // log that this user has no groups
            event!(Level::WARN, msg = "User has no groups");
            return Ok(());
        }
        // build the group filters, rejecting an empty group list that would match all documents
        let group_filters = build_group_filter(&self.retain.groups)?;
        // build the user-query clause with whatever cost bounding is enforced
        let query_clause = build_query_string_clause(&self.retain.query);
        // build the correct body depending on if we have any previous sort info or not
        let mut body = serde_json::json!({
            "pit": { "id": self.retain.pit, "keep_alive": "1d" },
            "query": {
                "bool": {
                    "must": query_clause,
                    "filter": [
                        {
                            "bool": {
                                "should": group_filters,
                            }
                        },
                        {
                            "range": {
                                "streamed": {
                                    "gte": self.retain.end,
                                    "lt": self.retain.start,
                                }
                            }
                        }
                    ]
                }
            },
            "highlight": {
                "pre_tags": vec!["@kibana-highlighted-field@"],
                "post_tags": vec!["@/kibana-highlighted-field@"],
                "fields": { "*": {}},
                // set the max analyzed offset to elastic's configured maximum to
                // avoid errors when fields are larger; highlights are truncated instead
                "max_analyzed_offset": shared.config.elastic.max_analyzed_offset,
            }
        });
        // add search after info if it's in our query
        if !self.retain.search_after.is_empty() {
            // we know body is an object, so we're okay to unwrap here
            body.as_object_mut()
                .ok_or(internal_err_unwrapped!(
                    "Elasticsearch query body is not a valid JSON object!".to_string()
                ))?
                .insert("search_after".to_string(), json!(self.retain.search_after));
        }
        // get the next page of docs from elastic
        let resp = shared
            .elastic
            // we don't need to specify an index when using point in time;
            // the point in time contains which index(es) we're searching on
            .search(SearchParts::None)
            .stored_fields(&["*"])
            .size(self.limit)
            .sort(&["streamed:desc", "_shard_doc:desc"])
            .body(body)
            .send()
            .await?;
        // deserialize the response if no error occured
        if resp.status_code().is_success() {
            // get the respose
            let cast = resp.json::<ElasticResponse>().await?;
            // pull out just the hits
            self.data = cast.hits.hits;
        } else {
            // deserialize our error
            let error = resp.json::<elasticsearch::http::response::Error>().await?;
            // return our error
            return Err(ApiError::from(error));
        }
        // update our cursors retained info based on the last item returned
        if let Some(last) = self.data.last() {
            // set the new search after value
            self.retain.search_after.clone_from(&last.sort);
        }
        Ok(())
    }

    /// Saves an elastic cursor to Scylla
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "ElasticCursor::save", skip_all, err(Debug))]
    pub async fn save(&self, shared: &Shared) -> Result<(), ApiError> {
        // serialize our retained info
        let data = serialize!(&self.retain);
        // build the key to save this cursor data too scoped to the creating user
        let key = cursors::data(CursorKind::Elastic, &self.username, &self.id, shared);
        // if this is a new cursor only hold onto it for 8 hours
        // if its an existing cursor that is being paged then hold onto it for 7 days
        let ttl = if self.new { 28_800 } else { 604_800 };
        // save this cursors data to redis
        let _: () = query!(cmd("set").arg(key).arg(data).arg("EX").arg(ttl), shared).await?;
        Ok(())
    }
}

/// The data to retain throughout a grep cursors life
///
/// This holds everything needed to continue a grep search, including the id of the inner
/// list cursor we are paging items with and any keys we listed but have not searched yet.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GrepCursorRetain {
    /// The regex pattern this cursor is searching for
    pub pattern: String,
    /// Whether this search should ignore case or not
    pub case_insensitive: bool,
    /// Whether this pattern can match across multiple lines
    pub multi_line: bool,
    /// The sources of data this cursor is searching
    pub sources: Vec<GrepSource>,
    /// The tools this cursor is restricting result searches too
    pub tools: Vec<String>,
    /// The groups this cursor is searching in
    pub groups: Vec<String>,
    /// The groups the user explicitly requested, before defaulting to all the
    /// groups they can see; used when getting an items results so the default
    /// group intersection semantics are preserved
    #[serde(default)]
    pub requested_groups: Vec<String>,
    /// The timestamp to start searching from for this cursor
    pub start: DateTime<Utc>,
    /// The timestamp to stop searching at for this cursor
    pub end: DateTime<Utc>,
    /// The serialized retained state of the inner list cursor we page items with
    ///
    /// The inner cursors state is embedded here instead of under its own redis key
    /// so one save atomically persists this cursor and the inner one together; a
    /// request dropped mid page can then never lose or split their state, and the
    /// inner state always lives exactly as long as this cursor does.
    pub inner: Option<String>,
    /// Whether the inner list cursor has been exhausted or not
    pub inner_exhausted: bool,
    /// The keys we have listed but not searched yet
    pub pending: VecDeque<String>,
    /// The within item progress to resume if the match budget died mid item
    ///
    /// None means the previous page ended cleanly on an item boundary. Defaulted
    /// so cursors saved before this field existed still deserialize.
    #[serde(default)]
    pub resume: Option<GrepResume>,
}

impl GrepCursorRetain {
    /// Narrow this cursor to the groups a user may still search
    ///
    /// Returns whether any saved group is left.
    ///
    /// # Arguments
    ///
    /// * `allowed` - The groups the user may still search
    fn narrow(&mut self, allowed: &HashSet<String>) -> bool {
        // keep only the groups this cursor pages items in that the user may still search
        self.groups.retain(|group| allowed.contains(group));
        // keep only the requested groups the user may still search, so getting an item's
        // results never asks for a group the user has lost
        self.requested_groups
            .retain(|group| allowed.contains(group));
        !self.groups.is_empty()
    }

    /// Rebuild the grep params for a single page of this cursor
    ///
    /// The rebuilt params use the groups the user explicitly requested rather than
    /// the expanded list we page items with so that getting an items results keeps
    /// its default group intersection semantics.
    ///
    /// These params only ever feed single item scans, so the page oriented fields
    /// (`cursor`, `limit`, and the scan budgets) are inert placeholders; this cursor
    /// enforces the real page limit and budgets itself between items.
    ///
    /// # Arguments
    ///
    /// * `max_matches` - The max number of matches this query can still return
    /// * `with_truncated` - Whether hit-less truncated or errored documents are kept
    #[must_use]
    pub fn params(&self, max_matches: usize, with_truncated: bool) -> GrepParams {
        GrepParams {
            pattern: self.pattern.clone(),
            case_insensitive: self.case_insensitive,
            multi_line: self.multi_line,
            groups: self.requested_groups.clone(),
            start: self.start,
            end: Some(self.end),
            cursor: None,
            // single item scans never read the page limit; use the clamp floor so
            // these params stay valid even if they are ever clamped or paged with
            limit: 1,
            sources: self.sources.clone(),
            tools: self.tools.clone(),
            max_matches: Some(max_matches),
            scan_time: None,
            scan_idle: None,
            with_truncated,
        }
    }
}

/// Build the retained state for a mid page checkpoint of a grep cursor
///
/// A checkpoint restores everything the current page has consumed but not yet
/// returned to the user, so retrying the cursor after a mid page error searches
/// every unreturned item again instead of losing them.
///
/// # Arguments
///
/// * `retain` - The current retained state for this cursor
/// * `consumed` - The keys this page already searched
/// * `page_resume` - The resume state this page started with
fn grep_checkpoint_retain(
    retain: &GrepCursorRetain,
    consumed: &[String],
    page_resume: Option<&GrepResume>,
) -> GrepCursorRetain {
    // start from the current retained state
    let mut checkpoint = retain.clone();
    // restore the keys this page searched to the front in their original order
    for key in consumed.iter().rev() {
        checkpoint.pending.push_front(key.clone());
    }
    // restore the resume state this page started with
    checkpoint.resume = page_resume.cloned();
    checkpoint
}

/// Get the clamped per page time budgets for a grep request
///
/// # Arguments
///
/// * `params` - The grep params to get the time budgets from
fn grep_scan_budgets(params: &GrepParams) -> Result<(Duration, Duration), ApiError> {
    // get the max wall clock time one page can scan for
    let scan_time = match params.scan_time {
        Some(scan_time) => Duration::from_millis(scan_time),
        None => return internal_err!("grep params were not clamped".to_owned()),
    };
    // get the max time one page keeps scanning after its last new match
    let scan_idle = match params.scan_idle {
        Some(scan_idle) => Duration::from_millis(scan_idle),
        None => return internal_err!("grep params were not clamped".to_owned()),
    };
    Ok((scan_time, scan_idle))
}

/// A cursor for a grep based search over many items
#[derive(Debug)]
pub struct GrepCursor {
    /// The id for this cursor
    pub id: Uuid,
    /// The username of the user this cursor is scoped to
    pub username: String,
    /// The info to retain throughout this cursors lifetime
    pub retain: GrepCursorRetain,
    /// The max number of items with matches to return in one page
    pub limit: usize,
    /// The max wall clock time one page can scan for
    pub scan_time: Duration,
    /// The max time one page keeps scanning after its last new match
    pub scan_idle: Duration,
    /// The query wide budget of matches still left
    pub remaining: Arc<AtomicUsize>,
    /// Whether hit-less truncated or errored documents are kept in responses
    pub with_truncated: bool,
    /// The matches this cursor has found
    pub data: Vec<GrepMatches>,
    /// The counts of documents this page scanned
    pub stats: GrepStats,
    /// Whether this is a new cursor or it already existed and is being continued
    pub new: bool,
}

impl GrepCursor {
    /// Create or get a grep cursor based on grep params
    ///
    /// # Arguments
    ///
    /// * `user` - The user this cursor is scoped to
    /// * `params` - The grep params to use
    /// * `requested_groups` - The groups the user explicitly requested before any defaults were applied
    /// * `earliest` - The earliest timestamp an item of the searched type can exist at
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "GrepCursor::from_params", skip(user, shared), err(Debug))]
    pub async fn from_params(
        user: &User,
        mut params: GrepParams,
        requested_groups: Vec<String>,
        earliest: i64,
        shared: &Shared,
    ) -> Result<Self, ApiError> {
        // if a cursor was specified then get an existing cursor
        if let Some(id) = params.cursor.take() {
            // get an existing cursor whose retained data already has the
            // requested groups from when the cursor was created
            GrepCursor::get(id, user, params, shared).await
        } else {
            // we don't have an existing cursor so make a new one
            GrepCursor::new(user, params, requested_groups, earliest)
        }
    }

    /// Create a new grep cursor
    ///
    /// # Arguments
    ///
    /// * `user` - The user this cursor is scoped to
    /// * `params` - The grep params to use
    /// * `requested_groups` - The groups the user explicitly requested before any defaults were applied
    /// * `earliest` - The earliest timestamp an item of the searched type can exist at
    #[instrument(name = "GrepCursor::new", skip(user), err(Debug))]
    fn new(
        user: &User,
        params: GrepParams,
        requested_groups: Vec<String>,
        earliest: i64,
    ) -> Result<Self, ApiError> {
        // get the end timestamp for this cursor or a sane default
        let end = params.end(earliest)?;
        // get the budget of matches this query can return
        let Some(max_matches) = params.max_matches else {
            return internal_err!("grep params were not clamped".to_owned());
        };
        // get the time budgets for this page
        let (scan_time, scan_idle) = grep_scan_budgets(&params)?;
        // build the initial retained data for this cursor
        let retain = GrepCursorRetain {
            pattern: params.pattern,
            case_insensitive: params.case_insensitive,
            multi_line: params.multi_line,
            sources: params.sources,
            tools: params.tools,
            groups: params.groups,
            requested_groups,
            start: params.start,
            end,
            inner: None,
            inner_exhausted: false,
            pending: VecDeque::default(),
            resume: None,
        };
        // build a new grep cursor
        let cursor = GrepCursor {
            id: Uuid::new_v4(),
            username: user.username.clone(),
            retain,
            limit: params.limit,
            scan_time,
            scan_idle,
            remaining: Arc::new(AtomicUsize::new(max_matches)),
            with_truncated: params.with_truncated,
            data: Vec::default(),
            stats: GrepStats::default(),
            new: true,
        };
        Ok(cursor)
    }

    /// Get an existing grep cursors data
    ///
    /// A user who isn't an admin resumes over only the saved groups they may still search,
    /// and the cursor is not found when none are left.
    ///
    /// # Arguments
    ///
    /// * `id` - The id of the cursor to get
    /// * `user` - The user this cursor is scoped to
    /// * `params` - The grep params to use
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "GrepCursor::get", skip(user, shared), err(Debug))]
    pub async fn get(
        id: Uuid,
        user: &User,
        params: GrepParams,
        shared: &Shared,
    ) -> Result<Self, ApiError> {
        // build the key to our cursor data in redis scoped to the creating user
        let key = cursors::data(CursorKind::Grep, &user.username, &id, shared);
        // get our cursor from redis
        let data: Option<String> = query!(cmd("get").arg(key), shared).await?;
        // get the budget of matches this query can return
        let Some(max_matches) = params.max_matches else {
            return internal_err!("grep params were not clamped".to_owned());
        };
        // get the time budgets for this page
        let (scan_time, scan_idle) = grep_scan_budgets(&params)?;
        // check if we got any cursor data
        match data {
            Some(data) => {
                // try to deserialize our cursor data
                let mut retain: GrepCursorRetain = deserialize!(&data);
                // a scoped token, a deleted group, or a user's removal from a group can leave
                // a user able to search fewer groups than were saved with this cursor, so
                // resume over only the saved groups still allowed and answer as for an
                // unknown id when none are left
                if !user.is_admin() {
                    // get the groups this user may still search
                    let allowed: HashSet<String> = user.groups.iter().cloned().collect();
                    // narrow our cursor to them, refusing it when none of its groups are left
                    if !retain.narrow(&allowed) {
                        return not_found!(format!(
                            "cursor {} was not found, perhaps it expired?",
                            id
                        ));
                    }
                }
                // build our cursor
                let cursor = GrepCursor {
                    id,
                    username: user.username.clone(),
                    retain,
                    limit: params.limit,
                    scan_time,
                    scan_idle,
                    remaining: Arc::new(AtomicUsize::new(max_matches)),
                    with_truncated: params.with_truncated,
                    data: Vec::default(),
                    stats: GrepStats::default(),
                    new: false,
                };
                Ok(cursor)
            }
            None => not_found!(format!("cursor {} was not found, perhaps it expired?", id)),
        }
    }

    /// Check if this cursor has been exhausted
    ///
    /// A grep cursor is only exhausted once the inner list cursor is exhausted, every
    /// key it handed us has been searched, and no item is waiting to be resumed.
    #[must_use]
    pub fn exhausted(&self) -> bool {
        self.retain.inner_exhausted
            && self.retain.pending.is_empty()
            && self.retain.resume.is_none()
    }

    /// Grep the next page of items for this cursor
    ///
    /// A page keeps scanning items until it has `limit` items to return, its scan
    /// time budget is spent, its match budget is exhausted, or the corpus runs out.
    /// Once at least one document with actual matches has been found a page also
    /// stops after `scan_idle` passes without finding another, so partially full
    /// pages still return promptly; items with only truncated or errored documents
    /// never arm or reset that cutoff. Both time budgets are checked between items,
    /// so a single slow item can overshoot them by its own scan time.
    ///
    /// Any keys that were listed but not searched are left in `pending` so that
    /// continuing this cursor resumes at the exact item we stopped at. If the match
    /// budget died inside an item then its within item progress is saved in `resume`
    /// and that item is finished first on the next page, skipping the matches it
    /// already returned so no match is ever duplicated or lost.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is grepping these items
    /// * `matcher` - The compiled matcher to search documents with
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "GrepCursor::next", skip_all, err(Debug))]
    pub async fn next<T: GrepSupport>(
        &mut self,
        user: &User,
        matcher: &RegexMatcher,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // if we have no groups then short circuit and do nothing
        if self.retain.groups.is_empty() {
            // log that this user has no groups
            event!(Level::WARN, msg = "User has no groups");
            return Ok(());
        }
        // how many items we have searched in this page
        let mut searched = 0;
        // how many pages of keys we have listed in this page
        let mut listed = 0;
        // the keys this page has already searched, so a mid page checkpoint can
        // restore them; nothing searched this page has been returned to the user yet
        let mut consumed: Vec<String> = Vec::new();
        // the resume state this page started with, so a checkpoint can restore it
        let page_resume = self.retain.resume.clone();
        // the wall clock this pages time budgets are measured against
        let started = Instant::now();
        // when this page last found an item with new matches
        let mut last_found = started;
        // why this page stopped scanning, assuming we run out of items unless a
        // budget breaks out of the loop first
        let mut reason = "exhausted";
        // keep searching items until we fill a page of matches, spend our time or
        // match budgets, or run out of items
        loop {
            // if we have a full page of items with matches then stop scanning
            if self.data.len() >= self.limit {
                reason = "page_full";
                break;
            }
            // only check our time budgets after some progress has been made so even
            // a tiny budget always advances this cursor instead of stalling it
            if (searched > 0 || listed > 0)
                && let Some(spent) = self.spent_time_budget(started, last_found)
            {
                reason = spent;
                break;
            }
            // finish any item whose match budget died mid search on an earlier page first
            if let Some(resume) = self.retain.resume.take() {
                // keep this items key since the resume state is moved into the search
                let key = resume.key.clone();
                // grep the rest of this items documents, skipping already returned matches
                let scan = match self
                    .scan_item::<T>(user, &key, matcher, Some(resume), shared)
                    .await
                {
                    Ok(scan) => scan,
                    // this item was deleted between pages so there is nothing left to resume
                    Err(error) if error.status() == StatusCode::NOT_FOUND => {
                        // log that we dropped the resume state for a deleted item
                        event!(
                            Level::WARN,
                            msg = "Dropped resume state for a missing item",
                            key = key.as_str()
                        );
                        // a deleted item still counts as progress so our time budgets
                        // bound how many keys one request can walk
                        searched += 1;
                        continue;
                    }
                    // any other error fails this page before its state is saved
                    Err(error) => return Err(error),
                };
                // count this item as searched even if it had no new matches
                searched += 1;
                // keep this items matches, and stop if the budget died inside it again
                if self.absorb_scan(scan, &mut last_found) {
                    reason = "match_budget";
                    break;
                }
                continue;
            }
            // if we have no keys left to search then list the next page of them
            if self.retain.pending.is_empty() {
                // if our inner cursor is exhausted then we have nothing left to search
                if self.retain.inner_exhausted {
                    break;
                }
                // list the next page of keys to grep
                let keys = T::list_greppable(user, self, shared).await?;
                // if our inner cursor handed back no cursor id then its exhausted
                self.retain.inner_exhausted = self.retain.inner.is_none();
                // count this listing as progress so our time budgets are now live
                listed += 1;
                // an empty listing can still advance the inner cursor (deduped or
                // sparse buckets), so keep listing until it is exhausted or a time
                // budget breaks instead of handing back a needlessly empty page
                if keys.is_empty() {
                    continue;
                }
                // queue these keys to be searched
                self.retain.pending.extend(keys);
                // checkpoint this cursor so a mid page error cannot lose these keys
                //
                // the advanced inner cursor state is embedded in this cursor, so this
                // one save atomically persists the listed keys and the position they
                // came from. The checkpoint restores everything this page has consumed
                // but not returned yet, so a retry after an error searches every
                // unreturned item again.
                self.save_checkpoint(&consumed, page_resume.as_ref(), shared)
                    .await?;
            }
            // if our match budget is exhausted then stop before searching another item
            if self.remaining.load(AtomicOrdering::Relaxed) == 0 {
                reason = "match_budget";
                break;
            }
            // pop the next key to search
            let Some(key) = self.retain.pending.pop_front() else {
                break;
            };
            // grep this single item
            let scan = match self.scan_item::<T>(user, &key, matcher, None, shared).await {
                Ok(scan) => scan,
                // this item was deleted after it was listed so just skip it
                Err(error) if error.status() == StatusCode::NOT_FOUND => {
                    // log that we skipped a deleted item
                    event!(
                        Level::WARN,
                        msg = "Skipped a missing item",
                        key = key.as_str()
                    );
                    // a deleted item still counts as progress so our time budgets
                    // bound how many keys one request can walk
                    searched += 1;
                    continue;
                }
                // any other error fails this page; the last saved state or checkpoint
                // still has this key pending so a retry searches it again
                Err(error) => return Err(error),
            };
            // remember this key so a mid page checkpoint can restore it
            consumed.push(key);
            // count this item as searched even if it had no matches
            searched += 1;
            // keep this items matches, and stop if the budget died inside it so the
            // saved resume state loses no matches
            if self.absorb_scan(scan, &mut last_found) {
                reason = "match_budget";
                break;
            }
        }
        // note how this page went so long scans are observable
        self.log_page(reason, searched, listed, started);
        Ok(())
    }

    /// Log how a page of this grep cursor went so long scans are observable
    ///
    /// # Arguments
    ///
    /// * `reason` - Why this page stopped scanning
    /// * `searched` - How many items this page searched
    /// * `listed` - How many pages of keys this page listed
    /// * `started` - When this page started scanning
    fn log_page(&self, reason: &str, searched: usize, listed: usize, started: Instant) {
        // log this pages stats and how long it scanned for
        event!(
            Level::INFO,
            msg = "Finished grep page",
            reason,
            searched,
            listed,
            matched = self.data.len(),
            found = self.stats.found,
            truncated = self.stats.truncated,
            errors = self.stats.errors,
            elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        );
    }

    /// Get which time budget this page has spent, if any
    ///
    /// # Arguments
    ///
    /// * `started` - When this page started scanning
    /// * `last_found` - When this page last found an item with new matches
    fn spent_time_budget(&self, started: Instant, last_found: Instant) -> Option<&'static str> {
        // if our scan time budget is spent then stop scanning
        if started.elapsed() >= self.scan_time {
            return Some("scan_time");
        }
        // if we have found matches but haven't found more recently then stop
        // scanning so a partially full page still returns promptly
        //
        // only documents with actual matches arm this cutoff, so a stream of
        // hit-less truncated or errored documents can never end a scan early
        if self.stats.found > 0 && last_found.elapsed() >= self.scan_idle {
            return Some("scan_idle");
        }
        None
    }

    /// Grep one item with the match budget this cursor has left
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is grepping this item
    /// * `key` - The key of the item to grep
    /// * `matcher` - The compiled matcher to search documents with
    /// * `resume` - The within item progress to resume at, if any
    /// * `shared` - Shared Thorium objects
    async fn scan_item<T: GrepSupport>(
        &self,
        user: &User,
        key: &str,
        matcher: &RegexMatcher,
        resume: Option<GrepResume>,
        shared: &Shared,
    ) -> Result<GrepScan, ApiError> {
        // build the params for this item with the budget we have left
        let params = self.retain.params(
            self.remaining.load(AtomicOrdering::Relaxed),
            self.with_truncated,
        );
        // grep this items documents
        grep::grep_item::<T>(
            user,
            key,
            &params,
            matcher,
            self.remaining.clone(),
            resume,
            shared,
        )
        .await
    }

    /// Fold one searched item into this page
    ///
    /// Returns true when the match budget died inside this item, after saving the
    /// state to resume it at.
    ///
    /// # Arguments
    ///
    /// * `scan` - The result of searching this item
    /// * `last_found` - When this page last found an item with new matches
    fn absorb_scan(&mut self, scan: GrepScan, last_found: &mut Instant) -> bool {
        // fold this items document counts into this pages stats
        self.stats.merge(&scan.stats);
        // only keep items that still have documents to return
        if !scan.matches.is_empty() {
            self.data.push(scan.matches);
        }
        // only an item that found actual matches resets our idle cutoff so
        // truncated or errored documents never keep a scan looking alive
        if scan.stats.found > 0 {
            *last_found = Instant::now();
        }
        // if the budget died inside this item then save where to resume
        match scan.resume {
            Some(resume) => {
                self.retain.resume = Some(resume);
                true
            }
            None => false,
        }
    }

    /// Save a mid page checkpoint of this cursor to redis
    ///
    /// Listing items advances the inner list cursor whose state is embedded in this
    /// cursor, so this one save atomically persists the freshly listed keys and the
    /// position they came from. A checkpoint restores everything this page has
    /// consumed but not yet returned, so a retry after a mid page error searches
    /// every unreturned item again instead of silently losing them.
    ///
    /// # Arguments
    ///
    /// * `consumed` - The keys this page already searched
    /// * `page_resume` - The resume state this page started with
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "GrepCursor::save_checkpoint", skip_all, err(Debug))]
    async fn save_checkpoint(
        &self,
        consumed: &[String],
        page_resume: Option<&GrepResume>,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // build the retained state this checkpoint should restore on a retry
        let checkpoint = grep_checkpoint_retain(&self.retain, consumed, page_resume);
        // serialize our checkpoint
        let data = serialize!(&checkpoint);
        // build the key to save this checkpoint too scoped to the creating user
        let key = cursors::data(CursorKind::Grep, &self.username, &self.id, shared);
        // hold onto checkpoints for the same window as a saved cursor
        let ttl = if self.new { 28_800 } else { 604_800 };
        // save this checkpoint to redis
        let _: () = query!(cmd("set").arg(key).arg(data).arg("EX").arg(ttl), shared).await?;
        Ok(())
    }

    /// Save this grep cursor to redis
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "GrepCursor::save", skip_all, err(Debug))]
    pub async fn save(&self, shared: &Shared) -> Result<(), ApiError> {
        // serialize our retained info
        let data = serialize!(&self.retain);
        // build the key to save this cursor data too scoped to the creating user
        let key = cursors::data(CursorKind::Grep, &self.username, &self.id, shared);
        // we only need to save this cursor if it still has items left to search
        if self.exhausted() {
            // delete any existing cursor data, including any mid page checkpoints a
            // new cursor may have written while listing items
            let _: () = query!(cmd("del").arg(key), shared).await?;
        } else {
            // if this is a new cursor only hold onto it for 8 hours
            // if its an existing cursor that is being paged then hold onto it for 7 days
            let ttl = if self.new { 28_800 } else { 604_800 };
            // save this cursors data to redis
            let _: () = query!(cmd("set").arg(key).arg(data).arg("EX").arg(ttl), shared).await?;
        }
        Ok(())
    }
}

/// The core logic all cursors must implement
pub trait RedisCursorSupport: Debug + Serialize + for<'a> Deserialize<'a> {
    /// The params to use when listing data in redis
    type Params: Debug;

    /// The type of data to return when listing without details
    type Names: Debug + Serialize + for<'a> Deserialize<'a>;

    /// The type of data we are sorting on
    type Sort: Debug + Ord + PartialOrd + Serialize + for<'a> Deserialize<'a>;

    /// Get our cursor id from params if one was provided
    ///
    /// # Arguments
    ///
    /// * `params` - The params to get a cursor id from
    fn get_id(params: &Self::Params) -> Option<Uuid>;

    /// Get the amount of data to return at most for this page
    ///
    /// # Arguments
    ///
    /// * `params` - The params ot get limits from
    fn get_limit(params: &Self::Params) -> usize;

    /// Get the groups to return data from
    ///
    /// # Arguments
    ///
    /// * `params` - The params ot get groups from
    fn get_groups(params: &Self::Params) -> &Vec<String>;

    /// Build the keys to a specific groups name set
    ///
    /// # Arguments
    ///
    /// * `params` - The params to build keys from
    /// * `shared` - Shared Thorium objects
    fn name_key(params: &Self::Params, group: &str, shared: &Shared) -> String;

    /// Get the name of items in a single group
    ///
    /// # Arguments
    ///
    /// * `group` - The group to get names in
    #[allow(async_fn_in_trait)]
    async fn get_names(
        params: &Self::Params,
        group_cursor: &RedisGroupCursor,
        shared: &Shared,
    ) -> Result<(u64, Vec<Self::Names>), ApiError>;

    /// Add new data to our cursor
    ///
    /// # Arguments
    ///
    /// * `names` - The names to add to this groups sorted list
    /// * `sorted` - The sorted list to add new names too
    fn add_names(names: Vec<Self::Names>, sorted: &mut BTreeMap<Self::Sort, Self::Names>);
}

/// A Single name row to possibly return to the user
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct RedisCursorName<D: RedisCursorSupport> {
    /// The name to return
    pub name: D::Names,
    /// What group this name came from if it was the last item from this groups page
    pub group: Option<String>,
}

/// A single cursor for a group in redis
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct RedisGroupCursor {
    /// The redis key to use for this cursor
    pub key: String,
    /// The value to use to continue this cursor in redis
    pub cursor: u64,
}

/// Get the initial page of data for a single group
///
/// # Arguments
///
/// * `params` - The parameters for listing data
/// * `group` - The group to list data from
/// * `shared` - Shared Thorium objects
async fn initial_names_helper<D: RedisCursorSupport>(
    params: &D::Params,
    group: String,
    shared: &Shared,
) -> Result<Option<(String, RedisGroupCursor, BTreeMap<D::Sort, D::Names>)>, ApiError> {
    // build the key to the data to list
    let key = D::name_key(params, &group, shared);
    // build a cursor with this key
    // default to 0 as the initial value for the cursor
    let mut group_cursor = RedisGroupCursor { key, cursor: 0 };
    // get this groups names
    let (new_cursor, names) = D::get_names(params, &group_cursor, shared).await?;
    // this group only has no data if its scan completed without returning names
    // an empty batch with a non zero cursor is valid and must keep this group alive
    if names.is_empty() && new_cursor == 0 {
        // this group contains no data
        Ok(None)
    } else {
        // this group contains data so update its cursor
        group_cursor.cursor = new_cursor;
        // instance a btree to sort our names into
        let mut sorted = BTreeMap::default();
        // sort our names into our new btree
        D::add_names(names, &mut sorted);
        // return the info for this group
        Ok(Some((group, group_cursor, sorted)))
    }
}

/// The data to retain throughout this redis cursors life
#[derive(Serialize, Deserialize, Debug, Default)]
#[serde(bound = "")]
pub struct RedisCursorRetain<D: RedisCursorSupport> {
    /// The groups whose redis scans are unfinished and the info to continue those scans
    pub groups: HashMap<String, RedisGroupCursor>,
    /// A sorted set of names to return in future iterations sorted by group
    ///
    /// Every group that may still yield data has an entry here even if its
    /// buffered names are currently empty
    pub remaining: BTreeMap<String, BTreeMap<D::Sort, D::Names>>,
}

impl<D: RedisCursorSupport> RedisCursorRetain<D> {
    /// Get the first page of data
    ///
    /// # Arguments
    ///
    /// * `params` - The parameters used for this cursors retained data
    /// * `shared` - Shared Thorium objects
    pub async fn new(params: &D::Params, shared: &Shared) -> Result<Self, ApiError> {
        // get the names of the groups we need to get data for
        let groups = D::get_groups(params).clone();
        // create a new empty retain to populate data into
        let mut retain = RedisCursorRetain {
            groups: HashMap::with_capacity(groups.len()),
            remaining: BTreeMap::default(),
        };
        // get the next page of data across all groups
        let cursor_iter = stream::iter(groups)
            .map(|group| initial_names_helper::<D>(params, group, shared))
            .buffer_unordered(10)
            .try_collect::<Vec<_>>()
            .await?
            .into_iter()
            .flatten();
        // step over and add any groups with data
        for (group, group_cursor, names) in cursor_iter {
            // only track this groups scan cursor if its scan has not yet completed
            // a cursor of 0 means this groups scan finished in a single pass
            if group_cursor.cursor != 0 {
                // add this group to our cursor
                retain.groups.insert(group.clone(), group_cursor);
            }
            // add this groups names to our cursor
            retain.remaining.insert(group, names);
        }
        Ok(retain)
    }
}

#[derive(Debug)]
pub struct RedisCursor<D: RedisCursorSupport>
where
    for<'de> D: Deserialize<'de> + Debug,
    D: Serialize,
    D: RedisCursorSupport,
    D: Debug,
{
    /// The Id for this cursor
    pub id: Uuid,
    /// The username of the user this cursor is scoped to
    pub username: String,
    /// The cursor settings/data to retain across cursor iterations
    pub retain: RedisCursorRetain<D>,
    /// The params to use with this cursor
    params: D::Params,
    /// The max amount of data to return at once
    pub limit: usize,
    /// Whether this cursor has been saved to redis before
    in_redis: bool,
    /// Whether this is a new cursor or it already existed and is being continued
    pub new: bool,
}

impl<D: RedisCursorSupport> RedisCursor<D> {
    /// Get or create a new redis cursor
    ///
    /// # Arguments
    ///
    /// * `params` - The parameters to use with this cursor
    /// * `username` - The username of the user this cursor is scoped to
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "RedisCursor::new", skip_all, err(Debug))]
    pub async fn new(params: D::Params, username: &str, shared: &Shared) -> Result<Self, ApiError> {
        // if a cursor id was passed then load that existing cursor from redis
        match D::get_id(&params) {
            Some(id) => RedisCursor::get(id, username, params, shared).await,
            None => {
                // get the limit on the amount of data to return
                let limit = D::get_limit(&params);
                // this is a new cursor so seed the first page of every group
                let retain = RedisCursorRetain::<D>::new(&params, shared).await?;
                // build our new cursor with a new id
                let cursor = RedisCursor {
                    id: Uuid::new_v4(),
                    username: username.to_owned(),
                    retain,
                    params,
                    limit,
                    in_redis: false,
                    new: true,
                };
                Ok(cursor)
            }
        }
    }

    /// Gets an existing redis cursors data from redis
    ///
    /// # Arguments
    ///
    /// * `cursor_id` - The id of the cursor to load
    /// * `username` - The username of the user this cursor is scoped to
    /// * `params` - The parameters to use with this cursor
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "RedisCursor::get", skip(params, shared), err(Debug))]
    pub async fn get(
        cursor_id: Uuid,
        username: &str,
        params: D::Params,
        shared: &Shared,
    ) -> Result<Self, ApiError> {
        // build the key to our cursor data in redis scoped to the creating user
        let key = cursors::data(CursorKind::Redis, username, &cursor_id, shared);
        // get our cursor from redis
        let data: Option<String> = query!(cmd("get").arg(key), shared).await?;
        // check if we got any cursor data
        match data {
            Some(data) => {
                // try to deserialize this cursors retained data
                let retain: RedisCursorRetain<D> = deserialize!(&data);
                // rebuild our cursor
                let cursor = RedisCursor {
                    id: cursor_id,
                    username: username.to_owned(),
                    retain,
                    limit: D::get_limit(&params),
                    params,
                    in_redis: true,
                    new: false,
                };
                Ok(cursor)
            }
            None => not_found!(format!("Cursor {cursor_id} doesn't exist")),
        }
    }

    /// Check if this cursor has been exhausted
    pub fn exhausted(&self) -> bool {
        // check if we still have any remaining data to return
        self.retain.remaining.is_empty()
    }

    /// Consume sorted data and add it to an existing vec
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    pub async fn consume(&mut self, shared: &Shared) -> Result<ApiCursor<D::Names>, ApiError>
    where
        D::Names: CursorMetadataSupport,
    {
        // get the correct size to preallocate for our api cursor data
        let size = std::cmp::min(self.limit, self.retain.remaining.len());
        // preallocate the vec of data to return
        let mut data = Vec::with_capacity(size);
        // keep consuming data until we have the requested amount
        while data.len() < self.limit {
            // get a mutable entry to the first group with data
            match self.retain.remaining.first_entry() {
                // we have a group that may have data to return still
                Some(mut first) => {
                    // get the next entry to return to the user
                    // we have to do this in a sub scope in order to avoid multiple
                    // mutable references at once
                    let maybe_next = {
                        // get a mutable entry to this entry
                        let entry = first.get_mut();
                        // try to get the first item from this group
                        entry.pop_first()
                    };
                    // try to get the first item from this group
                    match maybe_next {
                        Some((_, name)) => {
                            // add this name to our return data
                            data.push(name);
                        }
                        None => {
                            // this groups buffered names are drained so check if its
                            // redis scan still has more data to pull
                            // we have to refill before moving to the next group to ensure
                            // we don't return out of order data on the next page
                            match self.retain.groups.get_mut(first.key()) {
                                Some(group_cursor) => {
                                    // this groups scan is unfinished so get its next batch of names
                                    let (new_cursor, names) =
                                        D::get_names(&self.params, group_cursor, shared).await?;
                                    // update this groups cursor
                                    group_cursor.cursor = new_cursor;
                                    // get a mutable entry to this entry
                                    let entry = first.get_mut();
                                    // add these names to our remaining data
                                    D::add_names(names, entry);
                                    // if this scan has now completed then stop tracking its cursor
                                    if new_cursor == 0 {
                                        // get the group for this entry
                                        let group = first.key().clone();
                                        // remove this groups scan cursor since its scan is complete
                                        self.retain.groups.remove(&group);
                                        // if the final batch added no names then this group is
                                        // fully exhausted and can be removed
                                        if first.get().is_empty() {
                                            first.remove_entry();
                                        }
                                    }
                                    // if the batch was empty but the scan is unfinished we simply
                                    // loop and refill this group again on the next iteration
                                }
                                None => {
                                    // this groups scan already completed and its names are
                                    // drained so remove it from our remaining data
                                    first.remove_entry();
                                }
                            }
                        }
                    }
                }
                // we have no more data to return
                None => break,
            }
        }
        // drop any groups that are fully drained and fully scanned so our
        // exhausted check is accurate on exact page boundaries
        let groups = &self.retain.groups;
        self.retain
            .remaining
            .retain(|group, names| !names.is_empty() || groups.contains_key(group));
        // if our cursor is exhausted then don't include a cursor id
        let id = if self.exhausted() {
            None
        } else {
            Some(self.id)
        };
        // build our cursor object
        Ok(ApiCursor {
            cursor: id,
            data,
            metadata: None,
        })
    }

    /// Saves this cursor to Redis
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "RedisCursor::save", skip_all, err(Debug))]
    pub async fn save(&self, shared: &Shared) -> Result<(), ApiError> {
        // build the key to save this cursor data too scoped to the creating user
        let key = cursors::data(CursorKind::Redis, &self.username, &self.id, shared);
        // either save or delete this cursor based on whether its exhausted or not
        if self.exhausted() {
            // only delete if this cursor was actually written to redis
            if self.in_redis {
                // delete this cursor from redis
                let _: () = query!(cmd("del").arg(key), shared).await?;
            }
        } else {
            // serialize our retained data
            let data = serialize!(&self.retain);
            // if this is a new cursor only hold onto it for 8 hours
            // if its an existing cursor that is being paged then hold onto it for 7 days
            let ttl = if self.new { 28_800 } else { 604_800 };
            // save this cursors data to redis
            let _: () = query!(cmd("set").arg(key).arg(data).arg("EX").arg(ttl), shared).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ExistsCursor, GrepCursorRetain, GrepResume, GroupedScyllaCursor, GroupedScyllaCursorRetain,
        GroupedScyllaCursorSupport, ScyllaCursor, ScyllaCursorRetain, SortedLine, TagsRetain,
        build_group_filter, build_query_string_clause, grep_checkpoint_retain,
        scylla_cursor_save_expire,
    };
    use crate::models::schema::{Bucket, CursorLimit, DEFAULT_LIMIT_CAP};
    use crate::models::{
        NetworkPolicyListLine, NetworkPolicyListParams, NetworkPolicyListRow, SampleListLine,
        TagType,
    };
    use crate::utils::ApiError;
    use axum::http::StatusCode;
    use chrono::prelude::*;
    use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
    use uuid::Uuid;

    /// Build a UTC timestamp at midnight on a day
    ///
    /// # Arguments
    ///
    /// * `year` - The year of this timestamp
    /// * `month` - The month of this timestamp
    /// * `day` - The day of this timestamp
    fn day(year: i32, month: u32, day: u32) -> DateTime<Utc> {
        // every test date is a real day, so it always builds
        Utc.with_ymd_and_hms(year, month, day, 0, 0, 0).unwrap()
    }

    /// The bucket a timestamp falls in within a year
    ///
    /// # Arguments
    ///
    /// * `timestamp` - The timestamp
    /// * `year` - The year the bucket is counted in
    /// * `partition_size` - The size of each bucket in seconds
    fn bucket_of(timestamp: DateTime<Utc>, year: i32, partition_size: u16) -> u32 {
        Bucket::of(timestamp, year, partition_size).unwrap().get()
    }

    /// Walk every window an exists cursor queries and collect the buckets checked in each year
    ///
    /// # Arguments
    ///
    /// * `cursor` - The exists cursor to walk
    fn walk(mut cursor: ExistsCursor) -> HashMap<i32, HashSet<u32>> {
        // track the buckets checked in each year
        let mut checked: HashMap<i32, HashSet<u32>> = HashMap::new();
        // cap the walk so a broken cursor can't loop forever
        for _ in 0..100_000 {
            // get the next window and record the buckets it checks
            let (year, buckets, done) = cursor.next_window().unwrap();
            checked
                .entry(year)
                .or_default()
                .extend(buckets.into_iter().map(Bucket::get));
            // stop once the cursor says this was its final window
            if done {
                return checked;
            }
        }
        panic!("the exists cursor walk never finished");
    }

    /// `ExistsCursor::new` computes `end_bucket` as the bucket of the `end` timestamp in
    /// `end_year`. This lets the walk reach the final year's early buckets, so `exists()` checks
    /// them and `prune_data` doesn't delete repo data blobs still referenced by commits there.
    #[test]
    fn exists_cursor_end_bucket_uses_end_timestamp() {
        // check from Sept 24 2026 back to Feb 1 2020 in hour sized partitions
        let start = day(2026, 9, 24);
        let end = day(2020, 2, 1);
        let cursor = ExistsCursor::new(start, end, 3_600).unwrap();
        // the walk must stop at the end timestamp's bucket
        assert_eq!(
            cursor.end_bucket.get(),
            bucket_of(end, 2020, 3_600),
            "the end bucket must be computed from the end timestamp and end year"
        );
    }

    /// Walking an exists cursor from 2026 back to Feb 1 2020 checks the buckets of early 2020
    /// (here March 1 2020), so data referenced only there isn't treated as unreferenced and
    /// deleted.
    #[test]
    fn exists_walk_covers_end_year_early_buckets() {
        // walk from Sept 24 2026 back to Feb 1 2020
        let checked = walk(ExistsCursor::new(day(2026, 9, 24), day(2020, 2, 1), 3_600).unwrap());
        // a commit on March 1 2020 is inside the requested range
        let march = bucket_of(day(2020, 3, 1), 2020, 3_600);
        assert!(
            checked
                .get(&2020)
                .is_some_and(|buckets| buckets.contains(&march)),
            "the exists walk must check every bucket of its final year back to the end timestamp"
        );
    }

    /// Within a single year, walking from Sept 24 back to Jan 15 checks March 1 rather than only
    /// the start's window.
    #[test]
    fn exists_walk_covers_same_year_range() {
        // walk from Sept 24 2026 back to Jan 15 2026
        let checked = walk(ExistsCursor::new(day(2026, 9, 24), day(2026, 1, 15), 3_600).unwrap());
        // a commit on March 1 2026 is inside the requested range
        let march = bucket_of(day(2026, 3, 1), 2026, 3_600);
        assert!(
            checked
                .get(&2026)
                .is_some_and(|buckets| buckets.contains(&march)),
            "the exists walk must check every bucket between its start and end"
        );
    }

    /// Buckets in the years between the start and end years are checked.
    #[test]
    fn exists_walk_covers_middle_years() {
        // walk from Sept 24 2026 back to Feb 1 2020
        let checked = walk(ExistsCursor::new(day(2026, 9, 24), day(2020, 2, 1), 3_600).unwrap());
        // bucket 100 of 2023 lies between the start and end years
        assert!(
            checked
                .get(&2023)
                .is_some_and(|buckets| buckets.contains(&100))
        );
    }

    /// When a cursor walk rolls back into an earlier year it resets its bucket to the last
    /// partition of that year, sized by that year's own length. Rolling back into a leap year
    /// (2024, after the 365 day 2023) therefore starts at bucket 8783 with hourly partitions, so
    /// every partition inside the requested range is checked, including the final hours of
    /// Dec 31 2024.
    #[test]
    fn exists_walk_covers_leap_year_december_31() {
        // walk from Sept 24 2026 back to Feb 1 2020
        let checked = walk(ExistsCursor::new(day(2026, 9, 24), day(2020, 2, 1), 3_600).unwrap());
        // the last hour of 2024 (a leap year) is inside the requested range
        let last_hour = bucket_of(
            Utc.with_ymd_and_hms(2024, 12, 31, 23, 30, 0).unwrap(),
            2024,
            3_600,
        );
        assert!(
            checked
                .get(&2024)
                .is_some_and(|buckets| buckets.contains(&last_hour)),
            "the exists walk must check the final hours of a leap year"
        );
    }

    /// Rolling back into a regular year after a leap year checks that year's final hour.
    #[test]
    fn exists_walk_covers_regular_year_december_31() {
        // walk from Sept 24 2026 back to Feb 1 2020
        let checked = walk(ExistsCursor::new(day(2026, 9, 24), day(2020, 2, 1), 3_600).unwrap());
        // the last hour of 2025 (a regular year after the 2024 leap year) is inside the range
        let last_hour = bucket_of(
            Utc.with_ymd_and_hms(2025, 12, 31, 23, 30, 0).unwrap(),
            2025,
            3_600,
        );
        assert!(
            checked
                .get(&2025)
                .is_some_and(|buckets| buckets.contains(&last_hour))
        );
    }

    /// The last bucket of a year is the bucket of its final second, so it counts the length of
    /// that year: 366 days of 180 second buckets in 2024 and 365 days in 2025.
    #[test]
    fn last_year_bucket_counts_the_entered_year() {
        // 2024 is a leap year with 366 * 480 buckets of 180 seconds
        assert_eq!(Bucket::last_of_year(2024, 180), Ok(Bucket::new(175_679)));
        // 2025 is a regular year with 365 * 480 buckets of 180 seconds
        assert_eq!(Bucket::last_of_year(2025, 180), Ok(Bucket::new(175_199)));
    }

    /// A limit of 0 or above the cap is refused with a 400, and 1 and the cap are accepted
    #[test]
    fn check_limit_bounds() {
        // 0 and one past the cap are bad requests
        for limit in [0, DEFAULT_LIMIT_CAP as usize + 1, usize::MAX] {
            let status = CursorLimit::from_user(limit, DEFAULT_LIMIT_CAP, "limit")
                .map_err(|err| ApiError::from(err).status());
            assert_eq!(status, Err(StatusCode::BAD_REQUEST), "limit {limit}");
        }
        // 1 and the cap itself are allowed
        for limit in [1, DEFAULT_LIMIT_CAP as usize] {
            let checked = CursorLimit::from_user(limit, DEFAULT_LIMIT_CAP, "limit").unwrap();
            assert_eq!(checked.get(), limit);
        }
    }

    /// A limit that doesn't fit a CQL int is a 400, not a wrap or a truncation, and a limit
    /// plus the row cursors read to detect ties always fits
    #[test]
    fn limit_conversions_are_checked() {
        // limits whose next row or whose own value don't fit an int are refused
        for limit in [i32::MAX as usize, u32::MAX as usize + 1] {
            let status = CursorLimit::from_user(limit, DEFAULT_LIMIT_CAP, "limit")
                .map_err(|err| ApiError::from(err).status());
            assert_eq!(status, Err(StatusCode::BAD_REQUEST), "limit {limit}");
        }
        // limits that fit convert unchanged
        let limit = CursorLimit::from_user(50, DEFAULT_LIMIT_CAP, "limit").unwrap();
        assert_eq!(limit.to_cql(), 50);
        assert_eq!(limit.plus_one().to_cql(), 51);
        // what's left of a limit is never 0
        assert_eq!(limit.remaining(49).map(CursorLimit::get), Some(1));
        assert_eq!(limit.remaining(50), None);
    }

    /// Network policy listings refuse an `i32::MAX` limit, whose next row overflows
    #[test]
    fn network_policy_limit_is_capped() {
        // build params asking for i32::MAX rows
        let params = NetworkPolicyListParams {
            limit: i32::MAX as usize,
            ..NetworkPolicyListParams::default()
        };
        let status = NetworkPolicyListLine::get_limit(&params).map_err(|err| err.status());
        assert_eq!(status, Err(StatusCode::BAD_REQUEST));
    }

    /// The group filter must reject an empty group list rather than emit an empty `should`
    /// array that Elasticsearch rewrites to `match_all` (a full cross-tenant search read)
    #[test]
    fn group_filter_rejects_empty_groups() {
        // building a filter with no groups must fail rather than match every tenant's documents
        assert!(
            build_group_filter(&[]).is_err(),
            "empty group list produced a filter (match_all cross-tenant leak)"
        );
    }

    /// A non-empty group list builds one exact-match clause per group
    #[test]
    fn group_filter_builds_a_clause_per_group() {
        // build a filter scoped to two groups
        let filter = build_group_filter(&["team-a".to_owned(), "team-b".to_owned()])
            .expect("non-empty group list should build a filter");
        // the filter must be an array with exactly one clause per group
        let clauses = filter
            .as_array()
            .expect("group filter should be a JSON array of match clauses");
        assert_eq!(
            clauses.len(),
            2,
            "expected one match clause per group in the tenant-isolation filter"
        );
    }

    /// The user `query_string` clause disables leading wildcards, so one crafted query can't
    /// enumerate the term dictionary of the shared indexes
    #[test]
    fn query_string_clause_disables_leading_wildcards() {
        // build the clause for an arbitrary user query
        let clause = build_query_string_clause("password OR flag");
        // dig into the query_string options the clause carries
        let options = clause
            .get("query_string")
            .expect("clause should carry a query_string object");
        // leading wildcards must be explicitly disabled to bound multi-term expansion cost
        assert_eq!(
            options.get("allow_leading_wildcard"),
            Some(&serde_json::Value::Bool(false)),
            "query_string clause did not disable leading wildcards"
        );
    }

    /// `ScyllaCursor::save` holds a new cursor in redis for 8 hours and a paged one for 7 days
    #[test]
    fn scylla_cursor_save_expiry() {
        // a new cursor is only held for 8 hours
        assert_eq!(scylla_cursor_save_expire(true), 28_800);
        // a cursor being paged is held for 7 days
        assert_eq!(scylla_cursor_save_expire(false), 604_800);
    }

    /// Build a listed sample in some groups
    ///
    /// # Arguments
    ///
    /// * `sha256` - The sha256 of this sample
    /// * `groups` - The groups this submission is in
    /// * `uploaded` - When this submission was uploaded
    fn sample(sha256: &str, groups: &[&str], uploaded: DateTime<Utc>) -> SampleListLine {
        SampleListLine {
            groups: groups.iter().map(|group| (*group).to_owned()).collect(),
            sha256: sha256.to_owned(),
            submission: Some(Uuid::new_v4()),
            uploaded,
        }
    }

    /// Build fresh retained data for a sample listing cursor over 2020-2026
    fn sample_retain() -> ScyllaCursorRetain<SampleListLine> {
        ScyllaCursorRetain {
            start: day(2026, 9, 24),
            end: day(2020, 1, 2),
            extra_filter: (),
            group_by: vec!["g1".to_owned(), "g2".to_owned()],
            ties: Vec::new(),
            tags_retain: None,
            dedupe_set: HashSet::new(),
        }
    }

    /// Build a sample listing cursor with some retained data
    ///
    /// # Arguments
    ///
    /// * `retain` - The data this cursor retained
    /// * `limit` - The max number of items to return at once
    /// * `dedupe` - Whether to dedupe items or not
    fn sample_cursor(
        retain: ScyllaCursorRetain<SampleListLine>,
        limit: usize,
        dedupe: bool,
    ) -> ScyllaCursor<SampleListLine> {
        ScyllaCursor {
            id: Uuid::new_v4(),
            username: "user".to_owned(),
            retain,
            year: 2026,
            bucket: Bucket::new(0),
            end_year: 2020,
            end_bucket: Bucket::new(0),
            partition_size: 3_600,
            limit: CursorLimit::from_user(limit, DEFAULT_LIMIT_CAP, "limit").unwrap(),
            dedupe,
            data: Vec::default(),
            sorted: BTreeMap::default(),
            mapped: 0,
            buckets_exhausted: false,
            new: false,
        }
    }

    /// Add lines sharing one timestamp to a cursor's sorted buffer, each read from the
    /// partitions of its groups with its submission as its tie
    ///
    /// # Arguments
    ///
    /// * `cursor` - The cursor to add lines to
    /// * `lines` - The lines to add, all sharing one timestamp
    fn add_sorted(cursor: &mut ScyllaCursor<SampleListLine>, lines: Vec<SampleListLine>) {
        let timestamp = lines[0].uploaded;
        cursor.mapped += lines.len();
        // wrap each line with the tie and partitions its rows were read with
        let sorted = lines.into_iter().map(|line| SortedLine {
            tie: line.submission,
            scopes: line.groups.iter().cloned().collect(),
            line,
        });
        cursor.sorted.entry(timestamp).or_default().extend(sorted);
    }

    /// When a page ends inside a run of rows sharing one timestamp, a tie is recorded for the
    /// partition of every leftover row, not just the front one, so the next page returns the
    /// leftover rows of both groups
    #[test]
    fn consume_sorted_records_ties_for_all_leftover_groups() {
        let mut cursor = sample_cursor(sample_retain(), 1, false);
        // at one timestamp g2 has five rows and g1 has two
        let at = day(2026, 9, 1);
        let mut rows: Vec<SampleListLine> = (0..5)
            .map(|i| sample(&format!("i{i}"), &["g2"], at))
            .collect();
        rows.extend((0..2).map(|i| sample(&format!("j{i}"), &["g1"], at)));
        add_sorted(&mut cursor, rows);
        // fill a page of 1, which leaves rows of both groups whichever row it returns
        cursor.consume_sorted(None);
        // every group's leftover rows at this timestamp get a tie so the next page returns them
        for group in ["g1", "g2"] {
            assert!(
                cursor.retain.ties.iter().any(|(scope, _)| scope == group),
                "{group} has leftover rows at the page boundary but no tie: {:?}",
                cursor.retain.ties
            );
        }
    }

    /// The front leftover row's partition gets a tie at that row and the page is filled
    #[test]
    fn consume_sorted_records_tie_for_front_leftover_group() {
        let mut cursor = sample_cursor(sample_retain(), 4, false);
        let at = day(2026, 9, 1);
        let rows: Vec<SampleListLine> = (0..5)
            .map(|i| sample(&format!("i{i}"), &["g2"], at))
            .collect();
        let ids: Vec<Uuid> = rows.iter().filter_map(|row| row.submission).collect();
        add_sorted(&mut cursor, rows);
        cursor.consume_sorted(None);
        assert_eq!(cursor.data.len(), 4);
        // the row left over is the one the page didn't return
        let returned: Vec<Uuid> = cursor
            .data
            .iter()
            .filter_map(|row| row.submission)
            .collect();
        let last = ids.into_iter().find(|id| !returned.contains(id)).unwrap();
        assert_eq!(cursor.retain.ties, vec![("g2".to_owned(), last)]);
    }

    /// The samples a deduped listing already returned are retained between pages, so a sample
    /// whose submissions fall on different pages (here X) is only returned on the first
    #[test]
    fn dedupe_persists_across_pages() {
        // page 1 returns sample X's newest submission
        let mut page_one = sample_cursor(sample_retain(), 1, true);
        add_sorted(&mut page_one, vec![sample("X", &["g1"], day(2026, 9, 2))]);
        page_one.consume_sorted(None);
        assert_eq!(page_one.data[0].sha256, "X");
        // persist the retained data between requests just like saving to redis does
        let saved = serde_json::to_string(&page_one.retain).unwrap();
        let retain: ScyllaCursorRetain<SampleListLine> = serde_json::from_str(&saved).unwrap();
        // page 2 sees X's older submission followed by sample Z
        let mut page_two = sample_cursor(retain, 1, true);
        add_sorted(&mut page_two, vec![sample("X", &["g1"], day(2026, 8, 2))]);
        add_sorted(&mut page_two, vec![sample("Z", &["g1"], day(2026, 7, 2))]);
        page_two.consume_sorted(None);
        // X was already returned so page 2 must skip it
        let returned: Vec<&str> = page_two
            .data
            .iter()
            .map(|line| line.sha256.as_str())
            .collect();
        assert_eq!(
            returned,
            vec!["Z"],
            "a deduped listing must not repeat X on a later page"
        );
    }

    /// Duplicate submissions of a sample within one page are deduped
    #[test]
    fn dedupe_within_one_page() {
        let mut cursor = sample_cursor(sample_retain(), 2, true);
        add_sorted(&mut cursor, vec![sample("X", &["g1"], day(2026, 9, 2))]);
        add_sorted(&mut cursor, vec![sample("X", &["g1"], day(2026, 8, 2))]);
        add_sorted(&mut cursor, vec![sample("Z", &["g1"], day(2026, 7, 2))]);
        cursor.consume_sorted(None);
        let returned: Vec<&str> = cursor
            .data
            .iter()
            .map(|line| line.sha256.as_str())
            .collect();
        assert_eq!(returned, vec!["X", "Z"]);
    }

    /// Build a listed network policy row in a group
    ///
    /// # Arguments
    ///
    /// * `group` - The group this row is in
    /// * `name` - The name of this network policy
    fn policy_row(group: &str, name: &str) -> NetworkPolicyListRow {
        NetworkPolicyListRow {
            group: group.to_owned(),
            name: name.to_owned(),
            id: Uuid::new_v4(),
        }
    }

    /// Build a network policy listing cursor over groups g1 and g2
    ///
    /// # Arguments
    ///
    /// * `limit` - The max number of items to return at once
    /// * `ties` - The ties retained from a previous page
    fn policy_cursor(
        limit: usize,
        ties: Vec<(String, Vec<String>)>,
    ) -> GroupedScyllaCursor<NetworkPolicyListLine> {
        GroupedScyllaCursor {
            id: Uuid::new_v4(),
            username: "user".to_owned(),
            retain: GroupedScyllaCursorRetain {
                extra_filter: (),
                group_by: ["g1".to_owned(), "g2".to_owned()].into_iter().collect(),
                ties,
                current_sort_by: Some("b".to_owned()),
            },
            limit: CursorLimit::from_user(limit, DEFAULT_LIMIT_CAP, "limit").unwrap(),
            in_redis: true,
            data: Vec::default(),
            new: false,
        }
    }

    /// Consumed grouped ties are cleared even when no new leftover exists, so later pages don't
    /// return the same tied entity (here "c") again
    #[test]
    fn grouped_cursor_clears_consumed_ties() {
        // page 1 left entity c behind as a tie
        let mut cursor = policy_cursor(2, vec![("c".to_owned(), vec!["g1".to_owned()])]);
        // page 2's tie query returns c, which fits on this page with room to spare
        let filled = cursor.consume_tied_rows(vec![policy_row("g1", "c")]);
        assert!(!filled);
        assert_eq!(cursor.data.len(), 1);
        // the consumed tie must not be queried again on the next page
        assert!(
            cursor.retain.ties.iter().all(|(sort_by, _)| sort_by != "c"),
            "a consumed tie must be cleared, got {:?}",
            cursor.retain.ties
        );
    }

    /// When tied rows overflow the page, the leftover is recorded as a tie
    #[test]
    fn grouped_cursor_records_leftover_tie() {
        let mut cursor = policy_cursor(1, Vec::default());
        let filled = cursor.consume_tied_rows(vec![policy_row("g1", "c"), policy_row("g2", "c")]);
        assert!(filled);
        assert_eq!(
            cursor.retain.ties,
            vec![("c".to_owned(), vec!["g2".to_owned()])]
        );
    }

    /// Grep cursors saved before resume state existed must still deserialize.
    #[test]
    fn grep_cursor_retain_defaults_resume_for_old_cursors() {
        // build the JSON an older cursor would have saved before resume existed
        let old = serde_json::json!({
            "pattern": "corn",
            "case_insensitive": false,
            "multi_line": false,
            "sources": ["Results"],
            "tools": [],
            "groups": ["corn-group"],
            "requested_groups": [],
            "start": "2026-01-01T00:00:00Z",
            "end": "2025-01-01T00:00:00Z",
            "inner": null,
            "inner_exhausted": false,
            "pending": ["corn-key"],
        });
        // an old cursor should still deserialize with no resume state
        let retain: GrepCursorRetain = serde_json::from_value(old).unwrap();
        assert!(retain.resume.is_none());
        // and its other retained info should survive untouched
        assert_eq!(retain.pattern, "corn");
        assert_eq!(retain.pending, VecDeque::from(["corn-key".to_owned()]));
    }

    /// Mid page checkpoints restore consumed keys and the page start resume state.
    #[test]
    fn grep_checkpoint_restores_consumed_state() {
        // build the retained state partway through a page
        let retain = GrepCursorRetain {
            pattern: "corn".to_owned(),
            case_insensitive: false,
            multi_line: false,
            sources: Vec::default(),
            tools: Vec::default(),
            groups: vec!["corn-group".to_owned()],
            requested_groups: Vec::default(),
            start: Utc::now(),
            end: Utc::now(),
            inner: None,
            inner_exhausted: false,
            pending: VecDeque::from(["k3".to_owned(), "k4".to_owned()]),
            resume: None,
        };
        // this page already searched two keys and finished the resume item
        let consumed = vec!["k1".to_owned(), "k2".to_owned()];
        let page_resume = Some(GrepResume {
            key: "k0".to_owned(),
            docs: Vec::default(),
        });
        // build a checkpoint of this page
        let checkpoint = grep_checkpoint_retain(&retain, &consumed, page_resume.as_ref());
        // consumed keys should be restored to the front in their original order
        assert_eq!(
            checkpoint.pending,
            VecDeque::from([
                "k1".to_owned(),
                "k2".to_owned(),
                "k3".to_owned(),
                "k4".to_owned()
            ])
        );
        // the resume state this page started with should be restored
        assert_eq!(
            checkpoint.resume.as_ref().map(|resume| resume.key.as_str()),
            Some("k0")
        );
    }

    /// Grep cursors embed their inner list cursors retained state as a blob.
    #[test]
    fn grep_retain_round_trips_inner_blob() {
        // serialize an inner list cursors retained state into a blob, including
        // the dedupe set that keeps later pages from repeating items
        let mut inner = sample_retain();
        inner.dedupe_set.insert("corn-sha".to_owned());
        let blob = serde_json::to_string(&inner).unwrap();
        // embed that blob in a grep cursors retained state
        let retain = GrepCursorRetain {
            pattern: "corn".to_owned(),
            case_insensitive: false,
            multi_line: false,
            sources: Vec::default(),
            tools: Vec::default(),
            groups: vec!["corn-group".to_owned()],
            requested_groups: Vec::default(),
            start: Utc::now(),
            end: Utc::now(),
            inner: Some(blob),
            inner_exhausted: false,
            pending: VecDeque::default(),
            resume: None,
        };
        // round trip the grep cursors retained state like saving to redis does
        let saved = serde_json::to_string(&retain).unwrap();
        let restored: GrepCursorRetain = serde_json::from_str(&saved).unwrap();
        // the embedded inner state should restore into the same list cursor state
        let blob = restored.inner.unwrap();
        let rebuilt: ScyllaCursorRetain<SampleListLine> = serde_json::from_str(&blob).unwrap();
        assert_eq!(rebuilt.start, inner.start);
        assert_eq!(rebuilt.end, inner.end);
        assert_eq!(rebuilt.group_by, inner.group_by);
        // the dedupe set must survive so items are never repeated across pages
        assert!(rebuilt.dedupe_set.contains("corn-sha"));
    }

    /// Build the set of groups a request may list
    ///
    /// # Arguments
    ///
    /// * `groups` - The groups
    fn allowed(groups: &[&str]) -> HashSet<String> {
        groups.iter().map(|group| (*group).to_owned()).collect()
    }

    /// Narrowing a time cursor to the groups a request may list drops the other groups and the
    /// ties of their partitions, so a tie in a dropped group is never read
    #[test]
    fn narrow_drops_other_groups_and_their_ties() {
        // a cursor over g1 and g2 that ended inside a tie in each
        let (kept, dropped) = (Uuid::new_v4(), Uuid::new_v4());
        let mut retain = sample_retain();
        retain.ties = vec![("g1".to_owned(), kept), ("g2".to_owned(), dropped)];
        // narrow it to g1
        assert!(retain.narrow(&allowed(&["g1", "g3"])));
        assert_eq!(retain.group_by, vec!["g1".to_owned()]);
        assert_eq!(retain.ties, vec![("g1".to_owned(), kept)]);
    }

    /// Narrowing a tag listing drops the tag ties of the other groups
    #[test]
    fn narrow_drops_other_groups_tag_ties() {
        // a tag listing over g1 and g2 that ended inside a tie in each
        let mut retain = sample_retain();
        retain.tags_retain = Some(TagsRetain {
            tags: HashMap::new(),
            tag_type: TagType::Files,
            tags_required: 1,
            case_insensitive: false,
            ties: [("g1", "a"), ("g2", "b")]
                .into_iter()
                .map(|(group, tie)| (group.to_owned(), tie.to_owned()))
                .collect(),
        });
        // narrow it to g1
        assert!(retain.narrow(&allowed(&["g1"])));
        let ties = retain.tags_retain.map_or_default(|tags| tags.ties);
        assert_eq!(ties.keys().collect::<Vec<_>>(), vec!["g1"]);
    }

    /// A time cursor with none of its groups left is reported as having nothing to resume
    #[test]
    fn narrow_to_no_saved_group_is_empty() {
        let mut retain = sample_retain();
        assert!(!retain.narrow(&allowed(&["g3"])));
        assert_eq!(retain.group_by, Vec::<String>::new());
    }

    /// Narrowing a grouped cursor drops the other groups from its ties, and the ties left with
    /// no group, and reports when no saved group is left
    #[test]
    fn narrow_grouped_drops_other_groups_from_ties() {
        // a cursor over g1 and g2 that left "c" tied in both and "d" tied in g2
        let mut cursor = policy_cursor(
            1,
            vec![
                ("c".to_owned(), vec!["g1".to_owned(), "g2".to_owned()]),
                ("d".to_owned(), vec!["g2".to_owned()]),
            ],
        );
        // narrow it to g1
        assert!(cursor.retain.narrow(&allowed(&["g1"])));
        assert_eq!(cursor.retain.group_by, allowed(&["g1"]));
        assert_eq!(
            cursor.retain.ties,
            vec![("c".to_owned(), vec!["g1".to_owned()])]
        );
        // narrowing it to a group it wasn't saved over leaves nothing to resume
        assert!(!cursor.retain.narrow(&allowed(&["g3"])));
    }

    /// The group of a tie's scope is its first column when the scope also holds `IN` filters
    #[test]
    fn scope_group_reads_the_group_of_a_filtered_scope() {
        use crate::models::schema::TimeCursorStatements;
        use crate::models::schema::queries::{CommitishListListCursor, EntitiesListCursor};
        use crate::models::{CommitishKinds, EntityKinds};
        // entity scopes hold the group and the kind
        let scope = ("g1".to_owned(), EntityKinds::Vendor);
        assert_eq!(EntitiesListCursor::scope_group(&scope), "g1");
        // commitish scopes hold the group and the kind
        let scope = ("g2".to_owned(), CommitishKinds::Commit);
        assert_eq!(CommitishListListCursor::scope_group(&scope), "g2");
    }

    /// Build the retained state of a grep cursor over some groups
    ///
    /// # Arguments
    ///
    /// * `groups` - The groups the cursor pages items in
    /// * `requested` - The groups the user explicitly requested
    fn grep_retain(groups: &[&str], requested: &[&str]) -> GrepCursorRetain {
        // own our group names
        let own = |names: &[&str]| names.iter().map(|name| (*name).to_owned()).collect();
        GrepCursorRetain {
            pattern: "corn".to_owned(),
            case_insensitive: false,
            multi_line: false,
            sources: Vec::default(),
            tools: Vec::default(),
            groups: own(groups),
            requested_groups: own(requested),
            start: Utc::now(),
            end: Utc::now(),
            inner: None,
            inner_exhausted: false,
            pending: VecDeque::from(["k1".to_owned()]),
            resume: None,
        }
    }

    /// Narrowing a grep cursor keeps only the paged and requested groups still allowed, and
    /// leaves the rest of its state alone
    #[test]
    fn grep_narrow_drops_other_groups() {
        // a grep cursor over g1 and g2 that the user explicitly requested
        let mut retain = grep_retain(&["g1", "g2"], &["g1", "g2"]);
        // narrow it to g1
        assert!(retain.narrow(&allowed(&["g1", "g3"])));
        assert_eq!(retain.groups, vec!["g1".to_owned()]);
        assert_eq!(retain.requested_groups, vec!["g1".to_owned()]);
        assert_eq!(retain.pending, VecDeque::from(["k1".to_owned()]));
    }

    /// Narrowing a grep cursor whose groups defaulted to all of the user's keeps the requested
    /// groups empty, so getting an item's results keeps its default group semantics
    #[test]
    fn grep_narrow_keeps_defaulted_requested_groups_empty() {
        // a grep cursor over g1 and g2 whose groups defaulted to all of the user's
        let mut retain = grep_retain(&["g1", "g2"], &[]);
        // narrow it to g2
        assert!(retain.narrow(&allowed(&["g2"])));
        assert_eq!(retain.groups, vec!["g2".to_owned()]);
        assert_eq!(retain.requested_groups, Vec::<String>::new());
    }

    /// A grep cursor with none of its groups left is reported as having nothing to resume
    #[test]
    fn grep_narrow_to_no_saved_group_is_empty() {
        // a grep cursor over g1 and g2 that the user explicitly requested
        let mut retain = grep_retain(&["g1", "g2"], &["g1", "g2"]);
        // narrowing it to a group it wasn't saved over leaves nothing to resume
        assert!(!retain.narrow(&allowed(&["g3"])));
        assert_eq!(retain.groups, Vec::<String>::new());
        assert_eq!(retain.requested_groups, Vec::<String>::new());
    }

    /// The largest grep page a config may set is the largest page a list cursor serves, since
    /// a grep page lists its items with one
    #[test]
    fn grep_max_items_cap_is_the_listing_cap() {
        // the config's cap and the listing cap are the same number of rows
        assert_eq!(
            crate::conf::GREP_MAX_ITEMS_CAP,
            usize::try_from(DEFAULT_LIMIT_CAP).unwrap()
        );
    }
}
