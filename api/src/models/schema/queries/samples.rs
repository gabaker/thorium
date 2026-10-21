//! The statements on the samples, S3 id, and comments tables and their views

use crate::models::schema::scylla::{
    CommentsByIdView, CommentsTable, S3IdsTable, S3Sha256sView, SamplesListTable, SamplesView,
};
use crate::models::{
    BasicSubmissionRow, CommentIdRow, CommentRow, S3IdRow, Sha256Row, SubmissionListRow,
    SubmissionRow,
};

/// Statements on the `samples_list` table
#[thorium::table_queries(table = SamplesListTable)]
pub struct SamplesListQueries {
    /// Insert or overwrite one submission row for a group
    #[insert(columns = all, ttl = none)]
    insert: Insert,
    /// Delete one submission from several groups
    #[delete(by_key, r#in(group), eq(year, bucket, uploaded, id))]
    delete_multiple_groups: Delete,
    /// The cursor listing submissions newest first across groups
    #[cursor(kind = time, group_by = group, year = year, bucket = bucket, range = uploaded,
             tie = id, project = SubmissionListRow, limit = per_partition)]
    list: Cursor,
    /// The number of submissions in each partition of a token range, for the census
    #[census(project = crate::models::files::SampleCensusRow)]
    census: Census,
}

/// Statements on the samples view, which finds submissions by sha256
#[thorium::view_queries(view = SamplesView)]
pub struct SamplesQueries {
    /// Every submission of a sha256 in a set of groups
    #[select(project = SubmissionRow, eq(sha256), r#in(group))]
    get: Select,
    /// Every submission of a set of sha256s in a set of groups
    #[select(project = SubmissionRow, r#in(sha256, group), chunk = 50)]
    get_many: Select,
    /// Which of a set of sha256s at least one of a set of groups can see
    #[select(project = Sha256Row, r#in(sha256, group), group_by(sha256), chunk = 50)]
    auth: Select,
    /// The group, submitter, and id of every submission of a sha256
    #[select(project = BasicSubmissionRow, eq(sha256))]
    get_basic_submission_info: Select,
}

/// Statements on the `s3_ids` table
#[thorium::table_queries(table = S3IdsTable)]
pub struct S3IdsQueries {
    /// Insert an S3 id
    #[insert(columns = all, ttl = none)]
    insert: Insert,
    /// Whether an S3 id is already in use
    #[select(eq(object_type, id), exists)]
    id_exists: Select,
    /// Delete every row of an S3 id
    #[delete(partition, eq(object_type, id))]
    delete: Delete,
}

/// Statements on the `s3_sha256s` view, which finds S3 ids by object key
#[thorium::view_queries(view = S3Sha256sView)]
pub struct S3Sha256sQueries {
    /// Whether an object key already has an S3 id
    #[select(eq(sha256, object_type), exists)]
    object_exists: Select,
    /// The S3 id of an object key
    #[select(project = S3IdRow, eq(sha256, object_type), limit = 1)]
    get: Select,
}

/// Statements on the comments table
#[thorium::table_queries(table = CommentsTable)]
pub struct CommentsQueries {
    /// Insert a comment row for one group
    #[insert(columns = all, ttl = none)]
    insert: Insert,
    /// Every comment on a sha256 in a set of groups
    #[select(project = CommentRow, r#in(group), eq(sha256))]
    get: Select,
    /// Delete one comment row
    #[delete(by_key, eq(group, sha256, uploaded, id))]
    delete: Delete,
}

/// Statements on the `comments_by_id` view, which finds comments by id
#[thorium::view_queries(view = CommentsByIdView)]
pub struct CommentsByIdQueries {
    /// Which of a set of comment ids still exist in any group
    #[select(project = CommentIdRow, r#in(id), group_by(id))]
    exists: Select,
}
