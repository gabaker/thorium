//! Backup and restore support for repos

use ahash::AHasher;
use bytecheck::CheckBytes;
use chrono::prelude::*;
use rkyv::{Archive, Deserialize, Serialize};
use scylla::DeserializeRow;
use scylla::client::session::Session;
use scylla::errors::PrepareError;
use scylla::statement::prepared::PreparedStatement;
use scylla::value::MaybeUnset;
use std::hash::Hasher;
use thorium::Conf;
use uuid::Uuid;

use super::{Versioned, added_in_format_2};
use crate::backup::{Backup, Restore, Scrub, Utils};

/// A repos list row as format 2 archives it; `None` is a cell the source row held unset
#[derive(Debug, Archive, Serialize, Deserialize, DeserializeRow)]
#[archive_attr(derive(Debug, CheckBytes))]
#[scylla(flavor = "enforce_order", skip_name_checks)]
pub struct RepoList {
    /// The group this repo is in
    pub group: String,
    /// The year this repo was uploaded
    pub year: i32,
    /// The bucket this repo is in
    pub bucket: i32,
    /// When this repo was uploaded
    pub uploaded: DateTime<Utc>,
    /// The id for this repo
    pub id: Uuid,
    /// The user that added this repo to Thorium
    pub creator: Option<String>,
    /// The name of this repo
    pub name: Option<String>,
    /// Where this repo comes from (github/gitlab/...)
    pub provider: Option<String>,
    /// The scheme to use with this repo
    pub scheme: Option<String>,
    /// The url of this repo
    pub url: Option<String>,
    /// The user that uploaded this repo to its provider
    pub user: Option<String>,
    /// The branch or commitish this repo checks out by default
    pub default_checkout: Option<String>,
    /// When the earliest commitish of this repo was made
    pub earliest: Option<DateTime<Utc>>,
}

impl Utils for RepoList {
    /// The name of the table we are backing up
    fn name() -> &'static str {
        "repos_list"
    }
}

#[async_trait::async_trait]
impl Backup for RepoList {
    /// The columns of this table's partition key, used to name rows that fail to decode
    fn partition_key() -> &'static [&'static str] {
        &["group", "year", "bucket"]
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
        // build repos get prepared statement
        scylla
            .prepare(format!(
                "SELECT group, year, bucket, uploaded, id, creator, name, provider, scheme, url, user, \
                default_checkout, earliest \
                FROM {}.{} \
                Where token(group, year, bucket) >= ? AND token(group, year, bucket) <= ?",
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
        hasher.write(self.group.as_bytes());
        hasher.write_i32(self.year);
        hasher.write_i32(self.bucket);
        // finish this hash and get its value
        hasher.finish()
    }
}

/// Implement scrub support for the repos table
impl Scrub for RepoList {}

/// Implement restore support for the repos table
#[async_trait::async_trait]
impl Restore for RepoList {
    // The partition size is constant, so the partition config is just
    // the size itself
    type PartitionConf = u16;

    /// The group, year, bucket, upload time, id, url, provider, user, name, creator, scheme,
    /// default checkout, and earliest commitish time
    type Values = (
        String,
        i32,
        i32,
        DateTime<Utc>,
        Uuid,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        MaybeUnset<Option<String>>,
        MaybeUnset<Option<DateTime<Utc>>>,
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
                (group, year, bucket, uploaded, id, url, provider, user, name, creator, scheme, \
                default_checkout, earliest) \
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
    fn partition_conf(config: &Conf) -> u16 {
        config.thorium.repos.partition_size
    }

    /// Returns the values the restore's insert binds for this row, in a bucket of the target's
    /// partition size
    ///
    /// # Arguments
    ///
    /// * `partition_size` - The partition size to bucket the row with
    /// * `format` - The format version the row was archived in
    fn values(self, partition_size: &u16, format: u32) -> Self::Values {
        // calculate the new bucket
        let bucket = thorium::utils::helpers::partition(self.uploaded, self.year, *partition_size);
        // every column, leaving the default checkout and earliest commit format 1 didn't archive
        // unset
        (
            self.group,
            self.year,
            bucket,
            self.uploaded,
            self.id,
            self.url,
            self.provider,
            self.user,
            self.name,
            self.creator,
            self.scheme,
            added_in_format_2(self.default_checkout, format),
            added_in_format_2(self.earliest, format),
        )
    }
}

/// A single line of stage logs
#[derive(Debug, Archive, Serialize, Deserialize, DeserializeRow)]
#[archive_attr(derive(Debug, CheckBytes))]
#[scylla(flavor = "enforce_order", skip_name_checks)]
pub struct RepoData {
    /// The url of the repo this data blob is for
    pub repo: String,
    /// The sha256 of this data blob
    pub hash: String,
}

impl Utils for RepoData {
    /// The name of the table we are backing up
    fn name() -> &'static str {
        "repo_data"
    }
}

#[async_trait::async_trait]
impl Backup for RepoData {
    /// The columns of this table's partition key, used to name rows that fail to decode
    fn partition_key() -> &'static [&'static str] {
        &["repo"]
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
        // build repos get prepared statement
        scylla
            .prepare(format!(
                "SELECT repo, hash \
                FROM {}.{} \
                Where token(repo) >= ? AND token(repo) <= ?",
                ns,
                Self::name()
            ))
            .await
    }

    /// Hash this partitions info to see if we have changed partitions
    fn hash_partition(&self) -> u64 {
        // build a new hasher
        let mut hasher = AHasher::default();
        // ingest our partition key
        hasher.write(self.repo.as_bytes());
        // finish this hash and get its value
        hasher.finish()
    }
}

