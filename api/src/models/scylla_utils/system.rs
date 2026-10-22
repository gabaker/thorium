//! The scylla utils for systems structs
use chrono::prelude::*;

use crate::models::schema::ScyllaProjection;
use crate::models::schema::scylla::NodesTable;

/// An internal struct containing a single submission row in Scylla
#[derive(Debug, Serialize, Deserialize, ScyllaProjection)]
#[thorium_projection(of = NodesTable)]
pub struct NodeRow {
    /// The cluster this node is in
    pub cluster: String,
    /// This nodes name
    pub node: String,
    /// This nodes current health
    pub health: String,
    /// The serialized amount of resources this node has in total
    pub resources: String,
    /// The last time this node completed a health check
    pub heart_beat: Option<DateTime<Utc>>,
}

/// The primary keys of a worker row
pub struct WorkerName {
    /// The cluster this worker is assigned to
    pub cluster: String,
    /// The node this worker is on
    pub node: String,
    /// The name of this worker
    pub name: String,
}

impl WorkerName {
    /// Create a new worker name
    ///
    /// # Arguments
    ///
    /// * `cluster` - The cluster this worker is assigned to
    /// * `node` - The node this worker is on
    /// * `name` - The name of this worker
    pub fn new(cluster: String, node: String, name: String) -> Self {
        WorkerName {
            cluster,
            node,
            name,
        }
    }
}
