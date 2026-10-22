//! The scylla utils for entities

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::models::schema::ScyllaProjection;
use crate::models::schema::scylla::{EntitiesByIdView, EntitiesTable};
use crate::models::{EntityKinds, EntityMetadata};

/// A single row of an entity from Scylla
#[derive(Debug, Deserialize, ScyllaProjection)]
#[thorium_projection(of = EntitiesByIdView)]
pub struct EntityRow {
    /// The entity's unique ID
    pub id: Uuid,
    /// The group the entity is available to
    pub group: String,
    /// The entity's kind as a raw string
    pub kind: EntityKinds,
    /// The time this policy was created
    pub created: DateTime<Utc>,
    /// The name of the entity
    pub name: String,
    /// The user who originally submitted the entity
    pub submitter: String,
    /// The data specific to the entity's kind
    #[thorium(column = "kind_data")]
    pub metadata: EntityMetadata,
    /// The entity's description
    pub description: Option<String>,
    /// The path to this entities image if it has one
    pub image: Option<String>,
}

/// A single row from Scylla used for listing entities
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = EntitiesTable)]
pub struct EntityListRow {
    /// The kind of entity this is
    pub kind: EntityKinds,
    /// The group this entity is visible too
    pub group: String,
    /// The time this entity was created
    pub created: DateTime<Utc>,
    /// The entity's unique ID
    pub id: Uuid,
    /// The name of this entity
    pub name: String,
}

/// One group an entity is in, read from one of its rows
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = EntitiesByIdView)]
pub struct EntityGroupRow {
    /// The group this row of the entity is visible to
    pub group: String,
}

/// A single row from Scylla used to supplement tag rows missing name
/// and kind
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = EntitiesByIdView)]
pub struct EntityListSupplementRow {
    /// The entity's unique ID
    pub id: Uuid,
    /// The name of this entity
    pub name: String,
    /// The kind of entity this is
    pub kind: EntityKinds,
}

/// A single row from Scylla with an entity's group and kind
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = EntitiesByIdView)]
pub struct EntityGroupKindRow {
    /// The entity's unique ID
    pub id: Uuid,
    /// The group this row of the entity is visible to
    pub group: String,
    /// The kind of entity this is
    pub kind: EntityKinds,
}
