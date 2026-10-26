//! The repos related routes for Thorium

use axum::Router;
use axum::extract::{Json, Multipart, Path, State};
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum_extra::body::AsyncReadBody;
use tracing::instrument;
use utoipa::OpenApi;

use super::{OpenApiParameterExamples, OpenApiSecurity};
use crate::models::AuthedUser;
use crate::models::backends::{self, TagSupport};
use crate::models::{
    ApiCursor, Association, AssociationListParams, AssociationTargetColumn, Branch, BranchDetails,
    BranchRequest, Commit, CommitDetails, CommitRequest, Commitish, CommitishDetails,
    CommitishKinds, CommitishListParams, CommitishMapRequest, CommitishRequest, GitTag,
    GitTagDetails, GitTagRequest, GrepDocumentMatches, GrepMatch, GrepMatches, GrepQueryParams,
    GrepRequest, GrepSource, GrepStats, Output, OutputFormBuilder, OutputKind, OutputMap,
    OutputResponse, Repo, RepoCheckout, RepoCreateResponse, RepoDataUploadResponse,
    RepoDownloadOpts, RepoListLine, RepoListParams, RepoRequest, RepoScheme, RepoSubmissionChunk,
    ResultFileDownloadParams, ResultGetParams, TagDeleteRequest, TagRequest, TruncationReason,
};
use crate::utils::{ApiError, AppState, bounder};

