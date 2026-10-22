//! Wrappers for interacting with jobs within Thorium with different backends
//! Currently only Redis is supported

use chrono::prelude::*;
use std::collections::HashMap;
use tracing::{Level, event, instrument};
use uuid::Uuid;

use super::db;
use crate::models::{
    Checkpoint, GenericJob, GenericJobArgs, Group, ImageJobInfo, ImageScaler, JobDetailsList,
    JobHandleStatus, JobList, JobResets, JobStatus, Pipeline, RawJob, Reaction, RunningJob,
    StageLogsAdd, Stream, StreamObj, User, WorkerName, WorkloadRuntimes,
};
use crate::utils::{ApiError, Shared};
use crate::{
    bad, deserialize, deserialize_ext, deserialize_opt, extract, is_admin, not_found, serialize,
    unauthorized,
};

/// The least a reported runtime may run over its tool instance's runtime cap and still be
/// recorded (five minutes)
const MIN_RUNTIME_SLACK: u64 = 5 * 60;

/// Gets how far a reported runtime may run over its tool instance's runtime cap and still be
/// recorded
///
/// Agents measure a runtime when they notice a job finished, which can be a little after the
/// tool instance's timeout, so runtimes within this slack are recorded as the cap instead of
/// refused.
///
/// # Arguments
///
/// * `cap` - The longest a job of the tool instance may run in seconds
fn runtime_slack(cap: u64) -> u64 {
    // allow a tenth of the cap, but never less than the minimum slack
    (cap / 10).max(MIN_RUNTIME_SLACK)
}

impl JobList {
    /// Creates new raw jobs list object
    ///
    /// # Arguments
    ///
    /// * `cursor` - A cursor used to page to the next list of jobs
    /// * `names` - A list of job ids
    pub(super) fn new(cursor: Option<usize>, names: Vec<Uuid>) -> Self {
        JobList { cursor, names }
    }
}

impl JobDetailsList {
    /// Creates a new image details list object
    ///
    /// # Arguments
    ///
    /// * `cursor` - A cursor used to page to the next list of jobs
    /// * `details` - A list of job details
    pub(super) fn new(cursor: Option<usize>, details: Vec<RawJob>) -> Self {
        JobDetailsList { cursor, details }
    }
}

impl TryFrom<&StreamObj> for RunningJob {
    type Error = ApiError;
    /// Cast a `StreamObj` to a `RunningJob`
    ///
    /// # Arguments
    ///
    /// * `obj` - The `StreamObj` to cast
    fn try_from(obj: &StreamObj) -> Result<Self, Self::Error> {
        // ingest data from stream object
        // error can't be deserialized so were ok wrapping this
        Ok(deserialize!(&obj.data))
    }
}

impl RawJob {
    /// Builds a new job object
    ///
    /// # Arguments
    ///
    /// * `reaction` - The Reaction this job is a part of
    /// * `stage` - The name of the stage for this job, whose tool instance comes from the plan
    /// * `deadline` - The deadline this must be executed by
    /// * `info` - The `ImageJobInfo` of each stage's tool instance by stage name
    pub fn build(
        reaction: &Reaction,
        stage: &str,
        deadline: DateTime<Utc>,
        info: &HashMap<&String, ImageJobInfo>,
    ) -> Result<RawJob, ApiError> {
        // get args
        let args = match reaction.args.get(stage) {
            Some(args) => serde_json::to_string(args)?,
            None => "{}".to_owned(),
        };

        // create job object
        let cast = RawJob {
            reaction: reaction.id,
            id: Uuid::new_v4(),
            group: reaction.group.clone(),
            pipeline: reaction.pipeline.clone(),
            stage: stage.to_string(),
            creator: reaction.creator.clone(),
            args,
            status: JobStatus::Created,
            deadline,
            worker: None,
            parent: reaction.parent,
            generator: info[&stage.to_owned()].generator,
            scaler: info[&stage.to_owned()].scaler,
            samples: reaction.samples.clone(),
            ephemeral: reaction.ephemeral.clone(),
            parent_ephemeral: reaction.parent_ephemeral.clone(),
            repos: reaction.repos.clone(),
            trigger_depth: reaction.trigger_depth,
            tool_instance: reaction.plan.tool_instance(stage).unwrap_or_default(),
            run: reaction.run,
        };
        Ok(cast)
    }

