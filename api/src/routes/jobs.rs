use axum::Router;
use axum::extract::{Json, Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::{get, patch, post};
use tracing::instrument;
use utoipa::OpenApi;
use uuid::Uuid;

use super::OpenApiSecurity;

use crate::models::AuthedUser;
use crate::models::{
    Checkpoint, CommitishKinds, Deadline, GenericJob, GenericJobArgs, GenericJobOpts, Group,
    HandleJobResponse, ImageScaler, JobHandleParams, JobHandleStatus, JobListOpts,
    JobResetRequestor, JobResets, JobStatus, Pipeline, RawJob, RepoDependency, RunningJob,
    StageLogLine, StageLogsAdd, SystemComponents, WorkerName, WorkloadRuntimes,
};
use crate::utils::{ApiError, AppState};

/// Claims a job from a specific pipeline and stage for a worker
///
/// # Arguments
///
/// * `user` - The user that is claiming a job
/// * `group` - The group the target pipeline is in
/// * `pipeline` - The name of the pipeline to claim from
/// * `stage` - The specific stage to claim from
/// * `worker` - The name of the worker that is claiming this job
/// * `limit` - The max number of jobs to claim
/// * `state` - Shared Thorium objects
#[utoipa::path(
    patch,
    path = "/api/jobs/claim/:group/:pipeline/:stage/:cluster/:node/:worker/:limit",
    params(
        ("group" = String, Path, description = "The group the target pipeline is in"),
        ("pipeline" = String, Path, description = "The name of the pipeline to claim from"),
        ("stage" = String, Path, description = "The specific stage to claim from"),
        ("cluster" = String, Path, description = "The name of the cluster of the worker that is claiming this job"),
        ("node" = String, Path, description = "The name of the node of the worker that is claiming this job"),
        ("worker" = String, Path, description = "The name of the worker that is claiming this job"),
        ("limit" = usize, Path, description = "The max number of jobs to claim"),
    ),
    responses(
        (status = 200, description = "Slept the specified job", body = Vec<GenericJob>),
        (status = 401, description = "This user is not authorized to access this route"),
    ),
    security(
        ("basic" = []),
    )
)]
#[instrument(name = "routes::jobs::claim", skip_all, err(Debug))]
async fn claim(
    user: AuthedUser,
    Path((group, pipeline, stage, cluster, node, worker, limit)): Path<(
        String,
        String,
        String,
        String,
        String,
        String,
        usize,
    )>,
    State(state): State<AppState>,
) -> Result<Json<Vec<GenericJob>>, ApiError> {
    // get pipeline
    let (group, pipeline) = Pipeline::get(&user, &group, &pipeline, &state.shared).await?;
    // build the primary keys to the worker that is claiming this job
    let worker = WorkerName::new(cluster, node, worker);
    // claim jobs if any exist
    let claims = GenericJob::claim(
        &user,
        &group,
        &pipeline,
        &stage,
        limit,
        &worker,
        &state.shared,
    )
    .await?;
    // return claimed jobs
    Ok(Json(claims))
}

