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
#[derive(Debug, Archive, Serialize, Deserialize, DeserializeRow)]
#[archive_attr(derive(Debug, CheckBytes))]
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
    type V1 = NotificationV1;
}
