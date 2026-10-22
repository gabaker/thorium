//! Wrappers for interacting with reactions within Thorium with different backends
//! Currently only Redis is supported

use aws_sdk_s3::primitives::ByteStream;
use axum::extract::{FromRequestParts, Multipart};
use axum::http::request::Parts;
use base64::Engine as _;
use chrono::prelude::*;
use futures::StreamExt;
use futures::stream;
use std::collections::{HashMap, HashSet};
use tracing::{Level, event, instrument, span};
use uuid::Uuid;

use super::db;
use crate::models::ApiCursor;
use crate::models::ReactionCursorParams;
use crate::models::backends::run_files::{self, RunFileParts, ToolAccess};
use crate::models::backends::stage_tools::{self, ToolInstanceMemo};
use crate::models::{
    AgentProvenance, AuthedUser, PlannedStage, ReactionPlan, ReactionSource, RunProvenance,
    ScopedToken, ToolProvenance,
};
use crate::models::{
    AgenticTool, AgenticToolBacking, BulkReactionResponse, DEFAULT_PIPELINE_SLA, Entity,
    EntityMetadata, GenericJobOpts, Group, GroupAllowAction, HarnessSpec, ImageScaler,
    InstancedAgent, JobList, Pipeline, Reaction, ReactionCache, ReactionCacheUpdate,
    ReactionDetailsList, ReactionExpire, ReactionList, ReactionRequest, ReactionStatus,
    ReactionUpdate, ReactionWorkflowRun, Repo, RepoDependency, RepoDependencyRequest, RunInputs,
    RunModels, RunTokenScope, Sample, ScopedTokenRequest, ScopedTokenRole, StageLogs, StageLogsAdd,
    StatusUpdate, ToolInstanceEntity, ToolMechanism, ToolRunRequest, ToolSpec, User, UserRole,
    WorkflowInstanceEntity, WorkflowResultKey, WorkflowRunRequest,
};
use crate::utils::{ApiError, Shared, bounder};
use crate::{
    bad, can_delete, can_modify, deserialize, deserialize_ext, deserialize_opt, extract,
    internal_err_unwrapped, is_admin, not_found, unauthorized,
};

pub(crate) mod models;

/// Updates to apply to cache files
pub struct InternalReactionCacheFileUpdates {
    /// New or updated reaction cache files
    pub added: Vec<String>,
    /// Any files that were deleted from our reaction cache
    pub deleted: Vec<String>,
}

impl InternalReactionCacheFileUpdates {
    /// Create an empty ``InternalReactionCacheFileUpdates``
    ///
    /// # Arguments
    ///
    /// * `capacity` - The capacity to preallocate for added cache files
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        InternalReactionCacheFileUpdates {
            added: Vec::with_capacity(capacity),
            deleted: Vec::default(),
        }
    }
}

/// Validate a user may override the args of every stage whose options override them
///
/// The developer role is checked against the scaler of each overridden stage's tool instance.
///
/// # Arguments
///
/// * `user` - The user trying to override args
/// * `group` - The group the reaction is in
/// * `opts` - The stages and the options set for each of them
/// * `plan` - The reaction's resolved plan
/// * `memo` - The tool instances already read for this request
/// * `shared` - Shared Thorium objects
async fn can_override_opts<'a, I>(
    user: &User,
    group: &Group,
    opts: I,
    plan: &ReactionPlan,
    memo: &mut ToolInstanceMemo,
    shared: &Shared,
) -> Result<(), ApiError>
where
    I: IntoIterator<Item = (&'a String, &'a GenericJobOpts)>,
{
    // get the stages we are overriding
    let overrides = opts
        .into_iter()
        .filter(|(_, opts)| {
            opts.override_positionals || opts.override_kwargs || opts.override_cmd.is_some()
        })
        .map(|(name, _)| name)
        .collect::<Vec<&String>>();
    // only validate reactions if we found some overrides
    if !overrides.is_empty() {
        // get the scalers of the overridden stages' tool instances
        let scalers = override_scalers(&group.name, &overrides, plan, memo, shared).await?;
        // make sure we have the right developer roles
        if group.developer_many(user, &scalers).is_err() {
            return unauthorized!();
        }
    }
    Ok(())
}

impl ReactionRequest {
    /// Validate we are allowed to overwrite args for any stages we try to
    ///
    /// The developer role is checked against the scaler of each overridden stage's tool instance.
    ///
    /// # Arguments
    ///
    /// * `user` - The user trying to override args
    /// * `group` - The group this reaction is in
    /// * `plan` - The reaction's resolved plan
    /// * `memo` - The tool instances already read for this request
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "ReactionRequest::can_override", skip_all, err(Debug))]
    pub async fn can_override(
        &self,
        user: &User,
        group: &Group,
        plan: &ReactionPlan,
        memo: &mut ToolInstanceMemo,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // get the options this request sets for each stage
        let opts = self.args.iter().map(|(name, args)| (name, &args.opts));
        // make sure we may override args for every stage whose options override them
        can_override_opts(user, group, opts, plan, memo, shared).await
    }

    /// Uploads any ephemeral files required for this reaction to execute
    ///
    /// # Arguments
    ///
    /// * `reaction` - The reaction to upload ephemeral files for
    /// * `files` - The base64 encoded files to save to s3, keyed by file name
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "ReactionRequest::upload_files", skip_all, err(Debug))]
    pub async fn upload_files(
        reaction: &Uuid,
        files: HashMap<String, String>,
        shared: &Shared,
    ) -> Result<Vec<String>, ApiError> {
        // validate all file names are safe
        for name in files.keys() {
            bounder::file_name(name, "ephemeral file names", 1, 32)?;
        }
        // build the list of file paths
        let mut s3_paths = Vec::with_capacity(files.len());
        // try saving these files to s3
        for (name, encoded) in files {
            // build the path to save this file to in s3
            let path = format!("{reaction}/{name}");
            // write this file to s3
            shared.s3.ephemeral.upload_base64(&path, &encoded).await?;
            s3_paths.push(name);
        }
        Ok(s3_paths)
    }

    /// Casts a reaction request to a bounds checked Reaction
    ///
    /// # Arguments
    ///
    /// * `user` - The user casting this reaction request
    /// * `pipeline` - The pipeline this reaction is for
    /// * `plan` - The stages resolved for this reaction
    /// * `provenance` - The exact versions of the tool instances the plan runs
    /// * `parent_ephemeral` - Any ephemeral files from any parent reactions
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "ReactionRequest::cast", skip_all, err(Debug))]
    pub async fn cast<'a>(
        self,
        user: &User,
        pipeline: &'a Pipeline,
        plan: ReactionPlan,
        provenance: RunProvenance,
        parent_ephemeral: HashMap<String, Uuid>,
        shared: &Shared,
    ) -> Result<(Reaction, ReactionCache, &'a Pipeline), ApiError> {
        // ensure that all args defined are contained in the pipeline
        for image in self.args.keys() {
            if !pipeline.order.iter().any(|stage| stage.contains(image)) {
                return bad!(format!("image {} is not in this pipeline", image));
            }
        }
        // a pipeline reaction runs the pipeline's stages and records their tool instances
        let source = ReactionSource::Pipeline {
            name: pipeline.name.clone(),
            provenance,
        };
        // cast to a reaction with the pipeline's default sla
        let (cast, cache) = self
            .cast_source(
                user,
                Uuid::new_v4(),
                source,
                pipeline.sla,
                plan,
                parent_ephemeral,
                shared,
            )
            .await?;
        // let tests make a pipeline's reactions belong to a run
        #[cfg(feature = "test-utilities")]
        let cast = Reaction {
            run: crate::test_utilities::faults::reaction_run(&pipeline.name, cast.id),
            ..cast
        };
        Ok((cast, cache, pipeline))
    }

    /// Casts a reaction request to a bounds checked Reaction that runs a source
    ///
    /// Pipeline reactions, tool runs, and workflow runs are all cast here, so every reaction
    /// gets the input SHA256 and creator tags.
    ///
    /// # Arguments
    ///
    /// * `user` - The user casting this reaction request
    /// * `id` - The id the reaction gets
    /// * `source` - What the reaction runs
    /// * `default_sla` - The sla in seconds to use when the request sets none
    /// * `plan` - The stages resolved for this reaction
    /// * `parent_ephemeral` - Any ephemeral files from any parent reactions
    /// * `shared` - Shared Thorium objects
    #[allow(clippy::too_many_arguments)]
    #[instrument(name = "ReactionRequest::cast_source", skip_all, err(Debug))]
    pub async fn cast_source(
        mut self,
        user: &User,
        id: Uuid,
        source: ReactionSource,
        default_sla: u64,
        plan: ReactionPlan,
        parent_ephemeral: HashMap<String, Uuid>,
        shared: &Shared,
    ) -> Result<(Reaction, ReactionCache), ApiError> {
        // if no sla is given use the default
        let sla_seconds = self.sla.unwrap_or(default_sla);
        // bounds check sla
        bounder::number(sla_seconds as i64, "sla", 1, 3.154e+9 as i64)?;
        // build the repo dedendency objects
        let mut repos = Vec::with_capacity(self.repos.len());
        for req in self.repos {
            // try to get this repo to make sure this user actually has access
            let repo = Repo::get(user, &req.url, shared).await?;
            // get the commit we are going to be building against
            let commitish = match req.commitish {
                // the user specified a commit so just use that
                Some(commitish) => Some(commitish),
                // A commit wasn't specified so use the default checkout
                None => repo
                    .default_checkout
                    .map(|commitish| commitish.value().to_owned()),
            };
            // build a repo depdendency object and insert it
            repos.push(RepoDependency {
                url: req.url,
                commitish,
                kind: req.kind,
            });
        }
        // upload our extra files
        let ephemeral = Self::upload_files(&id, self.buffers, shared).await?;
        // automatically add the sha256 and submitter tags
        self.tags.append(&mut self.samples.clone());
        self.tags.push(user.username.clone());
        // create job instance
        let cast = Reaction {
            id,
            group: self.group,
            pipeline: self.pipeline,
            creator: user.username.clone(),
            status: ReactionStatus::Created,
            current_stage: 0,
            current_stage_progress: 0,
            current_stage_length: plan.step(0).map_or(0, <[PlannedStage]>::len) as u64,
            args: self.args,
            sla: Utc::now() + chrono::Duration::seconds(sla_seconds as i64),
            jobs: Vec::default(),
            tags: self.tags,
            parent: self.parent,
            sub_reactions: 0,
            completed_sub_reactions: 0,
            generators: Vec::default(),
            samples: self.samples,
            ephemeral,
            parent_ephemeral,
            repos,
            trigger_depth: self.trigger_depth,
            has_cache: !self.cache.is_empty(),
            source,
            plan,
            run_token: None,
            run: None,
        };
        Ok((cast, self.cache))
    }
}

impl ReactionList {
    /// Creates new reaction list object
    ///
    /// # Arguments
    ///
    /// * `cursor` - A cursor used to page to the next list of reactions
    /// * `names` - A list of reaction names
    pub(super) fn new(cursor: Option<usize>, names: Vec<String>) -> Self {
        ReactionList { cursor, names }
    }

    /// Turns a [`ReactionList`] into a [`ReactionDetailsList`]
    ///
    /// # Arguments
    ///
    /// * `group` - The group these reactions are from
    /// * `shared` - Shared Thorium objects
    pub(crate) async fn details(
        self,
        group: &str,
        shared: &Shared,
    ) -> Result<ReactionDetailsList, ApiError> {
        // use correct backend to list reaction details
        let details = db::reactions::list_details(group, &self.names, shared).await?;
        // cast to reaction details list
        let details_list = ReactionDetailsList::new(self.cursor, details);
        Ok(details_list)
    }
}

impl ReactionDetailsList {
    /// Creates a new reaction details list object
    ///
    /// # Arguments
    ///
    /// * `cursor` - A cursor used to page to the next list of reactions
    /// * `details` - A list of reaction details
    pub(super) fn new(cursor: Option<usize>, details: Vec<Reaction>) -> Self {
        ReactionDetailsList { cursor, details }
    }
}

