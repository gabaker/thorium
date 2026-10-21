//! Backup and restore support for entities

use ahash::AHasher;
use chrono::{DateTime, Utc};
use rkyv::{Archive, CheckBytes, Deserialize, Serialize};
use scylla::{
    DeserializeRow, client::session::Session, errors::PrepareError,
    statement::prepared::PreparedStatement,
};
use std::hash::Hasher;
use thorium::{Conf, models::EntityKinds};
use uuid::Uuid;

use super::Versioned;
use crate::backup::{Backup, Restore, Scrub, Utils};

/// An entities row as format 2 archives it; `None` is a cell the source row held unset
#[derive(Debug, Archive, Serialize, Deserialize, DeserializeRow)]
#[archive_attr(derive(Debug, CheckBytes))]
#[scylla(flavor = "enforce_order", skip_name_checks)]
pub struct Entity {
    /// The entity's kind as a raw string
    pub kind: EntityKinds,
    /// The group the entity is available to
    pub group: String,
    /// The year this entity is from
    pub year: i32,
    /// The bucket this entity is in
    pub bucket: i32,
    /// The time this policy was created
    pub created: DateTime<Utc>,
    /// The entity's unique ID
    pub id: Uuid,
    /// The name of the entity
    pub name: Option<String>,
    /// The user who originally submitted the entity
    pub submitter: Option<String>,
    /// The data specific to the entity's kind
    pub kind_data: Option<String>,
    /// The entity's description
    pub description: Option<String>,
    /// The path to this entities image if it has one
    pub image: Option<String>,
}

impl Utils for Entity {
    /// The name of the table we are backing up
    fn name() -> &'static str {
        "entities"
    }
}

#[async_trait::async_trait]
impl Backup for Entity {
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
        // build logs get prepared statement
        scylla
            .prepare(format!(
                "SELECT kind, group, year, bucket, created, id, name, submitter, kind_data, description, image \
                FROM {}.{} \
                WHERE token(kind, group, year, bucket) >= ? AND token(kind, group, year, bucket) <= ?",
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
        hasher.write(self.kind.as_str().as_bytes());
        hasher.write(self.group.as_bytes());
        hasher.write_i32(self.year);
        hasher.write_i32(self.bucket);
        // finish this hash and get its value
        hasher.finish()
    }
}

/// Implement scrub support for the samples list table
impl Scrub for Entity {}

/// Implement restore support for the samples list table
#[async_trait::async_trait]
impl Restore for Entity {
    // The partition size is constant, so the partition config is just
    // the size itself
    type PartitionConf = u16;

    /// The kind, group, year, bucket, creation time, id, name, submitter, kind data,
    /// description, and image
    type Values = (
        EntityKinds,
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
                (kind, group, year, bucket, created, id, name, submitter, kind_data, description, image) \
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
        config.thorium.entities.partition_size
    }

    /// Returns the values the restore's insert binds for this row, in a bucket of the target's
    /// partition size
    ///
    /// # Arguments
    ///
    /// * `partition_size` - The partition size to bucket the row with
    /// * `_format` - The format version the row was archived in
    fn values(self, partition_size: &u16, _format: u32) -> Self::Values {
        // calculate the new bucket
        let bucket = thorium::utils::helpers::partition(self.created, self.year, *partition_size);
        (
            self.kind,
            self.group,
            self.year,
            bucket,
            self.created,
            self.id,
            self.name,
            self.submitter,
            self.kind_data,
            self.description,
            self.image,
        )
    }
}

/// An entities row as format 1 archives it
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[archive_attr(derive(Debug, CheckBytes))]
pub struct EntityV1 {
    /// The entity's kind as a raw string
    pub kind: EntityKinds,
    /// The group the entity is available to
    pub group: String,
    /// The year this entity is from
    pub year: i32,
    /// The bucket this entity is in
    pub bucket: i32,
    /// The time this policy was created
    pub created: DateTime<Utc>,
    /// The entity's unique ID
    pub id: Uuid,
    /// The name of the entity
    pub name: String,
    /// The user who originally submitted the entity
    pub submitter: String,
    /// The data specific to the entity's kind
    pub kind_data: String,
    /// The entity's description
    pub description: Option<String>,
    /// The path to this entities image if it has one
    pub image: Option<String>,
}

impl From<EntityV1> for Entity {
    /// Converts a format-1 row, whose required fields are always set
    ///
    /// # Arguments
    ///
    /// * `row` - The format-1 row
    fn from(row: EntityV1) -> Self {
        // format 1 could only hold rows with these cells set
        Entity {
            kind: row.kind,
            group: row.group,
            year: row.year,
            bucket: row.bucket,
            created: row.created,
            id: row.id,
            name: Some(row.name),
            submitter: Some(row.submitter),
            kind_data: Some(row.kind_data),
            description: row.description,
            image: row.image,
        }
    }
}

/// Format 1 archives `Entity` rows as [`EntityV1`]
impl Versioned for Entity {
    /// Format 1 rows decode as [`EntityV1`], which converts into `Entity`
    type V1 = EntityV1;
}
