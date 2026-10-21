//! Backup and restore support for associations

use ahash::AHasher;
use chrono::{DateTime, Utc};
use rkyv::{Archive, CheckBytes, Deserialize, Serialize};
use scylla::SerializeRow;
use scylla::value::MaybeUnset;
use std::hash::Hasher;
use thorium::{
    Conf,
    models::{AssociationKind, Directionality},
};

use uuid::Uuid;

use super::{Versioned, added_in_format_2};
use crate::backup::{Backup, Restore, Scrub, Utils};
use thorium::models::schema::ScyllaProjection;
use thorium::models::schema::scylla::AssociationsTable;

/// An associations row as format 2 archives it; `None` is a cell the source row held unset
#[derive(Debug, Archive, Serialize, Deserialize, ScyllaProjection)]
#[archive_attr(derive(Debug, CheckBytes))]
#[thorium_projection(of = AssociationsTable)]
pub struct Association {
    /// The group this association is for
    pub group: String,
    /// The year the association was created
    pub year: i32,
    /// The bucket the association is in
    pub bucket: i32,
    /// When this association was created
    pub created: DateTime<Utc>,
    /// The direction for this association
    pub direction: Directionality,
    /// The kind of association this is
    pub kind: Option<AssociationKind>,
    /// The source for this association
    pub source: String,
    /// The other serialized data this association is with
    #[thorium(column = "target")]
    pub other: String,
    /// Who created this association
    pub submitter: Option<String>,
    /// Any extra info needed for the source column in this row
    pub extra_source: Option<String>,
    /// Any extra info needed for the target column in this row
    #[thorium(column = "extra_target")]
    pub extra_other: Option<String>,
    /// The submission of the source this association was made for
    pub submissions_source: Option<Uuid>,
    /// The submission of the other side this association was made for
    pub submissions_other: Option<Uuid>,
}

impl Utils for Association {
    /// The name of the table we are backing up
    fn name() -> &'static str {
        "associations"
    }
}

#[async_trait::async_trait]
impl Backup for Association {
    /// The table this archive struct backs up, read through its generated statements
    type Table = AssociationsTable;

    /// The columns of this table's partition key, used to name rows that fail to decode
    fn partition_key() -> &'static [&'static str] {
        &["group", "year", "bucket", "source"]
    }

    /// Hash this partitions info to see if we have changed partitions
    fn hash_partition(&self) -> u64 {
        // build a new hasher
        let mut hasher = AHasher::default();
        // ingest our partition key
        hasher.write(self.group.as_bytes());
        hasher.write_i32(self.year);
        hasher.write_i32(self.bucket);
        hasher.write(self.source.as_bytes());
        // finish this hash and get its value
        hasher.finish()
    }
}

/// Implement scrub support for the samples list table
impl Scrub for Association {}

/// The values a restored row binds by column name to the table's generated full-row insert
#[derive(SerializeRow)]
pub struct AssociationValues {
    /// The `group` column
    pub group: String,
    /// The `year` column
    pub year: i32,
    /// The `bucket` column
    pub bucket: i32,
    /// The `created` column
    pub created: DateTime<Utc>,
    /// The `direction` column
    pub direction: Directionality,
    /// The `kind` column
    pub kind: Option<AssociationKind>,
    /// The `source` column
    pub source: String,
    /// The `target` column
    pub target: String,
    /// The `submitter` column
    pub submitter: Option<String>,
    /// The `extra_source` column
    pub extra_source: Option<String>,
    /// The `extra_target` column
    pub extra_target: Option<String>,
    /// The `submissions_source` column
    pub submissions_source: MaybeUnset<Option<Uuid>>,
    /// The `submissions_other` column
    pub submissions_other: MaybeUnset<Option<Uuid>>,
}

/// Implement restore support for the samples list table
#[async_trait::async_trait]
impl Restore for Association {
    // The partition size is constant, so the partition config is just
    // the size itself
    type PartitionConf = u16;

    /// The row the generated full-row insert binds by name
    type Values = AssociationValues;

    /// Get the partition size for this data type
    ///
    /// # Arguments
    ///
    /// * `conf` - The Thorium config
    fn partition_conf(config: &Conf) -> u16 {
        config.thorium.associations.partition_size
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
        let bucket = thorium::utils::helpers::partition(self.created, self.year, *partition_size);
        // every column, leaving the submissions format 1 didn't archive unset
        AssociationValues {
            group: self.group,
            year: self.year,
            bucket,
            created: self.created,
            direction: self.direction,
            kind: self.kind,
            source: self.source,
            target: self.other,
            submitter: self.submitter,
            extra_source: self.extra_source,
            extra_target: self.extra_other,
            submissions_source: added_in_format_2(self.submissions_source, format),
            submissions_other: added_in_format_2(self.submissions_other, format),
        }
    }
}

/// An associations row as format 1 archives it
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[archive_attr(derive(Debug, CheckBytes))]
pub struct AssociationV1 {
    /// The group this association is for
    pub group: String,
    /// The year the association was created
    pub year: i32,
    /// The bucket the association is in
    pub bucket: i32,
    /// When this association was created
    pub created: DateTime<Utc>,
    /// The direction for this association
    pub direction: Directionality,
    /// The kind of association this is
    pub kind: AssociationKind,
    /// The source for this association
    pub source: String,
    /// The other serialized data this association is with
    pub other: String,
    /// Who created this association
    pub submitter: String,
    /// Any extra info needed for the source column in this row
    pub extra_source: Option<String>,
    /// Any extra info needed for the target column in this row
    pub extra_other: Option<String>,
}

impl From<AssociationV1> for Association {
    /// Converts a format-1 row, whose required fields are always set and which carries no
    /// submissions
    ///
    /// # Arguments
    ///
    /// * `row` - The format-1 row
    fn from(row: AssociationV1) -> Self {
        // format 1 could only hold rows with these cells set, and didn't archive the submissions
        Association {
            group: row.group,
            year: row.year,
            bucket: row.bucket,
            created: row.created,
            direction: row.direction,
            kind: Some(row.kind),
            source: row.source,
            other: row.other,
            submitter: Some(row.submitter),
            extra_source: row.extra_source,
            extra_other: row.extra_other,
            submissions_source: None,
            submissions_other: None,
        }
    }
}

/// Format 1 archives `Association` rows as [`AssociationV1`]
impl Versioned for Association {
    type V1 = AssociationV1;
}
