//! Backup and restore support for the samples list table

use ahash::AHasher;
use bytecheck::CheckBytes;
use chrono::prelude::*;
use rkyv::{Archive, Deserialize, Serialize};
use scylla::DeserializeRow;
use scylla::client::session::Session;
use scylla::errors::PrepareError;
use scylla::statement::prepared::PreparedStatement;
use scylla::value::MaybeUnset;
use std::collections::HashMap;
use std::hash::Hasher;
use std::path::{Path, PathBuf};
use thorium::Conf;
use thorium::models::OutputDisplayType;
use uuid::Uuid;

use super::{Versioned, added_in_format_2};
use crate::Error;
use crate::backup::S3Backup;
use crate::backup::S3Restore;
use crate::backup::s3::{Crawl, Named, object_key};
use crate::backup::{Backup, Restore, Scrub, Utils};

/// The directory under a result's id that holds the blob of each kind of entity it found
/// (`{id}/__thorium_entities/{kind}.json`)
pub const ENTITY_BLOB_DIR: &str = "__thorium_entities";

/// Returns the name of an entity kind as a result's entity blob key holds it
///
/// The API stores each kind in the `entities` column as JSON text (`"Device"`), while the blob's
/// key holds the bare name (`Device.json`); a kind that isn't JSON text is used as it is.
///
/// # Arguments
///
/// * `stored` - The kind as the `entities` column holds it
#[must_use]
pub fn entity_kind(stored: &str) -> String {
    // a JSON string is unquoted; anything else is already the name
    serde_json::from_str::<String>(stored).unwrap_or_else(|_| stored.to_owned())
}

/// A results row as format 2 archives it; `None` is a cell the source row held unset
#[derive(Debug, Archive, Serialize, Deserialize, DeserializeRow)]
#[archive_attr(derive(Debug, CheckBytes))]
#[scylla(flavor = "enforce_order", skip_name_checks)]
pub struct Output {
    /// The unique id for this result
    pub id: Uuid,
    /// The tool or pipeline this result comes from
    pub tool: Option<String>,
    /// The version of the tool or pipeline this result comes from
    pub tool_version: Option<String>,
    /// The command used to generate this result
    pub cmd: Option<String>,
    /// When this result was uploaded
    pub uploaded: DateTime<Utc>,
    /// The result
    pub result: Option<String>,
    /// An optional file tied to this result
    pub files: Option<Vec<String>>,
    /// The display type of this tool output
    pub display_type: Option<OutputDisplayType>,
    /// The children that were found when generating this result
    pub children: Option<HashMap<String, Uuid>>,
    /// The number of entities of each kind this result found, whose blobs the results bucket holds
    pub entities: Option<HashMap<String, i64>>,
}

impl Utils for Output {
    /// The name of the table we are backing up
    fn name() -> &'static str {
        "results"
    }
}

