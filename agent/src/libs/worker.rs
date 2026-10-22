use std::time::Duration;
use thorium::Error;
use thorium::Thorium;
use thorium::models::{StageLogsAdd, WorkerStatus};
use tokio::task::JoinHandle;
use tracing::{Level, event, instrument, span};
use uuid::Uuid;

use super::agents::{self, Agent};
use super::harness_files::RunEnv;
use super::{Lifetime, Target};
use crate::args::Args;

/// Build the stage logs sent to Thorium when erroring a job whose agent failed to spawn
///
/// The returned logs contain the spawn error so the user sees why the job failed instead of a
/// failed job with empty logs.
///
/// # Arguments
///
/// * `error` - The error that prevented the agent from spawning
fn spawn_error_logs(error: &Error) -> StageLogsAdd {
    // build the error log to send to Thorium
    let mut logs = StageLogsAdd::default();
    logs.add(format!("Spawn Error: {error:#?}"));
    logs
}

/// Whether this worker should exit or look for more jobs
pub enum ClaimJobStatus {
    /// The active job this worker claimed
    ActiveJob {
        job_id: Uuid,
        handle: JoinHandle<()>,
    },
    /// This worker did not claim a job but does not have to immeidately exit
    DidNotClaim,
    /// This worker needs to exit as it needs to update or has exceeded its lifetime
    ExitWhenPossible,
    /// This per-run worker's run ended, so the API no longer accepts its token
    RunEnded,
}

/// Whether an error means a per-run worker's run ended
///
/// The API deletes a run's token when the run ends, so the run's workers get `401` from then on.
///
/// # Arguments
///
/// * `run` - The run the worker serves, if it's a per-run worker
/// * `error` - The error from a call to the API
fn run_ended(run: Option<Uuid>, error: &Error) -> bool {
    run.is_some() && error.status() == Some(reqwest::StatusCode::UNAUTHORIZED)
}

/// A worker used to execute jobs in Thorium
pub struct Worker {
    /// A client for Thorium
    pub thorium: Thorium,
    /// The target job types to claim
    pub target: Target,
    /// The command line args passed to the agent
    pub args: Args,
    /// The node this worker is on
    pub node: String,
    /// This workers lifetime
    lifetime: Lifetime,
    /// Stop claiming new jobs as an update is needed
    halt_claiming: bool,
    /// Whether this agent has already been initialized
    agent_intialized: bool,
    /// The run's url, token, and models, when this per-run worker was built from its pod env
    pub run_env: Option<RunEnv>,
}

impl Worker {
    /// Build a new worker
    ///
    /// # Arguments
    ///
    /// * `args` - Arguments passed to the agent
    #[instrument(name = "Worker::new", skip_all, err(Debug))]
    pub async fn new(args: Args) -> Result<Self, Error> {
        // a per-run pod's env holds its run's url and token
        Self::build(args, RunEnv::from_env()).await
    }

    /// Build a new worker, with its client from its keys file when it was launched with one and
    /// from its run's env otherwise
    ///
    /// Per-run pods are launched without keys, while a shared worker's env can hold both run env
    /// names from its tool's own env, so a worker launched with keys never uses the env.
    ///
    /// # Arguments
    ///
    /// * `args` - Arguments passed to the agent
    /// * `run_env` - The run's url, token, and models from the pod env, if both are set
    #[instrument(name = "Worker::build", skip_all, err(Debug))]
    pub async fn build(args: Args, run_env: Option<RunEnv>) -> Result<Self, Error> {
        // a worker launched with keys ignores any run env
        let run_env = if args.keys.is_some() { None } else { run_env };
        // a per-run pod's client uses its run's token, and every other worker its keys file
        let thorium = match &run_env {
            Some(env) => env.client().await?,
            None => Thorium::from_key_file(args.keys_path()).await?,
        };
        // get the targets for this image
        let target = args.target(&thorium).await?;
        // set up lifetime
        let lifetime = Lifetime::new(&target);
        // get the node we are running on
        let node = args.node()?;
        // build our worker
        let worker = Worker {
            thorium,
            target,
            args,
            node,
            lifetime,
            halt_claiming: false,
            agent_intialized: false,
            run_env,
        };
        Ok(worker)
    }