impl Reaction {
    /// Creates a new reaction
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is creating this reaction
    /// * `group` - The group this reaction is in
    /// * `pipeline` - The pipeline this reaction is apart of
    /// * `request` - The `ReactionRequest` to build a reaction on
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Reactions::create", skip_all, fields(group = &group.name, pipeline = &pipeline.name), err(Debug))]
    pub async fn create(
        user: &User,
        group: &Group,
        pipeline: &Pipeline,
        request: ReactionRequest,
        shared: &Shared,
    ) -> Result<Reaction, ApiError> {
        // ensure that all args defined are contained in the pipeline
        for image in request.args.keys() {
            if !pipeline.order.iter().any(|stage| stage.contains(image)) {
                return bad!(format!("image {} is not in this pipeline", image));
            }
        }
        // make sure we can create reactions in this group
        group.allowable(GroupAllowAction::Reactions)?;
        // make sure we can create reactions in this group
        group.editable(user)?;
        // make sure we have access to any samples we are trying to create reactions for
        if !request.samples.is_empty() {
            // authorize this user has access to all the samples to pass in to this reaction
            Sample::authorize(user, &request.samples, shared).await?;
        }
        // resolve the tool instance each stage runs
        let mut memo = ToolInstanceMemo::default();
        let plan = stage_tools::resolve_plan(pipeline, &mut memo, shared).await?;
        // make sure we are allowed to override any args we try too
        request
            .can_override(user, group, &plan, &mut memo, shared)
            .await?;
        // record the tool instances the plan runs from the reads that resolved it
        let provenance = memo.provenance(&plan);
        // add reaction to backend, boxing its large future
        Box::pin(db::reactions::create(
            user, request, pipeline, plan, provenance, &mut memo, shared,
        ))
        .await
    }

    /// Creates a new reactions in bulk
    ///
    /// Every request is checked before anything is created: if any one request
    /// is refused (for example an argument override the user may not make, or a
    /// sample the user cannot read), the whole bulk request is refused and no
    /// reactions are created.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is creating this reaction
    /// * `requests` - The `ReactionRequests` to build reactions on
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Reactions::create_bulk", skip_all, err(Debug))]
    pub async fn create_bulk(
        user: &User,
        requests: Vec<ReactionRequest>,
        shared: &Shared,
    ) -> Result<BulkReactionResponse, ApiError> {
        // build cache of all different pipelines, their plans and provenance, and groups we are
        // creating reactions for
        let mut pipe_cache: HashMap<String, (Pipeline, ReactionPlan, RunProvenance)> =
            HashMap::with_capacity(1);
        let mut group_cache = HashMap::with_capacity(1);
        // read each tool instance at most once for this request
        let mut memo = ToolInstanceMemo::default();
        // build the map of stages we are overriding args for by pipeline
        let mut override_cache: HashMap<String, HashSet<&String>> = HashMap::default();
        for req in &requests {
            // build combined group + pipeline key
            let key = format!("{}:{}", req.group, req.pipeline);
            // get pipeline if we don't have it in the cache
            if let std::collections::hash_map::Entry::Vacant(e) = pipe_cache.entry(key.clone()) {
                // get this group and pipeline
                let (group, pipeline) =
                    Pipeline::get(user, &req.group, &req.pipeline, shared).await?;
                // resolve the tool instance each stage runs once per pipeline
                let plan = stage_tools::resolve_plan(&pipeline, &mut memo, shared).await?;
                // record the plan's tool instances once per pipeline from the reads above
                let provenance = memo.provenance(&plan);
                // insert into cache and set
                e.insert((pipeline, plan, provenance));
                // check if this group is also not in the cache
                group_cache.entry(&req.group).or_insert(group);
            }
            // get the stages this request overrides args for, including requests for a
            // pipeline that is already cached
            let overrides = req
                .args
                .iter()
                .filter(|(_, args)| {
                    args.opts.override_positionals
                        || args.opts.override_kwargs
                        || args.opts.override_cmd.is_some()
                })
                .map(|(name, _)| name)
                .collect::<Vec<&String>>();
            // if we are overriding args then add them to our map
            if !overrides.is_empty() {
                // get an entry to this pipeline's override set
                let entry = override_cache
                    .entry(key)
                    .or_insert_with(|| HashSet::with_capacity(overrides.len()));
                // add our stages
                entry.extend(overrides.into_iter());
            }
        }
        // make sure none of these pipelines are banned
        let banned_pipelines = pipe_cache
            .iter()
            .filter_map(|(key, (pipe, _, _))| (!pipe.bans.is_empty()).then_some(key))
            .collect::<Vec<&String>>();
        if !banned_pipelines.is_empty() {
            return bad!(format!(
                "Unable to create reaction(s)! The following pipelines have \
                one or more bans: '{banned_pipelines:?}'. See their notifications for details."
            ));
        }
        // make sure we can create reactions in all of these groups
        for group in group_cache.values() {
            // make sure we can create reactions in this group
            group.allowable(GroupAllowAction::Reactions)?;
            // make sure this group is editable
            group.editable(user)?;
        }
        // gather the samples of every request, each sample once
        let samples = requests
            .iter()
            .flat_map(|req| req.samples.iter().cloned())
            .collect::<HashSet<String>>()
            .into_iter()
            .collect::<Vec<String>>();
        // make sure we have access to every sample we are trying to create reactions for
        if !samples.is_empty() {
            // authorize this user has access to all the samples to pass in to these reactions
            Sample::authorize(user, &samples, shared).await?;
        }
        // make sure we can override args in each of the stages we try too
        for (key, stages) in override_cache {
            // get our pipeline, its plan, and its group from our caches
            let Some((pipeline, plan, _)) = pipe_cache.get(&key) else {
                return unauthorized!();
            };
            let Some(group) = group_cache.get(&pipeline.group) else {
                return unauthorized!();
            };
            // get the scalers of the overridden stages' tool instances
            let stages = stages.into_iter().collect::<Vec<&String>>();
            let scalers = override_scalers(&group.name, &stages, plan, &mut memo, shared).await?;
            // make sure we have the right developer roles
            if group.developer_many(user, &scalers).is_err() {
                return unauthorized!();
            }
        }
        // every request passed its checks, so create all of them
        let response = BulkReactionResponse::with_capacity(requests.len());
        let allowed = requests.into_iter().enumerate().collect();
        // add reaction to backend
        db::reactions::create_bulk(user, allowed, &pipe_cache, &mut memo, response, shared).await
    }

    /// Creates a new reactions in bulk for different users
    ///
    /// Reactions are not created for disabled users or for users whose bulk
    /// request is refused (for example a sample they can no longer read);
    /// their requests are rejected with per-user error entries instead of
    /// failing the whole request so bulk callers like the event handler can
    /// keep creating reactions for their other users. Server errors still
    /// fail the whole request.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is creating these reactions for others
    /// * `requests` - The `ReactionRequests` to build reactions on
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Reactions::create_bulk_by_user", skip_all, err(Debug))]
    pub async fn create_bulk_by_user(
        user: &User,
        requests: HashMap<String, Vec<ReactionRequest>>,
        shared: &Shared,
    ) -> Result<HashMap<String, BulkReactionResponse>, ApiError> {
        // only admins can create reactions for other users
        is_admin!(user);
        // build a map of reaction creation responses by user
        let mut resp = HashMap::with_capacity(requests.len());
        // create each users reactions
        for (username, reqs) in requests {
            // get this users info
            let other_user = User::force_get(&username, shared).await?;
            // don't create reactions for disabled users
            if other_user.role == UserRole::Disabled {
                // build a response with an error entry for every rejected request
                let mut user_resp = BulkReactionResponse::default();
                // add an error for each reaction request we are rejecting
                for index in 0..reqs.len() {
                    // reuse the disabled auth message so callers can match on it
                    user_resp
                        .errors
                        .insert(index, "This user has been disabled".to_owned());
                }
                // add this disabled users response
                resp.insert(username, user_resp);
                // move on to the next user without creating any reactions
                continue;
            }
            // keep how many requests this user made in case their bulk request is refused
            let req_count = reqs.len();
            // create this users reactions
            let user_resp = match Self::create_bulk(&other_user, reqs, shared).await {
                Ok(user_resp) => user_resp,
                // a refused bulk request creates nothing, so report it for this user alone
                Err(error) if error.status().is_client_error() => {
                    // only expose the message that is safe to show, or the status code otherwise
                    let msg = error
                        .public_msg()
                        .unwrap_or_else(|| error.status().to_string());
                    // build a response with an error entry for every refused request
                    let mut user_resp = BulkReactionResponse::default();
                    // add an error for each reaction request that was refused
                    for index in 0..req_count {
                        // add this request's error
                        user_resp.errors.insert(index, msg.clone());
                    }
                    user_resp
                }
                // fail the whole call on server errors so the caller can retry it
                Err(error) => return Err(error),
            };
            // add the responses for creating this users reactions
            resp.insert(username, user_resp);
        }
        Ok(resp)
    }

    /// Get the run-scoped token of a run's reaction
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is reading the run's token
    /// * `group` - The group the run's reaction is in
    /// * `id` - The id of the run's reaction
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Reaction::run_token", skip_all, fields(reaction = id.to_string()), err(Debug))]
    pub async fn run_token(
        user: &AuthedUser,
        group: &str,
        id: &Uuid,
        shared: &Shared,
    ) -> Result<ScopedToken, ApiError> {
        // only admins authenticated with their full token may read run tokens
        if user.is_scoped() {
            return unauthorized!();
        }
        is_admin!(user);
        // get the run's reaction
        let reaction = db::reactions::get(group, id, shared).await?;
        // read the run's live token from the user that owns it
        let owner = User::force_get(&reaction.creator, shared).await?;
        live_run_token(&reaction, &owner, shared).await
    }

    /// Get the models of a run's reaction with their credentials' secrets
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is reading the run's models
    /// * `group` - The group the run's reaction is in
    /// * `id` - The id of the run's reaction
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Reaction::run_models", skip_all, fields(reaction = id.to_string()), err(Debug))]
    pub async fn run_models(
        user: &AuthedUser,
        group: &str,
        id: &Uuid,
        shared: &Shared,
    ) -> Result<RunModels, ApiError> {
        // build the run's models from its recorded bindings
        models::run_models(user, group, id, shared).await
    }

    /// Gets a reaction object from the backend
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is getting this reaction
    /// * `group` - The group this reaction is in
    /// * `id` - The id of the reaction to retrieve
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Reactions::get", skip_all, fields(reaction = id.to_string()), err(Debug))]
    pub async fn get(
        user: &User,
        group: &str,
        id: &Uuid,
        shared: &Shared,
    ) -> Result<(Group, Self), ApiError> {
        // make sure we are a member of this group and it exists
        let group = Group::authorize(user, group, shared).await?;
        // get our reactions info from the db
        let reaction = db::reactions::get(&group.name, id, shared).await?;
        Ok((group, reaction))
    }

    /// Gets the status logs for a reaction
    ///
    /// # Arguments
    ///
    /// * `cursor` - The number of status logs to skip in the backend
    /// * `limit` - The max number of status logs to retrieve (strongly enforced)
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Reactions::logs", skip(self, shared), err(Debug))]
    pub async fn logs(
        &self,
        cursor: usize,
        limit: usize,
        shared: &Shared,
    ) -> Result<Vec<StatusUpdate>, ApiError> {
        // use correct backend to get reaction logs
        db::reactions::logs(self, cursor, limit, shared).await
    }

