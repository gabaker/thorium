//! Backup and restore support for tags

use ahash::AHasher;
use bytecheck::CheckBytes;
use chrono::prelude::*;
use rkyv::{Archive, Deserialize, Serialize};
use scylla::SerializeRow;
use std::hash::Hasher;
use thorium::Conf;
use thorium::models::backends::TagSupport;
use thorium::models::{TagRequest, TagType};

use super::Versioned;
use crate::backup::{Backup, Restore, Scrub, Utils};
use thorium::models::schema::ScyllaProjection;
use thorium::models::schema::queries::TagsQueries;
use thorium::models::schema::scylla::TagsTable;

/// A single line of stage logs
#[derive(Debug, Archive, Serialize, Deserialize, ScyllaProjection)]
#[archive_attr(derive(Debug, CheckBytes))]
#[thorium_projection(of = TagsTable)]
pub struct Tag {
    /// The type of object this tag is for
    #[thorium(column = "type")]
    pub tag_type: TagType,
    /// The group this tag is a part of
    pub group: String,
    /// The year this tag was submitted
    pub year: i32,
    /// The bucket this tag was submitted in
    pub bucket: i32,
    /// The key for this tag
    pub key: String,
    /// The value for this tag
    pub value: String,
    /// The timestamp this tag was submitted
    pub uploaded: DateTime<Utc>,
    /// The item we are getting tags for
    pub item: String,
}

impl Utils for Tag {
    /// The name of the table we are backing up
    fn name() -> &'static str {
        "tags"
    }
}

#[async_trait::async_trait]
impl Backup for Tag {
    /// The table this archive struct backs up, read through its generated statements
    type Table = TagsTable;

    /// The generated statement reading a token range of tags without the lower-cased key and
    /// value, which restore recomputes
    ///
    /// # Arguments
    ///
    /// * `ns` - The namespace the table is in
    fn scan_cql(ns: &str) -> String {
        // the generated scan of the columns a tag archive holds
        TagsQueries::backup_cql(ns)
    }

    /// The columns of this table's partition key, used to name rows that fail to decode
    fn partition_key() -> &'static [&'static str] {
        &["type", "group", "year", "bucket", "key", "value"]
    }

    /// Hash this partitions info to see if we have changed partitions
    fn hash_partition(&self) -> u64 {
        // build a new hasher
        let mut hasher = AHasher::default();
        // ingest our partition key
        hasher.write(self.tag_type.as_str().as_bytes());
        hasher.write(self.group.as_bytes());
        hasher.write_i32(self.year);
        hasher.write_i32(self.bucket);
        hasher.write(self.key.as_bytes());
        hasher.write(self.value.as_bytes());
        // finish this hash and get its value
        hasher.finish()
    }
}

/// Implement scrub support for the tags table
impl Scrub for Tag {}

/// The values a restored row binds by column name to the table's generated full-row insert
#[derive(SerializeRow)]
pub struct TagValues {
    /// The `type` column
    #[scylla(rename = "type")]
    pub kind: TagType,
    /// The `group` column
    pub group: String,
    /// The `item` column
    pub item: String,
    /// The `year` column
    pub year: i32,
    /// The `bucket` column
    pub bucket: i32,
    /// The `key` column
    pub key: String,
    /// The `value` column
    pub value: String,
    /// The `uploaded` column
    pub uploaded: DateTime<Utc>,
    /// The `key_lower` column
    pub key_lower: String,
    /// The `value_lower` column
    pub value_lower: String,
}

/// Implement restore support for the tags table
#[async_trait::async_trait]
impl Restore for Tag {
    // The partition config is within the tags config in the Thorium config
    type PartitionConf = thorium::conf::Tags;

    /// The row the generated full-row insert binds by name
    type Values = TagValues;

    /// Get the partition size for this data type
    ///
    /// # Arguments
    ///
    /// * `conf` - The Thorium config
    fn partition_conf(config: &Conf) -> Self::PartitionConf {
        config.thorium.tags.clone()
    }

    /// Returns the values the restore's insert binds for this row, in a bucket of the partition
    /// size the target gives its tag type, with its lower-cased key and value recomputed
    ///
    /// # Arguments
    ///
    /// * `partition_conf` - The tag settings holding each tag type's partition size
    /// * `_format` - The format version the row was archived in
    fn values(self, partition_conf: &Self::PartitionConf, _format: u32) -> Self::Values {
        // get the correct partition size based on tags
        let partition_size = partition_conf.map_type(&self.tag_type).partition_size;
        // calculate the new bucket
        let bucket = thorium::utils::helpers::partition(self.uploaded, self.year, partition_size);
        // get the lowercase versions of key/value
        let key_lower = self.key.to_lowercase();
        let value_lower = self.value.to_lowercase();
        TagValues {
            kind: self.tag_type,
            group: self.group,
            item: self.item,
            year: self.year,
            bucket,
            key: self.key,
            value: self.value,
            uploaded: self.uploaded,
            key_lower,
            value_lower,
        }
    }
}

impl<T: TagSupport> Utils for TagRequest<T> {
    /// The name of the table we are operating on
    fn name() -> &'static str {
        "tags"
    }
}

/// Format 2 archives tags as format 1 did
impl Versioned for Tag {
    type V1 = Tag;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the layout of the archived [`Tag`] row, which written backups depend on
    #[test]
    fn tag_layout_is_pinned() {
        // the archived row's size and the offset of every field must stay as backups hold them
        assert_layout!(ArchivedTag, 56, {
            tag_type: 52,
            group: 12,
            year: 20,
            bucket: 24,
            key: 28,
            value: 36,
            uploaded: 0,
            item: 44,
        });
    }
}
