//! Backup and restore support for notifications

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
    models::{NotificationLevel, NotificationType},
};
use uuid::Uuid;

use super::Versioned;
use crate::backup::{Backup, Restore, Scrub, Utils};

/// A notifications row as format 2 archives it, with its remaining TTL; `None` is a cell the
/// source row held unset
///
/// The archived row is `repr(C)` so its field offsets follow the declaration order below and do
/// not move when [`NotificationType`] gains a variant (rustc orders the fields of a plain struct by
/// niche size). `repr(C)` also makes rkyv validate out-of-line data in declaration order, which is
/// the order the serializer writes it in. Reordering the fields changes the archive format.
#[derive(Debug, Archive, Serialize, Deserialize, DeserializeRow)]
#[archive_attr(repr(C), derive(Debug, CheckBytes))]
#[scylla(flavor = "enforce_order", skip_name_checks)]
pub struct Notification {
    /// The kind of notification this is
    pub kind: NotificationType,
    /// The key to the item this notification relates to
    pub key: String,
    /// The time this notification was created
    pub created: DateTime<Utc>,
    /// The notification's unique ID
    pub id: Uuid,
    /// The notification's message
    pub msg: Option<String>,
    /// The notification's level
    pub level: Option<NotificationLevel>,
    /// The id of a ban this notification is referencing if there is one
    pub ban_id: Option<Uuid>,
    /// The seconds the row's message had left to live when it was backed up, or `None` when it
    /// was permanent
    pub ttl: Option<i32>,
}

impl Utils for Notification {
    /// The name of the table we are backing up
    fn name() -> &'static str {
        "notifications"
    }
}

#[async_trait::async_trait]
impl Backup for Notification {
    /// The columns of this table's partition key, used to name rows that fail to decode
    fn partition_key() -> &'static [&'static str] {
        &["kind", "key"]
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
                "SELECT kind, key, created, id, msg, level, ban_id, TTL(msg) \
                FROM {}.{} \
                WHERE token(kind, key) >= ? AND token(kind, key) <= ?",
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
        hasher.write(self.key.as_bytes());
        // finish this hash and get its value
        hasher.finish()
    }
}

/// Implement scrub support for the samples list table
impl Scrub for Notification {}

/// Implement restore support for the samples list table
#[async_trait::async_trait]
impl Restore for Notification {
    // notifications are not partitioned
    type PartitionConf = ();

    /// The kind, key, creation time, id, message, level, ban id, and TTL
    type Values = (
        NotificationType,
        String,
        DateTime<Utc>,
        Uuid,
        Option<String>,
        Option<NotificationLevel>,
        Option<Uuid>,
        MaybeUnset<i32>,
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
                (kind, key, created, id, msg, level, ban_id) \
                VALUES (?, ?, ?, ?, ?, ?, ?) USING TTL ?",
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
    fn partition_conf(_config: &Conf) {
        // notifications are not partitioned
    }

    /// Returns the values the restore's insert binds for this row: a format-2 row keeps the TTL
    /// it had left, 0 making a permanent row permanent again, while a format-1 row, which carries
    /// no TTL, leaves it unset so the row takes the table's default TTL
    ///
    /// # Arguments
    ///
    /// * `_partition_conf` - Unused, as notifications aren't bucketed
    /// * `format` - The format version the row was archived in
    fn values(self, _partition_conf: &(), format: u32) -> Self::Values {
        // an unset TTL gives the row the table's default
        let ttl = if format < 2 {
            MaybeUnset::Unset
        } else {
            MaybeUnset::Set(self.ttl.unwrap_or(0))
        };
        (
            self.kind,
            self.key,
            self.created,
            self.id,
            self.msg,
            self.level,
            self.ban_id,
            ttl,
        )
    }
}

/// A notifications row as format 1 archives it, without its TTL
#[derive(Debug, Clone, Archive, Serialize, Deserialize)]
#[archive_attr(derive(Debug, CheckBytes))]
pub struct NotificationV1 {
    /// The kind of notification this is
    pub kind: NotificationType,
    /// The key to the item this notification relates to
    pub key: String,
    /// The time this notification was created
    pub created: DateTime<Utc>,
    /// The notification's unique ID
    pub id: Uuid,
    /// The notification's message
    pub msg: String,
    /// The notification's level
    pub level: NotificationLevel,
    /// The id of a ban this notification is referencing if there is one
    pub ban_id: Option<Uuid>,
}

impl From<NotificationV1> for Notification {
    /// Converts a format-1 row, whose required fields are always set and which carries no TTL
    ///
    /// # Arguments
    ///
    /// * `row` - The format-1 row
    fn from(row: NotificationV1) -> Self {
        // format 1 could only hold rows with these cells set, and didn't archive the added columns
        Notification {
            kind: row.kind,
            key: row.key,
            created: row.created,
            id: row.id,
            msg: Some(row.msg),
            level: Some(row.level),
            ban_id: row.ban_id,
            ttl: None,
        }
    }
}

/// Format 1 archives `Notification` rows as [`NotificationV1`]
impl Versioned for Notification {
    /// Format 1 rows decode as [`NotificationV1`], which converts into `Notification`
    type V1 = NotificationV1;
}

#[cfg(test)]
mod tests {
    use super::*;
    use rkyv::ser::{Serializer, serializers::AllocSerializer};