    /// Try to cast a `HashMap` of strings into a `RawJob`
    ///
    /// # Arguments
    ///
    /// * `raw` - The `HashMap` to cast into a `RawJob`
    #[instrument(name = "RawJob::from_data", skip_all, err(Debug))]
    pub fn from_data(mut raw: HashMap<String, String>) -> Result<Self, ApiError> {
        // error if hashmap does not contain the required values
        if !raw.contains_key("reaction") || !raw.contains_key("id") {
            // check if this job was malformed or is just missing
            if raw.is_empty() {
                // this job is just empty
                event!(Level::ERROR, empty = true);
            } else {
                // this job contains data but not the requried values
                // log the data that was missing required values
                event!(Level::ERROR, malformed = true, data = format!("{raw:?}"));
            }
            // tell this user this job counldn't be found
            return not_found!("Job not found".to_owned());
        }
        // cast our raw data to a Job
        let job = RawJob {
            reaction: Uuid::parse_str(&raw["reaction"])?,
            id: Uuid::parse_str(&raw["id"])?,
            group: extract!(raw, "group"),
            pipeline: extract!(raw, "pipeline"),
            stage: extract!(raw, "stage"),
            creator: extract!(raw, "creator"),
            args: extract!(raw, "args"),
            status: deserialize_ext!(raw, "status"),
            deadline: deserialize_ext!(raw, "deadline"),
            worker: deserialize_ext!(raw, "worker"),
            parent: deserialize_opt!(raw, "parent", Uuid::parse_str),
            generator: deserialize_ext!(raw, "generator", false),
            scaler: deserialize_ext!(raw, "scaler", ImageScaler::default()),
            samples: deserialize_ext!(raw, "samples", Vec::default()),
            ephemeral: deserialize_ext!(raw, "ephemeral", Vec::default()),
            parent_ephemeral: deserialize_ext!(raw, "parent_ephemeral", HashMap::default()),
            repos: deserialize_ext!(raw, "repos", Vec::default()),
            trigger_depth: deserialize_opt!(raw, "trigger_depth"),
            tool_instance: deserialize_ext!(raw, "tool_instance", Uuid::nil()),
            run: deserialize_opt!(raw, "run"),
        };
        Ok(job)
    }

    /// Gets a job object from the backend
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is getting a job
    /// * `id` - The id of the job to retrieve
    /// * `shared` - Shared objects in Thorium
    /// * `span` - The span to log traces under
    #[instrument(name = "RawJob::get", skip_all, err(Debug))]
    pub async fn get(user: &User, id: &Uuid, shared: &Shared) -> Result<(Group, RawJob), ApiError> {
        // get job from the backend
        let job = db::jobs::get(id, shared).await?;
        // ensure we have access to this jobs group
        let group = Group::authorize(user, &job.group, shared).await?;
        Ok((group, job))
    }