/// Claims jobs of a tool instance for a worker
///
/// A per-run worker claims only its run's jobs of the tool instance; a shared worker claims the
/// jobs its user queued for the tool instance in the group.
///
/// # Arguments
///
/// * `user` - The user that is claiming jobs
/// * `group` - The group the jobs are in
/// * `tool_instance` - The tool instance whose jobs to claim
/// * `cluster` - The cluster of the worker that is claiming jobs
/// * `node` - The node of the worker that is claiming jobs
/// * `worker` - The name of the worker that is claiming jobs
/// * `limit` - The max number of jobs to claim
/// * `state` - Shared Thorium objects
#[utoipa::path(
    patch,
    path = "/api/jobs/workloads/claim/:group/:tool_instance/:cluster/:node/:worker/:limit",
    params(
        ("group" = String, Path, description = "The group the jobs are in"),
        ("tool_instance" = Uuid, Path, description = "The tool instance whose jobs to claim"),
        ("cluster" = String, Path, description = "The name of the cluster of the worker that is claiming jobs"),
        ("node" = String, Path, description = "The name of the node of the worker that is claiming jobs"),
        ("worker" = String, Path, description = "The name of the worker that is claiming jobs"),
        ("limit" = usize, Path, description = "The max number of jobs to claim"),
    ),
    responses(
        (status = 200, description = "The claimed jobs", body = Vec<GenericJob>),
        (status = 401, description = "This user is not authorized to claim jobs for this worker"),
    ),
    security(
        ("basic" = []),
    )
)]
#[instrument(name = "routes::jobs::claim_workload", skip_all, err(Debug))]
async fn claim_workload(
    user: AuthedUser,
    Path((group, tool_instance, cluster, node, worker, limit)): Path<(
        String,
        Uuid,
        String,
        String,
        String,
        usize,
    )>,
    State(state): State<AppState>,
) -> Result<Json<Vec<GenericJob>>, ApiError> {
    // make sure we are a member of this group and it exists
    let group = Group::authorize(&user, &group, &state.shared).await?;
    // build the primary keys to the worker that is claiming jobs
    let worker = WorkerName::new(cluster, node, worker);
    // claim jobs if any exist
    let claims =
        GenericJob::claim_workload(&user, &group, tool_instance, limit, &worker, &state.shared)
            .await?;
    Ok(Json(claims))
}

/// Recompute the average runtime of every active tool instance
///
/// # Arguments
///
/// * `user` - The user that is updating runtimes
/// * `state` - Shared Thorium objects
#[utoipa::path(
    post,
    path = "/api/jobs/workloads/runtimes/update",
    params(),
    responses(
        (status = 204, description = "Tool instance runtimes updated"),
        (status = 401, description = "This user is not an admin"),
    ),
    security(
        ("basic" = []),
    )
)]
#[instrument(name = "routes::jobs::update_workload_runtimes", skip_all, err(Debug))]
async fn update_workload_runtimes(
    user: AuthedUser,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    // update the tool instance runtimes
    RawJob::update_workload_runtimes(&user, &state.shared).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Get the average runtime of every tool instance that has one
///
/// # Arguments
///
/// * `user` - The user that is reading runtimes
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/jobs/workloads/runtimes",
    params(),
    responses(
        (status = 200, description = "The tool instance runtimes", body = WorkloadRuntimes),
        (status = 401, description = "This user is not an admin"),
    ),
    security(
        ("basic" = []),
    )
)]
#[instrument(name = "routes::jobs::workload_runtimes", skip_all, err(Debug))]
async fn workload_runtimes(
    user: AuthedUser,
    State(state): State<AppState>,
) -> Result<Json<WorkloadRuntimes>, ApiError> {
    // read the tool instance runtimes
    let runtimes = RawJob::workload_runtimes(&user, &state.shared).await?;
    Ok(Json(runtimes))
}

/// Proceed with this job a worker has just completed
///
/// # Arguments
///
/// * `user` - The user that is proceeding with this job
/// * `id` - The uuid of the job that was completed
/// * `runtime` - The amount of time in seconds this job took to complete
/// * `params` - The name of the worker that claimed this job
/// * `state` - Shared Thorium objects
/// * `logs` - Any logs to append to this jobs stage logs
#[utoipa::path(
    post,
    path = "/api/jobs/handle/:id/proceed/:runtime",
    params(
        ("id" = Uuid, Path, description = "The uuid of the job that was completed"),
        ("runtime" = u64, Path, description = "The amount of time in seconds this job took to complete"),
        ("worker" = Option<String>, Query, description = "The name of the worker that claimed this job (required for non-admins)"),
        ("logs" = StageLogsAdd, description = "Any logs to append to this jobs stage logs"),
    ),
    responses(
        (status = 202, description = "Proceeding with specified job", body = HandleJobResponse),
        (status = 400, description = "The worker that claimed this job was not set"),
        (status = 401, description = "This user is not authorized to access this route"),
    ),
    security(
        ("basic" = []),
    )
)]
#[instrument(name = "routes::jobs::proceed", skip_all, err(Debug))]
async fn proceed(
    user: AuthedUser,
    Path((id, runtime)): Path<(Uuid, u64)>,
    Query(params): Query<JobHandleParams>,
    State(state): State<AppState>,
    Json(logs): Json<StageLogsAdd>,
) -> Result<Response, ApiError> {
    // get job object
    let (group, job) = RawJob::get(&user, &id, &state.shared).await?;
    // get the name of the worker that claimed this job
    let worker = params.worker.as_deref();
    // proceed with job
    let status = job
        .proceed(&user, &group, runtime, logs, worker, &state.shared)
        .await?;
    // build response
    let response = Json(HandleJobResponse { status });
    Ok((StatusCode::ACCEPTED, response).into_response())
}