    /// Adds logs for a specific stage within a pipeline
    ///
    /// This is for stage logs not status logs for an entire reaction.
    ///
    /// # Arguments
    ///
    /// * `stage` - The stage these logs are from
    /// * `logs` - The logs to save into the backend
    /// * `run` - The reaction of the run the user's token is limited to, if it's a run token
    /// * `shared` - Shared objects in Thorium
    #[instrument(
        name = "Reactions::add_stage_logs",
        skip(self, shared, logs),
        err(Debug)
    )]
    pub async fn add_stage_logs(
        &self,
        stage: &str,
        logs: StageLogsAdd,
        run: Option<Uuid>,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        event!(Level::INFO, reaction = self.id.to_string());
        // make sure a run token only adds logs to its own run's reactions
        self.authorize_run(run)?;
        // use correct backend to get reaction logs
        db::reactions::add_stage_logs(&self.id, stage, logs, shared).await
    }

    /// Gets the stdout/stderr output from a specific stage with a cursor
    ///
    /// # Arguments
    ///
    /// * `stage` - The stage to retrieve logs from
    /// * `cursor` - The number of logs to skip in the backend
    /// * `limit` - The max number of logs to retrieve (strongly enforced)
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Reaction::stage_logs", skip(self, shared), err(Debug))]
    pub async fn stage_logs(
        &self,
        stage: &str,
        cursor: usize,
        limit: usize,
        shared: &Shared,
    ) -> Result<StageLogs, ApiError> {
        // use correct backend to get reaction logs
        db::reactions::stage_logs(self, stage, cursor, limit, shared).await
    }

    /// Lists reactions for a pipeline
    ///
    /// # Arguments
    ///
    /// * `pipeline` - The pipeline to list reactions from
    /// * `cursor` - The page of reactions to retrieve
    /// * `limit` - The max number of reactions to retrieve (weakly enforced)
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Reaction::list", skip(pipeline, shared), err(Debug))]
    pub async fn list(
        pipeline: &Pipeline,
        cursor: usize,
        limit: usize,
        shared: &Shared,
    ) -> Result<ReactionList, ApiError> {
        // use correct backend to list reaction names
        db::reactions::list(&pipeline.group, &pipeline.name, cursor, limit, shared).await
    }

    /// Lists reactions by a status
    ///
    /// # Arguments
    ///
    /// * `group` - The group to list reactions from
    /// * `pipeline` - The pipeline to list reactions from
    /// * `status` - The status of reactions to list
    /// * `cursor` - The page of reactions to retrieve
    /// * `limit` - The max number of reactions to retrieve (weakly enforced)
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Reaction::list_status", skip(group, pipeline, shared), fields(group = &group.name), err(Debug))]
    pub async fn list_status(
        group: &Group,
        pipeline: &Pipeline,
        status: &ReactionStatus,
        cursor: usize,
        limit: usize,
        shared: &Shared,
    ) -> Result<ReactionList, ApiError> {
        // use correct backend to list reaction names
        db::reactions::list_status(&group.name, &pipeline.name, status, cursor, limit, shared).await
    }

    /// Lists reactions by a tag
    ///
    /// # Arguments
    ///
    /// * `group` - The group to list reactions from
    /// * `tag` - The tag of reactions to list
    /// * `cursor` - The page of reactions to retrieve
    /// * `limit` - The max number of reactions to retrieve (weakly enforced)
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Reaction::list_tag", skip(group, shared), fields(group = &group.name), err(Debug))]
    pub async fn list_tag(
        group: &Group,
        tag: &str,
        cursor: usize,
        limit: usize,
        shared: &Shared,
    ) -> Result<ReactionList, ApiError> {
        // use correct backend to list reaction names
        db::reactions::list_tag(&group.name, tag, cursor, limit, shared).await
    }

    /// Lists reaction ids across groups with a redis cursor
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is listing reactions
    /// * `params` - The params to use when listing reactions
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Reaction::list_new", skip_all, err(Debug))]
    pub async fn list_new(
        user: &User,
        mut params: ReactionCursorParam,
        shared: &Shared,
    ) -> Result<ApiCursor<String>, ApiError> {
        // authorize the groups to list reactions from
        user.authorize_groups(params.mut_groups(), shared).await?;
        // use correct backend to list reaction names scoped to the requesting user
        db::reactions::list_new(user, params, shared).await
    }

    /// Lists reactions for an entire group with a set status
    ///
    /// # Arguments
    ///
    /// * `group` - The group to list reactions from
    /// * `status` - The status for this reaction
    /// * `cursor` - The page of reactions to retrieve
    /// * `limit` - The max number of reactions to retrieve (weakly enforced)
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Reaction::list_group_set", skip(group, shared), fields(group = &group.name), err(Debug))]
    pub async fn list_group_set(
        group: &Group,
        status: &ReactionStatus,
        cursor: usize,
        limit: usize,
        shared: &Shared,
    ) -> Result<ReactionList, ApiError> {
        // use correct backend to list reaction names
        db::reactions::list_group_set(&group.name, status, cursor, limit, shared).await
    }

    /// Lists sub reactions for a reaction
    ///
    /// # Arguments
    ///
    /// * `reaction` - The reaction to list subreactions for
    /// * `cursor` - The page of reactions to retrieve
    /// * `limit` - The max number of reactions to retrieve (weakly enforced)
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Reaction::list_sub", skip(reaction, shared), err(Debug))]
    pub async fn list_sub(
        reaction: &Reaction,
        cursor: usize,
        limit: usize,
        shared: &Shared,
    ) -> Result<ReactionList, ApiError> {
        // use correct backend to list reaction names
        db::reactions::list_sub(&reaction.group, &reaction.id, cursor, limit, shared).await
    }

    /// Lists sub reactions for a reaction by status
    ///
    /// # Arguments
    ///
    /// * `reaction` - The reaction to list subreactions for
    /// * `status` - The status of subreactions to list
    /// * `cursor` - The page of reactions to retrieve
    /// * `limit` - The max number of reactions to retrieve (weakly enforced)
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Reaction::list_sub_status", skip(reaction, shared), err(Debug))]
    pub async fn list_sub_status(
        reaction: &Reaction,
        status: &ReactionStatus,
        cursor: usize,
        limit: usize,
        shared: &Shared,
    ) -> Result<ReactionList, ApiError> {
        // use correct backend to list reaction names
        db::reactions::list_sub_status(&reaction.group, &reaction.id, status, cursor, limit, shared)
            .await
    }

    /// lists all jobs in a reaction
    ///
    /// # Arguments
    ///
    /// * `cursor` - The page of jobs to retrieve
    /// * `limit` - The max number of jobs to retrieve (weakly enforced)
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Reaction::list_jobs", skip(self, shared), err(Debug))]
    pub async fn list_jobs(
        &self,
        cursor: usize,
        limit: usize,
        shared: &Shared,
    ) -> Result<JobList, ApiError> {
        // use correct backend to list reaction names
        db::reactions::list_jobs(self, cursor, limit, shared).await
    }

    /// Check that a run token only changes the reactions of its own run
    ///
    /// A run token may act in every group it holds, so the group checks alone would let it change
    /// any reaction of its runner there; it also needs the reaction to be its run or one of the
    /// run's sub reactions.
    ///
    /// # Arguments
    ///
    /// * `run` - The reaction of the run the user's token is limited to, if it's a run token
    fn authorize_run(&self, run: Option<Uuid>) -> Result<(), ApiError> {
        // a run token may only change the reactions of its own run
        if let Some(run) = run
            && self.run != Some(run)
        {
            return unauthorized!();
        }
        Ok(())
    }

    /// Proceeds with the reaction
    ///
    /// This increments the current stage and either completes the reaction or creates
    /// jobs for that stage.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is proceeding with the reaction
    /// * `group` - The group this reaction is in
    /// * `run` - The reaction of the run the user's token is limited to, if it's a run token
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Reaction::proceed", skip_all, err(Debug))]
    pub async fn proceed(
        self,
        user: &User,
        group: &Group,
        run: Option<Uuid>,
        shared: &Shared,
    ) -> Result<ReactionStatus, ApiError> {
        // make sure we can modify reactions in this group
        can_modify!(self.creator, group, user);
        // make sure a run token only proceeds its own run's reactions
        self.authorize_run(run)?;
        // use correct backend for proceeding with this reaction
        let status = db::reactions::proceed(self, shared).await?;
        // cast this JobHandleStatus to a reaction status
        Ok(ReactionStatus::from(status))
    }

    /// Errors a reaction out
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is erroring out this reaction
    /// * `group` - The group this reaction is in
    /// * `run` - The reaction of the run the user's token is limited to, if it's a run token
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Reaction::fail", skip_all, err(Debug))]
    pub async fn fail(
        self,
        user: &User,
        group: &Group,
        run: Option<Uuid>,
        shared: &Shared,
    ) -> Result<ReactionStatus, ApiError> {
        // make sure we can modify reactions in this group
        can_modify!(self.creator, group, user);
        // make sure a run token only fails its own run's reactions
        self.authorize_run(run)?;
        // use correct backend for failing this reaction
        db::reactions::fail(self, shared).await
    }

    /// Deletes a reaction from the backend
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is deleting this reaction
    /// * `group` - The group this reaction is in
    /// * `run` - The reaction of the run the user's token is limited to, if it's a run token
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Reaction::delete", skip_all, err(Debug))]
    pub async fn delete(
        &self,
        user: &User,
        group: &Group,
        run: Option<Uuid>,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // Create our span
        span!(Level::INFO, "Deleting Reaction");
        // make sure we can modify reactions in this group
        can_delete!(self, group, user);
        // make sure a run token only deletes its own run's reactions
        self.authorize_run(run)?;
        // use correct backend for deleteing this reaction
        db::reactions::delete(self, shared).await
    }

    /// Deletes all reactions in a pipeline from the backend
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is deleting all reactions from a pipeline
    /// * `group` - The group the pipeline to delete reactions from is in
    /// * `pipeline` - The pipeline to delete reactions for
    /// * `skip_check` - skip checking if this user can delete these reactions
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Reaction::delete_all", skip_all, err(Debug))]
    pub async fn delete_all(
        user: &User,
        group: &Group,
        pipeline: &Pipeline,
        skip_check: bool,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // Create our span
        span!(Level::INFO, "Deleting All Reactions For Pipeline");
        // skip the perms check if we are sure this user is already able to delete this pipelines reactions
        if !skip_check {
            // make sure we are an owner of this group
            group.modifiable(user)?;
        }
        // use correct backend for deleting all reactions
        db::reactions::delete_all(user, group, pipeline, shared).await
    }

    /// Cleans up expired reactions in the reaction status list
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is deleting all reactions from a pipeline
    /// * `shared` - Shared objects in Thorium
    pub async fn expire_lists(user: &User, shared: &Shared) -> Result<(), ApiError> {
        // only admins can clean up reaction lists
        is_admin!(user);
        // use correct backend for proceeding with reaction
        db::reactions::expire_lists(shared).await
    }

    /// Updates the arguments for later stages of this [`Reaction`]
    ///
    /// Replacing an ephemeral file the reaction already lists needs the Developer role for the
    /// scaler of every stage whose options override its command, positionals, or kwargs.
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is updating this reaction
    /// * `group` - The group this reaction is in
    /// * `update` - The update to apply to this reaction
    /// * `run` - The reaction of the run the user's token is limited to, if it's a run token
    /// * `shared` - Shared objects in Thorium
    #[instrument(name = "Reaction::update", skip_all, err(Debug))]
    pub async fn update(
        mut self,
        user: &User,
        group: &Group,
        mut update: ReactionUpdate,
        run: Option<Uuid>,
        shared: &Shared,
    ) -> Result<Reaction, ApiError> {
        // make sure we can edit/create reactions in this group
        group.allowable(GroupAllowAction::Reactions)?;
        // make sure this user can modify this reaction
        can_modify!(self.creator, group, user);
        // make sure a run token only updates its own run's reactions
        self.authorize_run(run)?;
        // get the options this update sets that differ from each stage's current options
        let changed = update
            .args
            .iter()
            .filter_map(|(stage, args)| args.opts.as_ref().map(|opts| (stage, opts)))
            .filter(|(stage, opts)| self.args.get(*stage).map(|args| &args.opts) != Some(*opts));
        // make sure we may override args for any stage whose options this update changes
        let mut memo = ToolInstanceMemo::default();
        can_override_opts(user, group, changed, &self.plan, &mut memo, shared).await?;
        // apply all updates to the args in this reaction
        for (stage, mut args) in update.args.drain() {
            // get args entry or insert a default one
            let entry = self.args.entry(stage).or_default();
            // apply our updates
            // if any positionals were set overwrite the original ones
            if !args.positionals.is_empty() {
                entry.positionals = std::mem::take(&mut args.positionals);
            }
            // remove any kwargs requested to be removed
            entry
                .kwargs
                .retain(|key, _| !args.remove_kwargs.contains(key));
            // overlay any new kwargs ontop of the originals
            for (key, value) in args.kwargs {
                entry.kwargs.insert(key, value);
            }
            // remove any requested switches
            entry
                .switches
                .retain(|val| !args.remove_switches.contains(val));
            // inject new switches
            entry.switches.extend(args.add_switches);
            // overwrite our options if any have been set
            if let Some(opts) = args.opts.take() {
                entry.opts = opts;
            }
        }
        // update our reaction
        self.tags.retain(|tag| !update.remove_tags.contains(tag));
        // add new tags
        self.tags.extend(update.add_tags.clone());
        // a run's generated files are written only when the run is created, so a harness never
        // reads files someone else wrote under its run's name
        let generated = [
            run_files::RUN_FILE,
            run_files::AGENT_FILE,
            run_files::TOOLS_FILE,
        ];
        // refuse an update that writes any of them to a workflow run or tool run
        if !matches!(self.source, ReactionSource::Pipeline { .. })
            && update
                .ephemeral
                .keys()
                .any(|name| generated.contains(&name.as_str()))
        {
            return unauthorized!(format!(
                "a run's {} files can't be written by an update",
                generated.join(", ")
            ));
        }
        // a file this update replaces is one the reaction already lists
        let replaces = update
            .ephemeral
            .keys()
            .any(|name| self.ephemeral.contains(name));
        // replacing a file may swap the script a stage's override command runs, so it needs the
        // Developer role that setting those overrides needs
        if replaces {
            // get the options of every stage after this update's changes
            let stored = self.args.iter().map(|(stage, args)| (stage, &args.opts));
            // make sure we may override args for every stage whose options override them
            can_override_opts(user, group, stored, &self.plan, &mut memo, shared).await?;
        }
        // upload any new ephemeral files and add them to our reaction
        let paths = ReactionRequest::upload_files(&self.id, update.ephemeral, shared).await?;
        // files that replace an existing file are already listed so only add new names
        for path in paths {
            if !self.ephemeral.contains(&path) {
                self.ephemeral.push(path);
            }
        }
        // apply our updates to the reaction data in the backend
        db::reactions::update(&self, &update.add_tags, &update.remove_tags, shared).await?;
        Ok(self)
    }

    /// Downloads an ephemeral file tied to a reaction
    ///
    /// # Arguments
    ///
    /// * `name` - The name of the ephemeral file to download
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Reaction::download_ephemeral", skip(self, shared), err(Debug))]
    pub async fn download_ephemeral(
        &self,
        name: &str,
        shared: &Shared,
    ) -> Result<ByteStream, ApiError> {
        // make sure this is a valid ephemeral file for this reaction
        if self.ephemeral.iter().any(|value| value == name) {
            // build the path to save this file to in s3
            let path = format!("{}/{}", self.id, name);
            // download this attachment
            return shared.s3.ephemeral.download(&path).await;
        }
        not_found!(format!(
            "Ephemeral file {} not found for reaction {}",
            name, self.id
        ))
    }

    /// Get this reactions cache
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Reaction::get_cache", skip_all, err(Debug))]
    pub async fn get_cache(&self, shared: &Shared) -> Result<ReactionCache, ApiError> {
        // get this reactions cache if it has one
        db::reactions::get_cache(&self.id, shared).await
    }

    /// Update this reactions cache
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is updating this reactions cache
    /// * `group` - The group this reaction is in
    /// * `update` - The update to apply to this reactions cache
    /// * `run` - The reaction of the run the user's token is limited to, if it's a run token
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "ReactionRequest::update_cache", skip_all, err(Debug))]
    pub async fn update_cache(
        &self,
        user: &User,
        group: &Group,
        update: &ReactionCacheUpdate,
        run: Option<Uuid>,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // make sure we can edit/create reactions in this group
        group.allowable(GroupAllowAction::Reactions)?;
        // make sure this user can modify this reaction
        can_modify!(self.creator, group, user);
        // make sure a run token only writes its own run's caches
        self.authorize_run(run)?;
        // update this reactions cache
        db::reactions::update_cache(&self.group, &self.id, update, shared).await
    }

    /// Help upload files from a reaction cache file update form
    #[instrument(name = "ReactionRequest::add_cache_files_helper", skip_all, err(Debug))]
    pub async fn add_cache_files_helper(
        &self,
        mut form: Multipart,
        update: &mut InternalReactionCacheFileUpdates,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // handle data fields until this multipart form is complete
        while let Some(field) = form.next_field().await? {
            // handle this field based on its name
            let (s3_path, fname) = match (field.name(), field.file_name()) {
                // build the path to save this file to s3 at
                (Some("files"), Some(fname)) => {
                    (format!("{}/files/{fname}", self.id), fname.to_owned())
                }
                // error on unknown field names
                (Some(name), Some(_)) => return bad!(format!("Unknown field name: {name}")),
                // error on fields that have filenames but not a field name
                (None, Some(_)) => return bad!("Fields must have a field name".to_owned()),
                // error on any fields that don't have filenames or we don't know about
                (_, None) => return bad!("Fields must have a file name!".to_owned()),
            };
            // write this file to s3 and cart it
            shared
                .s3
                .reaction_cache
                .cart_and_stream(&s3_path, field)
                .await?;
            // add this uploaded file path to our tracking vec so we can clean up if needed
            update.added.push(fname);
        }
        Ok(())
    }

    /// Add a file to our reaction cache
    ///
    /// # Arguments
    ///
    /// * `user` - The user that is adding files to this reactions cache
    /// * `group` - The group this reaction is in
    /// * `form` - The form containing the files to add
    /// * `run` - The reaction of the run the user's token is limited to, if it's a run token
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "ReactionRequest::add_cache_files", skip_all, err(Debug))]
    pub async fn add_cache_files(
        &self,
        user: &User,
        group: &Group,
        form: Multipart,
        run: Option<Uuid>,
        shared: &Shared,
    ) -> Result<(), ApiError> {
        // make sure we can edit/create reactions in this group
        group.allowable(GroupAllowAction::Reactions)?;
        // make sure this user can modify this reaction
        can_modify!(self.creator, group, user);
        // make sure a run token only writes its own run's caches
        self.authorize_run(run)?;
        // get our current cache info
        let old = self.get_cache(shared).await?;
        // start with an empty update and add files to it as we go
        let mut update = InternalReactionCacheFileUpdates::with_capacity(1);
        // upload this requests cache files
        match self.add_cache_files_helper(form, &mut update, shared).await {
            Ok(()) => {
                // update our cache info
                db::reactions::update_cache_files(&self.group, &self.id, &update, shared).await?;
                // delete any of our old cache files from s3 in bulk
                stream::iter(update.deleted)
                    .map(|s3_path| async move { shared.s3.reaction_cache.delete(&s3_path).await })
                    .buffer_unordered(10)
                    .collect::<Vec<Result<(), ApiError>>>()
                    .await
                    .into_iter()
                    .collect::<Result<Vec<()>, ApiError>>()?;
                Ok(())
            }
            Err(error) => {
                // an error occured so delete any cache files we already uploaded
                // skip any paths that were not already in our update for partially applied updates
                // we don't delete existing data because we have already overwritten the old cache data
                // we could use s3_ids for this instead but those are stored in scylla and the short term
                // nature of these files make them unsuitable for storing in scylla
                let only_new = update
                    .added
                    .into_iter()
                    .filter(|s3_path| old.files.contains(s3_path));
                // delete any of our old cache files from s3 in bulk
                stream::iter(only_new)
                    .map(|s3_path| async move { shared.s3.reaction_cache.delete(&s3_path).await })
                    .buffer_unordered(10)
                    .collect::<Vec<Result<(), ApiError>>>()
                    .await
                    .into_iter()
                    .collect::<Result<Vec<()>, ApiError>>()?;
                // reraise our error
                Err(error)
            }
        }
    }

    /// Downloads a cache file tied to a reaction
    ///
    /// # Arguments
    ///
    /// * `file_path` - The path of the cache file to download
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Reaction::download_cache_file", skip(self, shared), err(Debug))]
    pub async fn download_cache_file(
        &self,
        file_path: &String,
        shared: &Shared,
    ) -> Result<ByteStream, ApiError> {
        // get our current cache
        let cache = self.get_cache(shared).await?;
        // make sure this file is in our cache
        if !cache.files.contains(file_path) {
            return not_found!(format!("Cache file {file_path} not found for {}", self.id));
        }
        // build the path to this cache file since we know it exists
        let s3_path = format!("{}/files/{file_path}", self.id);
        // download this attachment
        shared.s3.reaction_cache.download(&s3_path).await
    }
}