/// Implement backup support for the results table
#[async_trait::async_trait]
impl Backup for Output {
    /// The columns of this table's partition key, used to name rows that fail to decode
    fn partition_key() -> &'static [&'static str] {
        &["id"]
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
        // build output get prepared statement
        scylla
            .prepare(format!(
                "SELECT id, tool, tool_version, cmd, uploaded, result, files, display_type, children, entities \
                FROM {}.{} \
                Where token(id) >= ? AND token(id) <= ?",
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
        hasher.write(self.id.as_bytes());
        // finish this hash and get its value
        hasher.finish()
    }
}

/// Implement scrub support for the results table
impl Scrub for Output {}

/// Implement restore support for the samples list table
#[async_trait::async_trait]
impl Restore for Output {
    // The partition size is constant, so the partition config is just
    // the size itself
    type PartitionConf = u16;

    /// The id, upload time, tool, tool version, command, result, files, display type, children,
    /// and entity counts
    type Values = (
        Uuid,
        DateTime<Utc>,
        Option<String>,
        MaybeUnset<Option<String>>,
        Option<String>,
        Option<String>,
        Option<Vec<String>>,
        Option<OutputDisplayType>,
        Option<HashMap<String, Uuid>>,
        MaybeUnset<Option<HashMap<String, i64>>>,
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
                (id, uploaded, tool, tool_version, cmd, result, files, display_type, children, entities) \
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                ns,
                Self::name()
            ))
            .await
    }

    /// Get the partition size for this data type
    ///
    /// # Arguments
    ///
    /// * `conf` - The Thorium config
    fn partition_conf(config: &Conf) -> u16 {
        config.thorium.results.partition_size
    }

    /// Returns the values the restore's insert binds for this row
    ///
    /// # Arguments
    ///
    /// * `_partition_size` - Unused, as results aren't bucketed
    /// * `format` - The format version the row was archived in
    fn values(self, _partition_size: &u16, format: u32) -> Self::Values {
        // every column as the row holds it, leaving the ones format 1 didn't archive unset
        (
            self.id,
            self.uploaded,
            self.tool,
            added_in_format_2(self.tool_version, format),
            self.cmd,
            self.result,
            self.files,
            self.display_type,
            self.children,
            added_in_format_2(self.entities, format),
        )
    }
}

#[async_trait::async_trait]
impl S3Backup for Output {
    /// Get the result files and entity blobs and where to write them off to disk at
    ///
    /// # Arguments
    ///
    /// * `conf` - The config for this Thorium cluster
    /// * `root` - The root path to write objects too
    /// * `chars` - The number of characters to use in order to partition this data on disk
    /// * `buffer` - The slice of data to deserialize and crawl for s3 backup objects
    async fn paths(
        conf: &Conf,
        root: &PathBuf,
        chars: usize,
        buffer: &[u8],
    ) -> Result<Crawl, Error> {
        // cast our buffer to its archived type
        let rows = rkyv::check_archived_root::<Vec<Output>>(buffer)?;
        // assume each sample will have at least 3 result files
        let mut downloads = Vec::with_capacity(3);
        // crawl our results and build a list of their objects to write off to disk
        for row in rows.iter() {
            // each result file is keyed by its name under the result's id
            let files = row
                .files
                .iter()
                .flat_map(|files| files.iter())
                .map(|file| format!("{}/{}", row.id, file));
            // each kind of entity the result found has its blob under the result's id
            let blobs = row
                .entities
                .iter()
                .flat_map(|entities| entities.keys())
                .map(|kind| format!("{}/{ENTITY_BLOB_DIR}/{}.json", row.id, entity_kind(kind)));
            for key in files.chain(blobs) {
                // get the bucket for result files
                let bucket = conf.thorium.results.bucket.clone();
                // clone our root path
                let mut path = root.clone();
                // get the sub folder for this key
                path.push(&key[..chars]);
                // add the key to this path
                path.push(&key);
                // add this object to our download list, named by the result that holds it
                downloads.push(Named {
                    bucket,
                    key,
                    path,
                    referrer: format!("{}:{}", Output::name(), row.id),
                });
            }
        }
        Ok(downloads.into())
    }
}

impl S3Restore for Output {
    /// Get the bucket and key of a backed-up object from where the backup keeps it
    ///
    /// # Arguments
    ///
    /// * `path` - The path to the file we are restoring
    /// * `objects` - The component's `objects/` directory, which `path` is under
    /// * `conf` - The Thorium for this cluster
    fn parse(path: &Path, objects: &Path, conf: &Conf) -> Result<(String, String), Error> {
        // every result file is in the results bucket, keyed by result and file name
        let (_, key) = object_key(path, objects, false)?;
        Ok((conf.thorium.results.bucket.clone(), key))
    }
}

