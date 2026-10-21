//! The Scylla tables and views for samples, S3 ids, and comments

use chrono::{DateTime, Utc};
use std::collections::HashMap;
use uuid::Uuid;

use crate::models::S3Objects;
use crate::models::schema::{JsonText, ScyllaTable, ScyllaView};

/// The ground truth table for every sample submission, one row per group a submission is in
#[derive(ScyllaTable)]
#[thorium_table(name = "samples_list")]
pub struct SamplesListTable {
    /// The group this submission is visible to
    #[thorium(partition)]
    pub group: String,
    /// The year this submission was uploaded in
    #[thorium(partition, year)]
    pub year: i32,
    /// The time bucket within the year, from the upload time and `files.partition_size`
    #[thorium(partition, bucket(partition_size = "files.partition_size"))]
    pub bucket: i32,
    /// When this submission was uploaded
    #[thorium(cluster = "desc", timestamp)]
    pub uploaded: DateTime<Utc>,
    /// The submission id
    #[thorium(cluster = "desc")]
    pub id: Uuid,
    /// The sha256 of the sample
    pub sha256: String,
    /// The sha1 of the sample
    pub sha1: String,
    /// The md5 of the sample
    pub md5: String,
    /// The file name given at upload, if any
    pub name: Option<String>,
    /// A description of the submission, if any
    pub description: Option<String>,
    /// The user who submitted this sample
    pub submitter: String,
    /// The serialized `Origin` of this submission, if any
    pub origin: Option<String>,
}
/// Sample submissions by sha256, used to read every submission of a file
#[derive(ScyllaView)]
#[thorium_view(name = "samples", base = SamplesListTable)]
pub struct SamplesView {
    /// The sha256 of the sample
    #[thorium(partition)]
    pub sha256: String,
    /// The group this submission is visible to
    #[thorium(cluster = "asc")]
    pub group: String,
    /// When this submission was uploaded
    #[thorium(cluster = "desc")]
    pub uploaded: DateTime<Utc>,
    /// The submission id
    #[thorium(cluster = "desc")]
    pub id: Uuid,
    /// The year this submission was uploaded in
    #[thorium(cluster = "asc")]
    pub year: i32,
    /// The time bucket within the year
    #[thorium(cluster = "asc")]
    pub bucket: i32,
    /// The sha1 of the sample
    pub sha1: String,
    /// The md5 of the sample
    pub md5: String,
    /// The file name given at upload, if any
    pub name: Option<String>,
    /// A description of the submission, if any
    pub description: Option<String>,
    /// The user who submitted this sample
    pub submitter: String,
    /// The serialized `Origin` of this submission, if any
    pub origin: Option<String>,
}
/// Maps the random S3 object ids Thorium generates to the object they hold
#[derive(ScyllaTable)]
#[thorium_table(name = "s3_ids")]
pub struct S3IdsTable {
    /// The kind of object this id is for (`File` or `Repo`)
    #[thorium(partition, column = "type")]
    pub object_type: S3Objects,
    /// The S3 object id
    #[thorium(partition)]
    pub id: Uuid,
    /// The key of the object: a sha256 for files, `<repo url>/<sha256>` for repos
    #[thorium(cluster = "asc")]
    pub sha256: String,
}
/// S3 object ids by object key, used to find the object id for a sha256 or repo path
#[derive(ScyllaView)]
#[thorium_view(name = "s3_sha256s", base = S3IdsTable)]
pub struct S3Sha256sView {
    /// The key of the object: a sha256 for files, `<repo url>/<sha256>` for repos
    #[thorium(partition)]
    pub sha256: String,
    /// The kind of object this id is for
    #[thorium(cluster = "asc", column = "type")]
    pub object_type: S3Objects,
    /// The S3 object id
    #[thorium(cluster = "asc")]
    pub id: Uuid,
}
/// Comments on samples, one row per group a comment is shared with
#[derive(ScyllaTable)]
#[thorium_table(name = "comments")]
pub struct CommentsTable {
    /// The group this comment is shared with
    #[thorium(partition)]
    pub group: String,
    /// The sha256 of the sample this comment is on
    #[thorium(partition)]
    pub sha256: String,
    /// When this comment was created
    #[thorium(cluster = "asc", timestamp)]
    pub uploaded: DateTime<Utc>,
    /// The comment id
    #[thorium(cluster = "asc")]
    pub id: Uuid,
    /// The user who wrote this comment
    pub author: String,
    /// The comment text
    pub comment: String,
    /// The comment's attachments as a JSON map of file name to S3 id
    pub files: JsonText<HashMap<String, Uuid>>,
}
/// Comment keys by comment id, used to check whether a comment still exists in any group
#[derive(ScyllaView)]
#[thorium_view(name = "comments_by_id", base = CommentsTable)]
pub struct CommentsByIdView {
    /// The comment id
    #[thorium(partition)]
    pub id: Uuid,
    /// The sha256 of the sample this comment is on
    #[thorium(cluster = "asc")]
    pub sha256: String,
    /// The group this comment is shared with
    #[thorium(cluster = "asc")]
    pub group: String,
    /// When this comment was created
    #[thorium(cluster = "asc")]
    pub uploaded: DateTime<Utc>,
}