/// The parameters for listing reactions in redis using a cursor
#[derive(Debug)]
pub enum ReactionCursorParam {
    /// List reactions across all groups
    General {
        /// The pipeline to list reactions for
        pipeline: String,
        /// The shared params for listing reactions with a cursor
        params: ReactionCursorParams,
    },
    /// List reactions by status
    Status {
        /// The pipeline to list reactions for
        pipeline: String,
        /// The status that listed reactions must have
        status: ReactionStatus,
        /// The shared params for listing reactions with a cursor
        params: ReactionCursorParams,
    },
    /// List reactions by tag
    Tag {
        /// The tag that listed reactions must have
        tag: String,
        /// The shared params for listing reactions with a cursor
        params: ReactionCursorParams,
    },
    /// List sub reactions
    Sub {
        /// The parent reaction to list sub reactions for
        sub: Uuid,
        /// The shared params for listing reactions with a cursor
        params: ReactionCursorParams,
    },
    /// List sub reactions with a status
    SubAndStatus {
        /// The parent reaction to list sub reactions for
        sub: Uuid,
        /// The status that listed sub reactions must have
        status: ReactionStatus,
        /// The shared params for listing reactions with a cursor
        params: ReactionCursorParams,
    },
}

impl ReactionCursorParam {
    /// Get a mutable reference to the groups to return data from
    pub fn mut_groups(&mut self) -> &mut Vec<String> {
        match self {
            ReactionCursorParam::General { params, .. } => &mut params.groups,
            ReactionCursorParam::Status { params, .. } => &mut params.groups,
            ReactionCursorParam::Tag { params, .. } => &mut params.groups,
            ReactionCursorParam::Sub { params, .. } => &mut params.groups,
            ReactionCursorParam::SubAndStatus { params, .. } => &mut params.groups,
        }
    }
}

impl<S> FromRequestParts<S> for ReactionCursorParams
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // try to extract our query
        if let Some(query) = parts.uri.query() {
            // try to deserialize our query string
            Ok(serde_qs::Config::new()
                .max_depth(5)
                .deserialize_str(query)?)
        } else {
            Ok(Self::default())
        }
    }
}

/// This isn't a `TryFrom` because the error type would have to implement `Deserialize`
impl ReactionExpire {
    pub(super) fn cast(raw: &str) -> Result<Self, ApiError> {
        // Ok wrap this to get around error not be deserializable
        let expire = deserialize!(raw);
        Ok(expire)
    }
}

impl TryFrom<(HashMap<String, String>, Vec<String>, Vec<String>)> for Reaction {
    type Error = ApiError;

    /// Try to cast a `HashMap` of strings and a vector of strings into a Reaction
    ///
    /// # Arguments
    ///
    /// * `raw` - The `HashMap` and strings to cast into a Reaction
    fn try_from(
        raw: (HashMap<String, String>, Vec<String>, Vec<String>),
    ) -> Result<Self, Self::Error> {
        // unwrap into hashmap and vector of job ids
        let (mut map, raw_jobs, raw_generators) = raw;
        // return 404 if hashmap is empty
        if map.is_empty() {
            return not_found!("reaction not found".to_string());
        }

        // cast jobs to uuids
        let jobs = raw_jobs
            .iter()
            .map(|id| Uuid::parse_str(id))
            .filter_map(Result::ok)
            .collect();

        // cast generators to uuids
        let generators = raw_generators
            .iter()
            .map(|id| Uuid::parse_str(id))
            .filter_map(Result::ok)
            .collect();

        // get the pipeline or run label of this reaction
        let pipeline: String = extract!(map, "pipeline");
        // reactions stored before sources existed are pipeline reactions
        let source = deserialize_ext!(
            map,
            "source",
            ReactionSource::Pipeline {
                name: pipeline.clone(),
                provenance: RunProvenance::default(),
            }
        );
        // cast to a Reaction
        let reaction = Reaction {
            id: Uuid::parse_str(&extract!(map, "id"))?,
            group: extract!(map, "group"),
            pipeline,
            creator: extract!(map, "creator"),
            status: deserialize_ext!(map, "status"),
            current_stage: extract!(map, "current_stage").parse::<u64>()?,
            current_stage_progress: extract!(map, "current_stage_progress").parse::<u64>()?,
            current_stage_length: extract!(map, "current_stage_length").parse::<u64>()?,
            args: deserialize_ext!(map, "args"),
            sla: deserialize_ext!(map, "sla"),
            tags: deserialize_ext!(map, "tags"),
            jobs,
            parent: deserialize_opt!(map, "parent"),
            sub_reactions: extract!(map, "sub_reactions", "0".to_owned()).parse::<u64>()?,
            completed_sub_reactions: extract!(map, "completed_sub_reactions").parse::<u64>()?,
            generators,
            samples: deserialize_ext!(map, "samples", Vec::default()),
            ephemeral: deserialize_ext!(map, "ephemeral", Vec::default()),
            parent_ephemeral: deserialize_ext!(map, "parent_ephemeral", HashMap::default()),
            repos: deserialize_ext!(map, "repos", Vec::default()),
            trigger_depth: deserialize_opt!(map, "trigger_depth"),
            has_cache: deserialize_ext!(map, "has_cache", false),
            source,
            plan: deserialize_ext!(map, "plan", ReactionPlan::default()),
            run_token: map.remove("run_token"),
            run: deserialize_opt!(map, "run"),
        };
        Ok(reaction)
    }
}