    /// Gets a job object from the backend for an admin
    ///
    /// Only admins can read a job by its id alone; this is meant for tests.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is getting a job
    /// * `id` - The id of the job to retrieve
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "RawJob::admin_get", skip(user, shared), err(Debug))]
    pub async fn admin_get(user: &User, id: &Uuid, shared: &Shared) -> Result<RawJob, ApiError> {
        // only admins can read any job
        is_admin!(user);
        // get the job from the backend
        db::jobs::get(id, shared).await
    }

    /// Check that a non-admin user may handle this job as its creator while it is claimed
    ///
    /// A claim only pops jobs from the claiming user's queues, so the creator of a claimed job is
    /// the user its claiming worker runs as.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is handling this job
    fn authorize_claimed(&self, user: &User) -> Result<(), ApiError> {
        // admins may handle any job
        if user.is_admin() {
            return Ok(());
        }
        // only the creator of a claimed job may handle it
        if self.worker.is_none() || self.creator != user.username {
            return unauthorized!();
        }
        Ok(())
    }

    /// Check that a non-admin user may finish or fail this job as the worker that claimed it
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is finishing or failing this job
    /// * `worker` - The name of the worker the request says claimed this job
    fn authorize_worker(&self, user: &User, worker: Option<&str>) -> Result<(), ApiError> {
        // admins may finish or fail any job without naming its worker
        if user.is_admin() {
            return Ok(());
        }
        // non-admins must name the worker that claimed this job
        let Some(worker) = worker else {
            return bad!("The worker that claimed this job must be set".to_owned());
        };
        // the job must be claimed by us
        self.authorize_claimed(user)?;
        // the named worker must be the one that claimed this job
        if self.worker.as_deref() != Some(worker) {
            return unauthorized!();
        }
        Ok(())
    }

    /// Lists all job details in a list of jobs
    ///
    /// # Arguments
    ///
    /// * `jobs` - The job ids to get the details for
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "RawJob::list_details", skip_all, err(Debug))]
    pub async fn list_details(jobs: JobList, shared: &Shared) -> Result<JobDetailsList, ApiError> {
        // get the details on this jobs in our list
        db::jobs::list_details(jobs, shared).await
    }

    /// Proceeds with a job
    ///
    /// This will set the jobs status to complete and check if all other jobs in that stage have
    /// completed. If they have it will then create the next stage of the reactions jobs. A
    /// runtime far above what this job's image may run for is refused, and one a little above
    /// it is recorded as the most the image may run for.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is proceeding with a job
    /// * `group` - The name of the group this job is tied to
    /// * `runtime` - The amount of time it took to execute this job in seconds
    /// * `logs` - The logs to save for this job
    /// * `worker` - The name of the worker the request says claimed this job
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "RawJob::proceed", skip_all, err(Debug))]
    pub async fn proceed(
        self,
        user: &User,
        group: &Group,
        runtime: u64,
        logs: StageLogsAdd,
        worker: Option<&str>,
        shared: &Shared,
    ) -> Result<JobHandleStatus, ApiError> {
        // make sure this user can proceed with jobs from this group
        group.editable(user)?;
        // make sure this user's worker claimed this job
        self.authorize_worker(user, worker)?;
        // get the longest this job's tool instance may run for
        let groups = [self.group.clone()];
        let cap = db::jobs::runtime_cap(&groups, self.tool_instance, shared).await?;
        // refuse runtimes far above that cap, since they would poison the tool instance's
        // average runtime and every deadline computed from it
        let limit = cap.saturating_add(runtime_slack(cap));
        if runtime > limit {
            return bad!(format!(
                "A runtime of {runtime} seconds is above the {limit} seconds tool instance {} of stage {} may run for",
                self.tool_instance, self.stage
            ));
        }
        // a runtime a little above the cap comes from an agent that noticed the finish late, so
        // record the cap instead
        let runtime = runtime.min(cap);
        // use correct backend to handle starting job
        db::jobs::proceed(self, runtime, logs, shared).await
    }

    /// Errors out a job
    ///
    /// This will set the jobs status to error and fail out the rest of the pipeline.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is erroring out a job
    /// * `group` - The name of the group this job is tied to
    /// * `logs` - The logs to save for this job
    /// * `worker` - The name of the worker the request says claimed this job
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "RawJob::error", skip_all, err(Debug))]
    pub async fn error(
        self,
        user: &User,
        group: &Group,
        logs: StageLogsAdd,
        worker: Option<&str>,
        shared: &Shared,
    ) -> Result<JobHandleStatus, ApiError> {
        // make sure this user can error out jobs from this group
        group.editable(user)?;
        // make sure this user's worker claimed this job
        self.authorize_worker(user, worker)?;
        // use correct backend to handle starting job
        db::jobs::error(self, logs, shared).await
    }

    /// Checkpoints a job
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is erroring out a job
    /// * `group` - The name of the group this job is tied to
    /// * `checkpoint` - The checkpoint to set
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Job::checkpoint", skip(self, user, group, shared), err(Debug))]
    pub async fn checkpoint(
        mut self,
        user: &User,
        group: &Group,
        checkpoint: Checkpoint,
        shared: &Shared,
    ) -> Result<JobHandleStatus, ApiError> {
        // make sure this user can checkpoint jobs from this group
        group.editable(user)?;
        // make sure this is our claimed job
        self.authorize_claimed(user)?;
        // inject in this jobs new checkpoint arg
        let mut args: GenericJobArgs = deserialize!(&self.args);
        args.kwargs
            .insert("--checkpoint".to_owned(), vec![checkpoint.data]);
        self.args = serialize!(&args);
        // update this jobs args in redis
        db::jobs::set_args(&self, shared).await
    }

    /// Sets a job status as sleeping in redis
    ///
    /// This does not complete a job and complete must still be called.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is sleeping this job
    /// * `group` - The name of the group this job is tied to
    /// * `checkpoint` - The checkpoint to set
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Job::sleep", skip(self, user, group, shared), err(Debug))]
    pub async fn sleep(
        self,
        user: &User,
        group: &Group,
        checkpoint: Checkpoint,
        shared: &Shared,
    ) -> Result<JobHandleStatus, ApiError> {
        // make sure this user can sleep generators from this group
        group.editable(user)?;
        // make sure this is our claimed job
        self.authorize_claimed(user)?;
        // use correct backend to handle starting job
        db::jobs::sleep(self, checkpoint, shared).await
    }

    /// Resets jobs in bulk
    ///
    /// This will set all of the jobs statuses back to created except for any
    /// completed or failed jobs.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is erroring out a job
    /// * `resets` - The jobs to reset
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Job::bulk_reset", skip_all, err(Debug))]
    pub async fn bulk_reset(
        user: &User,
        resets: JobResets,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // make sure we are an admin
        is_admin!(user);
        // use correct backend to handle starting job
        db::jobs::bulk_reset(resets, false, shared).await
    }

    /// Lists running jobs between two timestamps
    ///
    /// This reads jobs from the running jobs stream and can only be called by an admin.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is performing this request
    /// * `scaler` - The scaler to get running jbos for
    /// * `start` - The timestamp in seconds (unix epoch) to start reading running jobs from
    /// * `end` - The timestamp in seconds (unix epoch) to stop reading running jobs at
    /// * `skip` - The number of entries in the running stream to skip
    /// * `limit` - The most running jobs to return at once
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Job::runner", skip(user, shared), err(Debug))]
    pub async fn running(
        user: &User,
        scaler: ImageScaler,
        start: i64,
        end: i64,
        skip: u64,
        limit: u64,
        shared: &Shared,
    ) -> Result<Vec<RunningJob>, ApiError> {
        // make sure we are an admin
        is_admin!(user);
        // cast our scaler to a string
        let ns = scaler.as_str();
        // read objects from streams
        let objects: Vec<StreamObj> = Stream::read(
            user, "system", ns, "running", start, end, skip, limit, shared,
        )
        .await?;
        // cast stream objects to running jobs
        let running: Vec<RunningJob> = objects
            .iter()
            .map(RunningJob::try_from)
            .filter_map(Result::ok)
            .collect();
        Ok(running)
    }

    /// Recompute the average runtime of every active tool instance
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is updating runtimes
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "RawJob::update_workload_runtimes", skip_all, err(Debug))]
    pub async fn update_workload_runtimes(user: &User, shared: &Shared) -> Result<(), ApiError> {
        // only admins can update runtimes
        is_admin!(user);
        // collect every group and its members a page at a time
        let mut groups = Vec::new();
        let mut cursor = 0;
        loop {
            let page = Group::list_details(user, cursor, 1000, shared).await?;
            for group in page.details {
                let members = group
                    .members()
                    .into_iter()
                    .cloned()
                    .collect::<Vec<String>>();
                groups.push((group.name, members));
            }
            // stop once every page is read
            match page.cursor {
                Some(next) => cursor = next,
                None => break,
            }
        }
        // average and prune the tool instances of these groups
        db::jobs::update_workload_runtimes(&groups, shared).await
    }

    /// Get the average runtime of every tool instance that has one
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is reading runtimes
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "RawJob::workload_runtimes", skip_all, err(Debug))]
    pub async fn workload_runtimes(
        user: &User,
        shared: &Shared,
    ) -> Result<WorkloadRuntimes, ApiError> {
        // only admins can read runtimes
        is_admin!(user);
        // read the runtimes from the backend
        db::jobs::workload_runtimes(shared).await
    }

    /// Serializes the parts of a job that go into the deadline stream
    #[must_use]
    pub fn stream_data(&self) -> String {
        // the deadline member is built the same way everywhere
        StreamObj::from(self).data
    }
}

