//! The Scylla tables and views for associations between entities, files, and repos

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::models::schema::ScyllaTable;
use crate::models::{AssociationKind, Directionality};

/// Associations between entities, files, and repos, stored once per direction
#[derive(ScyllaTable)]
#[thorium_table(name = "associations")]
pub struct AssociationsTable {
    /// The group this row grants access to
    #[thorium(partition)]
    pub group: String,
    /// The year this association was created in
    #[thorium(partition, year)]
    pub year: i32,
    /// The time bucket within the year (`associations.partition_size`)
    #[thorium(partition, bucket(partition_size = "associations.partition_size"))]
    pub bucket: i32,
    /// The serialized `AssociationTargetColumn` this row is listed under
    #[thorium(partition)]
    pub source: String,
    /// When this association was created
    #[thorium(cluster = "desc", timestamp)]
    pub created: DateTime<Utc>,
    /// The serialized `AssociationTargetColumn` on the other side
    #[thorium(cluster = "asc")]
    pub target: String,
    /// The direction of this row relative to `source`
    #[thorium(cluster = "asc")]
    pub direction: Directionality,
    /// The kind of association
    pub kind: AssociationKind,
    /// The user that created this association
    pub submitter: String,
    /// Extra data for the source (an entity name)
    pub extra_source: Option<String>,
    /// Extra data for the target (an entity name)
    pub extra_target: Option<String>,
    /// The submission tied to the source side, if any
    pub submissions_source: Option<Uuid>,
    /// The submission tied to the target side, if any
    pub submissions_other: Option<Uuid>,
}