/// Read the live run token of a run's reaction or of one of its sub reactions
///
/// The token is looked up by the name the reaction records, and a token under that name that
/// isn't scoped to the reaction's run (one its owner created after deleting the run's token) is
/// treated as missing, so it's never handed to the run's workers. A token whose owner can't edit
/// in the reaction's group is treated as missing too, since every call made with it is refused.
///
/// # Arguments
///
/// * `reaction` - The reaction whose run token to read
/// * `owner` - The user that created the reaction and owns its run token
/// * `shared` - Shared Thorium objects
pub(crate) async fn live_run_token(
    reaction: &Reaction,
    owner: &User,
    shared: &Shared,
) -> Result<ScopedToken, ApiError> {
    // a reaction without a run token has none to read
    let Some(name) = &reaction.run_token else {
        return not_found!(format!("reaction {} has no run token", reaction.id));
    };
    // read the token from the user that owns it
    let token = ScopedToken::get(owner, name, shared).await?;
    // only a token scoped to this reaction's run is its run token
    if reaction.run.is_none() || token.run.as_ref().map(|run| run.reaction) != reaction.run {
        return not_found!(format!("reaction {} has no run token", reaction.id));
    }
    // a token whose owner can no longer edit in the run group can't do anything for the run
    let group = db::groups::get(&reaction.group, shared).await?;
    if group.editable(owner).is_err() {
        return not_found!(format!(
            "reaction {} has no usable run token: its owner can't edit in {}",
            reaction.id, reaction.group
        ));
    }
    Ok(token)
}

/// Get the scalers of the tool instances of the stages a reaction overrides args for
///
/// # Arguments
///
/// * `group` - The group of the reaction
/// * `stages` - The stages whose args are overridden
/// * `plan` - The reaction's resolved plan
/// * `memo` - The tool instances already read for this request
/// * `shared` - Shared Thorium objects
async fn override_scalers(
    group: &str,
    stages: &[&String],
    plan: &ReactionPlan,
    memo: &mut ToolInstanceMemo,
    shared: &Shared,
) -> Result<Vec<ImageScaler>, ApiError> {
    // read the overridden stages' tool instances in one batch
    let instances = stages
        .iter()
        .filter_map(|stage| plan.tool_instance(stage))
        .collect::<Vec<Uuid>>();
    memo.prefetch(group, &instances, shared).await?;
    // get the scaler of each overridden stage's tool instance
    let mut scalers = Vec::with_capacity(stages.len());
    for stage in stages {
        // every overridden stage must be in the plan
        let Some(instance) = plan.tool_instance(stage) else {
            return bad!(format!("image {stage} is not in this pipeline"));
        };
        let info = memo.job_info(group, stage, instance, shared).await?;
        scalers.push(info.scaler);
    }
    Ok(scalers)
}

/// Explain why a user can't run in a group, if they can't
///
/// # Arguments
///
/// * `user` - The user starting the run
/// * `group` - The group to check
/// * `scalers` - The scalers of every workload the run may spawn
fn run_group_refusal(user: &User, group: &Group, scalers: &[ImageScaler]) -> Option<String> {
    // a non-admin must hold this group in their (possibly scoped) groups and be able to edit in it
    if !(user.is_admin() || user.groups.contains(&group.name)) || group.editable(user).is_err() {
        return Some("you can't edit in it".to_owned());
    }
    // the group must allow reactions
    if !group.allowed.is_allowed(GroupAllowAction::Reactions) {
        return Some("it doesn't allow reactions".to_owned());
    }
    // the user must be a developer for every scaler
    let missing = scalers
        .iter()
        .filter(|scaler| !user.is_developer(**scaler))
        .map(ToString::to_string)
        .collect::<Vec<String>>();
    if !missing.is_empty() {
        return Some(format!("you aren't a developer for {}", missing.join(", ")));
    }
    None
}

/// Choose the group a tool or workflow run happens in
///
/// A requested group must be one of the instance's groups where the user can edit, reactions are
/// allowed, and the user is a developer for every scaler. Requesting a group the user isn't in is
/// a plain `401`, whether or not the instance is in it. Without a request the single such group
/// is used; none is a `401` with a reason per group the user can see, and several is a `400`
/// listing them.
///
/// # Arguments
///
/// * `user` - The user starting the run
/// * `groups` - The details of every group of the Tool Instance or Workflow Instance
/// * `requested` - The run group the user asked for, if any
/// * `scalers` - The scalers of every workload the run may spawn
pub fn choose_run_group(
    user: &User,
    groups: &[Group],
    requested: Option<&str>,
    scalers: &[ImageScaler],
) -> Result<String, ApiError> {
    // check a requested group directly
    if let Some(requested) = requested {
        // a runner outside the requested group gets one plain answer whether or not the
        // instance is in it, so the answer never reveals the instance's hidden groups
        if !(user.is_admin() || user.groups.iter().any(|group| group == requested)) {
            return unauthorized!(format!("you can't run in `{requested}`"));
        }
        // the requested group must be one of the instance's
        let Some(group) = groups.iter().find(|group| group.name == requested) else {
            return bad!(format!("`{requested}` isn't one of this instance's groups"));
        };
        // the user must be able to run in it
        return match run_group_refusal(user, group, scalers) {
            Some(reason) => unauthorized!(format!("you can't run in `{requested}`: {reason}")),
            None => Ok(requested.to_owned()),
        };
    }
    // split the instance's groups into candidates and refusals
    let mut candidates = Vec::new();
    let mut refusals = Vec::new();
    for group in groups {
        match run_group_refusal(user, group, scalers) {
            // this group can be used
            None => candidates.push(group.name.clone()),
            // only explain refusals in groups this user can see
            Some(reason) => {
                if (user.is_admin() || user.groups.contains(&group.name))
                    && group.viewable(user).is_ok()
                {
                    refusals.push(format!("`{}`: {reason}", group.name));
                }
            }
        }
    }
    // pick the single candidate or explain why none or several match
    match candidates.len() {
        1 => Ok(candidates.remove(0)),
        0 if refusals.is_empty() => {
            unauthorized!("you can't run in any of this instance's groups".to_owned())
        }
        0 => unauthorized!(format!(
            "you can't run in any of this instance's groups: {}",
            refusals.join("; ")
        )),
        _ => {
            // quote each candidate for the message
            let quoted = candidates
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<String>>();
            bad!(format!(
                "several run groups are possible, pick one of: {}",
                quoted.join(", ")
            ))
        }
    }
}

/// Load an instance's groups and choose the group a run happens in
///
/// # Arguments
///
/// * `user` - The user starting the run
/// * `instance_groups` - The groups of the Tool Instance or Workflow Instance
/// * `requested` - The run group the user asked for, if any
/// * `scalers` - The scalers of every workload the run may spawn
/// * `shared` - Shared Thorium objects
#[instrument(name = "reactions::select_run_group", skip(user, shared), err(Debug))]
pub async fn select_run_group(
    user: &User,
    instance_groups: &[String],
    requested: Option<&str>,
    scalers: &[ImageScaler],
    shared: &Shared,
) -> Result<String, ApiError> {
    // get the details of every instance group
    let groups = db::groups::list_details(instance_groups.iter(), shared).await?;
    // choose the run group from them
    choose_run_group(user, &groups, requested, scalers)
}

/// The tag every workflow run's reaction is listed under
const WORKFLOW_RUN_TAG: &str = "WorkflowRun";

/// Get the name of a tool from the name of one of its instances (`{tool}@{version}`)
///
/// # Arguments
///
/// * `entity` - The tool instance entity
/// * `instance` - The tool instance's metadata
fn tool_name(entity: &Entity, instance: &ToolInstanceEntity) -> String {
    // instances are named after their tool and version
    let suffix = format!("@{}", instance.version);
    entity
        .name
        .strip_suffix(&suffix)
        .unwrap_or(&entity.name)
        .to_owned()
}

/// Get the tool instance metadata of an entity, if it's a tool instance
///
/// # Arguments
///
/// * `entity` - The entity to read
fn instance_meta(entity: &Entity) -> Option<&ToolInstanceEntity> {
    // only tool instances have tool instance metadata
    match &entity.metadata {
        EntityMetadata::ToolInstance(instance) => Some(instance),
        _ => None,
    }
}

/// Get the groups of a list that a user can read reactions in
///
/// # Arguments
///
/// * `user` - The user reading reactions
/// * `groups` - The groups to filter
fn readable_groups<'a>(user: &User, groups: &'a [String]) -> Vec<&'a String> {
    // admins read every group and other users read the groups they're in
    groups
        .iter()
        .filter(|group| user.is_admin() || user.groups.contains(*group))
        .collect()
}

/// Get an entity a user starts a run of, as a 404 when the user can't see it
///
/// # Arguments
///
/// * `user` - The user starting the run
/// * `id` - The id of the entity
/// * `shared` - Shared Thorium objects
async fn run_entity(user: &User, id: Uuid, shared: &Shared) -> Result<Entity, ApiError> {
    // a user in no groups can't see any entity
    if !user.is_admin() && user.groups.is_empty() {
        return not_found!(format!("{id} not found"));
    }
    // the entity must exist in one of the user's groups
    Entity::get(user, id, shared).await
}

/// Find a reaction by id in one of a list of groups
///
/// # Arguments
///
/// * `user` - The user looking for the reaction
/// * `groups` - The groups the reaction may be in
/// * `id` - The id of the reaction
/// * `shared` - Shared Thorium objects
async fn find_reaction(
    user: &User,
    groups: &[String],
    id: &Uuid,
    shared: &Shared,
) -> Result<Reaction, ApiError> {
    // check each group this user can read for the reaction
    for group in readable_groups(user, groups) {
        match db::reactions::get(group, id, shared).await {
            // this group holds the reaction
            Ok(reaction) => return Ok(reaction),
            // a missing reaction in this group just means it's in another one
            Err(error) if error.status() == axum::http::StatusCode::NOT_FOUND => (),
            // any other error is a failed read, not a missing reaction
            Err(error) => return Err(error),
        }
    }
    not_found!(format!("reaction {id} not found"))
}

/// Resolve the repos of a run to the commitish each one is checked out at
///
/// # Arguments
///
/// * `user` - The user starting the run
/// * `repos` - The repos the run was given
/// * `shared` - Shared Thorium objects
async fn resolve_repos(
    user: &User,
    repos: &[RepoDependencyRequest],
    shared: &Shared,
) -> Result<Vec<RepoDependency>, ApiError> {
    // resolve each repo the same way a reaction cast does
    let mut resolved = Vec::with_capacity(repos.len());
    for req in repos {
        // make sure the repo exists and this user can see it
        let repo = Repo::get(user, &req.url, shared).await?;
        // use the requested commitish or the repo's default checkout
        let commitish = req.commitish.clone().or_else(|| {
            repo.default_checkout
                .map(|commitish| commitish.value().to_owned())
        });
        resolved.push(RepoDependency {
            url: req.url.clone(),
            commitish,
            kind: req.kind,
        });
    }
    Ok(resolved)
}