impl GenericJob {
    /// Claims a requested number of pending generic jobs
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is claiming a job
    /// * `group` - The name of the group this job is tied to
    /// * `pipeline` - The name of the pipeline is tied to
    /// * `stage` - The name of the stage for this job
    /// * `limit` - The number of jobs to claim at once
    /// * `worker` - The worker that is claiming jobs
    /// * `shared` - Shared objects in Thorium
    /// * `span` - The span to log traces under
    #[allow(clippy::too_many_arguments)]
    #[instrument(
        name = "GenericJob::claim",
        skip(user, group, pipeline, worker, shared),
        err(Debug)
    )]
    pub async fn claim(
        user: &User,
        group: &Group,
        pipeline: &Pipeline,
        stage: &str,
        limit: usize,
        worker: &WorkerName,
        shared: &Shared,
    ) -> Result<Vec<GenericJob>, ApiError> {
        // make sure this user can claim jobs from this group
        group.editable(user)?;
        // claim job from backend if one exists
        let raw_claims = db::jobs::claim(user, pipeline, stage, limit, worker, shared).await?;
        // cast claims to GenericJobs
        let claims = raw_claims
            .into_iter()
            .map(GenericJob::try_from)
            .filter_map(Result::ok)
            .collect();
        Ok(claims)
    }

    /// Claims a requested number of pending jobs of a tool instance for a worker
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is claiming jobs
    /// * `group` - The group the jobs are in
    /// * `tool_instance` - The tool instance whose jobs to claim
    /// * `limit` - The number of jobs to claim at once
    /// * `worker` - The worker that is claiming jobs
    /// * `shared` - Shared objects in Thorium
    #[instrument(
        name = "GenericJob::claim_workload",
        skip(user, group, worker, shared),
        err(Debug)
    )]
    pub async fn claim_workload(
        user: &User,
        group: &Group,
        tool_instance: Uuid,
        limit: usize,
        worker: &WorkerName,
        shared: &Shared,
    ) -> Result<Vec<GenericJob>, ApiError> {
        // make sure this user can claim jobs from this group
        group.editable(user)?;
        // claim jobs from the worker's queue if any exist
        let raw_claims =
            db::jobs::claim_workload(user, &group.name, tool_instance, limit, worker, shared)
                .await?;
        // cast claims to GenericJobs
        let claims = raw_claims
            .into_iter()
            .map(GenericJob::try_from)
            .filter_map(Result::ok)
            .collect();
        Ok(claims)
    }

    /// Gets a job object from the backend
    ///
    /// # Arguments
    ///
    /// * `_group` - The Group the job is in (this is passed in for auth purposes)
    /// * `id` - The id of the job to retrieve
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "GenericJob::claim", skip_all, err(Debug))]
    pub async fn get(_group: &Group, id: &Uuid, shared: &Shared) -> Result<Self, ApiError> {
        let raw = db::jobs::get(id, shared).await;
        GenericJob::try_from(raw?)
    }
}