/// Error out this job that has just failed
///
/// # Arguments
///
/// * `user` - The user that is erroring out this job
/// * `id` - The uuid of the job that was failed
/// * `params` - The name of the worker that claimed this job
/// * `state` - Shared Thorium objects
/// * `logs` - Any logs to append to this jobs stage logs
#[utoipa::path(
    post,
    path = "/api/jobs/handle/:id/error",
    params(
        ("id" = Uuid, Path, description = "The uuid of the job that was completed"),
        ("worker" = Option<String>, Query, description = "The name of the worker that claimed this job (required for non-admins)"),
        ("logs" = StageLogsAdd, description = "Any logs to append to this jobs stage logs"),
    ),
    responses(
        (status = 202, description = "Handle specified failed job", body = HandleJobResponse),
        (status = 400, description = "The worker that claimed this job was not set"),
        (status = 401, description = "This user is not authorized to access this route"),
    ),
    security(
        ("basic" = []),
    )
)]
#[instrument(name = "routes::jobs::error", skip_all, fields(job = id.to_string()), err(Debug))]
async fn error(
    user: AuthedUser,
    Path(id): Path<Uuid>,
    Query(params): Query<JobHandleParams>,
    State(state): State<AppState>,
    Json(logs): Json<StageLogsAdd>,
) -> Result<Response, ApiError> {
    // get job object
    let (group, job) = RawJob::get(&user, &id, &state.shared).await?;
    // get the name of the worker that claimed this job
    let worker = params.worker.as_deref();
    // error out this job
    let status = job
        .error(&user, &group, logs, worker, &state.shared)
        .await?;
    // build response
    let response = Json(HandleJobResponse { status });
    Ok((StatusCode::ACCEPTED, response).into_response())
}

/// Sleep this generator job
///
/// Only generator jobs should be slept.
///
/// # Arguments
///
/// * `user` - The user that is sleeping this job
/// * `id` - The uuid of the job that is going to sleep
/// * `state` - Shared Thorium objects
/// * `checkpoint` - A checkpoint object to use when waking this job
#[utoipa::path(
    post,
    path = "/api/jobs/handle/:id/sleep",
    params(
        ("id" = Uuid, Path, description = "The uuid of the job that was completed"),
        ("checkpoint" = Checkpoint, description = "A checkpoint object to use when waking this job"),
    ),
    responses(
        (status = 202, description = "Slept the specified job", body = HandleJobResponse),
        (status = 401, description = "This user is not authorized to access this route"),
    ),
    security(
        ("basic" = []),
    )
)]
#[instrument(name = "routes::jobs::sleep", skip_all, fields(job = id.to_string()), err(Debug))]
async fn sleep(
    user: AuthedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
    Json(checkpoint): Json<Checkpoint>,
) -> Result<Response, ApiError> {
    // get job object
    let (group, job) = RawJob::get(&user, &id, &state.shared).await?;
    // sleep this job and set an optional checkpoint
    let status = job.sleep(&user, &group, checkpoint, &state.shared).await?;
    // build response
    let response = Json(HandleJobResponse { status });
    Ok((StatusCode::ACCEPTED, response).into_response())
}

