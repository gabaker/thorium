//! Backup and restore support for comments

use ahash::AHasher;
use bytecheck::CheckBytes;
use chrono::prelude::*;
use rkyv::{Archive, Deserialize, Serialize};
use scylla::SerializeRow;
use std::collections::HashMap;
use std::hash::Hasher;
use std::path::{Path, PathBuf};
use thorium::Conf;
use uuid::Uuid;

use super::Versioned;
use crate::Error;
use crate::backup::s3::{Crawl, Named, SkippedRow, object_key};
use crate::backup::{Backup, Restore, S3Backup, S3Restore, Scrub, Utils};
use thorium::models::schema::ScyllaProjection;
use thorium::models::schema::scylla::CommentsTable;

/// A comment row as format 2 archives it; `None` is a cell the source row held unset
#[derive(Debug, Archive, Serialize, Deserialize, ScyllaProjection)]
#[archive_attr(derive(Debug, CheckBytes))]
#[thorium_projection(of = CommentsTable)]
pub struct Comment {
    /// The group to share this comment with
    pub group: String,
    /// The sha256 this comment was for
    pub sha256: String,
    /// When this comment was uploaded
    pub uploaded: DateTime<Utc>,
    /// The uuid for this comment
    pub id: Uuid,
    /// The author for this comment
    pub author: Option<String>,
    /// The comment for this file
    pub comment: Option<String>,
    /// Any paths in s3 to files/attachements for this comment
    pub files: Option<String>,
}

impl Utils for Comment {
    /// The name of the table we are backing up
    fn name() -> &'static str {
        "comments"
    }
}

#[async_trait::async_trait]
impl Backup for Comment {
    /// The table this archive struct backs up, read through its generated statements
    type Table = CommentsTable;

    /// The columns of this table's partition key, used to name rows that fail to decode
    fn partition_key() -> &'static [&'static str] {
        &["group", "sha256"]
    }

    /// Hash this partitions info to see if we have changed partitions
    fn hash_partition(&self) -> u64 {
        // build a new hasher
        let mut hasher = AHasher::default();
        // ingest our partition key
        hasher.write(self.group.as_bytes());
        hasher.write(self.sha256.as_bytes());
        // finish this hash and get its value
        hasher.finish()
    }
}

/// Implement scrub support for the samples list table
impl Scrub for Comment {}

/// The values a restored row binds by column name to the table's generated full-row insert
#[derive(SerializeRow)]
pub struct CommentValues {
    /// The `group` column
    pub group: String,
    /// The `sha256` column
    pub sha256: String,
    /// The `uploaded` column
    pub uploaded: DateTime<Utc>,
    /// The `id` column
    pub id: Uuid,
    /// The `author` column
    pub author: Option<String>,
    /// The `comment` column
    pub comment: Option<String>,
    /// The `files` column
    pub files: Option<String>,
}

/// Implement restore support for the samples list table
#[async_trait::async_trait]
impl Restore for Comment {
    // The partition size is constant, so the partition config is just
    // the size itself
    type PartitionConf = u16;

    /// The row the generated full-row insert binds by name
    type Values = CommentValues;

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
    /// * `_partition_size` - Unused, as comments aren't bucketed
    /// * `_format` - The format version the row was archived in
    fn values(self, _partition_size: &u16, _format: u32) -> Self::Values {
        // every column as the row holds it
        CommentValues {
            group: self.group,
            sha256: self.sha256,
            uploaded: self.uploaded,
            id: self.id,
            author: self.author,
            comment: self.comment,
            files: self.files,
        }
    }
}

