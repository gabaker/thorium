//! Backup and restore support for s3_ids

use ahash::AHasher;
use bytecheck::CheckBytes;
use rkyv::{Archive, Deserialize, Serialize};
use scylla::DeserializeRow;
use scylla::client::session::Session;
use scylla::errors::PrepareError;
use scylla::statement::prepared::PreparedStatement;
use std::hash::Hasher;
use std::path::{Path, PathBuf};
use thorium::Conf;
use thorium::models::{ArchivedS3Objects, S3Objects};
use uuid::Uuid;

use super::Versioned;
use crate::Error;
use crate::backup::s3::{Crawl, Named, object_key};
use crate::backup::{Backup, Restore, S3Backup, S3Restore, Scrub, Utils};

/// A single line of stage logs
#[derive(Debug, Archive, Serialize, Deserialize, DeserializeRow)]
#[archive_attr(derive(Debug, CheckBytes))]
#[scylla(flavor = "enforce_order", skip_name_checks)]
pub struct S3Id {
    /// The type of object this id is for
    pub object_type: S3Objects,
    /// The id of this object
    pub id: Uuid,
    /// The sha256 of this object
    pub sha256: String,
}

impl Utils for S3Id {
    /// The name of the table we are backing up
    fn name() -> &'static str {
        "s3_ids"
    }
}

#[async_trait::async_trait]
impl Backup for S3Id {
    /// The columns of this table's partition key, used to name rows that fail to decode
    fn partition_key() -> &'static [&'static str] {
        &["type", "id"]
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
        // build logs get prepared statement
        scylla
            .prepare(format!(
                "SELECT type, id, sha256 \
                FROM {}.{} \
                Where token(type, id) >= ? AND token(type, id) <= ?",
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
        hasher.write(self.object_type.as_str().as_bytes());
        hasher.write(self.id.as_bytes());
        // finish this hash and get its value
        hasher.finish()
    }
}

/// Implement scrub support for the samples list table
impl Scrub for S3Id {}

/// Implement restore support for the samples list table
#[async_trait::async_trait]
impl Restore for S3Id {
    // The partition size is constant, so the partition config is just
    // the size itself
    type PartitionConf = u16;

    /// The object type, id, and sha256
    type Values = (S3Objects, Uuid, String);

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
                (type, id, sha256) \
                VALUES (?, ?, ?)",
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
    fn partition_conf(_config: &Conf) -> u16 {
        0
    }

    /// Returns the values the restore's insert binds for this row
    ///
    /// # Arguments
    ///
    /// * `_partition_size` - Unused, as s3 ids aren't bucketed
    /// * `_format` - The format version the row was archived in
    fn values(self, _partition_size: &u16, _format: u32) -> Self::Values {
        // every column as the row holds it
        (self.object_type, self.id, self.sha256)
    }
}

#[async_trait::async_trait]
impl S3Backup for S3Id {
    /// Get the s3 urls and where to write them off to disk at
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
        let rows = rkyv::check_archived_root::<Vec<S3Id>>(buffer)?;
        // crawl our rows and build the paths to write to s3
        let paths: Vec<Named> = rows
            .iter()
            .map(|row| {
                // clone our root path
                let mut path = root.clone();
                // get this rows bucket
                match row.object_type {
                    ArchivedS3Objects::File => {
                        // get the bucket for files
                        let bucket = conf.thorium.files.bucket.clone();
                        // get the key for this file in s3
                        let key = row.id.to_string();
                        // nest this in either a file folder or a repos folder
                        path.push("files");
                        // get the sub folder for this path
                        path.push(&key[..chars]);
                        // add the sha256 to this path
                        path.push(&key);
                        Named {
                            bucket,
                            key,
                            path,
                            referrer: format!("{}:{}", S3Id::name(), row.id),
                        }
                    }
                    ArchivedS3Objects::Repo => {
                        // get the bucket for files
                        let bucket = conf.thorium.repos.bucket.clone();
                        // get the key for this file in s3
                        let key = row.id.to_string();
                        // nest this in either a file folder or a repos folder
                        path.push("repos");
                        // get the sub folder for this path
                        path.push(&key[..chars]);
                        // add the sha256 to this path
                        path.push(&key);
                        Named {
                            bucket,
                            key,
                            path,
                            referrer: format!("{}:{}", S3Id::name(), row.id),
                        }
                    }
                }
            })
            .collect();
        Ok(paths.into())
    }
}

/// Format 2 archives s3 ids as format 1 did
impl Versioned for S3Id {
    /// Format 1 archived these rows with the same layout, so they decode as `S3Id` itself
    type V1 = S3Id;
}

impl S3Restore for S3Id {
    /// Get the bucket and key of a backed-up object from where the backup keeps it
    ///
    /// # Arguments
    ///
    /// * `path` - The path to the file we are restoring
    /// * `objects` - The component's `objects/` directory, which `path` is under
    /// * `conf` - The Thorium for this cluster
    fn parse(path: &Path, objects: &Path, conf: &Conf) -> Result<(String, String), Error> {
        // the layout keeps samples and repos apart by bucket role
        let (role, key) = object_key(path, objects, true)?;
        let bucket = match role.as_deref() {
            Some("files") => conf.thorium.files.bucket.clone(),
            Some("repos") => conf.thorium.repos.bucket.clone(),
            _ => {
                return Err(Error::new(format!(
                    "{} names an unknown bucket role",
                    path.display()
                )));
            }
        };
        Ok((bucket, key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the layout of the archived [`S3Id`] row, which written backups depend on
    #[test]
    fn s3_id_layout_is_pinned() {
        // the archived row's size and the offset of every field must stay as backups hold them
        assert_layout!(ArchivedS3Id, 28, {
            object_type: 24,
            id: 0,
            sha256: 16,
        });
    }
}
