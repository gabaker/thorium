//! The Scylla tables and views for tags on samples, repos, and entities

use chrono::{DateTime, Utc};

use crate::models::TagType;
use crate::models::schema::{ScyllaTable, ScyllaView};

/// Tags on samples, repos and entities, partitioned by (type, group, year, bucket, key, value) for listing by tag
#[derive(ScyllaTable)]
#[thorium_table(name = "tags")]
pub struct TagsTable {
    /// The kind of item this tag is on (Files, Repos, Entities)
    #[thorium(partition, column = "type")]
    pub tag_type: TagType,
    /// The group this tag is visible to
    #[thorium(partition)]
    pub group: String,
    /// The year of the earliest time this group saw the item
    #[thorium(partition, year)]
    pub year: i32,
    /// The time bucket (`tags.<type>.partition_size`) of that earliest time
    #[thorium(partition, bucket(partition_size = "tags.map_type(type)"))]
    pub bucket: i32,
    /// The tag key
    #[thorium(partition)]
    pub key: String,
    /// The tag value
    #[thorium(partition)]
    pub value: String,
    /// The earliest time this group saw the item (not the time the tag was added)
    #[thorium(cluster = "desc", timestamp)]
    pub uploaded: DateTime<Utc>,
    /// The tagged item (sha256, repo url, or entity id)
    #[thorium(cluster = "asc")]
    pub item: String,
    /// The lowercased tag key
    #[thorium(cluster = "asc")]
    pub key_lower: String,
    /// The lowercased tag value
    #[thorium(cluster = "asc")]
    pub value_lower: String,
}
/// Tags by item, for reading every tag on one item
#[derive(ScyllaView)]
#[thorium_view(name = "tags_by_item", base = TagsTable)]
pub struct TagsByItemView {
    /// The kind of item
    #[thorium(partition, column = "type")]
    pub tag_type: TagType,
    /// The tagged item
    #[thorium(partition)]
    pub item: String,
    /// The group the tag is visible to
    #[thorium(cluster = "asc")]
    pub group: String,
    /// The year of the bucket
    #[thorium(cluster = "asc")]
    pub year: i32,
    /// The time bucket
    #[thorium(cluster = "asc")]
    pub bucket: i32,
    /// The earliest time the group saw the item
    #[thorium(cluster = "asc")]
    pub uploaded: DateTime<Utc>,
    /// The tag key
    #[thorium(cluster = "asc")]
    pub key: String,
    /// The tag value
    #[thorium(cluster = "asc")]
    pub value: String,
    /// The lowercased tag key
    #[thorium(cluster = "asc")]
    pub key_lower: String,
    /// The lowercased tag value
    #[thorium(cluster = "asc")]
    pub value_lower: String,
}
/// Tags partitioned by lowercased key/value, for case-insensitive listing by tag
#[derive(ScyllaView)]
#[thorium_view(name = "tags_case_insensitive", base = TagsTable)]
pub struct TagsCaseInsensitiveView {
    /// The kind of item
    #[thorium(partition, column = "type")]
    pub tag_type: TagType,
    /// The group the tag is visible to
    #[thorium(partition)]
    pub group: String,
    /// The year of the bucket
    #[thorium(partition, year)]
    pub year: i32,
    /// The time bucket
    #[thorium(partition, bucket(partition_size = "tags.map_type(type)"))]
    pub bucket: i32,
    /// The lowercased tag key
    #[thorium(partition)]
    pub key_lower: String,
    /// The lowercased tag value
    #[thorium(partition)]
    pub value_lower: String,
    /// The earliest time the group saw the item (ascending here, unlike the base table)
    #[thorium(cluster = "asc", timestamp)]
    pub uploaded: DateTime<Utc>,
    /// The tagged item
    #[thorium(cluster = "asc")]
    pub item: String,
    /// The original-case tag key
    #[thorium(cluster = "asc")]
    pub key: String,
    /// The original-case tag value
    #[thorium(cluster = "asc")]
    pub value: String,
}