/// Allow users to add a repo to Thorium
///
/// # Arguments
///
/// * `user` - The user that is uploading sample
/// * `shared` - Shared Thorium objects
/// * `req` - The repo that is being added
#[utoipa::path(
    post,
    path = "/api/repos/",
    request_body = RepoRequest,
    responses(
        (status = 200, description = "Repo created", body = RepoCreateResponse),
        (status = 400, description = "The request is invalid: the JSON body is malformed or fails validation"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "A resource named in the request, such as a path value, group or body field, does not exist or is not visible to this user"),
        (status = 422, description = "The JSON body is well formed but does not match the expected shape, for example a required field is missing or has the wrong type"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::repos::create", skip_all, err(Debug))]
async fn create(
    user: AuthedUser,
    State(state): State<AppState>,
    Json(req): Json<RepoRequest>,
) -> Result<Json<RepoCreateResponse>, ApiError> {
    // save this repo into the backend
    let url = Repo::create(&user, req, &state.shared).await?;
    Ok(Json(RepoCreateResponse { url }))
}

/// Get info about a specific repo
///
/// # Arguments
///
/// * `user` - The user that is uploading sample
/// * `path` - The path of the repo to get info about
/// * `shared` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/repos/data/{repo_path}",
    params(
        ("repo_path" = String, Path, description = "The repo's path (for example `github.com/org/repo`), which contains slashes"),
    ),
    responses(
        (status = 200, description = "The repo", body = Repo),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "The repo does not exist"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::repos::get_repo", skip_all, err(Debug))]
async fn get_repo(
    user: AuthedUser,
    Path(repo): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<Repo>, ApiError> {
    // get this repos info
    let info = Repo::get(&user, &repo, &state.shared).await?;
    Ok(Json(info))
}

/// Save new data to a repo
///
/// # Arguments
///
/// * `user` - The user that is updating this repos commit data
/// * `path` - The path of the repo to add commits too
/// * `shared` - Shared Thorium objects
#[utoipa::path(
    post,
    path = "/api/repos/data/{repo_path}",
    params(
        ("repo_path" = String, Path, description = "The repo's path (for example `github.com/org/repo`), which contains slashes"),
    ),
    request_body(
        content = Vec<u8>,
        content_type = "multipart/form-data",
        description = "The multipart form containing the repo data to upload",
    ),
    responses(
        (status = 200, description = "The repo data was uploaded", body = RepoDataUploadResponse),
        (status = 400, description = "The request is invalid: the multipart form is malformed or fails validation"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "The repo does not exist"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::repos::upload", skip_all, err(Debug))]
async fn upload(
    user: AuthedUser,
    Path(repo_path): Path<String>,
    State(state): State<AppState>,
    multipart: Multipart,
) -> Result<Json<RepoDataUploadResponse>, ApiError> {
    // get this repos info
    let repo = Repo::get(&user, &repo_path, &state.shared).await?;
    // save the data for this repo into the backend
    let sha256 = repo.upload(&user, multipart, &state.shared).await?;
    Ok(Json(RepoDataUploadResponse { sha256 }))
}

/// Add commitishes to a repo
///
/// # Arguments
///
/// * `user` - The user that is updating this repos commit data
/// * `path` - The path containing this urls args
/// * `shared` - Shared Thorium objects
#[utoipa::path(
    post,
    path = "/api/repos/commitishes/{data}/{repo_path}",
    params(
        ("data" = String, Path, description = "The sha256 of the repo data zip these commitishes come from"),
        ("repo_path" = String, Path, description = "The repo's path (for example `github.com/org/repo`), which contains slashes"),
    ),
    request_body = CommitishMapRequest,
    responses(
        (status = 204, description = "Commitishes added"),
        (status = 400, description = "The request is invalid: the JSON body is malformed or fails validation"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "The repo does not exist"),
        (status = 422, description = "The JSON body is well formed but does not match the expected shape, for example a required field is missing or has the wrong type"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::repos::update_commitishes", skip_all, err(Debug))]
async fn update_commitishes(
    user: AuthedUser,
    Path((data, repo_path)): Path<(String, String)>,
    State(state): State<AppState>,
    Json(req): Json<CommitishMapRequest>,
) -> Result<StatusCode, ApiError> {
    // get this repos info
    let mut repo = Repo::get(&user, &repo_path, &state.shared).await?;
    // update the commits for this repo
    repo.add_commitishes(&user, &data, req, &state.shared)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Adds new tags to a repo
///
/// # Arguments
///
/// * `user` - The user that is checking whether a sample exists or not
/// * `sha256` - The sample to add a tag too
/// * `state` - Shared Thorium objects
/// * `tags` - The new tags to apply
#[utoipa::path(
    post,
    path = "/api/repos/tags/{repo_path}",
    params(
        ("repo_path" = String, Path, description = "The repo's path (for example `github.com/org/repo`), which contains slashes"),
    ),
    request_body = TagRequest<Repo>,
    responses(
        (status = 204, description = "Tags added"),
        (status = 400, description = "The request is invalid: the JSON body is malformed or fails validation"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "The repo does not exist"),
        (status = 422, description = "The JSON body is well formed but does not match the expected shape, for example a required field is missing or has the wrong type"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::repos::tag", skip_all, err(Debug))]
async fn tag(
    user: AuthedUser,
    Path(repo_path): Path<String>,
    State(state): State<AppState>,
    Json(tags): Json<TagRequest<Repo>>,
) -> Result<StatusCode, ApiError> {
    // get the repo we are adding tags too
    let repo = Repo::get(&user, &repo_path, &state.shared).await?;
    // try to add the new tags for this sample
    repo.tag(&user, tags, &state.shared).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Deletes a tag from a specific repo
///
/// # Arguments
///
/// * `user` - The user that is deleting a tag
/// * `sha256` - The repo to delete a tag from
/// * `key` - The key of the tag to delete
/// * `value` - The value of the tag to delete
/// * `params` - The url query params to use
/// * `state` - Shared Thorium objects
#[utoipa::path(
    delete,
    path = "/api/repos/tags/{repo_path}",
    params(
        ("repo_path" = String, Path, description = "The repo's path (for example `github.com/org/repo`), which contains slashes"),
    ),
    request_body = TagDeleteRequest<Repo>,
    responses(
        (status = 204, description = "Tags deleted"),
        (status = 400, description = "The request is invalid: the JSON body is malformed or fails validation"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "The repo does not exist"),
        (status = 422, description = "The JSON body is well formed but does not match the expected shape, for example a required field is missing or has the wrong type"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::repos::delete_tags", skip_all, err(Debug))]
async fn delete_tags(
    user: AuthedUser,
    Path(repo_path): Path<String>,
    State(state): State<AppState>,
    Json(tags_del): Json<TagDeleteRequest<Repo>>,
) -> Result<StatusCode, ApiError> {
    // get the repo we are deleting tags from
    let repo = Repo::get(&user, &repo_path, &state.shared).await?;
    // delete the tags from this repo
    repo.delete_tags(&user, tags_del, &state.shared).await?;
    // delete the tags from this
    Ok(StatusCode::NO_CONTENT)
}

/// List the associations for a repo
///
/// # Arguments
///
/// * `user` - The user that is listing associations
/// * `params` - The query params to use for this request
/// * `repo_path` - The repo to list associations for
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/repos/associations/{repo_path}",
    params(
        ("repo_path" = String, Path, description = "The repo's path (for example `github.com/org/repo`), which contains slashes"),
        AssociationListParams,
    ),
    responses(
        (status = 200, description = "JSON-formatted cursor response containing the associations for a repo", body = ApiCursor<Association>),
        (status = 400, description = "The request is invalid: a query parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "The repo does not exist"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::repos::list_associations", skip_all, err(Debug))]
async fn list_associations(
    user: AuthedUser,
    params: AssociationListParams,
    Path(repo_path): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<ApiCursor<Association>>, ApiError> {
    // make sure this user can access this repo
    let repo = Repo::get(&user, &repo_path, &state.shared).await?;
    // create the source to list associations from
    let source = AssociationTargetColumn::Repo(repo.url);
    // list the associations for this repo
    let cursor = Association::list(&user, params, &source, &state.shared).await?;
    Ok(Json(cursor))
}

/// List the commitishes for a repo
///
/// # Arguments
///
/// * `user` - The user that is listing the commits for a repo
/// * `params` - The params to use when listing commits for this repo
/// * `data` - The first segment of the repo's path, which the router splits from the rest
/// * `rest` - The rest of the repo's path, which contains slashes
/// * `shared` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/repos/commitishes/{data}/{repo_path}",
    params(
        ("data" = String, Path, description = "The first segment of the repo's path, which the router splits from the rest"),
        ("repo_path" = String, Path, description = "The rest of the repo's path, which contains slashes"),
        CommitishListParams,
    ),
    responses(
        (status = 200, description = "A page of the repo's commitishes", body = ApiCursor<Commitish>),
        (status = 400, description = "The request is invalid: a query parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "The repo does not exist"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::repos::commitshes", skip_all, err(Debug))]
async fn commitishes(
    user: AuthedUser,
    params: CommitishListParams,
    Path((data, rest)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Result<Json<ApiCursor<Commitish>>, ApiError> {
    // build our repo path, joining the `{data}`
    // param with the `{*repo_path}` wildcard
    let repo_path = format!("{data}/{rest}");
    // get this repos info
    let repo = Repo::get(&user, &repo_path, &state.shared).await?;
    // list the commits for this repo
    let cursor = repo
        .commitishes(&user, params, false, &state.shared)
        .await?;
    Ok(Json(cursor))
}

/// List the commit details for a repo
///
/// # Arguments
///
/// * `user` - The user that is listing the commit details for a repo
/// * `params` - The params to use when listing commit details for this repo
/// * `repo` - The repo to list commit details from
/// * `shared` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/repos/commitish-details/{repo_path}",
    params(
        ("repo_path" = String, Path, description = "The repo's path (for example `github.com/org/repo`), which contains slashes"),
        CommitishListParams,
    ),
    responses(
        (status = 200, description = "A page of the repo's commitishes with their details", body = ApiCursor<CommitishDetails>),
        (status = 400, description = "The request is invalid: a query parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "The repo does not exist"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::repos::commitsh_details", skip_all, err(Debug))]
async fn commitish_details(
    user: AuthedUser,
    params: CommitishListParams,
    Path(repo_path): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<ApiCursor<CommitishDetails>>, ApiError> {
    // get this repos info
    let repo = Repo::get(&user, &repo_path, &state.shared).await?;
    // list the next commits for this repo in our cursor
    let cursor = repo.commitishes(&user, params, true, &state.shared).await?;
    // get the details for each of the commits on this page of the cursor
    let cursor = cursor.details(&user, &repo.url, &state.shared).await?;
    Ok(Json(cursor))
}

/// Downloads a repo
///
/// # Arguments
///
/// * `user` - The user that is uploading sample
/// * `repo` - The repo to get info about
/// * `shared` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/repos/download/{repo_path}",
    params(
        ("repo_path" = String, Path, description = "The repo's path (for example `github.com/org/repo`), which contains slashes"),
        RepoDownloadOpts,
    ),
    responses(
        (status = 200, description = "The repo as a byte stream", body = Vec<u8>),
        (status = 400, description = "The request is invalid: a query parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "The repo does not exist"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::repos::download", skip_all, err(Debug))]
async fn download(
    user: AuthedUser,
    Path(repo_path): Path<String>,
    params: RepoDownloadOpts,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, ApiError> {
    // get this repos info
    let repo = Repo::get(&user, &repo_path, &state.shared).await?;
    // download this repos data scoped to the requesting user
    let stream = repo
        .download(&user, &params.kinds, params.commitish, &state.shared)
        .await?;
    // convert our byte stream to a streamable body
    let body = AsyncReadBody::new(stream.into_async_read());
    // say what the bytes are, since a browser or proxy otherwise guesses
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], body))
}

/// Lists repos by submission date
///
/// # Arguments
///
/// * `user` - The user that is listing repos
/// * `params` - The query params to use for this request
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/repos/",
    params(
        RepoListParams,
    ),
    responses(
        (status = 200, description = "List of repos by submission date", body = ApiCursor<RepoListLine>),
        (status = 400, description = "The request is invalid: a query parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "A resource named in the request, such as a path value, group or body field, does not exist or is not visible to this user"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[axum_macros::debug_handler]
#[instrument(name = "routes::repos::list", skip_all, err(Debug))]
async fn list(
    user: AuthedUser,
    params: RepoListParams,
    State(state): State<AppState>,
) -> Result<Json<ApiCursor<RepoListLine>>, ApiError> {
    // get a list of all samples in these groups
    let cursor = Repo::list(&user, params, &state.shared).await?;
    Ok(Json(cursor))
}

/// Lists repo data by submission date with details
///
/// # Arguments
///
/// * `user` - The user that is listing repos with details
/// * `params` - The query params to use for this request
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/repos/details/",
    params(
        RepoListParams,
    ),
    responses(
        (status = 200, description = "List of repo data by submission date with details", body = ApiCursor<Repo>),
        (status = 400, description = "The request is invalid: a query parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "A resource named in the request, such as a path value, group or body field, does not exist or is not visible to this user"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::repos::list_details", skip_all, err(Debug))]
async fn list_details(
    user: AuthedUser,
    params: RepoListParams,
    State(state): State<AppState>,
) -> Result<Json<ApiCursor<Repo>>, ApiError> {
    // get a list of all repos in these groups
    let list = Repo::list(&user, params, &state.shared).await?;
    // convert our list to a details list
    let cursor = list.details(&user, &state.shared).await?;
    Ok(Json(cursor))
}

/// Allow users to upload results for repos to Thorium
///
/// # Arguments
///
/// * `user` - The user submitting these results
/// * `path` - The repo path derived from the URL path
/// * `state` - Shared Thorium objects
/// * `upload` - The results being submitted
#[utoipa::path(
    post,
    path = "/api/repos/results/{repo_path}",
    params(
        ("repo_path" = String, Path, description = "The repo's path (for example `github.com/org/repo`), which contains slashes"),
    ),
    request_body(
        content = Vec<u8>,
        content_type = "multipart/form-data",
        description = "The multipart form containing the results and any result files",
    ),
    responses(
        (status = 200, description = "The results were uploaded", body = OutputResponse),
        (status = 400, description = "The request is invalid: the multipart form is malformed or fails validation"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "The repo does not exist"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::repos::upload_results", skip_all, err(Debug))]
async fn upload_results(
    user: AuthedUser,
    Path(repo_path): Path<String>,
    State(state): State<AppState>,
    upload: Multipart,
) -> Result<Json<OutputResponse>, ApiError> {
    // get our repo
    let repo = Repo::get(&user, &repo_path, &state.shared).await?;
    // build an empty form to stream results metadata into
    let form = OutputFormBuilder::<Repo>::default();
    // save these new results
    let result_id = form
        .create_results(&user, repo_path, &repo, upload, &state.shared)
        .await?;
    Ok(Json(OutputResponse { id: result_id }))
}

/// Get results for a specific repo
///
/// # Arguments
///
/// * `user` - The user getting these results
/// * `path` - The repo path derived from the URL path
/// * `params` - The query params to use with this request
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/repos/results/{repo_path}",
    params(
        ("repo_path" = String, Path, description = "The repo's path (for example `github.com/org/repo`), which contains slashes"),
        ResultGetParams,
    ),
    responses(
        (status = 200, description = "The repo's results by tool", body = OutputMap),
        (status = 400, description = "The request is invalid: a query parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "The repo does not exist"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::repos::get_results", skip_all, err(Debug))]
async fn get_results(
    user: AuthedUser,
    Path(repo_path): Path<String>,
    params: ResultGetParams,
    State(state): State<AppState>,
) -> Result<Json<OutputMap>, ApiError> {
    // get our repo
    let repo = Repo::get(&user, &repo_path, &state.shared).await?;
    // get the results for this repo
    let mut outputs = OutputMap::get(&repo_path, &repo, &user, params, &state.shared).await?;
    // trim our results to the max retained amount
    outputs.limit(state.shared.config.thorium.retention.results);
    Ok(Json(outputs))
}

/// Downloads a repo's results file from s3
///
/// # Arguments
///
/// * `user` - The user downloading this results file
/// * `path_params` - All params in this url path
/// * `state` - Shared Thorium objects
#[utoipa::path(
    get,
    path = "/api/repos/result-files/{repo_path}",
    params(
        ("repo_path" = String, Path, description = "The repo path, the tool, and the result id joined by slashes: `<repo path>/<tool>/<result id>`"),
        ResultFileDownloadParams,
    ),
    responses(
        (status = 200, description = "The result file as a byte stream", body = Vec<u8>),
        (status = 400, description = "The request is invalid: a query parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "The path has too few segments, or the result file does not exist"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::repos::download_result_file", skip_all, err(Debug))]
async fn download_result_file(
    user: AuthedUser,
    Path(path_params): Path<String>,
    params: ResultFileDownloadParams,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, ApiError> {
    // split the path on '/'
    let mut path_split: Vec<&str> = path_params.split('/').collect();
    // if we have less then 3 path params then return a 404
    if path_split.len() < 3 {
        return Err(ApiError::new(StatusCode::NOT_FOUND, None));
    }
    // pop the required params
    if let Some(raw_uuid) = path_split.pop() {
        let result_id = bounder::uuid(raw_uuid, "result id")?;
        if let Some(tool) = path_split.pop() {
            // build our repo path from what's left
            let repo_path = itertools::join(path_split.iter(), "/");
            // start streaming a results file from s3
            let stream = Output::download(
                OutputKind::Repos,
                &user,
                &repo_path,
                tool,
                &result_id,
                params.result_file,
                &state.shared,
            )
            .await?;
            // convert our byte stream to a streamable body
            let body = AsyncReadBody::new(stream.into_async_read());
            // say what the bytes are, since a browser or proxy otherwise guesses
            return Ok(([(header::CONTENT_TYPE, "application/octet-stream")], body).into_response());
        }
    }
    Err(ApiError::new(StatusCode::NOT_FOUND, None))
}

/// Search a page of repos with grep
///
/// This searches every repo this user can see, so it returns a cursor that can be
/// continued if more repos still need to be searched.
///
/// # Arguments
///
/// * `user` - The user that is grepping these repos
/// * `params` - The query params for this grep search
/// * `state` - Shared Thorium objects
/// * `request` - The grep request body containing the regex pattern
#[utoipa::path(
    post,
    path = "/api/repos/grep/",
    params(
        GrepQueryParams,
    ),
    request_body = GrepRequest,
    responses(
        (status = 200, description = "A page of grep matches, with a cursor to continue searching repos that were not reached", body = ApiCursor<GrepMatches>),
        (status = 400, description = "The request is invalid: the pattern is not a valid regex; the JSON body is malformed; a query parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "The cursor or a group named in the request does not exist or is not visible to this user"),
        (status = 422, description = "The JSON body is well formed but does not match the expected shape, for example a required field is missing or has the wrong type"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::repos::grep", skip_all, err(Debug))]
async fn grep(
    user: AuthedUser,
    params: GrepQueryParams,
    State(state): State<AppState>,
    Json(request): Json<GrepRequest>,
) -> Result<Json<ApiCursor<GrepMatches>>, ApiError> {
    // combine our query params and pattern into the full grep params
    let params = params.into_params(request);
    // grep a page of repos
    let matches = backends::grep::grep_many::<Repo>(&user, params, &state.shared).await?;
    Ok(Json(matches))
}

/// Search a single repo with grep
///
/// # Arguments
///
/// * `user` - The user that is grepping this repo
/// * `repo_path` - The url of the repo to grep
/// * `params` - The query params for this grep search
/// * `state` - Shared Thorium objects
/// * `request` - The grep request body containing the regex pattern
#[utoipa::path(
    post,
    path = "/api/repos/grep/{repo_path}",
    params(
        ("repo_path" = String, Path, description = "The repo's path (for example `github.com/org/repo`), which contains slashes"),
        GrepQueryParams,
    ),
    request_body = GrepRequest,
    responses(
        (status = 200, description = "The grep matches for this repo", body = GrepMatches),
        (status = 400, description = "The request is invalid: the pattern is not a valid regex; the JSON body is malformed; a query parameter has an invalid value"),
        (status = 401, description = "This user is not authorized to access this route"),
        (status = 404, description = "The repo does not exist or is not visible to this user"),
        (status = 422, description = "The JSON body is well formed but does not match the expected shape, for example a required field is missing or has the wrong type"),
        (status = 500, description = "An internal error occurred, such as a backing database, cache or object store failing"),
        (status = 503, description = "A service Thorium depends on, such as Redis or LDAP, is unavailable"),
    ),
    security(
        ("basic" = []),
        ("bearer" = []),
    )
)]
#[instrument(name = "routes::repos::grep_repo", skip_all, err(Debug))]
async fn grep_repo(
    user: AuthedUser,
    Path(repo_path): Path<String>,
    params: GrepQueryParams,
    State(state): State<AppState>,
    Json(request): Json<GrepRequest>,
) -> Result<Json<GrepMatches>, ApiError> {
    // combine our query params and pattern into the full grep params
    let params = params.into_params(request);
    // grep this single repo
    let matches =
        backends::grep::grep_one::<Repo>(&user, &repo_path, params, &state.shared).await?;
    Ok(Json(matches))
}

/// The struct containing our openapi docs
#[derive(OpenApi)]
#[openapi(
    paths(list, create, list_details, get_repo, upload, commitishes, update_commitishes, commitish_details, download, tag, delete_tags, list_associations, get_results, upload_results, download_result_file, grep, grep_repo),
    components(schemas(ApiCursor<Repo>, ApiCursor<RepoListLine>, ApiCursor<GrepMatches>, Branch, BranchDetails, BranchRequest, Commit, CommitDetails, Commitish, CommitishDetails, CommitishKinds, CommitishMapRequest, CommitishRequest, CommitRequest, GitTag, GitTagDetails, GitTagRequest, GrepDocumentMatches, GrepMatch, GrepMatches, GrepQueryParams, GrepRequest, GrepSource, GrepStats, OutputMap, OutputResponse, Repo, RepoCheckout, RepoCreateResponse, RepoDownloadOpts, RepoListParams, RepoDataUploadResponse, RepoRequest, RepoScheme, RepoSubmissionChunk, ResultGetParams, TagDeleteRequest<Repo>, TagRequest<Repo>, TruncationReason)),
    modifiers(&OpenApiSecurity, &OpenApiParameterExamples),
)]
pub struct RepoApiDocs;

/// Return the openapi docs for these routes
#[allow(dead_code)]
async fn openapi() -> Json<utoipa::openapi::OpenApi> {
    Json(RepoApiDocs::openapi())
}

/// Add the file routes to our router
///
/// # Arguments
///
/// * `router` - The router to add routes too
pub fn mount(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/repos/", get(list).post(create))
        .route("/repos/details/", get(list_details))
        .route("/repos/grep/", post(grep))
        .route("/repos/grep/{*repo_path}", post(grep_repo))
        .route("/repos/data/{*repo_path}", get(get_repo).post(upload))
        .route(
            "/repos/commitishes/{data}/{*repo_path}",
            get(commitishes).post(update_commitishes),
        )
        .route(
            "/repos/commitish-details/{*repo_path}",
            get(commitish_details),
        )
        .route("/repos/download/{*repo_path}", get(download))
        .route("/repos/tags/{*repo_path}", post(tag).delete(delete_tags))
        .route("/repos/associations/{*repo_path}", get(list_associations))
        .route(
            "/repos/results/{*repo_path}",
            get(get_results).post(upload_results),
        )
        .route(
            "/repos/result-files/{*repo_path}",
            get(download_result_file),
        )
}
