//! A workload source serving fixed tool instances for the launchers' tests

use http::StatusCode;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use thorium::models::{
    Entity, EntityKinds, EntityMetadata, ImageScaler, ToolInstanceEntity, ToolSpec, ToolVersion,
    WorkloadSpec,
};
use uuid::Uuid;

use super::WorkloadSource;
use crate::Error;

/// A workload source that serves fixed tool instances and records each fetch
#[derive(Default)]
pub struct FakeWorkloadSource {
    /// The tool instances this source serves by id
    pub entities: HashMap<Uuid, Entity>,
    /// The tool instances fetched in order
    pub fetches: Mutex<Vec<Uuid>>,
    /// The token of each run this source serves; every other run is not found
    pub run_tokens: HashMap<Uuid, String>,
    /// The group and run of each run token read in order
    pub token_reads: Mutex<Vec<(String, Uuid)>>,
    /// The runs whose token reads fail with a server error
    pub failing_runs: HashSet<Uuid>,
    /// The scaler and name of each worker deleted in order
    pub deletes: Mutex<Vec<(ImageScaler, String)>>,
}

impl FakeWorkloadSource {
    /// Serve a tool instance running a workload
    ///
    /// # Arguments
    ///
    /// * `id` - The tool instance's id
    /// * `tool` - The tool's name
    /// * `groups` - The tool instance's groups
    /// * `spec` - The workload the tool instance runs
    pub fn serve(&mut self, id: Uuid, tool: &str, groups: &[&str], spec: WorkloadSpec) {
        // build a tool instance entity named after its tool and version
        let entity = Entity {
            id,
            name: format!("{tool}@1.0.0"),
            kind: EntityKinds::ToolInstance,
            metadata: EntityMetadata::ToolInstance(ToolInstanceEntity {
                tool: Uuid::new_v4(),
                version: ToolVersion::from("1.0.0"),
                previous: None,
                spec_hash: String::new(),
                deprecated: false,
                spec: ToolSpec::Image(spec),
                portability: None,
                provides: Vec::new(),
            }),
            description: None,
            submitter: "thorium".to_owned(),
            groups: groups.iter().map(|group| (*group).to_owned()).collect(),
            tags: HashMap::new(),
            image: None,
            created: chrono::Utc::now(),
        };
        self.entities.insert(id, entity);
    }

    /// Get the tool instances fetched so far in order
    ///
    /// # Panics
    ///
    /// Panics if a fetch panicked while recording itself
    pub fn fetched(&self) -> Vec<Uuid> {
        self.fetches.lock().unwrap().clone()
    }

    /// Get the run tokens read so far in order
    ///
    /// # Panics
    ///
    /// Panics if a read panicked while recording itself
    pub fn token_reads(&self) -> Vec<(String, Uuid)> {
        self.token_reads.lock().unwrap().clone()
    }

    /// Get the workers deleted so far in order
    ///
    /// # Panics
    ///
    /// Panics if a delete panicked while recording itself
    pub fn deleted(&self) -> Vec<(ImageScaler, String)> {
        self.deletes.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl WorkloadSource for FakeWorkloadSource {
    /// Serve a fixed tool instance and record the fetch
    ///
    /// # Arguments
    ///
    /// * `id` - The id of the tool instance to get
    async fn tool_instance(&self, id: Uuid) -> Result<Entity, Error> {
        // record this fetch
        self.fetches.lock().unwrap().push(id);
        // serve the tool instance if we have it
        self.entities
            .get(&id)
            .cloned()
            .ok_or_else(|| Error::new(format!("tool instance {id} not found")))
    }

    /// Serve a fixed run token and record the read
    ///
    /// # Arguments
    ///
    /// * `group` - The group the run is in
    /// * `run` - The run whose token to get
    async fn run_token(&self, group: &str, run: Uuid) -> Result<String, Error> {
        // record this read
        self.token_reads
            .lock()
            .unwrap()
            .push((group.to_owned(), run));
        // fail the read with a server error if asked to
        if self.failing_runs.contains(&run) {
            return Err(Error::from(thorium::Error::Thorium {
                code: StatusCode::INTERNAL_SERVER_ERROR,
                msg: Some(format!("run {run} token read failed")),
            }));
        }
        // serve the run's token if it still has one
        self.run_tokens.get(&run).cloned().ok_or_else(|| {
            Error::from(thorium::Error::Thorium {
                code: StatusCode::NOT_FOUND,
                msg: Some(format!("run {run} has no token")),
            })
        })
    }

    /// Record the deleted workers
    ///
    /// # Arguments
    ///
    /// * `scaler` - The scaler the workers are registered under
    /// * `workers` - The names of the workers to delete
    async fn delete_workers(&self, scaler: ImageScaler, workers: Vec<String>) -> Result<(), Error> {
        // record each deleted worker
        self.deletes
            .lock()
            .unwrap()
            .extend(workers.into_iter().map(|name| (scaler, name)));
        Ok(())
    }
}