/// Encode a run file as a reaction buffer
///
/// # Arguments
///
/// * `value` - The run file to encode
fn buffer<T: serde::Serialize>(value: &T) -> Result<String, ApiError> {
    // buffers are uploaded base64 encoded
    let raw = serde_json::to_vec_pretty(value)?;
    Ok(base64::engine::general_purpose::STANDARD.encode(raw))
}

/// Get the groups a run's token is limited to
///
/// The token holds the run group plus every group of the run's inputs that the runner can see,
/// limited to the groups the runner is a member of (an admin's actual groups), so an admin's or
/// analyst's view of every group never widens it. The run group is never dropped, so a runner
/// who isn't a member of it is refused when the token is minted.
///
/// # Arguments
///
/// * `user` - The user starting the run
/// * `group` - The run group
/// * `samples` - The run's input files
/// * `repos` - The run's input repos
/// * `shared` - Shared Thorium objects
async fn run_token_groups(
    user: &User,
    group: &str,
    samples: &[String],
    repos: &[RepoDependency],
    shared: &Shared,
) -> Result<Vec<String>, ApiError> {
    // admins hold every group, so only their actual groups can scope a token
    let members = if user.role == UserRole::Admin {
        &user.actual_groups
    } else {
        &user.groups
    };
    // the run group is always in the token
    let mut groups = std::collections::BTreeSet::from([group.to_owned()]);
    // add the groups of each input file the runner can see and is a member of
    for sha256 in samples {
        let sample = Sample::get(user, sha256, shared).await?;
        groups.extend(
            sample
                .groups()
                .into_iter()
                .filter(|group| members.iter().any(|member| member == group))
                .map(ToOwned::to_owned),
        );
    }
    // add the groups of each input repo the runner can see and is a member of
    for dependency in repos {
        let repo = Repo::get(user, &dependency.url, shared).await?;
        groups.extend(
            repo.groups()
                .into_iter()
                .filter(|group| members.contains(group))
                .cloned(),
        );
    }
    // the set is already deduplicated and sorted
    Ok(groups.into_iter().collect())
}

/// Work out the SLA in seconds of a run that mints a run token
///
/// A run token expires with its run's SLA, so a run's SLA can't be longer than the deployment's
/// token lifetime (`thorium.auth.token_expire`), the longest a user's token value lives. An
/// omitted SLA is the default pipeline SLA, cut to the token lifetime when that is shorter.
///
/// # Arguments
///
/// * `requested` - The SLA in seconds the run request set, if it set one
/// * `token_expire` - The token lifetime in days (`thorium.auth.token_expire`)
fn run_sla(requested: Option<u64>, token_expire: u32) -> Result<u64, ApiError> {
    // the token lifetime in seconds is the longest SLA a run may have
    let max = u64::from(token_expire) * 24 * 60 * 60;
    match requested {
        // a longer SLA would give the run a token that outlives every user token
        Some(sla) if sla > max => bad!(format!(
            "a run's SLA can be at most {max} seconds ({token_expire} days, the token lifetime set by thorium.auth.token_expire), because its run token expires with it"
        )),
        // any other requested SLA is kept as is
        Some(sla) => Ok(sla),
        // an omitted SLA is the default, cut to the token lifetime
        None => Ok(DEFAULT_PIPELINE_SLA.min(max)),
    }
}

/// Mint the run-scoped token of a workflow run or direct tool run and record it on the run
///
/// The token is named after the run's reaction, acts as a user, and expires with the run's SLA,
/// which [`run_sla`] keeps within the deployment's token lifetime.
///
/// # Arguments
///
/// * `user` - The user starting the run
/// * `cast` - The run's reaction, before it's written
/// * `tool_instances` - The tool instances the run may run
/// * `shared` - Shared Thorium objects
#[instrument(name = "reactions::mint_run_token", skip_all, fields(reaction = cast.id.to_string()), err(Debug))]
async fn mint_run_token(
    user: &User,
    cast: &mut Reaction,
    tool_instances: Vec<Uuid>,
    shared: &Shared,
) -> Result<ScopedToken, ApiError> {
    // the token reaches the run group and the inputs' groups
    let groups = run_token_groups(user, &cast.group, &cast.samples, &cast.repos, shared).await?;
    // build a user token for this run that ends with the run's SLA
    let req = ScopedTokenRequest {
        name: format!("run-{}", cast.id),
        role: ScopedTokenRole::User,
        groups,
        expires: Some(cast.sla),
        run: Some(RunTokenScope {
            reaction: cast.id,
            tool_instances,
        }),
    };
    let token = ScopedToken::create(user, req, shared).await?;
    // the run's workers use this token, and the run is its own run
    cast.run_token = Some(token.name.clone());
    cast.run = Some(cast.id);
    Ok(token)
}

/// Write a run's reaction and its first jobs, deleting its run token if the write fails
///
/// # Arguments
///
/// * `cast` - The run's reaction
/// * `cache` - The run's initial cache
/// * `memo` - The tool instances already read for this request
/// * `deadline` - The deadline to give the run's first jobs instead of one from the SLA
/// * `token` - The run token minted for this run, if one was
/// * `shared` - Shared Thorium objects
async fn write_run(
    cast: Reaction,
    cache: ReactionCache,
    memo: &mut ToolInstanceMemo,
    deadline: Option<DateTime<Utc>>,
    token: Option<ScopedToken>,
    shared: &Shared,
) -> Result<Reaction, ApiError> {
    // write the run and queue its first jobs
    match Box::pin(db::reactions::create_cast(
        cast, cache, memo, deadline, shared,
    ))
    .await
    {
        Ok(reaction) => Ok(reaction),
        Err(error) => {
            // a run that wasn't written must leave no token behind
            if let Some(token) = token
                && let Err(cleanup) = db::users::delete_scoped_token(&token, shared).await
            {
                event!(
                    Level::ERROR,
                    msg = "Failed to delete an unused run token",
                    token = token.name,
                    error = cleanup.to_string()
                );
            }
            Err(error)
        }
    }
}

/// Get the deadline of a run's harness job, so its container tool calls are queued with it
///
/// # Arguments
///
/// * `parent` - The run whose job's deadline to get
/// * `shared` - Shared Thorium objects
async fn parent_deadline(
    parent: &Reaction,
    shared: &Shared,
) -> Result<Option<DateTime<Utc>>, ApiError> {
    // use the earliest deadline of the run's jobs
    let mut earliest: Option<DateTime<Utc>> = None;
    for job in &parent.jobs {
        let job = db::jobs::get(job, shared).await?;
        earliest = Some(earliest.map_or(job.deadline, |current| current.min(job.deadline)));
    }
    Ok(earliest)
}

/// A workflow instance and everything a run of it reads before it's created
struct WorkflowRunSources {
    /// The workflow instance entity
    entity: Entity,
    /// The workflow instance
    instance: WorkflowInstanceEntity,
    /// The entry agent
    entry: InstancedAgent,
    /// The entry agent's harness tool instance entity
    harness_entity: Entity,
    /// The entry agent's harness
    harness: HarnessSpec,
    /// The spec of every tool bound to any of the instance's agents
    specs: HashMap<Uuid, AgenticTool>,
    /// The scalers of every workload the run may spawn
    scalers: Vec<ImageScaler>,
    /// The tool instances the run's token may run: every tool bound to a non-native mechanism
    /// and the Image tool instance each of their `Container` backings pins
    run_tools: Vec<Uuid>,
    /// The exact versions of every agent and of every tool instance the run runs or may call
    provenance: RunProvenance,
}

