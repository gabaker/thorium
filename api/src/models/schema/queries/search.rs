//! The statements the search streamer reads results and tags with
//!
//! The streamer pages through each statement's rows, enumerating partitions one token range at a
//! time and reading the rows of each chunk of keys it finds.

use std::collections::HashMap;
use uuid::Uuid;

use crate::models::schema::ScyllaProjection;
use crate::models::schema::scylla::{
    ResultsAuthView, ResultsIdsView, ResultsTable, TagsByItemView,
};
use crate::models::{OutputKind, TagRow, TagType};

/// A result key found by enumerating the results by key
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = ResultsIdsView)]
pub struct ResultEnumerateRow {
    /// The sample sha256 or repo url the results are for
    pub key: String,
}

/// A result's id with its body, files, and children
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = ResultsTable)]
pub struct ResultRow {
    /// The result's id
    pub id: Uuid,
    /// The result
    pub result: String,
    /// The files associated with this result
    pub files: Vec<String>,
    /// The children found by the tool
    pub children: HashMap<String, Uuid>,
}

/// The result info a key's results are streamed with in the init phase
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = ResultsIdsView)]
pub struct ResultInitDataRow {
    /// The key the result pertains to
    pub key: String,
    /// The kind of item the result pertains to
    pub kind: OutputKind,
    /// The group the result is in
    pub group: String,
    /// The result's id
    pub id: Uuid,
}

/// The result info an event's results are streamed with
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = ResultsAuthView)]
pub struct ResultEventInfoRow {
    /// The group the result is in
    pub group: String,
    /// The result's id
    pub id: Uuid,
}

/// A tagged item found by enumerating the tags by item
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = TagsByItemView)]
pub struct TagEnumerateRow {
    /// The kind of item it is
    #[thorium(column = "type")]
    pub kind: TagType,
    /// The item key itself
    pub item: String,
}

/// A tag an event's tags are streamed with
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = TagsByItemView)]
pub struct TagEventRow {
    /// The group the event pertains to
    pub group: String,
    /// The tag key
    pub key: String,
    /// The tag value
    pub value: String,
}

/// The search streamer's statements on the `results_ids` view
#[thorium::view_queries(view = ResultsIdsView)]
#[derive(Clone)]
pub struct SearchResultsIdsQueries {
    /// The keys with results in a token range, each once
    #[select(project = ResultEnumerateRow, distinct, token_range, paged)]
    enumerate: Select,
    /// The groups and ids of the results for a set of keys
    #[select(project = ResultInitDataRow, r#in(key), paged)]
    init_data: Select,
}

/// The search streamer's statements on the results table
#[thorium::table_queries(table = ResultsTable)]
#[derive(Clone)]
pub struct SearchResultsQueries {
    /// The bodies, files, and children of a set of results
    #[select(project = ResultRow, r#in(id), paged)]
    results: Select,
}

/// The search streamer's statements on the `results_auth` view
#[thorium::view_queries(view = ResultsAuthView)]
#[derive(Clone)]
pub struct SearchResultsAuthQueries {
    /// The groups and ids of a key's results in a set of groups
    #[select(project = ResultEventInfoRow, eq(key, kind), r#in(group), paged)]
    event_data: Select,
}

/// The search streamer's statements on the `tags_by_item` view
#[thorium::view_queries(view = TagsByItemView)]
#[derive(Clone)]
pub struct SearchTagsByItemQueries {
    /// The tagged items in a token range, each once
    #[select(project = TagEnumerateRow, distinct, token_range, paged)]
    enumerate: Select,
    /// The tags of a set of items of one kind
    #[select(project = TagRow, eq(tag_type), r#in(item), paged)]
    init: Select,
    /// The tags of an item in a set of groups
    #[select(project = TagEventRow, eq(tag_type, item), r#in(group), paged)]
    event: Select,
}

/// Every search streamer statement's text for a keyspace, named `search.<object>.<statement>`
///
/// # Arguments
///
/// * `ns` - The keyspace the statements target
#[must_use]
pub fn statements(ns: &str) -> Vec<(String, String)> {
    // list each queries struct's statements under its object's name
    let groups = [
        ("results_ids", SearchResultsIdsQueries::statements(ns)),
        ("results", SearchResultsQueries::statements(ns)),
        ("results_auth", SearchResultsAuthQueries::statements(ns)),
        ("tags_by_item", SearchTagsByItemQueries::statements(ns)),
    ];
    groups
        .into_iter()
        .flat_map(|(object, statements)| {
            statements
                .into_iter()
                .map(move |(name, cql)| (format!("search.{object}.{name}"), cql))
        })
        .collect()
}
