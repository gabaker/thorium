//! Backup and restore support for the samples list table

use ahash::AHasher;
use bytecheck::CheckBytes;
use chrono::prelude::*;
use rkyv::{Archive, Deserialize, Serialize};
use scylla::SerializeRow;
use std::hash::Hasher;
use thorium::Conf;
use uuid::Uuid;

use super::Versioned;
use crate::backup::{Backup, Restore, Scrub, Utils};
use thorium::models::schema::ScyllaProjection;
use thorium::models::schema::scylla::SamplesListTable;

/// A samples list row as format 2 archives it; `None` is a cell the source row held unset
#[derive(Debug, Archive, Serialize, Deserialize, ScyllaProjection)]
#[archive_attr(derive(Debug, CheckBytes))]
#[thorium_projection(of = SamplesListTable)]
pub struct SamplesList {
    /// The group this sample is in
    pub group: String,
    /// The year this sample is from
    pub year: i32,
    /// the bucket this sample is in
    pub bucket: i32,
    /// The sha256 of this sample
    pub sha256: Option<String>,
    /// The sha1 of this sample
    pub sha1: Option<String>,
    /// The md5 of this sample
    pub md5: Option<String>,
    /// A UUID for this submission
    pub id: Uuid,
    /// The name of this sample if one was specified
    pub name: Option<String>,
    /// A description for this sample
    pub description: Option<String>,
    /// The user who submitted this sample
    pub submitter: Option<String>,
    /// Where this sample originates from if anywhere in serial form
    pub origin: Option<String>,
    /// When this sample was uploaded
    pub uploaded: DateTime<Utc>,
}

impl Utils for SamplesList {
    /// The name of the table we are backing up
    fn name() -> &'static str {
        "samples_list"
    }
}

/// Implement backup support for the samples list table
#[async_trait::async_trait]
impl Backup for SamplesList {
    /// The table this archive struct backs up, read through its generated statements
    type Table = SamplesListTable;

    /// The columns of this table's partition key, used to name rows that fail to decode
    fn partition_key() -> &'static [&'static str] {
        &["group", "year", "bucket"]
    }

    /// Hash this partitions info to see if we have changed partitions
    fn hash_partition(&self) -> u64 {
        // build a new hasher
        let mut hasher = AHasher::default();
        // ingest our partition key
        hasher.write(self.group.as_bytes());
        hasher.write_i32(self.year);
        hasher.write_i32(self.bucket);
        // finish this hash and get its value
        hasher.finish()
    }
}

/// Implement scrub support for the samples list table
impl Scrub for SamplesList {}

/// The values a restored row binds by column name to the table's generated full-row insert
#[derive(SerializeRow)]
pub struct SamplesListValues {
    /// The `group` column
    pub group: String,
    /// The `year` column
    pub year: i32,
    /// The `bucket` column
    pub bucket: i32,
    /// The `sha256` column
    pub sha256: Option<String>,
    /// The `sha1` column
    pub sha1: Option<String>,
    /// The `md5` column
    pub md5: Option<String>,
    /// The `id` column
    pub id: Uuid,
    /// The `name` column
    pub name: Option<String>,
    /// The `description` column
    pub description: Option<String>,
    /// The `submitter` column
    pub submitter: Option<String>,
    /// The `origin` column
    pub origin: Option<String>,
    /// The `uploaded` column
    pub uploaded: DateTime<Utc>,
}

/// Implement restore support for the samples list table
#[async_trait::async_trait]
impl Restore for SamplesList {
    // The partition size is constant, so the partition config is just
    // the size itself
    type PartitionConf = u16;

    /// The row the generated full-row insert binds by name
    type Values = SamplesListValues;

    /// Get the partition size for this data type
    ///
    /// # Arguments
    ///
    /// * `conf` - The Thorium config
    fn partition_conf(config: &Conf) -> u16 {
        config.thorium.files.partition_size
    }

    /// Returns the values the restore's insert binds for this row, in a bucket of the target's
    /// partition size
    ///
    /// # Arguments
    ///
    /// * `partition_size` - The partition size to bucket the row with
    /// * `_format` - The format version the row was archived in
    fn values(self, partition_size: &u16, _format: u32) -> Self::Values {
        // calculate the new bucket
        let bucket = thorium::utils::helpers::partition(self.uploaded, self.year, *partition_size);
        SamplesListValues {
            group: self.group,
            year: self.year,
            bucket,
            sha256: self.sha256,
            sha1: self.sha1,
            md5: self.md5,
            id: self.id,
            name: self.name,
            description: self.description,
            submitter: self.submitter,
            origin: self.origin,
            uploaded: self.uploaded,
        }
    }
}

/// A samples list row as format 1 archives it
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[archive_attr(derive(Debug, CheckBytes))]
pub struct SamplesListV1 {
    /// The group this sample is in
    pub group: String,
    /// The year this sample is from
    pub year: i32,
    /// the bucket this sample is in
    pub bucket: i32,
    /// The sha256 of this sample
    pub sha256: String,
    /// The sha1 of this sample
    pub sha1: String,
    /// The md5 of this sample
    pub md5: String,
    /// A UUID for this submission
    pub id: Uuid,
    /// The name of this sample if one was specified
    pub name: Option<String>,
    /// A description for this sample
    pub description: Option<String>,
    /// The user who submitted this sample
    pub submitter: String,
    /// Where this sample originates from if anywhere in serial form
    pub origin: Option<String>,
    /// When this sample was uploaded
    pub uploaded: DateTime<Utc>,
}

impl From<SamplesListV1> for SamplesList {
    /// Converts a format-1 row, whose required fields are always set
    ///
    /// # Arguments
    ///
    /// * `row` - The format-1 row
    fn from(row: SamplesListV1) -> Self {
        // format 1 could only hold rows with these cells set
        SamplesList {
            group: row.group,
            year: row.year,
            bucket: row.bucket,
            sha256: Some(row.sha256),
            sha1: Some(row.sha1),
            md5: Some(row.md5),
            id: row.id,
            name: row.name,
            description: row.description,
            submitter: Some(row.submitter),
            origin: row.origin,
            uploaded: row.uploaded,
        }
    }
}

/// Format 1 archives `SamplesList` rows as [`SamplesListV1`]
impl Versioned for SamplesList {
    type V1 = SamplesListV1;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the layout of the archived [`SamplesList`] row, which written backups depend on
    #[test]
    fn samples_list_layout_is_pinned() {
        // the archived row's size and the offset of every field must stay as backups hold them
        assert_layout!(ArchivedSamplesList, 128, {
            group: 96,
            year: 104,
            bucket: 108,
            sha256: 0,
            sha1: 12,
            md5: 24,
            id: 112,
            name: 36,
            description: 48,
            submitter: 60,
            origin: 72,
            uploaded: 84,
        });
    }

    /// Pins the layout of the archived [`SamplesListV1`] row, which written backups depend on
    #[test]
    fn samples_list_v1_layout_is_pinned() {
        // the archived row's size and the offset of every field must stay as backups hold them
        assert_layout!(ArchivedSamplesListV1, 112, {
            group: 48,
            year: 56,
            bucket: 60,
            sha256: 64,
            sha1: 72,
            md5: 80,
            id: 88,
            name: 0,
            description: 12,
            submitter: 104,
            origin: 24,
            uploaded: 36,
        });
    }
}
