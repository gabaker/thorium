//! Handles the launching of jobs for specific reactor types
//!
//! Currently only windows is supported;

use std::collections::{HashMap, HashSet};

#[cfg(any(target_os = "linux", target_os = "windows"))]
use http::StatusCode;
use thorium::Thorium;
#[cfg(any(target_os = "linux", target_os = "windows"))]
use thorium::models::runtime::DEFAULT_PROJECTED_RUNTIME;
#[cfg(any(target_os = "linux", target_os = "windows"))]
use thorium::models::{Entity, EntityMetadata, Image, ImageScaler, ToolSpec, WorkerDeleteMap};
use thorium::models::{Node, Worker};
#[cfg(any(target_os = "linux", target_os = "windows"))]
use uuid::Uuid;

use crate::Error;

#[cfg(target_os = "linux")]
mod bare_metal;
//#[cfg(feature = "kvm")]
#[cfg(target_os = "linux")]
#[cfg(feature = "kvm")]
pub mod kvm;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
pub use bare_metal::BareMetal;
#[cfg(target_os = "windows")]
pub use windows::Windows;

/// Where launchers get the workloads of the tool instances their workers run, and where the
/// reactor removes workers that can never launch
#[cfg(any(target_os = "linux", target_os = "windows"))]
#[async_trait::async_trait]
pub trait WorkloadSource: Send + Sync {
    /// Get a tool instance
    ///
    /// # Arguments
    ///
    /// * `id` - The id of the tool instance to get
    async fn tool_instance(&self, id: Uuid) -> Result<Entity, Error>;

    /// Get the token a run's workers authenticate with
    ///
    /// # Arguments
    ///
    /// * `group` - The group the run is in
    /// * `run` - The run whose token to get
    async fn run_token(&self, group: &str, run: Uuid) -> Result<String, Error>;

    /// Delete workers, treating workers that are already gone as deleted
    ///
    /// # Arguments
    ///
    /// * `scaler` - The scaler the workers are registered under
    /// * `workers` - The names of the workers to delete
    async fn delete_workers(&self, scaler: ImageScaler, workers: Vec<String>) -> Result<(), Error>;

    /// Get the image a launcher reads to launch a worker
    ///
    /// The worker's tool instance is fetched once, its workload must run in the launcher's
    /// environment, and its spec is projected into the image shape the launchers read, in the
    /// worker's group and named after the tool.
    ///
    /// # Arguments
    ///
    /// * `worker` - The worker to get the image for
    /// * `scaler` - The environment of the launcher reading the image
    async fn workload_image(&self, worker: &Worker, scaler: ImageScaler) -> Result<Image, Error> {
        // get the worker's tool instance
        let entity = self
            .tool_instance(worker.tool_instance)
            .await
            .map_err(|err| {
                Error::new(format!(
                    "worker {}: failed to get tool instance {}: {err}",
                    worker.name, worker.tool_instance
                ))
            })?;
        // get the workload the tool instance runs
        let EntityMetadata::ToolInstance(meta) = &entity.metadata else {
            return Err(Error::new(format!(
                "worker {}: {} is not a tool instance",
                worker.name, worker.tool_instance
            )));
        };
        let spec = match &meta.spec {
            ToolSpec::Image(spec) => spec,
            ToolSpec::Harness(harness) => &harness.workload,
            ToolSpec::Agentic(_) => {
                return Err(Error::new(format!(
                    "worker {}: tool instance {} runs no workload",
                    worker.name, worker.tool_instance
                )));
            }
        };
        // the workload must run in this launcher's environment
        let environment = spec.environment.scaler();
        if environment != scaler {
            return Err(Error::new(format!(
                "worker {}: tool instance {} runs in {environment}, not {scaler}",
                worker.name, worker.tool_instance
            )));
        }
        // the worker's group must be one of the tool instance's groups
        if !entity.groups.contains(&worker.group) {
            return Err(Error::new(format!(
                "worker {}: tool instance {} is not in group {}",
                worker.name, worker.tool_instance, worker.group
            )));
        }
        // a tool instance is named after its tool and version
        let suffix = format!("@{}", meta.version);
        let name = entity.name.strip_suffix(&suffix).unwrap_or(&entity.name);
        // project the spec into the image the launchers read
        Ok(spec.project_image(&worker.group, name, DEFAULT_PROJECTED_RUNTIME))
    }
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
#[async_trait::async_trait]
impl WorkloadSource for Thorium {
    /// Get a tool instance from the API
    ///
    /// # Arguments
    ///
    /// * `id` - The id of the tool instance to get
    async fn tool_instance(&self, id: Uuid) -> Result<Entity, Error> {
        // read the tool instance entity
        Ok(self.entities.get(id).await?)
    }