    /// Archives a value and returns the bytes written
    ///
    /// # Arguments
    ///
    /// * `value` - The value to archive
    fn archive<T: Serialize<AllocSerializer<256>>>(value: &T) -> Vec<u8> {
        // serialize the value as a root object
        let mut serializer = AllocSerializer::<256>::default();
        serializer.serialize_value(value).unwrap();
        serializer.into_serializer().into_inner().to_vec()
    }

    /// Builds a format 2 row with every optional cell set
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind to give the row
    /// * `level` - The level to give the row
    fn row(kind: NotificationType, level: NotificationLevel) -> Notification {
        // fill every cell, including the out-of-line strings
        Notification {
            kind,
            key: "an image key long enough to be stored out of line".to_owned(),
            created: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            id: Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef),
            msg: Some("a message long enough to be stored out of line".to_owned()),
            level: Some(level),
            ban_id: Some(Uuid::from_u128(7)),
            ttl: Some(3600),
        }
    }

    /// Pins the layout of the archived [`Notification`] row, which written backups depend on
    #[test]
    fn notification_layout_is_pinned() {
        // the archived row's size and the offset of every field must stay as backups hold them
        assert_layout!(ArchivedNotification, 80, {
            kind: 0,
            key: 4,
            created: 12,
            id: 24,
            msg: 40,
            level: 52,
            ban_id: 54,
            ttl: 72,
        });
    }

    /// Pins the layout of the archived [`NotificationV1`] row, which written backups depend on
    #[test]
    fn notification_v1_layout_is_pinned() {
        // the archived row's size and the offset of every field must stay as backups hold them
        assert_layout!(ArchivedNotificationV1, 64, {
            kind: 62,
            key: 16,
            created: 32,
            id: 0,
            msg: 24,
            level: 44,
            ban_id: 45,
        });
    }

    /// Pins the byte each notification kind and level archives as
    #[test]
    fn kinds_and_levels_archive_as_their_positions() {
        // every kind archives as its position in the enum
        assert_eq!(archive(&NotificationType::Images), [0]);
        assert_eq!(archive(&NotificationType::Pipelines), [1]);
        // every level archives as its position in the enum
        assert_eq!(archive(&NotificationLevel::Info), [0]);
        assert_eq!(archive(&NotificationLevel::Warn), [1]);
        assert_eq!(archive(&NotificationLevel::Error), [2]);
    }

    /// Checks a written row keeps its kind and level bytes at the pinned offsets and validates
    #[test]
    fn a_written_row_validates_with_its_fields_where_the_layout_says() {
        // archive a row whose kind and level are not the first variants
        let bytes = archive(&row(NotificationType::Pipelines, NotificationLevel::Error));
        // the root object sits at the end of the buffer
        let root = bytes.len() - std::mem::size_of::<ArchivedNotification>();
        // the kind is stored at its pinned offset as its position
        let kind = std::mem::offset_of!(ArchivedNotification, kind);
        assert_eq!(bytes[root + kind], 1);
        // the level is an archived option, a tag byte and then the level's position
        let level = std::mem::offset_of!(ArchivedNotification, level);
        assert_eq!(bytes[root + level..root + level + 2], [1, 2]);
        // the written bytes must pass validation, which checks out-of-line data in field order
        let archived = rkyv::check_archived_root::<Notification>(&bytes).unwrap();
        assert_eq!(archived.ttl, Some(3600_i32));
    }
}