    /// Check if we need an update or not and apply it if possible
    #[instrument(name = "Worker::needs_update", skip_all, err(Debug))]
    async fn needs_update(&mut self) -> Result<(), Error> {
        // Get the current Thorium version
        let version = self.thorium.updates.get_version().await?;
        // get the current version
        let current = env!("CARGO_PKG_VERSION");
        // compare to our version and see if its different
        if version.thorium != semver::Version::parse(current)? {
            // start our update needed span
            event!(
                Level::WARN,
                update_needed = true,
                current = current,
                api = version.thorium.to_string()
            );
            // set the halt spawning flag so we stop spawning new agents
            self.halt_claiming = true;
        }
        Ok(())
    }

    /// Claims and executes jobs on a worker
    async fn claim_jobs(&mut self) -> ClaimJobStatus {
        // if we have exceeded our lifetime or need to halt claiming then exit when possible
        if self.lifetime.exceeded() || self.halt_claiming {
            return ClaimJobStatus::ExitWhenPossible;
        }
        // get any jobs if they exist
        let mut jobs = match self
            .target
            .thorium
            .jobs
            .claim_workload(
                &self.target.group,
                self.target.tool_instance,
                &self.args.cluster,
                &self.node,
                &self.target.name,
                1,
            )
            .await
        {
            Ok(jobs) => jobs,
            // a per-run worker whose run ended has nothing left to claim
            Err(error) if run_ended(self.target.run, &error) => {
                event!(
                    Level::INFO,
                    msg = "Run ended",
                    run = format!("{:?}", self.target.run)
                );
                return ClaimJobStatus::RunEnded;
            }
            Err(error) => {
                // start our jobs claim error span
                span!(
                    Level::ERROR,
                    "Failed To Claim Jobs",
                    user = self.target.user.username,
                    group = self.target.group,
                    tool_instance = self.target.tool_instance.to_string(),
                    name = self.target.name,
                    error = error.msg()
                );
                // return false since we didn't claim any jobs
                return ClaimJobStatus::DidNotClaim;
            }
        };
        // agents will only ever execute a single job or less
        debug_assert!(jobs.len() <= 1);
        // either execute any claimed jobs or immediately return false if we claimed no jobs
        match jobs.pop() {
            Some(job) => {
                // start our spawn jobs span
                let span = span!(Level::INFO, "Spawning Job");
                // log this job is going to be spawned
                event!(
                    parent: &span,
                    Level::INFO,
                    reaction = job.reaction.to_string(),
                    job = job.id.to_string(),
                    user = self.target.user.username,
                    group = self.target.group,
                    tool_instance = self.target.tool_instance.to_string(),
                    name = self.target.name,
                );
                // increment our job counter
                self.lifetime.claimed_job();
                // get this jobs reaction and job id
                let job_id = job.id;
                // build the path to write this jobs logs to
                let log_path = format!("/tmp/{}-thorium.log", job.id);
                // build an agent for this job
                match Agent::new(self, &self.target, job) {
                    // agent successfully built so start executing it
                    Ok(agent) => {
                        // if we haven't run the agent initialization step then do that
                        if !self.agent_intialized {
                            // initialize this worker for our agents
                            if let Err(error) = agent.executor.init().await {
                                // log that we failed to init our worker and should exit
                                event!(
                                    Level::ERROR,
                                    msg = "Failed to initialize worker",
                                    error = error.to_string(),
                                );
                                // make sure our worker doesn't claim any new jobs
                                self.halt_claiming = true;
                                // exit this worker when possible
                                return ClaimJobStatus::ExitWhenPossible;
                            }
                            // mark that this worker has been initialized for agents
                            self.agent_intialized = true;
                        }
                        // try to spawn this worker
                        let handle =
                            tokio::spawn(async move { agents::execute(agent, log_path).await });
                        ClaimJobStatus::ActiveJob { job_id, handle }
                    }
                    // we ran into a problem building our agent
                    Err(error) => {
                        // log this error to our tracer
                        event!(parent: &span, Level::ERROR, error = error.msg());
                        // build the spawn error logs to send to Thorium
                        let logs = spawn_error_logs(&error);
                        // get the jobs client to send our error logs with
                        let client = &self.target.thorium.jobs;
                        // send our error logs to Thorium
                        if let Err(error) = client.error(&job_id, &logs, &self.target.name).await {
                            // log that we failed to update our stage logs in thorium
                            event!(
                                parent: &span,
                                Level::ERROR,
                                msg = "Failed to send stage logs",
                                error = error.msg()
                            );
                        }
                        // delete this log file
                        if let Err(error) = tokio::fs::remove_file(log_path).await {
                            // log this error to our tracer
                            event!(
                                parent: &span,
                                Level::ERROR,
                                msg = "Failed to remove log file",
                                error = error.to_string()
                            );
                        }
                        // We ran into a problem and didn't claim a job
                        ClaimJobStatus::DidNotClaim
                    }
                }
            }
            // no job was claimed
            None => ClaimJobStatus::DidNotClaim,
        }
    }