/// A results stream row as format 2 archives it; `None` is a cell the source row held unset
#[derive(Debug, Archive, Serialize, Deserialize, DeserializeRow)]
#[archive_attr(derive(Debug, CheckBytes))]
#[scylla(flavor = "enforce_order", skip_name_checks)]
pub struct OutputStream {
    /// The kind of result this is for
    pub kind: String,
    /// The group this result is in
    pub group: String,
    /// The year this result was added
    pub year: i32,
    /// The bucket this result is in
    pub bucket: i32,
    /// The key for this result
    pub key: String,
    /// The tool this result comes from
    pub tool: Option<String>,
    /// The version of the tool this result comes from
    pub tool_version: Option<String>,
    /// The display type of this tool output
    pub display_type: Option<OutputDisplayType>,
    /// The timestamp for this tool
    pub uploaded: DateTime<Utc>,
    /// The command that was run to generate these results
    pub cmd: Option<String>,
    /// The unique id for this result
    pub id: Uuid,
}

impl Utils for OutputStream {
    /// The name of the table we are backing up
    fn name() -> &'static str {
        "results_stream"
    }
}

/// Implement backup support for the results table
#[async_trait::async_trait]
impl Backup for OutputStream {
    /// The columns of this table's partition key, used to name rows that fail to decode
    fn partition_key() -> &'static [&'static str] {
        &["kind", "group", "year", "bucket"]
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
        // build output get prepared statement
        scylla
            .prepare(format!(
                "SELECT kind, group, year, bucket, key, tool, tool_version, display_type, uploaded, cmd, id \
                FROM {}.{} \
                Where token(kind, group, year, bucket) >= ? AND token(kind, group, year, bucket) <= ?",
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
        hasher.write(self.kind.as_bytes());
        hasher.write(self.group.as_bytes());
        hasher.write_i32(self.year);
        hasher.write_i32(self.bucket);
        // finish this hash and get its value
        hasher.finish()
    }
}

/// Implement scrub support for the results table
impl Scrub for OutputStream {}

/// Implement restore support for the samples list table
#[async_trait::async_trait]
impl Restore for OutputStream {
    // The partition size is constant, so the partition config is just
    // the size itself
    type PartitionConf = u16;

    /// The kind, group, year, bucket, key, tool, tool version, display type, upload time,
    /// command, and id
    type Values = (
        String,
        String,
        i32,
        i32,
        String,
        Option<String>,
        MaybeUnset<Option<String>>,
        Option<OutputDisplayType>,
        DateTime<Utc>,
        Option<String>,
        Uuid,
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
                (kind, group, year, bucket, key, tool, tool_version, display_type, uploaded, cmd, id) \
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                ns,
                Self::name()
            ))
            .await
    }

    /// Get the partition size for this data type
    ///
    /// # Arguments
    ///
    /// * `conf` - The Thorium config
    fn partition_conf(config: &Conf) -> u16 {
        config.thorium.results.partition_size
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
        // every column, leaving the tool version format 1 didn't archive unset
        (
            self.kind,
            self.group,
            self.year,
            bucket,
            self.key,
            self.tool,
            added_in_format_2(self.tool_version, format),
            self.display_type,
            self.uploaded,
            self.cmd,
            self.id,
        )
    }
}

/// A results row as format 1 archives it
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[archive_attr(derive(Debug, CheckBytes))]
pub struct OutputV1 {
    /// The unique id for this result
    pub id: Uuid,
    /// The tool or pipeline this result comes from
    pub tool: String,
    /// The command used to generate this result
    pub cmd: Option<String>,
    /// When this result was uploaded
    pub uploaded: DateTime<Utc>,
    /// The result
    pub result: String,
    /// An optional file tied to this result
    pub files: Option<Vec<String>>,
    /// The display type of this tool output
    pub display_type: OutputDisplayType,
    /// The children that were found when generating this result
    pub children: Option<HashMap<String, Uuid>>,
}