/// Implement scrub support for the repos table
#[async_trait::async_trait]
impl Scrub for RepoData {}

/// Implement restore support for the repos table
#[async_trait::async_trait]
impl Restore for RepoData {
    // The partition size is constant, so the partition config is just
    // the size itself
    type PartitionConf = u16;

    /// The repo and the data's hash
    type Values = (String, String);

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
                (repo, hash) \
                VALUES (?, ?)",
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
    fn partition_conf(config: &Conf) -> u16 {
        config.thorium.repos.partition_size
    }

    /// Returns the values the restore's insert binds for this row
    ///
    /// # Arguments
    ///
    /// * `_partition_size` - Unused, as repo data isn't bucketed
    /// * `_format` - The format version the row was archived in
    fn values(self, _partition_size: &u16, _format: u32) -> Self::Values {
        // both columns as the row holds them
        (self.repo, self.hash)
    }
}

/// A repos list row as format 1 archives it
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[archive_attr(derive(Debug, CheckBytes))]
pub struct RepoListV1 {
    /// The group this repo is in
    pub group: String,
    /// The year this repo was uploaded
    pub year: i32,
    /// The bucket this repo is in
    pub bucket: i32,
    /// When this repo was uploaded
    pub uploaded: DateTime<Utc>,
    /// The id for this repo
    pub id: Uuid,
    /// The user that added this repo to Thorium
    pub creator: String,
    /// The name of this repo
    pub name: String,
    /// Where this repo comes from (github/gitlab/...)
    pub provider: String,
    /// The scheme to use with this repo
    pub scheme: String,
    /// The url of this repo
    pub url: String,
    /// The user that uploaded this repo to its provider
    pub user: String,
}

impl From<RepoListV1> for RepoList {
    /// Converts a format-1 row, whose required fields are always set and which carries none of the columns format 2 added
    ///
    /// # Arguments
    ///
    /// * `row` - The format-1 row
    fn from(row: RepoListV1) -> Self {
        // format 1 could only hold rows with these cells set, and didn't archive the added columns
        RepoList {
            group: row.group,
            year: row.year,
            bucket: row.bucket,
            uploaded: row.uploaded,
            id: row.id,
            creator: Some(row.creator),
            name: Some(row.name),
            provider: Some(row.provider),
            scheme: Some(row.scheme),
            url: Some(row.url),
            user: Some(row.user),
            default_checkout: None,
            earliest: None,
        }
    }
}

/// Format 1 archives `RepoList` rows as [`RepoListV1`]
impl Versioned for RepoList {
    type V1 = RepoListV1;
}

/// Format 2 archives repo data as format 1 did
impl Versioned for RepoData {
    type V1 = RepoData;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the layout of the archived [`RepoList`] row, which written backups depend on
    #[test]
    fn repo_list_layout_is_pinned() {
        // the archived row's size and the offset of every field must stay as backups hold them
        assert_layout!(ArchivedRepoList, 144, {
            group: 112,
            year: 120,
            bucket: 124,
            uploaded: 100,
            id: 128,
            creator: 0,
            name: 12,
            provider: 24,
            scheme: 36,
            url: 48,
            user: 60,
            default_checkout: 72,
            earliest: 84,
        });
    }

    /// Pins the layout of the archived [`RepoData`] row, which written backups depend on
    #[test]
    fn repo_data_layout_is_pinned() {
        // the archived row's size and the offset of every field must stay as backups hold them
        assert_layout!(ArchivedRepoData, 16, {
            repo: 0,
            hash: 8,
        });
    }

    /// Pins the layout of the archived [`RepoListV1`] row, which written backups depend on
    #[test]
    fn repo_list_v1_layout_is_pinned() {
        // the archived row's size and the offset of every field must stay as backups hold them
        assert_layout!(ArchivedRepoListV1, 92, {
            group: 12,
            year: 20,
            bucket: 24,
            uploaded: 0,
            id: 28,
            creator: 44,
            name: 52,
            provider: 60,
            scheme: 68,
            url: 76,
            user: 84,
        });
    }
}