    /// Get a run's token through the admin run token route
    ///
    /// # Arguments
    ///
    /// * `group` - The group the run is in
    /// * `run` - The run whose token to get
    async fn run_token(&self, group: &str, run: Uuid) -> Result<String, Error> {
        // read the run's scoped token
        Ok(self.reactions.run_token(group, run).await?.token)
    }

    /// Delete workers through the system worker route
    ///
    /// # Arguments
    ///
    /// * `scaler` - The scaler the workers are registered under
    /// * `workers` - The names of the workers to delete
    async fn delete_workers(&self, scaler: ImageScaler, workers: Vec<String>) -> Result<(), Error> {
        // build the list of workers to delete
        let deletes = workers
            .into_iter()
            .fold(WorkerDeleteMap::with_capacity(0), WorkerDeleteMap::add);
        // delete the workers in one batch; a worker that is already gone is not an error
        match self.system.delete_workers(scaler, &deletes).await {
            Ok(_) => Ok(()),
            // a 404 means at least one of these workers is already gone and, since the API
            // deletes a batch all or nothing, none of them were deleted; the workers still
            // registered are deleted on the next poll
            Err(error) if error.status() == Some(StatusCode::NOT_FOUND) => Ok(()),
            Err(error) => Err(Error::from(error)),
        }
    }
}

#[async_trait::async_trait]
pub trait Launcher: Send + Sync + 'static {
    /// The data returned for an individual launched worker
    type LaunchedWorkerData: Send;

    /// Launch a worker in a new task, returning the worker that was launched and
    /// any data to resolve in the launcher's state afterward
    ///
    /// # Arguments
    ///
    /// * `thorium` - A Thorium client
    /// * `worker` - The worker to launch
    async fn launch(
        &self,
        thorium: &Thorium,
        worker: Worker,
    ) -> Result<(Worker, Self::LaunchedWorkerData), Error>;

    /// Resolve all of the data from this batch of launches, modifying any relevant data
    /// in the launcher's state
    ///
    /// # Arguments
    ///
    /// * `launches` - The batch of workers and their data to resolve
    fn resolve_launches(&mut self, launches: Vec<(&Worker, Self::LaunchedWorkerData)>);

    /// Reconcile the current worker state with what is reported in the API,
    /// handling worker failures, reclaiming workers, etc.
    ///
    /// # Arguments
    ///
    /// * `thorium` - A Thorium client
    /// * `info` - Info about our node and its workers
    /// * `active` - The names of the currently active workers in the reactor
    async fn reconcile(
        &mut self,
        thorium: &Thorium,
        info: &mut Node,
        active: &mut HashMap<String, Worker>,
    ) -> Result<(), Error>;

    /// Shutdown a list of workers
    ///
    /// # Arguments
    ///
    /// * `thorium` - A Thorium client
    /// * `workers` - The workers to shutdown
    async fn shutdown(
        &mut self,
        thorium: &Thorium,
        mut workers: HashSet<String>,
    ) -> Result<(), Error>;
}

#[cfg(test)]
pub mod fake;

#[cfg(test)]
pub mod tests {
    use super::WorkloadSource;
    use super::fake::FakeWorkloadSource;
    use thorium::models::{
        ImageScaler, Kvm, KvmEnvironment, Pools, Resources, Worker, WorkerStatus,
        WorkloadEnvironment, WorkloadSpec,
    };
    use uuid::Uuid;

    /// Build a worker of a tool instance in a group
    ///
    /// # Arguments
    ///
    /// * `tool_instance` - The tool instance the worker runs
    /// * `group` - The group the worker runs in
    #[must_use]
    pub fn worker(tool_instance: Uuid, group: &str) -> Worker {
        Worker {
            cluster: "cluster0".to_owned(),
            node: "node0".to_owned(),
            scaler: ImageScaler::Kvm,
            name: "amalthea-worker".to_owned(),
            user: "amalthea".to_owned(),
            group: group.to_owned(),
            tool_instance,
            run: None,
            status: WorkerStatus::Spawning,
            spawned: chrono::Utc::now(),
            heart_beat: None,
            resources: Resources::default(),
            pool: Pools::Deadline,
            active: None,
        }
    }

    /// Build a Kvm workload spec
    ///
    /// # Arguments
    ///
    /// * `kvm` - The stored golden paths, if any
    #[must_use]
    pub fn kvm_spec(kvm: Option<Kvm>) -> WorkloadSpec {
        WorkloadSpec {
            environment: WorkloadEnvironment::Kvm(KvmEnvironment {
                kvm,
                ..KvmEnvironment::default()
            }),
            ..WorkloadSpec::default()
        }
    }

