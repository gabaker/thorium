//! The statements on the results, result stream, and tags tables and their views

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::models::schema::scylla::{
    ResultsAuthView, ResultsIdsView, ResultsStreamTable, ResultsTable, TagsByItemView,
    TagsCaseInsensitiveView, TagsTable,
};
use crate::models::{
    FullTagRow, OutputIdRow, OutputRow, ResultIdRow, ResultUploadedRow, TagBackupRow, TagListRow,
    TagRow,
};

/// Statements on the results table
#[thorium::table_queries(table = ResultsTable)]
pub struct ResultsQueries {
    /// Insert a result, leaving the children found from it unset
    #[insert(
        columns = [id, uploaded, tool, tool_version, cmd, result, files, entities, display_type],
        ttl = none
    )]
    insert: Insert,
    /// Every result with one of a set of ids
    #[select(project = OutputRow, r#in(id))]
    get: Select,
    /// The upload time of every result with one of a set of ids
    #[select(project = ResultUploadedRow, r#in(id))]
    get_uploaded_by_id: Select,
    /// Delete every row of a result
    #[delete(partition, eq(id))]
    delete: Delete,
    /// Add a child sample to a result's children map, an update of one map entry
    #[query(
        "UPDATE {ns}.results SET children[?] = ? WHERE id = ? AND uploaded = ?",
        binds(sha256: &str, submission: Uuid, id: Uuid, uploaded: DateTime<Utc>)
    )]
    update_children: Raw,
}

/// Statements on the `results_stream` table
#[thorium::table_queries(table = ResultsStreamTable)]
pub struct ResultsStreamQueries {
    /// Insert or overwrite a result's stream row for a group
    #[insert(columns = all, ttl = none)]
    insert_stream: Insert,
    /// Delete a result's stream rows for a group
    #[delete(prefix, eq(kind, group, year, bucket, uploaded, id))]
    delete_stream: Delete,
}

/// Statements on the `results_auth` view, which finds results by key
#[thorium::view_queries(view = ResultsAuthView)]
pub struct ResultsAuthQueries {
    /// The ids of a tool's results for a key in a set of groups
    #[select(project = ResultIdRow, eq(key, kind), r#in(group), eq(tool))]
    get_id: Select,
    /// The ids and details of every result for a key in a set of groups
    #[select(project = OutputIdRow, eq(key, kind), r#in(group))]
    get_with_key: Select,
    /// The ids and details of some tools' results for a key in a set of groups
    #[select(project = OutputIdRow, eq(key, kind), r#in(group, tool), chunk = 10)]
    get_with_key_and_tool: Select,
}

/// Statements on the `results_ids` view, which finds result ids by key
#[thorium::view_queries(view = ResultsIdsView)]
pub struct ResultsIdsQueries {
    /// The rows that still reach a result from a key
    #[select(project = ResultIdRow, eq(key, id, kind))]
    count: Select,
}

/// Statements on the tags table
#[thorium::table_queries(table = TagsTable)]
pub struct TagsQueries {
    /// Insert or overwrite one tag on an item in a group
    #[insert(columns = all, ttl = none)]
    insert: Insert,
    /// Delete one tag on an item in a group
    #[delete(prefix, eq(tag_type, group, year, bucket, key, value, uploaded, item))]
    delete: Delete,
    /// The cursor paging through the items with one tag, newest first across groups
    #[cursor(kind = time, group_by = group, eq(tag_type, key, value), year = year,
             bucket = bucket, range = uploaded, tie = item, project = TagListRow, limit = none,
             paged)]
    list: Cursor,
    /// The number of tags in each partition of a token range, for the census
    #[census(project = crate::models::tags::TagCensusRow)]
    census: Census,
    /// Every tag in a token range but its lower-cased key and value, for backups
    #[select(project = TagBackupRow, token_range)]
    backup: Select,
}

/// Statements on the `tags_case_insensitive` view, which finds tags by their lowercased key
/// and value
#[thorium::view_queries(view = TagsCaseInsensitiveView)]
pub struct TagsCaseInsensitiveQueries {
    /// The cursor paging through the items with one tag in any case, across groups
    #[cursor(kind = time, group_by = group, eq(tag_type, key_lower, value_lower), year = year,
             bucket = bucket, range = uploaded, tie = item, project = TagListRow, limit = none,
             paged)]
    list: Cursor,
    /// The number of tags in each partition of a token range, for the case-insensitive census
    #[census(project = crate::models::tags::TagCensusCaseInsensitiveRow)]
    census: Census,
}

/// Statements on the `tags_by_item` view, which finds tags by the item they're on
#[thorium::view_queries(view = TagsByItemView)]
pub struct TagsByItemQueries {
    /// The tags on an item in a set of groups
    #[select(project = TagRow, eq(tag_type), r#in(group), eq(item))]
    get: Select,
    /// The tag rows on an item in a set of groups, with their time buckets
    #[select(project = FullTagRow, eq(tag_type), r#in(group), eq(item))]
    get_rows: Select,
}