impl From<OutputV1> for Output {
    /// Converts a format-1 row, whose required fields are always set and which carries none of the columns format 2 added
    ///
    /// # Arguments
    ///
    /// * `row` - The format-1 row
    fn from(row: OutputV1) -> Self {
        // format 1 could only hold rows with these cells set, and didn't archive the added columns
        Output {
            id: row.id,
            tool: Some(row.tool),
            cmd: row.cmd,
            uploaded: row.uploaded,
            result: Some(row.result),
            files: row.files,
            display_type: Some(row.display_type),
            children: row.children,
            tool_version: None,
            entities: None,
        }
    }
}

/// Format 1 archives `Output` rows as [`OutputV1`]
impl Versioned for Output {
    type V1 = OutputV1;
}

/// A results stream row as format 1 archives it
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[archive_attr(derive(Debug, CheckBytes))]
pub struct OutputStreamV1 {
    /// The kind of result this is for
    pub kind: String,
    /// The group this result is in
    pub group: String,
    /// The year this result was added
    pub year: i32,
    /// The bucket this result is in
    pub bucket: i32,
    /// The key for this result
    pub key: String,
    /// The tool this result comes from
    pub tool: String,
    /// The display type of this tool output
    pub display_type: OutputDisplayType,
    /// The timestamp for this tool
    pub uploaded: DateTime<Utc>,
    /// The command that was run to generate these results
    pub cmd: Option<String>,
    /// The unique id for this result
    pub id: Uuid,
}

impl From<OutputStreamV1> for OutputStream {
    /// Converts a format-1 row, whose required fields are always set and which carries none of the columns format 2 added
    ///
    /// # Arguments
    ///
    /// * `row` - The format-1 row
    fn from(row: OutputStreamV1) -> Self {
        // format 1 could only hold rows with these cells set, and didn't archive the added columns
        OutputStream {
            kind: row.kind,
            group: row.group,
            year: row.year,
            bucket: row.bucket,
            key: row.key,
            tool: Some(row.tool),
            display_type: Some(row.display_type),
            uploaded: row.uploaded,
            cmd: row.cmd,
            id: row.id,
            tool_version: None,
        }
    }
}

/// Format 1 archives `OutputStream` rows as [`OutputStreamV1`]
impl Versioned for OutputStream {
    type V1 = OutputStreamV1;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the layout of the archived [`Output`] row, which written backups depend on
    #[test]
    fn output_layout_is_pinned() {
        // the archived row's size and the offset of every field must stay as backups hold them
        assert_layout!(ArchivedOutput, 124, {
            id: 104,
            tool: 0,
            tool_version: 12,
            cmd: 24,
            uploaded: 92,
            result: 36,
            files: 48,
            display_type: 120,
            children: 60,
            entities: 76,
        });
    }

    /// Pins the layout of the archived [`OutputStream`] row, which written backups depend on
    #[test]
    fn output_stream_layout_is_pinned() {
        // the archived row's size and the offset of every field must stay as backups hold them
        assert_layout!(ArchivedOutputStream, 100, {
            kind: 48,
            group: 56,
            year: 64,
            bucket: 68,
            key: 72,
            tool: 0,
            tool_version: 12,
            display_type: 96,
            uploaded: 36,
            cmd: 24,
            id: 80,
        });
    }

    /// Pins the layout of the archived [`OutputV1`] row, which written backups depend on
    #[test]
    fn output_v1_layout_is_pinned() {
        // the archived row's size and the offset of every field must stay as backups hold them
        assert_layout!(ArchivedOutputV1, 88, {
            id: 52,
            tool: 68,
            cmd: 0,
            uploaded: 40,
            result: 76,
            files: 12,
            display_type: 84,
            children: 24,
        });
    }

    /// Pins the layout of the archived [`OutputStreamV1`] row, which written backups depend on
    #[test]
    fn output_stream_v1_layout_is_pinned() {
        // the archived row's size and the offset of every field must stay as backups hold them
        assert_layout!(ArchivedOutputStreamV1, 84, {
            kind: 24,
            group: 32,
            year: 40,
            bucket: 44,
            key: 48,
            tool: 56,
            display_type: 80,
            uploaded: 12,
            cmd: 0,
            id: 64,
        });
    }
}
