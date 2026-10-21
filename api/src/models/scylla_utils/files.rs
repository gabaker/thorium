//! The scylla utils/structs for files

use chrono::prelude::*;
use uuid::Uuid;

use crate::models::schema::ScyllaProjection;
use crate::models::schema::scylla::{
    CommentsByIdView, CommentsTable, S3Sha256sView, SamplesListTable, SamplesView,
};

/// An internal struct containing a single submission row in Scylla
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = SamplesListTable)]
pub struct SubmissionListRow {
    /// The group this submission was apart of (used only for cursor generation)
    pub group: String,
    /// The sha256 of this sample
    pub sha256: String,
    /// The submission ID for this instance of this sample
    #[thorium(column = "id")]
    pub submission: Uuid,
    /// The timestamp this was last uploaded
    pub uploaded: DateTime<Utc>,
}

/// An internal struct containing a single submission row in Scylla
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = SamplesView)]
pub struct SubmissionRow {
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
    /// The group this submisison is for
    pub group: String,
    /// The user who submitted this sample
    pub submitter: String,
    /// Where this sample originates from if anywhere in serial form
    pub origin: Option<String>,
    /// When this sample was uploaded
    pub uploaded: DateTime<Utc>,
}

/// The sha256 of a submission, read when checking which sha256s some groups can see
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = SamplesView)]
pub struct Sha256Row {
    /// The sha256 of the sample
    pub sha256: String,
}

/// The group, submitter, and id of one submission of a sample
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = SamplesView)]
pub struct BasicSubmissionRow {
    /// The group this submission is visible to
    pub group: String,
    /// The user who submitted this sample
    pub submitter: String,
    /// The submission id
    pub id: Uuid,
}

/// The S3 id of an object
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = S3Sha256sView)]
pub struct S3IdRow {
    /// The S3 object id
    pub id: Uuid,
}

/// A request for a comment about a specific sample
#[derive(Serialize, Deserialize, Debug, Clone, ScyllaProjection)]
#[thorium_projection(of = CommentsTable)]
pub struct CommentRow {
    /// The group to share this comment with
    pub group: String,
    /// The sha256 of the sample this comment is for
    pub sha256: String,
    /// When this comment was uploaded
    pub uploaded: DateTime<Utc>,
    /// The uuid for this comment
    pub id: Uuid,
    /// the author of this comment
    pub author: String,
    /// The comment for this file
    pub comment: String,
    /// Any paths in s3 to files/attachements for this comment in serialized form
    pub files: String,
}

/// The id of a comment that still exists in some group
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = CommentsByIdView)]
pub struct CommentIdRow {
    /// The comment id
    pub id: Uuid,
}
