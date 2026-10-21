//! Backup and restore support for entities

use ahash::AHasher;
use chrono::{DateTime, Utc};
use rkyv::{Archive, CheckBytes, Deserialize, Serialize};
use scylla::SerializeRow;
use std::hash::Hasher;
use thorium::{Conf, models::EntityKinds};
use uuid::Uuid;

use super::Versioned;
use crate::backup::{Backup, Restore, Scrub, Utils};
use thorium::models::schema::ScyllaProjection;
use thorium::models::schema::scylla::EntitiesTable;

/// An entities row as format 2 archives it; `None` is a cell the source row held unset
#[derive(Debug, Archive, Serialize, Deserialize, ScyllaProjection)]
#[archive_attr(derive(Debug, CheckBytes))]
#[thorium_projection(of = EntitiesTable)]
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
    /// The table this archive struct backs up, read through its generated statements
    type Table = EntitiesTable;

    /// The columns of this table's partition key, used to name rows that fail to decode
    fn partition_key() -> &'static [&'static str] {
        &["kind", "group", "year", "bucket"]
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

/// The values a restored row binds by column name to the table's generated full-row insert
#[derive(SerializeRow)]
pub struct EntityValues {
    /// The `kind` column
    pub kind: EntityKinds,
    /// The `group` column
    pub group: String,
    /// The `year` column
    pub year: i32,
    /// The `bucket` column
    pub bucket: i32,
    /// The `created` column
    pub created: DateTime<Utc>,
    /// The `id` column
    pub id: Uuid,
    /// The `name` column
    pub name: Option<String>,
    /// The `submitter` column
    pub submitter: Option<String>,
    /// The `kind_data` column
    pub kind_data: Option<String>,
    /// The `description` column
    pub description: Option<String>,
    /// The `image` column
    pub image: Option<String>,
}

/// Implement restore support for the samples list table
#[async_trait::async_trait]
impl Restore for Entity {
    // The partition size is constant, so the partition config is just
    // the size itself
    type PartitionConf = u16;

    /// The row the generated full-row insert binds by name
    type Values = EntityValues;

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
        EntityValues {
            kind: self.kind,
            group: self.group,
            year: self.year,
            bucket,
            created: self.created,
            id: self.id,
            name: self.name,
            submitter: self.submitter,
            kind_data: self.kind_data,
            description: self.description,
            image: self.image,
        }
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
    type V1 = EntityV1;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the layout of the archived [`Entity`] row, which written backups depend on
    #[test]
    fn entity_layout_is_pinned() {
        // the archived row's size and the offset of every field must stay as backups hold them
        assert_layout!(ArchivedEntity, 108, {
            kind: 104,
            group: 72,
            year: 80,
            bucket: 84,
            created: 60,
            id: 88,
            name: 0,
            submitter: 12,
            kind_data: 24,
            description: 36,
            image: 48,
        });
    }

    /// Pins the layout of the archived [`EntityV1`] row, which written backups depend on
    #[test]
    fn entity_v1_layout_is_pinned() {
        // the archived row's size and the offset of every field must stay as backups hold them
        assert_layout!(ArchivedEntityV1, 96, {
            kind: 92,
            group: 36,
            year: 44,
            bucket: 48,
            created: 24,
            id: 52,
            name: 68,
            submitter: 76,
            kind_data: 84,
            description: 0,
            image: 12,
        });
    }
}