    /// check the process of any active jobs and if necessary continue executing them
    ///
    /// # Arguments
    ///
    /// * `job_id` - The id for the job to wait for
    /// * `handle` - The handle to the task that is executing this job
    ///
    /// # Returns
    ///
    /// Returns true if this job completes successfully and false if it failed.
    #[instrument(name = "Worker::wait_for_job", skip_all, fields(job = job_id.to_string()))]
    async fn await_job_completion(&mut self, job_id: Uuid, handle: JoinHandle<()>) -> bool {
        // wait for our job to complete
        match handle.await {
            Ok(()) => {
                // log that our job completed
                event!(Level::INFO, status = "Completed");
                // return true that our job didn't fail
                true
            }
            Err(error) => {
                // log that we failed this job
                event!(
                    Level::ERROR,
                    user = &self.target.user.username,
                    group = &self.target.group,
                    tool_instance = self.target.tool_instance.to_string(),
                    error = error.to_string()
                );
                // return false since our job ran into an external error and we should exit
                false
            }
        }
    }

    /// Starts the worker loop
    #[instrument(name = "Worker::start", skip_all, err(Debug))]
    pub async fn start(&mut self) -> Result<(), Error> {
        // apply any needed updates
        self.needs_update().await?;
        // tell Thorium we are running, unless our run already ended
        match self.target.update_worker(WorkerStatus::Running).await {
            Ok(()) => (),
            // the worker's record is cleared for it, since its token can't delete it
            Err(error) if run_ended(self.target.run, &error) => return Ok(()),
            Err(error) => return Err(error),
        }
        // track how long this work should sit in limbo before exiting without a job to claim
        let mut limbo = self.args.limbo;
        loop {
            // apply any needed updates
            self.needs_update().await?;
            // try and claim enough jobs to fill any open job slots
            match self.claim_jobs().await {
                // we have an active job so wait 25ms before checking if this job finished yet
                ClaimJobStatus::ActiveJob { job_id, handle } => {
                    // reset our limbo
                    limbo = self.args.limbo;
                    // block until our active job completes
                    if !self.await_job_completion(job_id, handle).await {
                        break;
                    }
                }
                // we did not claim a job so check if we have exceeded our limbo
                ClaimJobStatus::DidNotClaim => {
                    // log how much limbo we have left
                    event!(Level::INFO, limbo_left = limbo);
                    // if we have no more limbo left then exit
                    if limbo == 0 {
                        break;
                    }
                    // otherwise decrement our limbo
                    limbo -= 1;
                    // sleep for 1 second before looking for another job
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                ClaimJobStatus::ExitWhenPossible => break,
                // the worker's record is cleared for it, since its token can't delete it
                ClaimJobStatus::RunEnded => return Ok(()),
            }
        }
        // tell Thorium this worker is exiting
        self.target.remove_worker(&self.args).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use thorium::models::{
        BareMetalEnvironment, GenericJob, GenericJobArgs, JobStatus, Pools, WorkloadEnvironment,
        WorkloadSpec,
    };

    use super::*;
    use crate::libs::test_server::{self, MockThorium};

    /// Build a job of a tool instance with no inputs
    ///
    /// # Arguments
    ///
    /// * `tool_instance` - The tool instance the job runs
    fn job(tool_instance: Uuid) -> GenericJob {
        // a job with no samples, repos, or ephemeral files
        GenericJob {
            reaction: Uuid::new_v4(),
            id: Uuid::new_v4(),
            group: "corn".into(),
            pipeline: "harvest".into(),
            stage: "harvester".into(),
            creator: "mcarson".into(),
            args: GenericJobArgs::default(),
            status: JobStatus::Running,
            deadline: chrono::Utc::now(),
            parent: None,
            generator: false,
            samples: Vec::new(),
            ephemeral: Vec::new(),
            parent_ephemeral: std::collections::HashMap::default(),
            repos: Vec::new(),
            trigger_depth: None,
            tool_instance,
            run: None,
        }
    }

    /// Get the base64 authorization header a client sends for a token
    ///
    /// # Arguments
    ///
    /// * `token` - The token
    fn auth_for(token: &str) -> String {
        // the client sends the token base64 encoded with padding
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut encoded = String::new();
        for chunk in token.as_bytes().chunks(3) {
            let bytes = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let packed =
                (u32::from(bytes[0]) << 16) | (u32::from(bytes[1]) << 8) | u32::from(bytes[2]);
            for index in 0..4 {
                if index <= chunk.len() {
                    encoded.push(ALPHABET[((packed >> (18 - 6 * index)) & 0x3f) as usize] as char);
                } else {
                    encoded.push('=');
                }
            }
        }
        format!("authorization: token {encoded}").to_lowercase()
    }

    /// A worker built with both run env values uses the run-scoped token for every call and makes
    /// no token request, and exits on `401` without deleting its record; one built without them
    /// uses its keys file
    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn per_run_worker_uses_env_token() {
        // a mock API serving a K8s tool instance, the user, and a per-run worker whose claims
        // return 401
        let id = Uuid::new_v4();
        let run = Uuid::new_v4();
        let entity = test_server::tool_instance_entity(
            id,
            "harvester",
            "1.2.0",
            thorium::models::ToolSpec::Image(test_server::k8s_spec()),
        );
        let entity_json = serde_json::to_string(&entity).unwrap();
        let mut worker_value: serde_json::Value =
            serde_json::from_str(&test_server::worker_json("worker0", id)).unwrap();
        worker_value["run"] = serde_json::json!(run);
        let worker_json = worker_value.to_string();
        let version = format!(r#"{{"thorium":"{}"}}"#, env!("CARGO_PKG_VERSION"));
        let mock = MockThorium::start(move |line| {
            if line.starts_with("GET /api/entities/") {
                (200, entity_json.clone())
            } else if line.starts_with("GET /api/users/whoami ") {
                (200, test_server::user_json())
            } else if line.starts_with("GET /api/system/worker/") {
                (200, worker_json.clone())
            } else if line.starts_with("GET /api/version ") {
                (200, version.clone())
            } else if line.starts_with("PATCH /api/jobs/workloads/claim/") {
                (401, r#"{"error":"unauthorized"}"#.to_owned())
            } else {
                (204, String::new())
            }
        })
        .await;
        // a worker with run env values launched without keys, as per-run pods are
        let line = [
            "thorium-agent",
            "--cluster",
            "cluster0",
            "--node",
            "node0",
            "--group",
            "corn",
            "--tool-instance",
            &id.to_string(),
            "--name",
            "worker0",
            "k8s",
            "--entrypoint",
            "[]",
            "--cmd",
            "[]",
        ];
        let args = Args::try_parse_from(line).unwrap();
        let env = RunEnv::from_values(
            Some(mock.host.clone()),
            Some("run-scoped-token".to_owned()),
            None,
        );
        let mut worker = Worker::build(args.clone(), env).await.unwrap();
        assert!(worker.run_env.is_some());
        assert_eq!(worker.target.run, Some(run));
        // its run ends on the first claim and it exits cleanly without deleting its record
        worker.start().await.unwrap();
        let requests = mock.requests.lock().unwrap().clone();
        assert!(requests.len() >= 4, "{requests:?}");
        let expected = auth_for("run-scoped-token");
        for request in &requests {
            assert!(request.to_lowercase().contains(&expected), "{request}");
        }
        let lines = mock.lines();
        assert!(
            lines
                .iter()
                .any(|line| line.contains("/api/jobs/workloads/claim/"))
        );
        assert!(
            !lines.iter().any(|line| line.starts_with("DELETE ")),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("/api/users/auth")),
            "{lines:?}"
        );
        // a worker with only one env value uses its keys file instead
        let dir = test_server::scratch();
        let keys = dir.join("keys.yml");
        tokio::fs::write(&keys, format!("api: {}\ntoken: full-token\n", mock.host))
            .await
            .unwrap();
        let mut keyed = args;
        keyed.keys = Some(keys.to_string_lossy().into_owned());
        let env = RunEnv::from_values(Some(mock.host.clone()), None, None);
        mock.requests.lock().unwrap().clear();
        let worker = Worker::build(keyed, env).await.unwrap();
        assert!(worker.run_env.is_none());
        let requests = mock.requests.lock().unwrap().clone();
        assert_ne!(requests.len(), 0);
        let full = auth_for("full-token");
        for request in &requests {
            assert!(request.to_lowercase().contains(&full), "{request}");
        }
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    /// A shared worker launched with a keys file uses it even when both run env values are set,
    /// as they are when a tool's own env names them
    #[tokio::test]
    async fn shared_worker_with_keys_ignores_env_token() {
        // a mock API serving a K8s tool instance, the user, and a shared worker
        let id = Uuid::new_v4();
        let entity = test_server::tool_instance_entity(
            id,
            "harvester",
            "1.2.0",
            thorium::models::ToolSpec::Image(test_server::k8s_spec()),
        );
        let entity_json = serde_json::to_string(&entity).unwrap();
        let worker_json = test_server::worker_json("worker0", id);
        let mock = MockThorium::start(move |line| {
            if line.starts_with("GET /api/entities/") {
                (200, entity_json.clone())
            } else if line.starts_with("GET /api/users/whoami ") {
                (200, test_server::user_json())
            } else if line.starts_with("GET /api/system/worker/") {
                (200, worker_json.clone())
            } else {
                (204, String::new())
            }
        })
        .await;
        // a keys file holding the user's token
        let dir = test_server::scratch();
        let keys = dir.join("keys.yml");
        tokio::fs::write(
            &keys,
            format!("api: {}\ntoken: users-full-token\n", mock.host),
        )
        .await
        .unwrap();
        let keys = keys.to_string_lossy().into_owned();
        let line = [
            "thorium-agent",
            "--cluster",
            "cluster0",
            "--node",
            "node0",
            "--group",
            "corn",
            "--tool-instance",
            &id.to_string(),
            "--name",
            "worker0",
            "--keys",
            &keys,
            "k8s",
            "--entrypoint",
            "[]",
            "--cmd",
            "[]",
        ];
        let args = Args::try_parse_from(line).unwrap();
        // the pod env names both values with the tool's own url and token
        let env = RunEnv::from_values(
            Some(mock.host.clone()),
            Some("tool-author-token".to_owned()),
            None,
        );
        let worker = Worker::build(args, env).await.unwrap();
        // every request used the keys file's token and no run env is kept
        assert!(worker.run_env.is_none());
        assert_eq!(worker.target.run, None);
        let requests = mock.requests.lock().unwrap().clone();
        assert_ne!(requests.len(), 0);
        let full = auth_for("users-full-token");
        for request in &requests {
            assert!(request.to_lowercase().contains(&full), "{request}");
        }
        tokio::fs::remove_dir_all(&dir).await.unwrap();
    }

    /// The claim loop claims through the workload route, a claimed job resets the idle count,
    /// and a shared worker deletes its record after `limbo` empty claims in a row
    #[tokio::test]
    async fn claim_loop_claims_workloads_until_limbo() {
        // a mock API whose third claim returns a job and every other claim returns none
        let id = Uuid::new_v4();
        let claimed = job(id);
        let claim_json = serde_json::to_string(&vec![claimed.clone()]).unwrap();
        let claims = Arc::new(AtomicUsize::new(0));
        let counter = claims.clone();
        let version = format!(r#"{{"thorium":"{}"}}"#, env!("CARGO_PKG_VERSION"));
        let mock = MockThorium::start(move |line| {
            if line.starts_with("GET /api/version ") {
                (200, version.clone())
            } else if line.starts_with("PATCH /api/jobs/workloads/claim/") {
                // only the third claim returns a job
                if counter.fetch_add(1, Ordering::SeqCst) == 2 {
                    (200, claim_json.clone())
                } else {
                    (200, "[]".to_owned())
                }
            } else if line.starts_with("POST /api/jobs/handle/") {
                (200, r#"{"status":"Errored"}"#.to_owned())
            } else {
                (204, String::new())
            }
        })
        .await;
        let thorium = Thorium::build(&mock.host)
            .token("token")
            .build()
            .await
            .unwrap();
        // a bare metal tool with no command, so its job errors as soon as it runs
        let mut workload = test_server::workload(None);
        workload.spec = WorkloadSpec {
            environment: WorkloadEnvironment::BareMetal(BareMetalEnvironment::default()),
            ..WorkloadSpec::default()
        };
        let image = workload.project("corn");
        // a shared worker with a limbo of 2
        let line = [
            "thorium-agent",
            "--cluster",
            "cluster0",
            "--node",
            "node0",
            "--group",
            "corn",
            "--tool-instance",
            &id.to_string(),
            "--name",
            "worker0",
            "--limbo",
            "2",
            "bare-metal",
        ];
        let args = Args::try_parse_from(line).unwrap();
        let target = Target {
            name: "worker0".to_owned(),
            group: "corn".to_owned(),
            tool_instance: id,
            workload,
            image,
            user: serde_json::from_str(&test_server::user_json()).unwrap(),
            thorium: thorium.clone(),
            pool: Pools::Deadline,
            run: None,
        };
        let lifetime = Lifetime::new(&target);
        let mut worker = Worker {
            thorium,
            target,
            args,
            node: "node0".to_owned(),
            lifetime,
            halt_claiming: false,
            agent_intialized: false,
            run_env: None,
        };
        // run the worker until it exits
        worker.start().await.unwrap();
        // every claim went to the workload route with the worker's settings
        let lines = mock.lines();
        let expected =
            format!("PATCH /api/jobs/workloads/claim/corn/{id}/cluster0/node0/worker0/1 HTTP/1.1");
        let claim_lines: Vec<&String> =
            lines.iter().filter(|line| line.contains("claim")).collect();
        assert!(
            claim_lines.iter().all(|line| **line == expected),
            "{claim_lines:?}"
        );
        // two empty claims, the job, then a fresh limbo of two empty claims and the last check
        assert_eq!(claim_lines.len(), 6, "{lines:?}");
        // the claimed job ran and errored, and the worker deleted its record last
        assert!(
            lines
                .iter()
                .any(|line| line.contains(&format!("/api/jobs/handle/{}/error", claimed.id))),
            "{lines:?}"
        );
        assert_eq!(
            lines.last().unwrap(),
            "DELETE /api/system/worker/BareMetal HTTP/1.1"
        );
        let delete = mock.matching("DELETE /api/system/worker/BareMetal");
        assert!(delete[0].contains("worker0"), "{delete:?}");
    }

    /// Build a bare metal worker for a mock API
    ///
    /// # Arguments
    ///
    /// * `host` - The mock API's url
    /// * `run` - The run the worker serves, if it's a per-run worker
    async fn worker_for(host: &str, run: Option<Uuid>) -> Worker {
        // a worker with no limbo for a bare metal tool
        let id = Uuid::new_v4();
        let thorium = Thorium::build(host).token("token").build().await.unwrap();
        let workload = test_server::workload(None);
        let image = workload.project("corn");
        let line = [
            "thorium-agent",
            "--cluster",
            "cluster0",
            "--node",
            "node0",
            "--group",
            "corn",
            "--tool-instance",
            &id.to_string(),
            "--name",
            "worker0",
            "--limbo",
            "0",
            "bare-metal",
        ];
        let args = Args::try_parse_from(line).unwrap();
        let target = Target {
            name: "worker0".to_owned(),
            group: "corn".to_owned(),
            tool_instance: id,
            workload,
            image,
            user: serde_json::from_str(&test_server::user_json()).unwrap(),
            thorium: thorium.clone(),
            pool: Pools::Deadline,
            run,
        };
        let lifetime = Lifetime::new(&target);
        Worker {
            thorium,
            target,
            args,
            node: "node0".to_owned(),
            lifetime,
            halt_claiming: false,
            agent_intialized: false,
            run_env: None,
        }
    }

    /// A per-run worker whose run ended exits cleanly once its claim or its worker update returns
    /// `401`, without deleting its worker record, while a shared worker still deletes its record
    #[tokio::test]
    async fn per_run_worker_exits_once_its_run_ends() {
        // an API that refuses every claim with 401
        let version = format!(r#"{{"thorium":"{}"}}"#, env!("CARGO_PKG_VERSION"));
        let claims_refused = version.clone();
        let mock = MockThorium::start(move |line| {
            if line.starts_with("GET /api/version ") {
                (200, claims_refused.clone())
            } else if line.starts_with("PATCH /api/jobs/workloads/claim/") {
                (401, r#"{"error":"token not found"}"#.to_owned())
            } else {
                (204, String::new())
            }
        })
        .await;
        // a per-run worker exits after one refused claim and keeps its record
        let mut worker = worker_for(&mock.host, Some(Uuid::new_v4())).await;
        assert!(worker.start().await.is_ok());
        let lines = mock.lines();
        assert_eq!(
            lines.iter().filter(|line| line.contains("/claim/")).count(),
            1,
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.starts_with("DELETE")),
            "{lines:?}"
        );
        // a shared worker treats the refusal as an empty claim and deletes its record
        let mut shared = worker_for(&mock.host, None).await;
        shared.start().await.unwrap();
        assert!(
            mock.lines()
                .iter()
                .any(|line| line.starts_with("DELETE /api/system/worker/"))
        );
        // an API that refuses the worker update with 401
        let mock = MockThorium::start(move |line| {
            if line.starts_with("GET /api/version ") {
                (200, version.clone())
            } else if line.starts_with("PATCH /api/system/worker/") {
                (401, r#"{"error":"token not found"}"#.to_owned())
            } else {
                (200, "[]".to_owned())
            }
        })
        .await;
        // a per-run worker exits at once, claiming nothing and keeping its record
        let mut worker = worker_for(&mock.host, Some(Uuid::new_v4())).await;
        assert!(worker.start().await.is_ok());
        let lines = mock.lines();
        assert!(
            !lines.iter().any(|line| line.contains("/claim/")),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.starts_with("DELETE")),
            "{lines:?}"
        );
    }

    /// When the agent for a claimed job fails to spawn, the worker errors the job in Thorium
    /// with stage logs that carry the spawn error, so the user sees why the job failed rather
    /// than a failed job with no explanation.
    #[test]
    fn spawn_error_logs_carry_the_error() {
        // the error that kept the agent from spawning
        let error = Error::new("bad entrypoint");
        // build the logs sent to Thorium when erroring this job
        let logs = spawn_error_logs(&error);
        // the logs must explain why the job failed
        assert!(
            logs.logs
                .iter()
                .any(|log| log.line.contains("Spawn Error") && log.line.contains("bad entrypoint")),
            "the spawn error must be included in the job's stage logs, got {:?}",
            logs.logs
        );
    }
}
