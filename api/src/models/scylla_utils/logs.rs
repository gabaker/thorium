//! The scylla utils for reaction stage logs and notifications

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::models::schema::ScyllaProjection;
use crate::models::schema::scylla::{LogsTable, NotificationsTable};
use crate::models::{NotificationLevel, NotificationType};

/// A stage-log line read back from Scylla
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = LogsTable)]
pub struct LogLineRow {
    /// The position of this line in the stage's log
    pub position: i64,
    /// The text of this line
    pub line: String,
}

/// A notification read back from Scylla, with its key as the text it's stored as
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = NotificationsTable)]
pub struct NotificationRow {
    /// The stored key of the notification's related entity
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

/// A notification as backups read it, with the seconds its message has left to live; every
/// column a row may hold unset is optional
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = NotificationsTable)]
pub struct NotificationBackupRow {
    /// The kind of object the notification is for
    pub kind: NotificationType,
    /// The stored key of the notification's related entity
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
    /// The seconds the message has left to live, or `None` when it never expires
    #[thorium(expr = "TTL(msg)")]
    pub ttl: Option<i32>,
}
