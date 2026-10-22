use chrono::prelude::*;
use uuid::Uuid;

#[cfg(feature = "trace")]
use tracing::instrument;

use super::Error;
use crate::models::{
    Checkpoint, Deadline, GenericJob, HandleJobResponse, ImageScaler, JobResets, RawJob,
    RunningJob, StageLogsAdd, WorkloadRuntimes,
};
use crate::{send, send_build};

// import our static runtime if we need a blocking client
#[cfg(feature = "sync")]
use super::RUNTIME;

// import python bindings
#[cfg(feature = "python")]
use pyo3::{pyclass, pymethods};

#[cfg_attr(feature = "sync", thorium_derive::blocking_struct(python))]
#[derive(Clone)]
pub struct Jobs {
    host: String,
    /// token to use for auth
    token: String,
    client: reqwest::Client,
}

#[cfg(test)]
impl Jobs {
    /// The auth string this handler sends with its requests
    pub(crate) fn auth_token(&self) -> &str {
        &self.token
    }
}

#[cfg_attr(feature = "sync", thorium_derive::blocking_struct)]
impl Jobs {
    /// Creates a new jobs handler
    ///
    /// Instead of directly creating this handler you likely want to simply create a
    /// `thorium::Thorium` and use the handler within that instead.
    ///
    /// # Arguments
    ///
    /// * `host` - url/ip of the Thorium api
    /// * `token` - The token used for authentication
    /// * `client` - The reqwest client to use
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::client::Jobs;
    ///
    /// let client = reqwest::Client::new();
    /// let jobs = Jobs::new("http://127.0.0.1", "token", &client);
    /// ```
    #[must_use]
    pub fn new(host: &str, token: &str, client: &reqwest::Client) -> Self {
        // build basic route handler
        Jobs {
            host: host.to_owned(),
            token: token.to_owned(),
            client: client.clone(),
        }
    }