/// Checkpoint generator job
///
/// Only generator jobs should use checkpoints.
///
/// # Arguments
///
/// * `user` - The user that is checkpointing this job
/// * `id` - The uuid of the job that is being checkpointed
/// * `state` - Shared Thorium objects
/// * `checkpoint` - A checkpoint object to use when waking this job
#[utoipa::path(
    post,
    path = "/api/jobs/handle/:id/checkpoint",
    params(
        ("id" = Uuid, Path, description = "The uuid of the job that was completed"),
        ("checkpoint" = Checkpoint, description = "A checkpoint object to use when waking this job"),
    ),
    responses(
        (status = 202, description = "Slept the specified job", body = HandleJobResponse),
        (status = 401, description = "This user is not authorized to access this route"),
    ),
    security(
        ("basic" = []),
    )
)]
#[instrument(name = "routes::jobs::checkpoint", skip_all, fields(job = id.to_string()), err(Debug))]
async fn checkpoint(
    user: AuthedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
    Json(checkpoint): Json<Checkpoint>,
) -> Result<Response, ApiError> {
    // get job object
    let (group, job) = RawJob::get(&user, &id, &state.shared).await?;
    // checkpoint this job
    let status = job
        .checkpoint(&user, &group, checkpoint, &state.shared)
        .await?;
    // build response
    let response = Json(HandleJobResponse { status });
    Ok((StatusCode::ACCEPTED, response).into_response())
}

/// Resets jobs in bulk
///
/// # Arguments
///
/// * `user` - The user that is reseting these jobs
/// * `state` - Shared Thorium objects
/// * `resets` - The jobs to reset
#[utoipa::path(
    post,
    path = "/api/jobs/bulk/reset",
    params(
        ("resets" = JobResets, description = "The jobs to reset"),
    ),
    responses(
        // TODO_UTOIPA: this func doesn't seem to return *anything*, a sanity check here would be good
        (status = 200, description = "Reset the specified job(s)"),
        (status = 401, description = "This user is not authorized to access this route"),
    ),
    security(
        ("basic" = []),
    )
)]
#[instrument(name = "routes::jobs::bulk_reset", skip_all, err(Debug))]
async fn bulk_reset(
    user: AuthedUser,
    State(state): State<AppState>,
    Json(resets): Json<JobResets>,
) -> Result<StatusCode, ApiError> {
    // proceed with job
    RawJob::bulk_reset(&user, resets, &state.shared).await?;
    // return a 200
    Ok(StatusCode::OK)
}

/// List jobs in the deadline stream by when they must be started by.
///
/// # Arguments
///
/// * `user` - The user that is listing jobs
/// * `scaler` - The scaler to get deadlines for
/// * `start` - The starting timestamp in epoch time to list from
/// * `end` - The ending timestamp in epoch time to list to
/// * `params` - The query params to use for this request
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/jobs/deadlines/:scaler/:start/:end",
    params(
        ("scalar" = ImageScaler, Path, description = "The scaler to get deadlines for"),
        ("start" = i64, Path, description = "The starting timestamp in epoch time to list from"),
        ("end" = i64, Path, description = "The ending timestamp in epoch time to list to"),
        ("params" = JobListOpts, Query, description = "The query params to use for this request"),
    ),
    responses(
        (status = 200, description = "Slept the specified job", body = Vec<Deadline>),
        (status = 401, description = "This user is not authorized to access this route"),
    ),
    security(
        ("basic" = []),
    )
)]
#[instrument(name = "routes::jobs::read_deadlines", skip_all, err(Debug))]
async fn read_deadlines(
    user: AuthedUser,
    Path((scaler, start, end)): Path<(ImageScaler, i64, i64)>,
    Query(params): Query<JobListOpts>,
    State(state): State<AppState>,
) -> Result<Json<Vec<Deadline>>, ApiError> {
    // read from the deadline stream
    let deadlines = Deadline::read(
        &user,
        scaler,
        start,
        end,
        params.skip,
        params.limit,
        &state.shared,
    )
    .await?;
    Ok(Json(deadlines))
}