#[async_trait::async_trait]
impl S3Backup for Comment {
    /// Get the comment attachments and where to write them off to disk at
    ///
    /// A comment whose attachments column isn't valid JSON is returned as a skipped row, and
    /// the attachments of the other comments are still listed.
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
        let rows = rkyv::check_archived_root::<Vec<Comment>>(buffer)?;
        // start with an empty list since most comments probably won't have attachments
        let mut downloads = Vec::default();
        // the comments whose attachments column can't be read
        let mut skipped = Vec::default();
        // crawl our comments and build a list of attachments to write off to disk
        for row in rows.iter() {
            // a comment without its attachments column names no attachments
            let Some(files) = row.files.as_ref() else {
                continue;
            };
            // deserialize our files map, skipping only this comment when the column isn't a files map
            let files: HashMap<String, Uuid> = match serde_json::from_str(files.as_str()) {
                Ok(files) => files,
                Err(error) => {
                    // name the comment so the rest of the archive's attachments are still backed up
                    skipped.push(SkippedRow {
                        referrer: format!("{}:{}/{}", Comment::name(), row.sha256, row.id),
                        error: error.to_string(),
                    });
                    continue;
                }
            };
            // crawl the attachments for this comment
            for (_, id) in files.iter() {
                // get the bucket for comment attachments
                let bucket = conf.thorium.attachments.bucket.clone();
                // get the key for this attachment
                let key = format!("{}/{}/{}", row.sha256, row.id, id);
                // clone our root path
                let mut path = root.clone();
                // get the sub folder for this key
                path.push(&row.sha256[..chars]);
                // add the rest of the key to this path
                path.push(row.sha256.as_str());
                path.push(row.id.to_string());
                path.push(id.to_string());
                // add this attachment to our download list, named by the comment that holds it
                downloads.push(Named {
                    bucket,
                    key,
                    path,
                    referrer: format!("{}:{}/{}", Comment::name(), row.sha256, row.id),
                });
            }
        }
        Ok(Crawl { downloads, skipped })
    }
}

/// A comment row as format 1 archives it
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[archive_attr(derive(Debug, CheckBytes))]
pub struct CommentV1 {
    /// The group to share this comment with
    pub group: String,
    /// The sha256 this comment was for
    pub sha256: String,
    /// When this comment was uploaded
    pub uploaded: DateTime<Utc>,
    /// The uuid for this comment
    pub id: Uuid,
    /// The author for this comment
    pub author: String,
    /// The comment for this file
    pub comment: String,
    /// Any paths in s3 to files/attachements for this comment
    pub files: String,
}

impl From<CommentV1> for Comment {
    /// Converts a format-1 row, whose required fields are always set
    ///
    /// # Arguments
    ///
    /// * `row` - The format-1 row
    fn from(row: CommentV1) -> Self {
        // format 1 could only hold rows with these cells set
        Comment {
            group: row.group,
            sha256: row.sha256,
            uploaded: row.uploaded,
            id: row.id,
            author: Some(row.author),
            comment: Some(row.comment),
            files: Some(row.files),
        }
    }
}

/// Format 1 archives `Comment` rows as [`CommentV1`]
impl Versioned for Comment {
    type V1 = CommentV1;
}

impl S3Restore for Comment {
    /// Get the bucket and key of a backed-up object from where the backup keeps it
    ///
    /// # Arguments
    ///
    /// * `path` - The path to the file we are restoring
    /// * `objects` - The component's `objects/` directory, which `path` is under
    /// * `conf` - The Thorium for this cluster
    fn parse(path: &Path, objects: &Path, conf: &Conf) -> Result<(String, String), Error> {
        // every attachment is in the attachments bucket, keyed by sample, comment, and id
        let (_, key) = object_key(path, objects, false)?;
        Ok((conf.thorium.attachments.bucket.clone(), key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the layout of the archived [`Comment`] row, which written backups depend on
    #[test]
    fn comment_layout_is_pinned() {
        // the archived row's size and the offset of every field must stay as backups hold them
        assert_layout!(ArchivedComment, 80, {
            group: 48,
            sha256: 56,
            uploaded: 36,
            id: 64,
            author: 0,
            comment: 12,
            files: 24,
        });
    }

    /// Pins the layout of the archived [`CommentV1`] row, which written backups depend on
    #[test]
    fn comment_v1_layout_is_pinned() {
        // the archived row's size and the offset of every field must stay as backups hold them
        assert_layout!(ArchivedCommentV1, 68, {
            group: 12,
            sha256: 20,
            uploaded: 0,
            id: 28,
            author: 44,
            comment: 52,
            files: 60,
        });
    }
}
