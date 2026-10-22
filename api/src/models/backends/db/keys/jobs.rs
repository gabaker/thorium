use uuid::Uuid;

use crate::models::jobs::{JobStatus, RawJob};
use crate::utils::Shared;

/// The keys to store/retrieve job data/queues
#[derive(Debug)]
pub struct JobKeys {
    /// The priority queue the job is in
    pub status: String,
    /// The job data
    pub data: String,
}

impl JobKeys {
    /// Builds the keys to access job queues/data in redis
    ///
    /// # Arguments
    ///
    /// * `job` - Job object to build keys for
    /// * `shared` - Shared Thorium objects
    pub fn new(job: &RawJob, shared: &Shared) -> Self {
        // base key to job queue
        let status = Self::status_queue(job, &job.status, shared);
        // build key to store job data at
        let data = Self::data(&job.id, shared);
        // build key object
        JobKeys { status, data }
    }

    /// Builds the key to the status queue a job is in for a status
    ///
    /// A job of a run is queued on its run's queue for its tool instance, and any other job on
    /// the shared workload queue of its group, tool instance, and creator.
    ///
    /// # Arguments
    ///
    /// * `job` - The job to build the status queue key for
    /// * `status` - The status of the queue
    /// * `shared` - Shared Thorium objects
    pub fn status_queue(job: &RawJob, status: &JobStatus, shared: &Shared) -> String {
        // pick the run queue or the shared workload queue
        match job.run {
            Some(run) => Self::run_queue(&job.group, &run, &job.tool_instance, status, shared),
            None => {
                Self::workload_queue(&job.group, &job.tool_instance, &job.creator, status, shared)
            }
        }
    }

    /// Builds the key to a shared workload queue
    ///
    /// # Arguments
    ///
    /// * `group` - The group the jobs are in
    /// * `tool_instance` - The tool instance the jobs run
    /// * `user` - The user who created the jobs
    /// * `status` - The status of the jobs in the queue
    /// * `shared` - Shared Thorium objects
    pub fn workload_queue(
        group: &str,
        tool_instance: &Uuid,
        user: &str,
        status: &JobStatus,
        shared: &Shared,
    ) -> String {
        // shared queues are per group, tool instance, and user
        format!(
            "{ns}:workload_queue:{group}:{tool_instance}:{user}:{status}",
            ns = shared.config.thorium.namespace,
        )
    }

    /// Builds the key to a run's queue for a tool instance
    ///
    /// # Arguments
    ///
    /// * `group` - The group the jobs are in
    /// * `run` - The run the jobs belong to
    /// * `tool_instance` - The tool instance the jobs run
    /// * `status` - The status of the jobs in the queue
    /// * `shared` - Shared Thorium objects
    pub fn run_queue(
        group: &str,
        run: &Uuid,
        tool_instance: &Uuid,
        status: &JobStatus,
        shared: &Shared,
    ) -> String {
        // run queues are per group, run, and tool instance
        format!(
            "{ns}:run_queue:{group}:{run}:{tool_instance}:{status}",
            ns = shared.config.thorium.namespace,
        )
    }

    /// Builds the key to the status queue of a pipeline stage
    ///
    /// Jobs are queued on workload and run queues, so these queues stay empty; the stage claim
    /// and pipeline status still read them.
    ///
    /// # Arguments
    ///
    /// * `group` - The group the job is in
    /// * `pipeline` - The pipeline the job is for
    /// * `stage` - The stage of the pipeline the job is in
    /// * `user` - The user that is requesting this job
    /// * `status` - The status for this job
    /// * `shared` - Shared Thorium objects
    pub fn stage_queue(
        group: &str,
        pipeline: &str,
        stage: &str,
        user: &str,
        status: &JobStatus,
        shared: &Shared,
    ) -> String {
        // base key to build the queue key off of
        format!(
            "{ns}:job_queue:{group}:{pipeline}:{stage}:{user}:{status}",
            ns = shared.config.thorium.namespace,
        )
    }

    /// Builds the key to the recent runtimes of a tool instance in a group
    ///
    /// # Arguments
    ///
    /// * `group` - The group the tool instance ran in
    /// * `tool_instance` - The tool instance
    /// * `shared` - Shared Thorium objects
    pub fn workload_ttc(group: &str, tool_instance: &Uuid, shared: &Shared) -> String {
        // runtimes are kept per group and tool instance
        format!(
            "{ns}:workload_ttc:{group}:{tool_instance}",
            ns = shared.config.thorium.namespace,
        )
    }

    /// Builds the key to the sorted set of the tool instances with shared jobs in a group
    ///
    /// Each tool instance is scored by the time a job of it was last queued.
    ///
    /// # Arguments
    ///
    /// * `group` - The group to build the key for
    /// * `shared` - Shared Thorium objects
    pub fn workloads(group: &str, shared: &Shared) -> String {
        // one set per group
        format!(
            "{ns}:workloads:{group}",
            ns = shared.config.thorium.namespace,
        )
    }

    /// Builds the key to the hash of each tool instance's average runtime
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    pub fn workload_runtimes(shared: &Shared) -> String {
        // one hash for every tool instance
        format!(
            "{ns}:workload_runtimes",
            ns = shared.config.thorium.namespace,
        )
    }

    /// Builds key to job data
    ///
    /// # Arguments
    ///
    /// * `id` - The uuidv4 of the job
    /// * `shared` - Shared Thorium objects
    pub fn data(id: &Uuid, shared: &Shared) -> String {
        format!(
            "{ns}:job_data:{id}",
            ns = shared.config.thorium.namespace,
            id = id
        )
    }

    /// Builds key to job data
    ///
    /// # Arguments
    ///
    /// * `id` - The uuidv4 of the job as a str
    /// * `shared` - Shared Thorium objects
    pub fn data_str(id: &Uuid, shared: &Shared) -> String {
        format!(
            "{ns}:job_data:{id}",
            ns = shared.config.thorium.namespace,
            id = id
        )
    }
}