    /// Gets one [`RawJob`] by its id
    ///
    /// Only admins can read a job by its id alone; this is meant for tests.
    ///
    /// # Arguments
    ///
    /// * `id` - The id of the job to get
    #[cfg_attr(
        feature = "trace",
        instrument(name = "Thorium::Jobs::get", skip(self), err(Debug))
    )]
    pub async fn get(&self, id: Uuid) -> Result<RawJob, Error> {
        // build the url for reading this job
        let url = format!("{}/api/jobs/{id}", self.host);
        // build our request
        let req = self.client.get(&url).header("authorization", &self.token);
        // send this request and build the job from the response
        send_build!(self.client, req, RawJob)
    }

    /// Claims [`GenericJob`]s from Thorium for a specific stage in a pipeline if any exist
    ///
    /// # Arguments
    ///
    /// * `group` - The group this pipeline is from
    /// * `pipeline` - The pipeline to claim jobs for
    /// * `stage` - The stage in the the pipeline to claim a job for
    /// * `worker` - The name of the worker that is claiming jobs
    /// * `count` - The number of jobs to claim
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    /// # use thorium::Error;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// // create Thorium client
    /// let thorium = Thorium::build("http://127.0.0.1").token("<token>").build().await?;
    /// // claim a job from Thorium
    /// let jobs = thorium.jobs.claim("Corn", "Harvest", "CornHarvester", "prod", "node0", "esoteria", 1).await?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    #[cfg_attr(
        feature = "trace",
        instrument(name = "Thorium::Jobs::claim", skip(self), err(Debug))
    )]
    pub async fn claim(
        &self,
        group: &str,
        pipeline: &str,
        stage: &str,
        cluster: &str,
        node: &str,
        worker: &str,
        count: u64,
    ) -> Result<Vec<GenericJob>, Error> {
        // build url for claiming a job
        let url = format!(
            "{base}/api/jobs/claim/{group}/{pipeline}/{stage}/{cluster}/{node}/{worker}/{count}",
            base = self.host,
            group = group,
            pipeline = pipeline,
            stage = stage,
            worker = worker,
            count = count
        );
        // build request
        let req = self.client.patch(&url).header("authorization", &self.token);
        // send this request and build a generic job from the response
        send_build!(self.client, req, Vec<GenericJob>)
    }

    /// Claims [`GenericJob`]s of a tool instance for a worker if any exist
    ///
    /// A per-run worker claims only its run's jobs; a shared worker claims the jobs its user
    /// queued for the tool instance in the group.
    ///
    /// # Arguments
    ///
    /// * `group` - The group the jobs are in
    /// * `tool_instance` - The tool instance whose jobs to claim
    /// * `cluster` - The cluster the claiming worker is in
    /// * `node` - The node the claiming worker is on
    /// * `worker` - The name of the worker that is claiming jobs
    /// * `count` - The most jobs to claim
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    /// use uuid::Uuid;
    /// # use thorium::Error;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// // create Thorium client
    /// let thorium = Thorium::build("http://127.0.0.1").token("<token>").build().await?;
    /// // claim a job of a tool instance from Thorium
    /// let tool_instance = Uuid::new_v4();
    /// let jobs = thorium.jobs.claim_workload("Corn", tool_instance, "prod", "node0", "esoteria", 1).await?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    #[cfg_attr(
        feature = "trace",
        instrument(name = "Thorium::Jobs::claim_workload", skip(self), err(Debug))
    )]
    pub async fn claim_workload(
        &self,
        group: &str,
        tool_instance: Uuid,
        cluster: &str,
        node: &str,
        worker: &str,
        count: u64,
    ) -> Result<Vec<GenericJob>, Error> {
        // build url for claiming a tool instance's jobs
        let url = format!(
            "{base}/api/jobs/workloads/claim/{group}/{tool_instance}/{cluster}/{node}/{worker}/{count}",
            base = self.host,
        );
        // build request
        let req = self.client.patch(&url).header("authorization", &self.token);
        // send this request and build the claimed jobs from the response
        send_build!(self.client, req, Vec<GenericJob>)
    }

    /// Recompute the average runtime of every active tool instance (admins only)
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    /// # use thorium::Error;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// // create Thorium client
    /// let thorium = Thorium::build("http://127.0.0.1").token("<token>").build().await?;
    /// // update the tool instance runtimes
    /// thorium.jobs.update_workload_runtimes().await?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    #[cfg_attr(
        feature = "trace",
        instrument(
            name = "Thorium::Jobs::update_workload_runtimes",
            skip(self),
            err(Debug)
        )
    )]
    pub async fn update_workload_runtimes(&self) -> Result<reqwest::Response, Error> {
        // build url for updating the tool instance runtimes
        let url = format!(
            "{base}/api/jobs/workloads/runtimes/update",
            base = self.host
        );
        // build request
        let req = self.client.post(&url).header("authorization", &self.token);
        // send this request
        send!(self.client, req)
    }

    /// Get the average runtime of every tool instance that has one (admins only)
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    /// # use thorium::Error;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// // create Thorium client
    /// let thorium = Thorium::build("http://127.0.0.1").token("<token>").build().await?;
    /// // get the tool instance runtimes
    /// let runtimes = thorium.jobs.workload_runtimes().await?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    #[cfg_attr(
        feature = "trace",
        instrument(name = "Thorium::Jobs::workload_runtimes", skip(self), err(Debug))
    )]
    pub async fn workload_runtimes(&self) -> Result<WorkloadRuntimes, Error> {
        // build url for reading the tool instance runtimes
        let url = format!("{base}/api/jobs/workloads/runtimes", base = self.host);
        // build request
        let req = self.client.get(&url).header("authorization", &self.token);
        // send this request and build the runtimes from the response
        send_build!(self.client, req, WorkloadRuntimes)
    }

    /// Tell Thorium this job has succeeded and to proceed with it
    ///
    /// # Arguments
    ///
    /// * `job` - The job to proceed with
    /// * `logs` - The stdout/stderr logs to add for this stage
    /// * `runtime` - How long this job took in seconds
    /// * `worker` - The name of the worker that claimed this job
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    /// use thorium::models::{StageLogsAdd, JobHandleStatus};
    /// # use thorium::Error;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// // create Thorium client
    /// let thorium = Thorium::build("http://127.0.0.1").token("<token>").build().await?;
    /// // claim a job from Thorium
    /// let jobs = thorium.jobs.claim("Corn", "Harvest", "CornHarvester", "prod0", "node0", "esoteria", 1).await?;
    /// // execute all claimed jobs
    /// for job in jobs.iter() {
    ///     // do some work for this job and track execution time
    ///     // build the logs object we want to save
    ///     let logs = StageLogsAdd::default()
    ///         .logs(vec!("line1", "line2", "line3"));
    ///     // then proceed with it if it succeeds
    ///     thorium.jobs.proceed(job, &logs, 10, "esoteria").await?;
    /// }
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    #[cfg_attr(
        feature = "trace",
        instrument(
            name = "Thorium::Jobs::proceed",
            skip(self, job, logs),
            fields(job = job.id.to_string()),
            err(Debug)
        )
    )]
    pub async fn proceed(
        &self,
        job: &GenericJob,
        logs: &StageLogsAdd,
        runtime: u64,
        worker: &str,
    ) -> Result<HandleJobResponse, Error> {
        // build url for proceeding with a job
        let url = format!(
            "{base}/api/jobs/handle/{id}/proceed/{runtime}",
            base = self.host,
            id = job.id,
            runtime = runtime
        );
        // build request naming the worker that claimed this job
        let req = self
            .client
            .post(&url)
            .header("authorization", &self.token)
            .query(&[("worker", worker)])
            .json(logs);
        // send this request and build a json value from the response
        send_build!(self.client, req, HandleJobResponse)
    }

    /// Tell Thorium this job has failed and to fail out the reaction
    ///
    /// # Arguments
    ///
    /// * `id` - The id of the job to error out
    /// * `logs` - The stdout/stderr logs to add for this stage
    /// * `worker` - The name of the worker that claimed this job
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::{Thorium, models::StageLogsAdd};
    /// # use thorium::Error;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// // create Thorium client
    /// let thorium = Thorium::build("http://127.0.0.1").token("<token>").build().await?;
    /// // claim a job from Thorium
    /// let jobs = thorium.jobs.claim("Corn", "Harvest", "CornHarvester", "prod0", "node0", "esoteria", 1).await?;
    /// // execute all claimed jobs
    /// for job in jobs.iter() {
    ///     // do some work for this job
    ///     // build the logs object we want to save
    ///     let logs = StageLogsAdd::default()
    ///         .logs(vec!("line1", "line2", "error_line"));
    ///     // something went wrong error out
    ///     thorium.jobs.error(&job.id, &logs, "esoteria").await?;
    /// }
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    #[cfg_attr(
        feature = "trace",
        instrument(
            name = "Thorium::Jobs::error",
            skip_all,
            fields(job = id.to_string()),
            err(Debug)
        )
    )]
    pub async fn error(
        &self,
        id: &Uuid,
        logs: &StageLogsAdd,
        worker: &str,
    ) -> Result<HandleJobResponse, Error> {
        // build url for erroring out a job
        let url = format!(
            "{base}/api/jobs/handle/{id}/error",
            base = self.host,
            id = id
        );
        // build request naming the worker that claimed this job
        let req = self
            .client
            .post(&url)
            .header("authorization", &self.token)
            .query(&[("worker", worker)])
            .json(logs);
        // send this request and build a json value from the response
        send_build!(self.client, req, HandleJobResponse)
    }

    /// Tell Thorium this generator should be slept instead of completed at the next complete
    ///
    /// # Arguments
    ///
    /// * `job` - The generator job to set as sleeping
    /// * `checkpoint` - An optional checkpoint to set
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::{Thorium, models::StageLogsAdd};
    /// use uuid::Uuid;
    /// # use thorium::Error;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// // create Thorium client
    /// let thorium = Thorium::build("http://127.0.0.1").token("<token>").build().await?;
    /// // start with the generator's job id
    /// // (given by Thorium to the generator with the `--job` kwarg)
    /// let job_id = Uuid::new_v4();
    /// thorium.jobs.sleep(&job_id, "checkpoint-1").await?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    #[cfg_attr(
        feature = "trace",
        instrument(
            name = "Thorium::Jobs::sleep",
            skip_all,
            fields(job = job_id.to_string()),
            err(Debug)
        )
    )]
    pub async fn sleep<T: Into<String>>(
        &self,
        job_id: &Uuid,
        checkpoint: T,
    ) -> Result<HandleJobResponse, Error> {
        // build url for sleeping a generator
        let url = format!(
            "{base}/api/jobs/handle/{job_id}/sleep",
            base = self.host,
            job_id = job_id
        );
        // build request
        let req = self
            .client
            .post(&url)
            .header("authorization", &self.token)
            .json(&Checkpoint {
                data: checkpoint.into(),
            });
        // send this request and build a json value from the response
        send_build!(self.client, req, HandleJobResponse)
    }

    /// Set a new checkpoint for this job
    ///
    /// This is mainly used by generators but it can be used by anything that can resume execution
    /// based on a string.
    ///
    /// # Arguments
    ///
    /// * `job` - The generator job to set as sleeping
    /// * `checkpoint` - The new checkpoint to set
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::{Thorium, models::StageLogsAdd};
    /// # use thorium::Error;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// // create Thorium client
    /// let thorium = Thorium::build("http://127.0.0.1").token("<token>").build().await?;
    /// // claim a job from Thorium
    /// let jobs = thorium.jobs.claim("Corn", "Harvest", "CornHarvester", "prod0", "node0", "esoteria", 1).await?;
    /// // execute all claimed jobs
    /// for job in jobs.iter() {
    ///     // checkpoint this job
    ///     thorium.jobs.checkpoint(job, "checkpoint-1").await?;
    /// }
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    #[cfg_attr(
        feature = "trace",
        instrument(
            name = "Thorium::Jobs::checkpoint",
            skip_all,
            fields(job = job.id.to_string()),
            err(Debug)
        )
    )]
    pub async fn checkpoint<T: Into<String>>(
        &self,
        job: &GenericJob,
        checkpoint: T,
    ) -> Result<HandleJobResponse, Error> {
        // build url for exhausting a generator
        let url = format!(
            "{base}/api/jobs/handle/{id}/checkpoint",
            base = self.host,
            id = job.id,
        );
        // build request
        let req = self
            .client
            .post(&url)
            .header("authorization", &self.token)
            .json(&Checkpoint {
                data: checkpoint.into(),
            });
        // send this request and build a json value from the response
        send_build!(self.client, req, HandleJobResponse)
    }

    /// List the deadlines between two timestamps up to a certain limit
    ///
    /// Due to how sorted sets work in redis if you have more deadlines then your limit it can
    /// be very difficult to crawl them. If your use case does require crawling them you should
    /// expect to miss deadlines when crawling as it is impossible to consistently page through
    /// them while new jobs are being added and claimed consistently.
    ///
    /// # Arguments
    ///
    /// * `start` - The timestamp to start listing deadlines at
    /// * `end` - The timestamp to stop listing deadlines at
    /// * `limit` - The max number of deadlines to retrieve
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    /// use thorium::models::ImageScaler;
    /// use chrono::prelude::*;
    /// # use thorium::Error;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// // create Thorium client
    /// let thorium = Thorium::build("http://127.0.0.1").token("<token>").build().await?;
    /// // list the next 10k deadlines due in the next hour
    /// let start = Utc::now();
    /// let end = start + chrono::Duration::hours(1);
    /// let deadlines = thorium.jobs.deadlines(ImageScaler::K8s, &start, &end, 10_000).await?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    #[cfg_attr(
        feature = "trace",
        instrument(
            name = "Thorium::Jobs::deadlines",
            skip(self, start, end),
            fields(start = start.timestamp(), end = end.timestamp()),
            err(Debug)
        )
    )]
    pub async fn deadlines(
        &self,
        scaler: ImageScaler,
        start: &DateTime<Utc>,
        end: &DateTime<Utc>,
        limit: u64,
    ) -> Result<Vec<Deadline>, Error> {
        // build url for reading deadlines
        let url = format!(
            "{base}/api/jobs/deadlines/{scaler}/{start}/{end}",
            base = self.host,
            scaler = scaler,
            start = start.timestamp(),
            end = end.timestamp()
        );
        // build request
        let req = self
            .client
            .get(&url)
            .header("authorization", &self.token)
            .query(&[("limit", limit)]);
        // send this request and build a vector of deadlines from the response
        send_build!(self.client, req, Vec<Deadline>)
    }

    /// List the running jobs between two timestamps up to a certain limit
    ///
    /// Due to how sorted sets work in redis if you have more running jobs then your limit it can
    /// be very difficult to crawl them. If your use case does require crawling them you should
    /// expect to miss running jobs when crawling as it is impossible to consistently page through
    /// them while new jobs are being added and removed.
    ///
    /// # Arguments
    ///
    /// * `start` - The timestamp to start listing deadlines at
    /// * `end` - The timestamp to stop listing deadlines at
    /// * `limit` - The max number of deadlines to retrieve
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    /// use thorium::models::ImageScaler;
    /// use chrono::prelude::*;
    /// # use thorium::Error;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// // create Thorium client
    /// let thorium = Thorium::build("http://127.0.0.1").token("<token>").build().await?;
    /// // list the running jobs for the last hour to a minute after now
    /// // we do a minute after now so we don't miss any jobs that started running while this
    /// // request was being sent
    /// let start = Utc::now() - chrono::Duration::hours(1);
    /// let end = Utc::now() + chrono::Duration::minutes(1);
    /// let running = thorium.jobs.running(ImageScaler::K8s, &start, &end, 10_000).await?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    #[cfg_attr(
        feature = "trace",
        instrument(
            name = "Thorium::Jobs::running",
            skip(self, start, end),
            fields(
                start = start.timestamp(),
                end = end.timestamp()
            ),
            err(Debug)
        )
    )]
    pub async fn running(
        &self,
        scaler: ImageScaler,
        start: &DateTime<Utc>,
        end: &DateTime<Utc>,
        limit: u64,
    ) -> Result<Vec<RunningJob>, Error> {
        // build url for getting currently running jobs
        let url = format!(
            "{base}/api/jobs/bulk/running/{scaler}/{start}/{end}",
            base = self.host,
            scaler = scaler,
            start = start.timestamp(),
            end = end.timestamp()
        );
        // build request
        let req = self
            .client
            .get(&url)
            .header("authorization", &self.token)
            .query(&[("limit", limit)]);
        // send this request and build a vector of running jobs from the response
        send_build!(self.client, req, Vec<RunningJob>)
    }

    /// Resets jobs in Thoium in bulk
    ///
    /// These jobs are normally reset because their worker was killed while executing this job.
    ///
    /// # Arguments
    ///
    /// * `ids` - The job IDs to reset
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::Thorium;
    /// use thorium::models::{JobResets, ImageScaler};
    /// use chrono::prelude::*;
    /// use uuid::Uuid;
    /// # use thorium::Error;
    ///
    /// # async fn exec() -> Result<(), Error> {
    /// // create Thorium client
    /// let thorium = Thorium::build("http://127.0.0.1").token("<token>").build().await?;
    /// // build a job reset request
    /// let resets = JobResets::new(ImageScaler::K8s, "Servers were angry").add(Uuid::new_v4());
    /// thorium.jobs.bulk_reset(&resets).await?;
    /// # // allow test code to be compiled but don't unwrap as no API instance would be up
    /// # Ok(())
    /// # }
    /// # tokio_test::block_on(async {
    /// #    exec().await
    /// # });
    /// ```
    #[cfg_attr(
        feature = "trace",
        instrument(
            name = "Thorium::Jobs::bulk_reset",
            skip_all,
            fields(scaler = resets.scaler.as_str(), ids_len = resets.jobs.len()),
            err(Debug)
        )
    )]
    pub async fn bulk_reset(&self, resets: &JobResets) -> Result<reqwest::Response, Error> {
        // build url for getting currently running jobs
        let url = format!("{base}/api/jobs/bulk/reset", base = self.host);
        // build request
        let req = self
            .client
            .post(&url)
            .header("authorization", &self.token)
            .json(&resets);
        // send this request
        send!(self.client, req)
    }
}

