//! Backup and restore support for commits

use ahash::AHasher;
use bytecheck::CheckBytes;
use chrono::prelude::*;
use rkyv::{Archive, Deserialize, Serialize};
use scylla::SerializeRow;
use std::hash::Hasher;
use thorium::Conf;
use thorium::models::CommitishKinds;

use super::Versioned;
use crate::backup::{Backup, Restore, Scrub, Utils};
use thorium::models::schema::ScyllaProjection;
use thorium::models::schema::scylla::{CommitishListTable, CommitishesTable};

/// A commitishes row as format 2 archives it; `None` is a cell the source row held unset
#[derive(Debug, Archive, Serialize, Deserialize, ScyllaProjection)]
#[archive_attr(derive(Debug, CheckBytes))]
#[thorium_projection(of = CommitishesTable)]
pub struct Commitish {
    /// The kind of commitish this row represents
    pub kind: CommitishKinds,
    /// The group this commitish is in
    pub group: String,
    /// The repo this commitish is for
    pub repo: String,
    /// The key for this commitish
    pub key: String,
    /// The data for this commitish
    pub data: Option<String>,
    /// The hash for this commits repo data blob
    pub repo_data: Option<String>,
    /// When this commit was added to its repo
    pub timestamp: Option<DateTime<Utc>>,
}

impl Utils for Commitish {
    /// The name of the table we are backing up
    fn name() -> &'static str {
        "commitishes"
    }
}

#[async_trait::async_trait]
impl Backup for Commitish {
    /// The table this archive struct backs up, read through its generated statements
    type Table = CommitishesTable;

    /// The columns of this table's partition key, used to name rows that fail to decode
    fn partition_key() -> &'static [&'static str] {
        &["kind", "group", "repo", "key"]
    }

    /// Hash this partitions info to see if we have changed partitions
    fn hash_partition(&self) -> u64 {
        // build a new hasher
        let mut hasher = AHasher::default();
        // ingest our partition key
        hasher.write(self.group.as_bytes());
        hasher.write(self.repo.as_bytes());
        hasher.write(self.key.as_bytes());
        // finish this hash and get its value
        hasher.finish()
    }
}

/// Implement scrub support for the tags table
impl Scrub for Commitish {}

/// The values a restored row binds by column name to the table's generated full-row insert
#[derive(SerializeRow)]
pub struct CommitishValues {
    /// The `kind` column
    pub kind: CommitishKinds,
    /// The `group` column
    pub group: String,
    /// The `repo` column
    pub repo: String,
    /// The `key` column
    pub key: String,
    /// The `timestamp` column
    pub timestamp: Option<DateTime<Utc>>,
    /// The `data` column
    pub data: Option<String>,
    /// The `repo_data` column
    pub repo_data: Option<String>,
}

/// Implement restore support for the tags table
#[async_trait::async_trait]
impl Restore for Commitish {
    // The partition size is constant, so the partition config is just
    // the size itself
    type PartitionConf = u16;

    /// The row the generated full-row insert binds by name
    type Values = CommitishValues;

    /// Get the partition size for this data type
    ///
    /// # Arguments
    ///
    /// * `conf` - The Thorium config
    fn partition_conf(config: &Conf) -> u16 {
        config.thorium.repos.partition_size
    }

    /// Returns the values the restore's insert binds for this row
    ///
    /// # Arguments
    ///
    /// * `_partition_size` - Unused, as commitishes aren't bucketed
    /// * `_format` - The format version the row was archived in
    fn values(self, _partition_size: &u16, _format: u32) -> Self::Values {
        // every column as the row holds it
        CommitishValues {
            kind: self.kind,
            group: self.group,
            repo: self.repo,
            key: self.key,
            timestamp: self.timestamp,
            data: self.data,
            repo_data: self.repo_data,
        }
    }
}

/// A commitish list row as format 2 archives it; `None` is a cell the source row held unset
#[derive(Debug, Archive, Serialize, Deserialize, ScyllaProjection)]
#[archive_attr(derive(Debug, CheckBytes))]
#[thorium_projection(of = CommitishListTable)]
pub struct CommitishList {
    /// The kind of commitish this row represents
    pub kind: CommitishKinds,
    /// The group this commitish is in
    pub group: String,
    /// The year this commitish was added to its repo
    pub year: i32,
    /// The bucket for this commitish
    pub bucket: i32,
    /// The repo this commitish is for
    pub repo: String,
    /// When this commitish was added to its repo
    pub timestamp: DateTime<Utc>,
    /// The key of this commitish
    pub key: String,
    /// The hash for this commitish's repo data blob
    pub repo_data: Option<String>,
}

impl Utils for CommitishList {
    /// The name of the table we are backing up
    fn name() -> &'static str {
        "commitish_list"
    }
}

#[async_trait::async_trait]
impl Backup for CommitishList {
    /// The table this archive struct backs up, read through its generated statements
    type Table = CommitishListTable;

    /// The columns of this table's partition key, used to name rows that fail to decode
    fn partition_key() -> &'static [&'static str] {
        &["kind", "group", "year", "bucket", "repo"]
    }

    /// Hash this partitions info to see if we have changed partitions
    fn hash_partition(&self) -> u64 {
        // build a new hasher
        let mut hasher = AHasher::default();
        // ingest our partition key
        hasher.write(self.group.as_bytes());
        hasher.write_i32(self.year);
        hasher.write_i32(self.bucket);
        hasher.write(self.repo.as_bytes());
        // finish this hash and get its value
        hasher.finish()
    }
}

/// Implement scrub support for the tags table
impl Scrub for CommitishList {}