impl WorkflowRunSources {
    /// Read a workflow instance and what a run of it needs
    ///
    /// The instance must be `Ready`, its entry agent must have a harness, and every bound tool
    /// and every Image tool instance a container tool pins must still exist.
    ///
    /// # Arguments
    ///
    /// * `user` - The user starting the run
    /// * `id` - The id of the workflow instance
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "WorkflowRunSources::load", skip(user, shared), err(Debug))]
    #[allow(clippy::too_many_lines)]
    async fn load(user: &User, id: Uuid, shared: &Shared) -> Result<Self, ApiError> {
        // the instance must exist, be visible, and be a workflow instance
        let entity = run_entity(user, id, shared).await?;
        let EntityMetadata::WorkflowInstance(instance) = &entity.metadata else {
            return not_found!(format!("workflow instance {id} not found"));
        };
        let instance = instance.clone();
        // only a ready instance can be run
        if !instance.is_ready() {
            return bad!(format!(
                "workflow instance `{}` is {} and can't be run",
                entity.name,
                instance.status.as_str()
            ));
        }
        // the entry agent and its harness must be in the instance
        let Some(entry) = instance.entry_agent.and_then(|agent| instance.agent(agent)) else {
            return bad!(format!(
                "workflow instance `{}` has no entry agent",
                entity.name
            ));
        };
        let entry = entry.clone();
        let Some(harness_id) = entry.harness else {
            return bad!(format!(
                "the entry agent `{}` of workflow instance `{}` has no harness",
                entry.name, entity.name
            ));
        };
        // read the harness and every bound tool in one lookup
        let mut ids = vec![harness_id];
        for binding in instance.agents.iter().flat_map(|agent| &agent.bindings) {
            if !ids.contains(&binding.tool_instance) {
                ids.push(binding.tool_instance);
            }
        }
        let mut read = db::entities::get_many(&entity.groups, &ids, shared)
            .await?
            .into_iter()
            .map(|entity| (entity.id, entity))
            .collect::<HashMap<Uuid, Entity>>();
        // the harness must still be a harness tool instance
        let Some(harness_entity) = read.remove(&harness_id) else {
            return not_found!(format!(
                "harness tool instance {harness_id} no longer exists"
            ));
        };
        let Some(ToolSpec::Harness(harness)) =
            instance_meta(&harness_entity).map(|meta| &meta.spec)
        else {
            return bad!(format!(
                "the harness `{}` of workflow instance `{}` isn't a Harness tool instance",
                harness_entity.name, entity.name
            ));
        };
        let harness = harness.clone();
        // every bound tool must still be an agentic tool instance
        let mut specs = HashMap::with_capacity(ids.len());
        let mut pinned = Vec::new();
        for tool_instance in ids.iter().skip(1) {
            let Some(tool) = read.get(tool_instance) else {
                return not_found!(format!("tool instance {tool_instance} no longer exists"));
            };
            let Some(ToolSpec::Agentic(spec)) = instance_meta(tool).map(|meta| &meta.spec) else {
                return bad!(format!("bound tool `{}` isn't an Agentic tool", tool.name));
            };
            // remember the image tool instance each container tool runs
            if let AgenticToolBacking::Container(container) = &spec.backing
                && !pinned.contains(&container.tool_instance)
            {
                pinned.push(container.tool_instance);
            }
            specs.insert(*tool_instance, spec.clone());
        }
        // the run may run every tool bound to a non-native mechanism and the image it pins
        let mut run_tools = Vec::new();
        for binding in instance.agents.iter().flat_map(|agent| &agent.bindings) {
            // native tools run inside the harness, never as tool runs
            if binding.mechanism == ToolMechanism::Native
                || run_tools.contains(&binding.tool_instance)
            {
                continue;
            }
            run_tools.push(binding.tool_instance);
            // a container tool runs the image it pins
            if let Some(AgenticTool {
                backing: AgenticToolBacking::Container(container),
                ..
            }) = specs.get(&binding.tool_instance)
                && !run_tools.contains(&container.tool_instance)
            {
                run_tools.push(container.tool_instance);
            }
        }
        // the harness and every image a container tool pins can spawn a workload
        let mut scalers = vec![harness.workload.environment.scaler()];
        let images = if pinned.is_empty() {
            Vec::new()
        } else {
            db::entities::get_many(&entity.groups, &pinned, shared).await?
        };
        for image in &pinned {
            // each pinned image must still be an Image tool instance
            let found = images.iter().find(|entity| entity.id == *image);
            let Some(ToolSpec::Image(workload)) =
                found.and_then(instance_meta).map(|meta| &meta.spec)
            else {
                return not_found!(format!("image tool instance {image} no longer exists"));
            };
            let scaler = workload.environment.scaler();
            if !scalers.contains(&scaler) {
                scalers.push(scaler);
            }
        }
        // record every agent's hashes and the harness, bound tools, and pinned images just read
        let agents = instance
            .agents
            .iter()
            .map(|agent| AgentProvenance {
                agent: agent.agent,
                name: agent.name.clone(),
                config_hash: agent.config_hash.clone(),
                toolset_digest: agent.toolset_digest.clone(),
            })
            .collect();
        let tools = std::iter::once(&harness_entity)
            .chain(read.values())
            .chain(images.iter())
            .filter_map(ToolProvenance::from_entity)
            .collect();
        let provenance = RunProvenance::new(agents, tools);
        Ok(WorkflowRunSources {
            entity,
            instance,
            entry,
            harness_entity,
            harness,
            specs,
            scalers,
            run_tools,
            provenance,
        })
    }

    /// Create a run of this workflow instance in a run group
    ///
    /// # Arguments
    ///
    /// * `user` - The user starting the run
    /// * `group` - The run group
    /// * `inputs` - The files and repos to run on
    /// * `tags` - The tags to add to the run's reaction
    /// * `sla` - The SLA of the run in seconds
    /// * `parent` - The failed run this run retries, if it's a retry
    /// * `shared` - Shared Thorium objects
    #[allow(clippy::too_many_arguments)]
    #[instrument(name = "WorkflowRunSources::create", skip_all, fields(group = group), err(Debug))]
    #[allow(clippy::too_many_lines)]
    async fn create(
        self,
        user: &User,
        group: String,
        inputs: RunInputs,
        mut tags: Vec<String>,
        sla: Option<u64>,
        parent: Option<Uuid>,
        shared: &Shared,
    ) -> Result<Reaction, ApiError> {
        // the run's token expires with its SLA, so the SLA can't pass the token lifetime
        let sla = run_sla(sla, shared.config.thorium.auth.token_expire)?;
        // the runner must be able to see every input file
        if !inputs.samples.is_empty() {
            Sample::authorize(user, &inputs.samples, shared).await?;
        }
        // resolve each repo to the commitish it's checked out at
        let repos = resolve_repos(user, &inputs.repos, shared).await?;
        // resolve each agent's model before anything is created
        let bindings = models::resolve_run(
            user,
            &models::RunModelSources {
                entity: &self.entity,
                instance: &self.instance,
                harness_entity: &self.harness_entity,
                harness: &self.harness,
            },
            &group,
            shared,
        )
        .await?;
        // the run's results are stored under the instance's name and id when the run starts
        let result_key = WorkflowResultKey::build(&self.entity.name, self.entity.id);
        let id = Uuid::new_v4();
        let harness_id = self.harness_entity.id;
        // build the run's files
        let access = ToolAccess::resolve(
            &self.harness,
            &shared.config.thorium.tool_access,
            &run_files::worker_api_url(&shared.config),
        );
        let tools = run_files::tools_file(&self.instance, &self.harness, &self.specs, &access)
            .map_err(|err| internal_err_unwrapped!(err))?;
        let agents = run_files::agent_file(&self.instance, &self.entry)
            .map_err(|err| internal_err_unwrapped!(err))?;
        let run = run_files::run_file(&RunFileParts {
            reaction: id,
            samples: &inputs.samples,
            repos: &repos,
            workflow_instance: self.entity.id,
            instance: &self.instance,
            entry: &self.entry,
            harness: &self.harness,
            models: &bindings,
        });
        let buffers = HashMap::from([
            (run_files::RUN_FILE.to_owned(), buffer(&run)?),
            (run_files::AGENT_FILE.to_owned(), buffer(&agents)?),
            (run_files::TOOLS_FILE.to_owned(), buffer(&tools)?),
        ]);
        // list the run under its workflow, its instance, and as a workflow run
        tags.push(WORKFLOW_RUN_TAG.to_owned());
        tags.push(format!("Workflow={}", self.instance.workflow));
        tags.push(format!("WorkflowInstance={}", self.entity.id));
        // build the request this run's reaction is cast from
        let request = ReactionRequest {
            group,
            pipeline: format!("workflow:{}", self.entity.name),
            args: HashMap::default(),
            sla: Some(sla),
            tags,
            parent,
            samples: inputs.samples,
            buffers,
            repos: repos
                .iter()
                .map(|repo| RepoDependencyRequest {
                    url: repo.url.clone(),
                    commitish: repo.commitish.clone(),
                    kind: repo.kind,
                })
                .collect(),
            trigger_depth: None,
            cache: ReactionCache::default(),
        };
        // run the entry agent's harness as the one stage
        let stage = match instance_meta(&self.harness_entity) {
            Some(meta) => tool_name(&self.harness_entity, meta),
            None => self.harness_entity.name.clone(),
        };
        let plan = ReactionPlan {
            stages: vec![vec![PlannedStage {
                name: stage,
                tool_instance: harness_id,
            }]],
        };
        let source = ReactionSource::WorkflowRun(ReactionWorkflowRun {
            workflow_instance: self.entity.id,
            digest: self.instance.digest.clone(),
            agent: self.entry.agent,
            harness: harness_id,
            result_key,
            models: bindings,
            provenance: self.provenance,
        });
        // the harness was already read, so queueing its job reads nothing more
        let mut memo = ToolInstanceMemo::default();
        memo.remember(self.harness_entity);
        let (mut cast, cache) = request
            .cast_source(
                user,
                id,
                source,
                DEFAULT_PIPELINE_SLA,
                plan,
                HashMap::default(),
                shared,
            )
            .await?;
        // mint the run's token before its job is queued
        let token = mint_run_token(user, &mut cast, self.run_tools, shared).await?;
        // write the run, boxing its large future
        Box::pin(write_run(cast, cache, &mut memo, None, Some(token), shared)).await
    }
}

impl Reaction {
    /// Run a single tool instance on some files or repos
    ///
    /// An Image tool instance runs itself and an Agentic tool instance with a `Container`
    /// backing runs the Image tool instance it pins; every other kind is refused. A run with a
    /// parent runs in its parent's group.
    ///
    /// A direct tool run mints its own run token. A tool run made with a run token (a harness's
    /// tool call) must run one of the token's tool instances, is a sub run of the token's run in
    /// its group, mints nothing, and runs on the run's inputs when it names none.
    ///
    /// # Arguments
    ///
    /// * `user` - The user starting the run
    /// * `run` - The run the user's token is limited to, if it's a run token
    /// * `id` - The id of the tool instance to run
    /// * `req` - The inputs, run group, and args of the run
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Reaction::run_tool", skip(user, run, req, shared), err(Debug))]
    #[allow(clippy::too_many_lines)]
    pub async fn run_tool(
        user: &User,
        run: Option<&RunTokenScope>,
        id: Uuid,
        mut req: ToolRunRequest,
        shared: &Shared,
    ) -> Result<Reaction, ApiError> {
        // a run token may only run its run's own tool instances
        if let Some(run) = run
            && !run.tool_instances.contains(&id)
        {
            return unauthorized!(format!("tool instance {id} isn't one of this run's tools"));
        }
        // a run token acts as a plain user, so its tool calls can't override the image's command,
        // positionals, or kwargs
        let opts = &req.args.opts;
        if run.is_some()
            && (opts.override_positionals || opts.override_kwargs || opts.override_cmd.is_some())
        {
            return unauthorized!("A run token's tool calls can't override args".to_owned());
        }
        // a run that mints its own token can't have an SLA past the token lifetime, while a run
        // token's tool call shares its run's token and keeps its own SLA
        if run.is_none() {
            req.sla = Some(run_sla(req.sla, shared.config.thorium.auth.token_expire)?);
        }
        // the tool instance must exist and be visible
        let entity = run_entity(user, id, shared).await?;
        let Some(meta) = instance_meta(&entity) else {
            return not_found!(format!("tool instance {id} not found"));
        };
        // find the Image tool instance this run's job runs, and the provenance of a pinned one
        let mut memo = ToolInstanceMemo::default();
        let (stage, image, scaler, pinned_provenance) = match &meta.spec {
            // an Image tool instance runs itself
            ToolSpec::Image(workload) => (
                tool_name(&entity, meta),
                id,
                workload.environment.scaler(),
                None,
            ),
            // a container tool runs the Image tool instance it pins
            ToolSpec::Agentic(AgenticTool {
                backing: AgenticToolBacking::Container(container),
                ..
            }) => {
                // read the pinned tool instance in the container tool's groups
                let pinned =
                    db::entities::get_untagged(&entity.groups, container.tool_instance, shared)
                        .await?;
                // the pinned entity must still be a tool instance
                let Some(pinned_meta) = instance_meta(&pinned) else {
                    return not_found!(format!(
                        "image tool instance {} no longer exists",
                        container.tool_instance
                    ));
                };
                // the pinned tool instance must be an Image tool instance
                let ToolSpec::Image(workload) = &pinned_meta.spec else {
                    return bad!(format!(
                        "tool instance `{}` pins `{}`, which isn't an Image tool instance",
                        entity.name, pinned.name
                    ));
                };
                // the job runs the pinned image as its stage, recording its provenance
                let found = (
                    tool_name(&pinned, pinned_meta),
                    pinned.id,
                    workload.environment.scaler(),
                    ToolProvenance::from_entity(&pinned),
                );
                // remember the pinned image so queueing its job reads it no more
                memo.remember(pinned);
                found
            }
            // Thorium MCP tools run through Thorium's MCP server, not as tool runs
            ToolSpec::Agentic(AgenticTool {
                backing: AgenticToolBacking::ThoriumMcp { .. },
                ..
            }) => {
                return bad!(format!(
                    "tool instance `{}` is an Agentic tool with a ThoriumMcp backing, which runs through Thorium's MCP server, not as a tool run",
                    entity.name
                ));
            }
            // native tools only run inside the harness that provides them
            ToolSpec::Agentic(AgenticTool {
                backing: AgenticToolBacking::HarnessNative { .. },
                ..
            }) => {
                return bad!(format!(
                    "tool instance `{}` is an Agentic tool with a HarnessNative backing, which runs only inside its harness",
                    entity.name
                ));
            }
            // a harness runs only as a workflow run's stage
            ToolSpec::Harness(_) => {
                return bad!(format!(
                    "tool instance `{}` is a Harness, which runs only as a workflow run's stage",
                    entity.name
                ));
            }
        };
        // a tool run made with a run token is a sub run of the token's run
        let parent = match run {
            Some(run) => {
                // the run is found in the token's groups, whatever parent the request names
                req.parent = Some(run.reaction);
                Some(find_reaction(user, &user.groups, &run.reaction, shared).await?)
            }
            None => match &req.parent {
                Some(parent) => Some(find_reaction(user, &entity.groups, parent, shared).await?),
                None => None,
            },
        };
        // a harness's tool call names no inputs when it runs on the run's own inputs
        if let (Some(_), Some(parent)) = (run, &parent)
            && req.inputs.samples.is_empty()
            && req.inputs.repos.is_empty()
        {
            req.inputs.samples.clone_from(&parent.samples);
            req.inputs.repos = parent
                .repos
                .iter()
                .map(|repo| RepoDependencyRequest {
                    url: repo.url.clone(),
                    commitish: repo.commitish.clone(),
                    kind: repo.kind,
                })
                .collect();
        }
        // a run needs something to run on
        if let Err(err) = req.inputs.validate() {
            return bad!(err);
        }
        // a sub run takes its parent's group and ignores any requested group
        let (requested, parent_ephemeral) = match &parent {
            Some(parent) => {
                let ephemeral = db::reactions::get_parent_ephemeral(
                    &parent.group,
                    &Some(parent.id),
                    HashMap::default(),
                    shared,
                )
                .await?;
                (Some(parent.group.clone()), ephemeral)
            }
            None => (req.group.clone(), HashMap::default()),
        };
        // a run token's tool call takes the run's group without choosing one, but the group is
        // read again so a reactions kill switch or a demotion made mid-run still applies; any
        // other run checks the runner can run the image there
        let group = match (run, &parent) {
            (Some(_), Some(parent)) => {
                // read the run's group as it is now
                let group = db::groups::get(&parent.group, shared).await?;
                // the group must still allow reactions
                group.allowable(GroupAllowAction::Reactions)?;
                // and the runner must still be able to edit in it
                group.editable(user)?;
                parent.group.clone()
            }
            _ => {
                select_run_group(
                    user,
                    &entity.groups,
                    requested.as_deref(),
                    &[scaler],
                    shared,
                )
                .await?
            }
        };
        // the runner must be able to see every input file
        if !req.inputs.samples.is_empty() {
            Sample::authorize(user, &req.inputs.samples, shared).await?;
        }
        // build the request this run's reaction is cast from
        let request = ReactionRequest {
            group,
            pipeline: format!("tool:{} v{}", tool_name(&entity, meta), meta.version),
            args: HashMap::from([(stage.clone(), req.args)]),
            sla: req.sla,
            tags: req.tags,
            parent: req.parent,
            samples: req.inputs.samples,
            buffers: HashMap::default(),
            repos: req.inputs.repos,
            trigger_depth: None,
            cache: ReactionCache::default(),
        };
        // run the image as the one stage
        let plan = ReactionPlan {
            stages: vec![vec![PlannedStage {
                name: stage,
                tool_instance: image,
            }]],
        };
        // record the tool instance this run runs and the image it pins, both already read
        let tools = ToolProvenance::from_entity(&entity)
            .into_iter()
            .chain(pinned_provenance)
            .collect();
        let source = ReactionSource::Tool {
            tool_instance: id,
            provenance: RunProvenance::new(Vec::new(), tools),
        };
        // an Image tool instance was already read, so queueing its job reads nothing more
        if image == id {
            memo.remember(entity);
        }
        let (mut cast, cache) = request
            .cast_source(
                user,
                Uuid::new_v4(),
                source,
                DEFAULT_PIPELINE_SLA,
                plan,
                parent_ephemeral,
                shared,
            )
            .await?;
        match (run, parent) {
            // a run token's tool call shares its run's token, workers, and job deadline
            (Some(run), Some(parent)) => {
                cast.run_token.clone_from(&parent.run_token);
                cast.run = Some(run.reaction);
                let deadline = parent_deadline(&parent, shared).await?;
                // write the call, boxing its large future
                Box::pin(write_run(cast, cache, &mut memo, deadline, None, shared)).await
            }
            // a direct tool run mints its own token scoped to this tool instance
            _ => {
                let token = mint_run_token(user, &mut cast, vec![id], shared).await?;
                // write the run, boxing its large future
                Box::pin(write_run(cast, cache, &mut memo, None, Some(token), shared)).await
            }
        }
    }