/// List jobs in the running stream by when they were started
///
/// # Arguments
///
/// * `user` - The user that is listing jobs
/// * `scaler` - The scaler to get running jbos for
/// * `start` - The starting timestamp in epoch time to list from
/// * `end` - The ending timestamp in epoch time to list to
/// * `params` - The query params to use for this request
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/jobs/bulk/running/:scaler/:start/:end",
    params(
        ("scalar" = ImageScaler, Path, description = "The scaler to get running jbos for"),
        ("start" = i64, Path, description = "The starting timestamp in epoch time to list from"),
        ("end" = i64, Path, description = "The ending timestamp in epoch time to list to"),
        ("params" = JobListOpts, Query, description = "The query params to use for this request"),
    ),
    responses(
        (status = 200, description = "List of running jobs", body = Vec<RunningJob>),
        (status = 401, description = "This user is not authorized to access this route"),
    ),
    security(
        ("basic" = []),
    )
)]
#[instrument(name = "routes::jobs::bulk_running", skip_all, err(Debug))]
async fn bulk_running(
    user: AuthedUser,
    Path((scaler, start, end)): Path<(ImageScaler, i64, i64)>,
    Query(params): Query<JobListOpts>,
    State(state): State<AppState>,
) -> Result<Json<Vec<RunningJob>>, ApiError> {
    // read from the deadline stream
    let running = RawJob::running(
        &user,
        scaler,
        start,
        end,
        params.skip,
        params.limit,
        &state.shared,
    )
    .await?;
    Ok(Json(running))
}

/// Read one job by its id
///
/// Only admins can read a job by its id alone, and this is meant for tests.
///
/// # Arguments
///
/// * `user` - The user that is reading a job
/// * `id` - The id of the job to read
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/jobs/{id}",
    params(
        ("id" = Uuid, Path, description = "The id of the job to read"),
    ),
    responses(
        (status = 200, description = "The job", body = RawJob),
        (status = 401, description = "This user is not an admin"),
        (status = 404, description = "The job does not exist"),
    ),
    security(
        ("basic" = []),
    )
)]
#[instrument(name = "routes::jobs::get_job", skip_all, err(Debug))]
async fn get_job(
    user: AuthedUser,
    Path(id): Path<Uuid>,
    State(state): State<AppState>,
) -> Result<Json<RawJob>, ApiError> {
    // read this job as an admin
    let job = RawJob::admin_get(&user, &id, &state.shared).await?;
    Ok(Json(job))
}

/// The struct containing our openapi docs
#[derive(OpenApi)]
#[openapi(
    paths(claim, claim_workload, update_workload_runtimes, workload_runtimes, proceed, error, sleep, checkpoint, bulk_reset, read_deadlines, bulk_running, get_job),
    components(schemas(Checkpoint, CommitishKinds, Deadline, GenericJob, GenericJobArgs, GenericJobOpts, HandleJobResponse, ImageScaler, JobHandleParams, JobHandleStatus, JobListOpts, JobResetRequestor, JobResets, JobHandleStatus, JobStatus, RawJob, RepoDependency, RunningJob, StageLogLine, StageLogsAdd, SystemComponents, WorkloadRuntimes)),
    modifiers(&OpenApiSecurity),
)]
pub struct JobApiDocs;

/// Return the openapi docs for these routes
#[allow(dead_code)]
async fn openapi() -> Json<utoipa::openapi::OpenApi> {
    Json(JobApiDocs::openapi())
}

/// Add the streams routes to our router
///
/// # Arguments
///
// * `router` - The router to add routes too
pub fn mount(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/jobs/{id}", get(get_job))
        .route(
            "/jobs/claim/{group}/{pipeline}/{stage}/{cluster}/{node}/{worker}/{limit}",
            patch(claim),
        )
        .route(
            "/jobs/workloads/claim/{group}/{tool_instance}/{cluster}/{node}/{worker}/{limit}",
            patch(claim_workload),
        )
        .route(
            "/jobs/workloads/runtimes/update",
            post(update_workload_runtimes),
        )
        .route("/jobs/workloads/runtimes", get(workload_runtimes))
        .route("/jobs/handle/{id}/proceed/{runtime}", post(proceed))
        .route("/jobs/handle/{id}/error", post(error))
        .route("/jobs/handle/{id}/sleep", post(sleep))
        .route("/jobs/handle/{id}/checkpoint", post(checkpoint))
        .route("/jobs/bulk/reset", post(bulk_reset))
        .route(
            "/jobs/deadlines/{scaler}/{start}/{end}",
            get(read_deadlines),
        )
        .route(
            "/jobs/bulk/running/{scaler}/{start}/{end}",
            get(bulk_running),
        )
}
