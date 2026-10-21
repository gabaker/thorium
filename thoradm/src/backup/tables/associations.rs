//! Backup and restore support for associations

use ahash::AHasher;
use chrono::{DateTime, Utc};
use rkyv::{Archive, CheckBytes, Deserialize, Serialize};
use scylla::{
    DeserializeRow, client::session::Session, errors::PrepareError,
    statement::prepared::PreparedStatement, value::MaybeUnset,
};
use std::hash::Hasher;
use thorium::{
    Conf,
    models::{AssociationKind, Directionality},
};

use uuid::Uuid;

use super::{Versioned, added_in_format_2};
use crate::backup::{Backup, Restore, Scrub, Utils};

/// An associations row as format 2 archives it; `None` is a cell the source row held unset
#[derive(Debug, Archive, Serialize, Deserialize, DeserializeRow)]
#[archive_attr(derive(Debug, CheckBytes))]
#[scylla(flavor = "enforce_order", skip_name_checks)]
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
    pub other: String,
    /// Who created this association
    pub submitter: Option<String>,
    /// Any extra info needed for the source column in this row
    pub extra_source: Option<String>,
    /// Any extra info needed for the target column in this row
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
    /// The columns of this table's partition key, used to name rows that fail to decode
    fn partition_key() -> &'static [&'static str] {
        &["group", "year", "bucket", "source"]
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
                "SELECT group, year, bucket, created, direction, kind, source, target, submitter, extra_source, extra_target, \
                submissions_source, submissions_other \
                FROM {}.{} \
                WHERE token(group, year, bucket, source) >= ? AND token(group, year, bucket, source) <= ?",
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
        hasher.write(self.source.as_bytes());
        // finish this hash and get its value
        hasher.finish()
    }
}

/// Implement scrub support for the samples list table
impl Scrub for Association {}

/// Implement restore support for the samples list table
#[async_trait::async_trait]
impl Restore for Association {
    // The partition size is constant, so the partition config is just
    // the size itself
    type PartitionConf = u16;

    /// The group, year, bucket, creation time, direction, kind, source, target, submitter, extra
    /// data of each side, and submission of each side
    type Values = (
        String,
        i32,
        i32,
        DateTime<Utc>,
        Directionality,
        Option<AssociationKind>,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        MaybeUnset<Option<Uuid>>,
        MaybeUnset<Option<Uuid>>,
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
                (group, year, bucket, created, direction, kind, source, target, submitter, extra_source, extra_target, \
                submissions_source, submissions_other) \
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
        (
            self.group,
            self.year,
            bucket,
            self.created,
            self.direction,
            self.kind,
            self.source,
            self.other,
            self.submitter,
            self.extra_source,
            self.extra_other,
            added_in_format_2(self.submissions_source, format),
            added_in_format_2(self.submissions_other, format),
        )
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
    /// Format 1 rows decode as [`AssociationV1`], which converts into `Association`
    type V1 = AssociationV1;
}
