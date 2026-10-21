//! The Scylla tables and views for entities

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::models::schema::{ScyllaTable, ScyllaView};
use crate::models::{EntityKinds, EntityMetadata};

/// Entities by kind, group, and time bucket; the ground truth for entity listing
#[derive(ScyllaTable)]
#[thorium_table(name = "entities")]
pub struct EntitiesTable {
    /// The kind of entity (stored as JSON text, e.g. `"Device"` with quotes, via `ScyllaStoreJson`)
    #[thorium(partition)]
    pub kind: EntityKinds,
    /// The group this row grants access to
    #[thorium(partition)]
    pub group: String,
    /// The year this entity was created in
    #[thorium(partition, year)]
    pub year: i32,
    /// The time bucket within the year, from the creation time and `entities.partition_size`
    #[thorium(partition, bucket(partition_size = "entities.partition_size"))]
    pub bucket: i32,
    /// When this entity was created
    #[thorium(cluster = "asc", timestamp)]
    pub created: DateTime<Utc>,
    /// The entity's id
    #[thorium(cluster = "asc")]
    pub id: Uuid,
    /// The entity's name
    pub name: String,
    /// The user that created this entity
    pub submitter: String,
    /// The kind-specific data (JSON text via `ScyllaStoreJson`)
    pub kind_data: EntityMetadata,
    /// An optional description
    pub description: Option<String>,
    /// An optional image (S3 path) for this entity
    pub image: Option<String>,
}
/// Entities by id, for getting an entity's details and its groups
#[derive(ScyllaView)]
#[thorium_view(name = "entities_by_id", base = EntitiesTable)]
pub struct EntitiesByIdView {
    /// The entity's id
    #[thorium(partition)]
    pub id: Uuid,
    /// The group this row grants access to
    #[thorium(cluster = "asc")]
    pub group: String,
    /// The kind of entity
    #[thorium(cluster = "asc")]
    pub kind: EntityKinds,
    /// When this entity was created
    #[thorium(cluster = "asc")]
    pub created: DateTime<Utc>,
    /// The year this entity was created in
    #[thorium(cluster = "asc")]
    pub year: i32,
    /// The time bucket within the year
    #[thorium(cluster = "asc")]
    pub bucket: i32,
    /// The entity's name (the one non-key base column allowed in the view key)
    #[thorium(cluster = "asc")]
    pub name: String,
    /// The user that created this entity
    pub submitter: String,
    /// The kind-specific data
    pub kind_data: EntityMetadata,
    /// An optional description
    pub description: Option<String>,
    /// An optional image
    pub image: Option<String>,
}