    /// A Kvm tool instance's image is projected in the run's group and named after its tool, so
    /// the golden paths come from them whether or not the stored golden paths are set
    #[tokio::test]
    async fn kvm_projection_uses_run_group_and_tool_name() {
        // a winvm tool instance in groups vesta and ceres, with and without stored golden paths
        let stored = Kvm {
            xml: "/elsewhere/winvm.xml".to_owned(),
            qcow2: "/elsewhere/winvm.qcow2".to_owned(),
        };
        for kvm in [None, Some(stored)] {
            let id = Uuid::new_v4();
            let mut source = FakeWorkloadSource::default();
            source.serve(id, "winvm", &["vesta", "ceres"], kvm_spec(kvm));
            // a run in ceres projects ceres and winvm
            let image = source
                .workload_image(&worker(id, "ceres"), ImageScaler::Kvm)
                .await
                .unwrap();
            assert_eq!(image.group, "ceres");
            assert_eq!(image.name, "winvm");
            assert_eq!(image.scaler, ImageScaler::Kvm);
            assert_eq!(source.fetched(), vec![id]);
        }
    }

    /// A worker whose tool instance isn't in its group, or doesn't run a workload, is refused
    #[tokio::test]
    async fn workload_image_refuses_mismatches() {
        // a Kvm tool instance only in vesta
        let id = Uuid::new_v4();
        let mut source = FakeWorkloadSource::default();
        source.serve(id, "winvm", &["vesta"], kvm_spec(None));
        // a worker in another group is refused
        assert!(
            source
                .workload_image(&worker(id, "ceres"), ImageScaler::Kvm)
                .await
                .is_err()
        );
        // a bare metal launcher refuses a Kvm workload
        let err = source
            .workload_image(&worker(id, "vesta"), ImageScaler::BareMetal)
            .await
            .unwrap_err();
        assert!(err.msg().contains("amalthea-worker"), "{err}");
        // an unknown tool instance is an error
        assert!(
            source
                .workload_image(&worker(Uuid::new_v4(), "vesta"), ImageScaler::Kvm)
                .await
                .is_err()
        );
    }

    /// Start a local API that answers every request with one status and records each request line
    ///
    /// # Arguments
    ///
    /// * `status` - The status line to answer with, like `404 Not Found`
    async fn answer_with(
        status: &'static str,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // bind to a random local port
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        // keep the request lines for the test to check
        let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = lines.clone();
        // answer each connection once its request headers and body are read
        tokio::spawn(async move {
            loop {
                // wait for the next connection
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut chunk = [0_u8; 4096];
                loop {
                    // read the next part of the request
                    let read = stream.read(&mut chunk).await.unwrap();
                    request.extend_from_slice(&chunk[..read]);
                    let text = String::from_utf8_lossy(&request).to_string();
                    // stop once the headers and the body they announce have arrived
                    if let Some(end) = text.find("\r\n\r\n") {
                        // get the length of the body
                        let length = text
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|len| len.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        // record the request line once the whole request is here
                        if request.len() >= end + 4 + length || read == 0 {
                            recorded
                                .lock()
                                .unwrap()
                                .push(text.lines().next().unwrap_or_default().to_owned());
                            break;
                        }
                    }
                    // stop when the client closed the connection
                    if read == 0 {
                        break;
                    }
                }
                // answer with the status and no body
                let response =
                    format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            }
        });
        (host, lines)
    }

    /// Deleting workers through the API treats a 404 as done and returns any other error
    #[tokio::test(flavor = "multi_thread")]
    async fn api_worker_deletes_ignore_only_not_found() {
        for (status, code) in [
            ("200 OK", None),
            ("404 Not Found", None),
            ("500 Internal Server Error", Some(500)),
            ("401 Unauthorized", Some(401)),
        ] {
            // point a client at an API that answers with this status
            let (host, lines) = answer_with(status).await;
            let thorium = thorium::Thorium::build(&host)
                .token("t")
                .build()
                .await
                .unwrap();
            // delete two workers through the system worker route
            let result = WorkloadSource::delete_workers(
                &thorium,
                ImageScaler::BareMetal,
                vec!["io-0".to_owned(), "io-1".to_owned()],
            )
            .await;
            assert_eq!(
                lines.lock().unwrap().as_slice(),
                ["DELETE /api/system/worker/BareMetal HTTP/1.1"],
                "{status}"
            );
            // only a success or a 404 is Ok, and other errors keep their status
            match (result, code) {
                (Ok(()), None) => {}
                (Err(crate::Error::Thorium(error)), Some(code)) => {
                    assert_eq!(error.status().map(|status| status.as_u16()), Some(code));
                }
                (result, _) => panic!("{status} gave {result:?}"),
            }
        }
    }
}