/// The values a restored row binds by column name to the table's generated full-row insert
#[derive(SerializeRow)]
pub struct CommitishListValues {
    /// The `kind` column
    pub kind: CommitishKinds,
    /// The `group` column
    pub group: String,
    /// The `year` column
    pub year: i32,
    /// The `bucket` column
    pub bucket: i32,
    /// The `repo` column
    pub repo: String,
    /// The `timestamp` column
    pub timestamp: DateTime<Utc>,
    /// The `key` column
    pub key: String,
    /// The `repo_data` column
    pub repo_data: Option<String>,
}

/// Implement restore support for the tags table
#[async_trait::async_trait]
impl Restore for CommitishList {
    // The partition size is constant, so the partition config is just
    // the size itself
    type PartitionConf = u16;

    /// The row the generated full-row insert binds by name
    type Values = CommitishListValues;

    /// Get the partition size for this data type
    ///
    /// # Arguments
    ///
    /// * `conf` - The Thorium config
    fn partition_conf(config: &Conf) -> u16 {
        config.thorium.repos.partition_size
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
        let bucket = thorium::utils::helpers::partition(self.timestamp, self.year, *partition_size);
        CommitishListValues {
            kind: self.kind,
            group: self.group,
            year: self.year,
            bucket,
            repo: self.repo,
            timestamp: self.timestamp,
            key: self.key,
            repo_data: self.repo_data,
        }
    }
}

/// A commitishes row as format 1 archives it
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[archive_attr(derive(Debug, CheckBytes))]
pub struct CommitishV1 {
    /// The kind of commitish this row represents
    pub kind: CommitishKinds,
    /// The group this commitish is in
    pub group: String,
    /// The repo this commitish is for
    pub repo: String,
    /// The key for this commitish
    pub key: String,
    /// The data for this commitish
    pub data: String,
    /// The hash for this commits repo data blob
    pub repo_data: String,
    /// When this commit was added to its repo
    pub timestamp: DateTime<Utc>,
}

impl From<CommitishV1> for Commitish {
    /// Converts a format-1 row, whose required fields are always set
    ///
    /// # Arguments
    ///
    /// * `row` - The format-1 row
    fn from(row: CommitishV1) -> Self {
        // format 1 could only hold rows with these cells set
        Commitish {
            kind: row.kind,
            group: row.group,
            repo: row.repo,
            key: row.key,
            data: Some(row.data),
            repo_data: Some(row.repo_data),
            timestamp: Some(row.timestamp),
        }
    }
}

/// Format 1 archives `Commitish` rows as [`CommitishV1`]
impl Versioned for Commitish {
    type V1 = CommitishV1;
}

/// A commitish list row as format 1 archives it
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[archive_attr(derive(Debug, CheckBytes))]
pub struct CommitishListV1 {
    /// The kind of commitish this row represents
    pub kind: CommitishKinds,
    /// The group this commitish is in
    pub group: String,
    /// The year this commitish was added to its repo
    pub year: i32,
    /// The bucket for this commitish
    pub bucket: i32,
    /// The repo this commitish is for
    pub repo: String,
    /// When this commitish was added to its repo
    pub timestamp: DateTime<Utc>,
    /// The key of this commitish
    pub key: String,
    /// The hash for this commitish's repo data blob
    pub repo_data: String,
}

impl From<CommitishListV1> for CommitishList {
    /// Converts a format-1 row, whose required fields are always set
    ///
    /// # Arguments
    ///
    /// * `row` - The format-1 row
    fn from(row: CommitishListV1) -> Self {
        // format 1 could only hold rows with these cells set
        CommitishList {
            kind: row.kind,
            group: row.group,
            year: row.year,
            bucket: row.bucket,
            repo: row.repo,
            timestamp: row.timestamp,
            key: row.key,
            repo_data: Some(row.repo_data),
        }
    }
}

/// Format 1 archives `CommitishList` rows as [`CommitishListV1`]
impl Versioned for CommitishList {
    type V1 = CommitishListV1;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the layout of the archived [`Commitish`] row, which written backups depend on
    #[test]
    fn commitish_layout_is_pinned() {
        // the archived row's size and the offset of every field must stay as backups hold them
        assert_layout!(ArchivedCommitish, 68, {
            kind: 64,
            group: 40,
            repo: 48,
            key: 56,
            data: 0,
            repo_data: 12,
            timestamp: 24,
        });
    }

    /// Pins the layout of the archived [`CommitishList`] row, which written backups depend on
    #[test]
    fn commitish_list_layout_is_pinned() {
        // the archived row's size and the offset of every field must stay as backups hold them
        assert_layout!(ArchivedCommitishList, 60, {
            kind: 56,
            group: 24,
            year: 32,
            bucket: 36,
            repo: 40,
            timestamp: 12,
            key: 48,
            repo_data: 0,
        });
    }

    /// Pins the layout of the archived [`CommitishV1`] row, which written backups depend on
    #[test]
    fn commitish_v1_layout_is_pinned() {
        // the archived row's size and the offset of every field must stay as backups hold them
        assert_layout!(ArchivedCommitishV1, 56, {
            kind: 52,
            group: 12,
            repo: 20,
            key: 28,
            data: 36,
            repo_data: 44,
            timestamp: 0,
        });
    }

    /// Pins the layout of the archived [`CommitishListV1`] row, which written backups depend on
    #[test]
    fn commitish_list_v1_layout_is_pinned() {
        // the archived row's size and the offset of every field must stay as backups hold them
        assert_layout!(ArchivedCommitishListV1, 56, {
            kind: 52,
            group: 12,
            year: 20,
            bucket: 24,
            repo: 28,
            timestamp: 0,
            key: 36,
            repo_data: 44,
        });
    }
}
