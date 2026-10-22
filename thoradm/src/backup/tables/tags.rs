//! Backup and restore support for tags

use ahash::AHasher;
use bytecheck::CheckBytes;
use chrono::prelude::*;
use rkyv::{Archive, Deserialize, Serialize};
use scylla::DeserializeRow;
use scylla::client::session::Session;
use scylla::errors::PrepareError;
use scylla::statement::prepared::PreparedStatement;
use std::hash::Hasher;
use thorium::Conf;
use thorium::models::backends::TagSupport;
use thorium::models::{TagRequest, TagType};

use super::Versioned;
use crate::backup::{Backup, Restore, Scrub, Utils};

/// A single line of stage logs
#[derive(Debug, Archive, Serialize, Deserialize, DeserializeRow)]
#[archive_attr(derive(Debug, CheckBytes))]
#[scylla(flavor = "enforce_order", skip_name_checks)]
pub struct Tag {
    /// The type of object this tag is for
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
    /// The columns of this table's partition key, used to name rows that fail to decode
    fn partition_key() -> &'static [&'static str] {
        &["type", "group", "year", "bucket", "key", "value"]
    }

    /// The prepared statement to use when retrieving data from Scylla
    ///
    /// # Arguments
    ///
    /// * `scylla` - The scylla session to build a prepared statement with
    /// * `ns` - The namespace for this prepared statement
    async fn prepared_statement(
        scylla: &Session,
        ns: &str,
    ) -> Result<PreparedStatement, PrepareError> {
        // build tags get prepared statement
        scylla
            .prepare(format!(
                "SELECT type, group, year, bucket, key, value, uploaded, item \
                FROM {}.{} \
                Where token(type, group, year, bucket, key, value) >= ? AND token(type, group, year, bucket, key, value) <= ?",
                ns,
                Self::name(),
            ))
            .await
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

/// Implement restore support for the tags table
#[async_trait::async_trait]
impl Restore for Tag {
    // The partition config is within the tags config in the Thorium config
    type PartitionConf = thorium::conf::Tags;

    /// The tag type, group, item, year, bucket, key, value, upload time, and lower-cased key and
    /// value
    type Values = (
        TagType,
        String,
        String,
        i32,
        i32,
        String,
        String,
        DateTime<Utc>,
        String,
        String,
    );

    /// The prepared statement to use when restoring data to scylla
    ///
    /// # Arguments
    ///
    /// * `scylla` - A scylla client
    /// * `ns` - The namespace in scylla this table is from
    async fn prepared_statement(
        scylla: &Session,
        ns: &str,
    ) -> Result<PreparedStatement, PrepareError> {
        scylla
            .prepare(format!(
                "INSERT INTO {}.{} \
                (type, group, item, year, bucket, key, value, uploaded, key_lower, value_lower) \
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                ns,
                Self::name(),
            ))
            .await
    }

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
        (
            self.tag_type,
            self.group,
            self.item,
            self.year,
            bucket,
            self.key,
            self.value,
            self.uploaded,
            key_lower,
            value_lower,
        )
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
    /// Format 1 archived these rows with the same layout, so they decode as `Tag` itself
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