impl TryFrom<RawJob> for GenericJob {
    type Error = ApiError;

    /// Try to cast a `RawJob` into a `GenericJob`
    ///
    /// # Arguments
    ///
    /// * `raw` - The `RawJob` to cast to a `GenericJob`
    fn try_from(raw: RawJob) -> Result<Self, Self::Error> {
        // cast to GenericJob
        let cast = GenericJob {
            reaction: raw.reaction,
            id: raw.id,
            group: raw.group,
            pipeline: raw.pipeline,
            stage: raw.stage,
            creator: raw.creator,
            args: deserialize!(&raw.args),
            status: raw.status,
            deadline: raw.deadline,
            parent: raw.parent,
            generator: raw.generator,
            samples: raw.samples,
            ephemeral: raw.ephemeral,
            parent_ephemeral: raw.parent_ephemeral,
            repos: raw.repos,
            trigger_depth: raw.trigger_depth,
            tool_instance: raw.tool_instance,
            run: raw.run,
        };
        Ok(cast)
    }
}

//impl TryFrom<HashMap<String, String>> for RawJob {
//    type Error = ApiError;
//
//}

#[cfg(test)]
mod tests {
    use super::runtime_slack;
    use crate::models::RawJob;
    use std::collections::HashMap;
    use uuid::Uuid;

    /// Short caps get the minimum slack and long caps get a tenth of the cap
    #[test]
    fn runtime_slack_bounds() {
        assert_eq!(runtime_slack(0), 300);
        assert_eq!(runtime_slack(300), 300);
        assert_eq!(runtime_slack(3000), 300);
        assert_eq!(runtime_slack(36_000), 3600);
        assert_eq!(runtime_slack(u64::MAX), u64::MAX / 10);
    }

    /// A finished job hash written before tool instances existed reads with the nil tool instance
    #[test]
    fn job_hash_without_tool_instance() {
        // a finished job hash with only the fields earlier versions wrote
        let raw = [
            ("reaction", Uuid::new_v4().to_string()),
            ("id", Uuid::new_v4().to_string()),
            ("group", "jupiter".to_owned()),
            ("pipeline", "orbit".to_owned()),
            ("stage", "plant".to_owned()),
            ("creator", "europa".to_owned()),
            ("args", "{}".to_owned()),
            ("status", "\"Completed\"".to_owned()),
            ("deadline", "\"2026-10-19T00:00:00Z\"".to_owned()),
            ("worker", "null".to_owned()),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect::<HashMap<String, String>>();
        // it reads with the nil tool instance and no run
        let job = RawJob::from_data(raw).expect("a job");
        assert_eq!(job.tool_instance, Uuid::nil());
        assert_eq!(job.run, None);
    }
}