    /// Run a workflow instance on some files or repos
    ///
    /// # Arguments
    ///
    /// * `user` - The user starting the run
    /// * `id` - The id of the workflow instance to run
    /// * `req` - The inputs and run group of the run
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Reaction::run_workflow", skip(user, req, shared), err(Debug))]
    pub async fn run_workflow(
        user: &User,
        id: Uuid,
        req: WorkflowRunRequest,
        shared: &Shared,
    ) -> Result<Reaction, ApiError> {
        // read the instance and what a run of it needs
        let sources = WorkflowRunSources::load(user, id, shared).await?;
        // a run needs something to run on
        if let Err(err) = req.inputs.validate() {
            return bad!(err);
        }
        // choose the run group, checking the runner can run every workload there
        let group = select_run_group(
            user,
            &sources.entity.groups,
            req.group.as_deref(),
            &sources.scalers,
            shared,
        )
        .await?;
        // create the run, boxing its large future
        Box::pin(sources.create(user, group, req.inputs, req.tags, req.sla, None, shared)).await
    }

    /// Retry a failed workflow run as a new run with the same inputs, linked to it as its parent
    ///
    /// Only `Failed` runs can be retried. The retry runs in the failed run's group, which is
    /// checked again.
    ///
    /// # Arguments
    ///
    /// * `user` - The user retrying the run
    /// * `id` - The id of the workflow instance the failed run ran
    /// * `reaction` - The id of the failed run's reaction
    /// * `shared` - Shared Thorium objects
    #[instrument(name = "Reaction::retry_workflow_run", skip(user, shared), err(Debug))]
    pub async fn retry_workflow_run(
        user: &User,
        id: Uuid,
        reaction: Uuid,
        shared: &Shared,
    ) -> Result<Reaction, ApiError> {
        // read the instance and what a run of it needs
        let sources = WorkflowRunSources::load(user, id, shared).await?;
        // find the failed run in one of the instance's groups
        let failed = find_reaction(user, &sources.entity.groups, &reaction, shared).await?;
        // it must be a run of this instance
        let is_run = matches!(
            &failed.source,
            ReactionSource::WorkflowRun(run) if run.workflow_instance == id
        );
        if !is_run {
            return bad!(format!(
                "reaction {reaction} isn't a run of workflow instance {id}"
            ));
        }
        // only failed runs can be retried
        if failed.status != ReactionStatus::Failed {
            return bad!(format!(
                "only failed runs can be retried, but reaction {reaction} is {}",
                failed.status
            ));
        }
        // check the runner can still run in the failed run's group
        let group = select_run_group(
            user,
            &sources.entity.groups,
            Some(&failed.group),
            &sources.scalers,
            shared,
        )
        .await?;
        // keep the tags the failed run's creator added, leaving out the ones every run gets
        let automatic = [
            failed.creator.clone(),
            WORKFLOW_RUN_TAG.to_owned(),
            format!("Workflow={}", sources.instance.workflow),
            format!("WorkflowInstance={id}"),
        ];
        let tags = failed
            .tags
            .iter()
            .filter(|tag| !automatic.contains(tag) && !failed.samples.contains(tag))
            .cloned()
            .collect();
        // rerun on the same files and repos
        let inputs = RunInputs {
            samples: failed.samples.clone(),
            repos: failed
                .repos
                .iter()
                .map(|repo| RepoDependencyRequest {
                    url: repo.url.clone(),
                    commitish: repo.commitish.clone(),
                    kind: repo.kind,
                })
                .collect(),
        };
        // create the retry, boxing its large future
        Box::pin(sources.create(user, group, inputs, tags, None, Some(failed.id), shared)).await
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::{choose_run_group, run_sla};
    use crate::models::backends::groups::tests::{group, user};
    use crate::models::{DEFAULT_PIPELINE_SLA, Group, ImageScaler, UserRole};

    /// A K8s-only developer role
    const K8S_DEV: UserRole = UserRole::Developer {
        k8s: true,
        bare_metal: false,
        windows: false,
        external: false,
        kvm: false,
    };

    /// Build the run groups `a` and `b` where bob is a user
    fn both() -> Vec<Group> {
        vec![
            group("a", &[], &["bob"], &[]),
            group("b", &[], &["bob"], &[]),
        ]
    }

    /// A single candidate group is picked when none is requested
    #[test]
    fn run_group_single_candidate() {
        // bob can only edit group a
        let bob = user("bob", K8S_DEV, &["a", "b"]);
        let groups = vec![
            group("a", &[], &["bob"], &[]),
            group("b", &[], &[], &["bob"]),
        ];
        // a is the only candidate
        let picked = choose_run_group(&bob, &groups, None, &[ImageScaler::K8s]).unwrap();
        assert_eq!(picked, "a");
    }

    /// Several candidates without a request is a 400 listing them
    #[test]
    fn run_group_several_candidates() {
        // bob can run in both groups
        let bob = user("bob", K8S_DEV, &["a", "b"]);
        // nothing is requested
        let err = choose_run_group(&bob, &both(), None, &[ImageScaler::K8s]).unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            err.msg().unwrap_or_default(),
            "several run groups are possible, pick one of: `a`, `b`"
        );
        // requesting one picks it
        let picked = choose_run_group(&bob, &both(), Some("b"), &[ImageScaler::K8s]).unwrap();
        assert_eq!(picked, "b");
    }

    /// No candidates is a 401 with a reason per visible group
    #[test]
    fn run_group_no_candidates() {
        // bob monitors a, b disallows reactions, and he isn't in secret
        let bob = user("bob", K8S_DEV, &["a", "b"]);
        let mut closed = group("b", &[], &["bob"], &[]);
        closed.allowed.reactions = false;
        let groups = vec![
            group("a", &[], &[], &["bob"]),
            closed,
            group("secret", &["alice"], &[], &[]),
        ];
        // nothing can be picked
        let err = choose_run_group(&bob, &groups, None, &[ImageScaler::K8s]).unwrap_err();
        assert_eq!(err.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            err.msg().unwrap_or_default(),
            "you can't run in any of this instance's groups: `a`: you can't edit in it; `b`: it doesn't allow reactions"
        );
    }

    /// A runner missing a scaler's developer role can't run there
    #[test]
    fn run_group_needs_developer() {
        // bob develops only for K8s and a plain user develops for nothing
        let bob = user("bob", K8S_DEV, &["a", "b"]);
        let plain = user("bob", UserRole::User, &["a", "b"]);
        // a BareMetal workload is refused for bob
        let err =
            choose_run_group(&bob, &both(), Some("a"), &[ImageScaler::BareMetal]).unwrap_err();
        assert_eq!(err.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            err.msg().unwrap_or_default(),
            "you can't run in `a`: you aren't a developer for BareMetal"
        );
        // a plain user is refused for K8s
        let err = choose_run_group(&plain, &both(), Some("a"), &[ImageScaler::K8s]).unwrap_err();
        assert_eq!(err.status(), StatusCode::UNAUTHORIZED);
        // an analyst who isn't a group user can still run
        let mut groups = both();
        groups[0].analysts.insert("ann".to_owned());
        let ann = user("ann", UserRole::Analyst, &["a"]);
        let picked = choose_run_group(&ann, &groups, None, &[ImageScaler::BareMetal]).unwrap();
        assert_eq!(picked, "a");
    }

    /// A requested group the runner can see that isn't one of the instance's is a 400
    #[test]
    fn run_group_requested_outside_instance() {
        // bob is in a, b, and c
        let bob = user("bob", K8S_DEV, &["a", "b", "c"]);
        // request a group the instance isn't in
        let err = choose_run_group(&bob, &both(), Some("c"), &[ImageScaler::K8s]).unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            err.msg().unwrap_or_default(),
            "`c` isn't one of this instance's groups"
        );
    }

    /// Requesting a group the runner isn't in gets the same answer whether or not the instance
    /// is in it, so the answer never reveals the instance's hidden groups
    #[test]
    fn run_group_requested_hidden() {
        // bob is only in a, and the instance is also in the hidden group secret
        let bob = user("bob", K8S_DEV, &["a"]);
        let groups = vec![
            group("a", &[], &["bob"], &[]),
            group("secret", &["alice"], &[], &[]),
        ];
        // request the hidden group the instance is in and one it isn't in
        let inside =
            choose_run_group(&bob, &groups, Some("secret"), &[ImageScaler::K8s]).unwrap_err();
        let outside =
            choose_run_group(&bob, &groups, Some("other"), &[ImageScaler::K8s]).unwrap_err();
        // both get the same plain refusal
        assert_eq!(inside.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(inside.status(), outside.status());
        assert_eq!(
            inside.msg().unwrap_or_default(),
            "you can't run in `secret`"
        );
        assert_eq!(
            outside.msg().unwrap_or_default(),
            "you can't run in `other`"
        );
    }

    /// One day in seconds
    const DAY: u64 = 24 * 60 * 60;

    /// A requested SLA up to the token lifetime is kept as is
    #[test]
    fn run_sla_within_token_expire() {
        // a one second SLA and one of exactly 90 days both fit a 90 day token lifetime
        assert_eq!(run_sla(Some(1), 90).unwrap(), 1);
        assert_eq!(run_sla(Some(90 * DAY), 90).unwrap(), 90 * DAY);
    }

    /// A requested SLA past the token lifetime is a 400 that names the limit and its setting
    #[test]
    fn run_sla_past_token_expire() {
        // one second past a 90 day token lifetime is refused
        let err = run_sla(Some(90 * DAY + 1), 90).unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            err.msg().unwrap_or_default(),
            "a run's SLA can be at most 7776000 seconds (90 days, the token lifetime set by thorium.auth.token_expire), because its run token expires with it"
        );
        // and so is the century the generic SLA bound allows
        let err = run_sla(Some(3_000_000_000), 90).unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    /// An omitted SLA is the default pipeline SLA, cut to the token lifetime when that's shorter
    #[test]
    fn run_sla_default() {
        // a 90 day token lifetime keeps the 7 day default
        assert_eq!(run_sla(None, 90).unwrap(), DEFAULT_PIPELINE_SLA);
        // a 3 day token lifetime cuts it to 3 days
        assert_eq!(run_sla(None, 3).unwrap(), 3 * DAY);
    }
}
