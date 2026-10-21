//! The Scylla tables and views for notifications for images and pipelines

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::models::schema::ScyllaTable;
use crate::models::{NotificationLevel, NotificationType};

/// Notifications for images and pipelines; expire after `retention.notifications` unless inserted with TTL 0
#[derive(ScyllaTable)]
#[thorium_table(name = "notifications", ttl = "thorium.retention.notifications")]
pub struct NotificationsTable {
    /// The kind of object this notification is for (`as_str` text via `ScyllaStoreAsStr`)
    #[thorium(partition)]
    pub kind: NotificationType,
    /// The object's key (JSON of `ImageKey`/`PipelineKey`; `N::Key` in the API)
    #[thorium(partition)]
    pub key: String,
    /// When this notification was created
    #[thorium(cluster = "asc")]
    pub created: DateTime<Utc>,
    /// The notification's id
    #[thorium(cluster = "asc")]
    pub id: Uuid,
    /// The message
    pub msg: String,
    /// The level (`as_str` text)
    pub level: NotificationLevel,
    /// The ban this notification was created for, if any
    pub ban_id: Option<Uuid>,
}