// Python wrapper functions
#[cfg(feature = "python")]
#[pymethods]
impl JobsBlocking {
    /// Tell Thorium this generator should be slept instead of completed at the next complete
    ///
    /// # Arguments
    ///
    /// * `job` - The generator job to set as sleeping
    /// * `checkpoint` - An optional checkpoint to set
    #[pyo3(
        name = "sleep",
        signature = (job_id: "UUID", checkpoint: "str") -> "HandleJobResponse"
    )]
    pub fn sleep_py(&self, job_id: Uuid, checkpoint: &str) -> Result<HandleJobResponse, Error> {
        self.sleep(&job_id, checkpoint)
    }

    /// Set a new checkpoint for this job
    ///
    /// This is mainly used by generators but it can be used by anything that can resume execution
    /// based on a string.
    ///
    /// # Arguments
    ///
    /// * `job` - The generator job to set as sleeping
    /// * `checkpoint` - The new checkpoint to set
    #[pyo3(name = "checkpoint")]
    pub fn checkpoint_py(
        &self,
        job: &GenericJob,
        checkpoint: &str,
    ) -> Result<HandleJobResponse, Error> {
        self.checkpoint(job, checkpoint)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use uuid::Uuid;

    use crate::Thorium;
    use crate::models::{GenericJob, GenericJobArgs, JobStatus, StageLogsAdd};

    /// The request lines a recording mock server has received
    type Recorded = Arc<Mutex<Vec<String>>>;

    /// Start a mock Thorium server that records each request line and answers every request
    /// with a job handle response
    async fn recording_server() -> (String, Recorded) {
        // bind to a random local port
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // track the request lines we receive
        let recorded: Recorded = Arc::new(Mutex::new(Vec::new()));
        let inner = recorded.clone();
        // answer requests in the background
        tokio::spawn(async move {
            loop {
                // accept the next connection
                let (mut stream, _) = listener.accept().await.unwrap();
                // read until the end of the request head and its json body
                let mut buf = Vec::new();
                let mut chunk = [0_u8; 4096];
                loop {
                    // read the next chunk of this request
                    let read = stream.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..read]);
                    // check whether the request head is complete
                    let text = String::from_utf8_lossy(&buf).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        // get the content length of this request
                        let length = text[..end]
                            .lines()
                            .find_map(|line| {
                                line.to_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|len| len.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        // stop once the whole body has arrived or the client closed the connection
                        if buf.len() >= end + 4 + length || read == 0 {
                            break;
                        }
                    }
                    // stop if the client closed the connection before finishing its head
                    if read == 0 {
                        break;
                    }
                }
                // record this request line
                let text = String::from_utf8_lossy(&buf).to_string();
                let line = text.lines().next().unwrap_or_default().to_owned();
                inner.lock().unwrap().push(line);
                // answer with a job handle response and close the connection
                let body = r#"{"status":"Completed"}"#;
                let resp = format!(
                    "HTTP/1.1 202 Accepted\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(resp.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            }
        });
        // return the server's address and the request lines it records
        (format!("http://{addr}"), recorded)
    }

    /// Build a running job to handle
    fn job() -> GenericJob {
        GenericJob {
            reaction: Uuid::new_v4(),
            id: Uuid::new_v4(),
            group: "corn".into(),
            pipeline: "harvest".into(),
            stage: "husk".into(),
            creator: "mcarson".into(),
            args: GenericJobArgs::default(),
            status: JobStatus::Running,
            deadline: chrono::Utc::now(),
            parent: None,
            generator: false,
            samples: Vec::default(),
            ephemeral: Vec::default(),
            parent_ephemeral: std::collections::HashMap::default(),
            repos: Vec::default(),
            trigger_depth: None,
            tool_instance: Uuid::new_v4(),
            run: None,
        }
    }

    /// `proceed` and `error` send the claiming worker's name as the `worker` query
    /// parameter, URL encoded
    #[tokio::test]
    async fn job_handles_name_the_worker() {
        // start a recording mock server and a client for it
        let (host, recorded) = recording_server().await;
        let thorium = Thorium::build(&host).token("token").build().await.unwrap();
        // build a running job and empty logs to handle
        let job = job();
        let logs = StageLogsAdd::default();
        // proceed and error with a plain worker name
        thorium
            .jobs
            .proceed(&job, &logs, 5, "worker-0")
            .await
            .unwrap();
        thorium
            .jobs
            .error(&job.id, &logs, "worker-0")
            .await
            .unwrap();
        // proceed and error with a worker name that needs encoding
        thorium
            .jobs
            .proceed(&job, &logs, 5, "pod/a b")
            .await
            .unwrap();
        thorium.jobs.error(&job.id, &logs, "pod/a b").await.unwrap();
        // check each request named its worker
        let lines = recorded.lock().unwrap().clone();
        let id = job.id;
        assert_eq!(
            lines,
            vec![
                format!("POST /api/jobs/handle/{id}/proceed/5?worker=worker-0 HTTP/1.1"),
                format!("POST /api/jobs/handle/{id}/error?worker=worker-0 HTTP/1.1"),
                format!("POST /api/jobs/handle/{id}/proceed/5?worker=pod%2Fa+b HTTP/1.1"),
                format!("POST /api/jobs/handle/{id}/error?worker=pod%2Fa+b HTTP/1.1"),
            ]
        );
    }
}
