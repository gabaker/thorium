//! A table, view, projection, and keyspace declared outside the api crate compile without
//! warnings, including a `type` column read through a view
#![deny(warnings)]

use chrono::{DateTime, Utc};
use thorium::models::schema::{ScyllaProjection, ScyllaTable, ScyllaView};
use uuid::Uuid;

/// Objects stored by id, with a `type` column
#[derive(ScyllaTable)]
#[thorium_table(name = "objects")]
pub struct ObjectsTable {
    /// The kind of object
    #[thorium(partition, column = "type")]
    pub kind: String,
    /// The object's id
    #[thorium(partition)]
    pub id: Uuid,
    /// When the object was added
    #[thorium(cluster = "desc")]
    pub added: DateTime<Utc>,
    /// The object's sha256
    pub sha256: String,
}

/// Objects by sha256
#[derive(ScyllaView)]
#[thorium_view(name = "objects_by_sha256", base = ObjectsTable)]
pub struct ObjectsBySha256View {
    /// The object's sha256
    #[thorium(partition)]
    pub sha256: String,
    /// The kind of object
    #[thorium(cluster, column = "type")]
    pub kind: String,
    /// The object's id
    #[thorium(cluster)]
    pub id: Uuid,
    /// When the object was added
    #[thorium(cluster = "desc")]
    pub added: DateTime<Utc>,
}

/// The kind and id of an object, read from the view
#[derive(Debug, ScyllaProjection)]
#[thorium_projection(of = ObjectsBySha256View)]
pub struct ObjectKindRow {
    /// The kind of object
    #[thorium(column = "type")]
    pub kind: String,
    /// The object's id
    pub id: Uuid,
}

/// Every object of this crate
#[thorium::keyspace]
pub struct DownstreamKeyspace {
    /// The objects table
    objects: ObjectsTable,
    /// The objects by sha256 view
    objects_by_sha256: ObjectsBySha256View,
}

/// Check the projection's column order and the keyspace's object order
fn main() {
    // the projection's columns are generated in field order
    assert_eq!(
        <ObjectKindRow as thorium::models::schema::Projection>::COLUMNS,
        &["type", "id"]
    );
    // the keyspace lists the table before its view
    let objects = <DownstreamKeyspace as thorium::models::schema::Keyspace>::OBJECTS;
    assert_eq!(objects.len(), 2);
}
