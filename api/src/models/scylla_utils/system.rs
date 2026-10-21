//! The scylla utils for systems structs
use chrono::prelude::*;
use uuid::Uuid;

#[cfg(feature = "api")]
use crate::models::{ActiveJob, Worker};

use crate::models::schema::ScyllaProjection;
use crate::models::schema::scylla::NodesTable;
use crate::models::{ImageScaler, Pools, WorkerStatus};

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

/// An internal struct for getting worker info from the db
#[derive(Debug)]
pub struct WorkerRow {
    /// The scaler this worker was spawned under
    pub scaler: ImageScaler,
    /// The name of this worker
    pub name: String,
    /// The user this worker is executing a job for
    pub user: String,
    /// The group this worker is executing a job in
    pub group: String,
    /// The pipeline this worker is executing a job in
    pub pipeline: String,
    /// The stage this worker is executing a job for
    pub stage: String,
    /// The current status of this worker
    pub status: WorkerStatus,
    /// When this worker was spawned
    pub spawned: DateTime<Utc>,
    /// The last time this worker checked in with Thorium
    pub heart_beat: Option<DateTime<Utc>>,
    /// The resources used to spawn this worker
    pub resources: String,
    /// The pool this worker was spawned in
    pub pool: Pools,
    /// The reaction this worker is executing a job in
    pub reaction: Option<Uuid>,
    /// The job this worker is executing
    pub job: Option<Uuid>,
}

impl WorkerRow {
    /// Convert a [`WorkerRow`] to a [`Worker`]
    ///
    /// # Arguments
    ///
    /// * `cluster` - The cluster this workers node is in
    /// * `node` - The node this worker is from
    #[cfg(feature = "api")]
    pub fn to_worker(self, cluster: &str, node: &str) -> Result<Worker, crate::utils::ApiError> {
        // if we have reaction and job info then set that
        let active = match (self.reaction, self.job) {
            (Some(reaction), Some(job)) => Some(ActiveJob { reaction, job }),
            _ => None,
        };
        let worker = Worker {
            cluster: cluster.to_owned(),
            node: node.to_owned(),
            scaler: self.scaler,
            name: self.name,
            user: self.user,
            group: self.group,
            pipeline: self.pipeline,
            stage: self.stage,
            status: self.status,
            spawned: self.spawned,
            heart_beat: self.heart_beat,
            resources: crate::deserialize!(&self.resources),
            pool: self.pool,
            active,
        };
        Ok(worker)
    }
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
