//! The Scylla tables and views for the nodes registered with Thorium

use chrono::{DateTime, Utc};

use crate::models::schema::ScyllaTable;
use crate::models::{NodeHealth, Resources};

/// The nodes registered with Thorium
#[derive(ScyllaTable)]
#[thorium_table(name = "nodes")]
pub struct NodesTable {
    /// The cluster this node is in
    #[thorium(partition)]
    pub cluster: String,
    /// The node's name
    #[thorium(cluster = "asc")]
    pub node: String,
    /// The node's health (JSON text via `ScyllaStoreJson`)
    pub health: NodeHealth,
    /// The node's resources (JSON text via `ScyllaStoreJson`; contains u64s inside the JSON)
    pub resources: Resources,
    /// The last time this node sent a heart beat (unset until the first heart-beat update)
    pub heart_beat: Option<DateTime<Utc>>,
}
