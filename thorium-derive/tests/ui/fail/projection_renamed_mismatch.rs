//! A renamed projection field is checked against the column it names
use thorium::models::schema::{ScyllaProjection, ScyllaTable};
use uuid::Uuid;

/// The base table the views select from
#[derive(ScyllaTable)]
#[thorium_table(name = "base")]
pub struct BaseTable {
    /// The partition column
    #[thorium(partition)]
    pub group: String,
    /// The clustering column
    #[thorium(cluster)]
    pub id: Uuid,
    /// A bigint column
    pub count: i64,
    /// A text column
    pub sha256: String,
}

/// A row reading the uuid column into a string through a rename
#[derive(ScyllaProjection)]
#[thorium_projection(of = BaseTable)]
pub struct RenamedRow {
    /// The uuid column read as text
    #[thorium(column = "id")]
    pub submission: String,
}

fn main() {}
