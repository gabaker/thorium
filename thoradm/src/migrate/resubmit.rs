//! The `resubmit-reactions` migration: resubmits the requests a drain recorded as their creators
//!
//! The drain's cancel records each reaction it fails as a `drain_request` row (its args in a
//! `drain_request_args` row that expires). This migration creates each request of the drain's
//! attempt again once, as its creator, through the same checks the create route makes plus
//! identity checks on the creator and the pipeline. It has no catalog step: the operator runs it
//! inside the conversion pause with `--in-pause <attempt>`, and an admin may run it by hand.
//!
//! Each row moves forward only (`recorded` or `stale`, then `resubmitting` with a reserved id,
//! then `resubmitted` or `refused`), each move conditional on the status read, so a rerun or an
//! overlapping run never creates a request twice. The reserved id is derived from the cancelled
//! reaction and its attempt, so runs that overlap where that condition is a read then a plain
//! write (a keyspace on tablets) reserve the same id, and the create's existence check still makes
//! one reaction. A reserved id whose reaction exists is resubmitted; one whose reaction is missing
//! is created again only by the operator's in-pause run of the same attempt, or by the run that
//! reserved it.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock};
use thorium::models::backends::db;
use thorium::models::schema::drain_requests::{
    DrainRequest, DrainRequestRow, DrainRequestStore, DrainStatus, PipelineIdentity, SERVICE_USERS,
    args_sha256,
};
use thorium::models::schema::{
    Batch, MigrateError, MigrationRunner, Outcome, RecordStatus, RunContext, RunMode, Slice,
    append_history, backend_error, read_status,
};
use thorium::models::upgrades::{
    AppliedStep, Catalog, RESET_STATE_STEP, RESTORED_STEP, Record, RevisionId, StepState,
    UPGRADE_STOPPED_STEP, attempt_start,
};
use thorium::models::{
    Actions, Group, Pipeline, Reaction, ReactionArgs, ReactionRequest, StatusUpdate, User,
};
use thorium::utils::{ApiError, Shared};
use uuid::Uuid;

use super::registry::{Backend, MigrationDef, Support};

/// The id of this migration
pub const ID: &str = "resubmit-reactions";

/// The revision whose drain records the requests, and which must be recorded before any is
/// resubmitted, since a pipeline isn't converted to tools before it
pub const REVISION: &str = "2026-10-v04";

/// The id of the drain step whose history entries open the attempt window
const DRAIN_STEP: &str = "drain-reactions";

/// The reason a reserved id whose reaction is missing is refused outside the operator's run
const MISSING_RESERVATION: &str =
    "reserved reaction not found; it may have been created and then deleted or expired";

/// What one pass knows about its namespace, found when it splits into slices
struct PassState {
    /// The drain request rows
    store: Arc<DrainRequestStore>,
    /// The `at` of the drain `Running` entry whose requests this pass resubmits, if any
    window: Option<DateTime<Utc>>,
    /// Whether the run carries the operator's in-pause flag for that attempt
    flagged: bool,
    /// The cancelled reactions whose new id this pass reserved, which it may create on a retry
    reserved: std::sync::Mutex<HashSet<String>>,
    /// The offline `Shared` that reads users, groups, and pipelines and creates reactions the way
    /// the API does, built when the pass first needs it
    shared: tokio::sync::OnceCell<Arc<Shared>>,
}

/// Each namespace's current pass state
static PASSES: LazyLock<tokio::sync::Mutex<HashMap<String, Arc<PassState>>>> =
    LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));

/// Where the test fault hook stops a resubmission
#[cfg_attr(not(feature = "test-support"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResubmitFault {
    /// A backend error before the new id is reserved
    BeforeReserve,
    /// A backend error after the id is reserved and before the reaction is created
    AfterReserve,
    /// A backend error after the reaction is created and before its row is marked resubmitted
    AfterCreate,
}

/// The armed faults: per namespace, the cancelled reaction's id and where to fail; each fires once
#[cfg(feature = "test-support")]
static FAULTS: std::sync::Mutex<std::collections::BTreeMap<String, (String, ResubmitFault)>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Arm a one-shot fault for one request's resubmission in a namespace
///
/// # Arguments
///
/// * `ns` - The namespace the run works in
/// * `id` - The cancelled reaction's id
/// * `fault` - Where the resubmission fails
#[cfg(feature = "test-support")]
pub fn arm_resubmit_fault(ns: &str, id: &str, fault: ResubmitFault) {
    // a poisoned lock still holds the map
    FAULTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(ns.to_owned(), (id.to_owned(), fault));
}

/// Fail as a backend error when a test armed a fault for this request at this point
///
/// # Arguments
///
/// * `ns` - The namespace the run works in
/// * `id` - The cancelled reaction's id
/// * `point` - Where the resubmission is
#[cfg(feature = "test-support")]
fn fault(ns: &str, id: &str, point: ResubmitFault) -> Result<(), MigrateError> {
    // a matching fault fires once and is disarmed
    let mut faults = FAULTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if faults.get(ns) == Some(&(id.to_owned(), point)) {
        faults.remove(ns);
        return Err(MigrateError::Backend(format!(
            "an injected fault at {point:?}"
        )));
    }
    Ok(())
}

/// Fail as a backend error when a test armed a fault, which only test builds can
///
/// # Arguments
///
/// * `ns` - The namespace the run works in
/// * `id` - The cancelled reaction's id
/// * `point` - Where the resubmission is
#[cfg(not(feature = "test-support"))]
#[allow(clippy::unnecessary_wraps)]
fn fault(ns: &str, id: &str, point: ResubmitFault) -> Result<(), MigrateError> {
    // no fault can be armed outside the tests
    let _ = (ns, id, point);
    Ok(())
}

/// The id a recorded request is resubmitted under: the first 16 bytes of the SHA-256 of the
/// cancelled reaction's id and the attempt that recorded it, as a random-variant UUID
///
/// Every run reserves the same id for the same request, so two runs that overlap where the row's
/// guard is a read then a plain write still create at most one reaction: the create refuses an
/// id that already holds one. A later attempt that records the same reaction again gets another id.
///
/// # Arguments
///
/// * `old` - The cancelled reaction's id
/// * `attempt` - The drain attempt that recorded the request, if any
#[must_use]
pub fn reserved_id(old: &str, attempt: Option<&str>) -> Uuid {
    // hash a fixed label, the old id, and the attempt, each separated by a NUL no id holds
    let mut hasher = Sha256::new();
    hasher.update(b"thorium-drain-resubmit\0");
    hasher.update(old.as_bytes());
    hasher.update(b"\0");
    hasher.update(attempt.unwrap_or_default().as_bytes());
    let digest = hasher.finalize();
    // the first 16 bytes, with the version and variant bits set as a random UUID's are
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    uuid::Builder::from_random_bytes(bytes).into_uuid()
}

/// Parse an RFC 3339 time, as history entries and rows store attempts
///
/// # Arguments
///
/// * `at` - The time's text
fn parse_at(at: &str) -> Option<DateTime<Utc>> {
    // stored times are normalized, so compare them as times rather than as text
    DateTime::parse_from_rfc3339(at)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

/// Find the attempt whose requests may be resubmitted: the `at` of the drain `Running` entry that
/// started the newest drain run's attempt, when a drain `Done` entry follows that run and no stop,
/// restore, or state reset follows that
///
/// A drain run whose Job failed leaves its attempt open, so the retry that finishes it carries the
/// first run's start and its requests, as the operator passes the same start to both runs.
///
/// # Arguments
///
/// * `history` - The recorded history entries
#[must_use]
pub fn attempt_window(history: &[AppliedStep]) -> Option<DateTime<Utc>> {
    // the entries with a readable time, oldest first
    let mut timed = history
        .iter()
        .filter_map(|entry| Some((parse_at(entry.at.as_deref()?)?, entry)))
        .collect::<Vec<_>>();
    timed.sort_by_key(|(at, _)| *at);
    // the newest drain run that started, and the first run of its attempt
    let newest = timed
        .iter()
        .rposition(|(_, entry)| entry.step == DRAIN_STEP && entry.outcome == StepState::Running)?;
    let entries = timed.iter().map(|(_, entry)| *entry).collect::<Vec<_>>();
    let (start, _) = timed[attempt_start(&entries, DRAIN_STEP, newest)];
    // the drain's end after the newest run
    let (done, _) = timed[newest..]
        .iter()
        .find(|(_, entry)| entry.step == DRAIN_STEP && entry.outcome == StepState::Done)?;
    // a stop, restore, or reset after that end closes the window
    let closed = timed.iter().any(|(at, entry)| {
        at > done
            && [UPGRADE_STOPPED_STEP, RESTORED_STEP, RESET_STATE_STEP]
                .contains(&entry.step.as_str())
    });
    (!closed).then_some(start)
}

/// Get the offline `Shared` the pass's reads and creates go through, building it on first use
///
/// It is built once per pass rather than once per process, since its connections belong to the
/// runtime that built them.
///
/// # Arguments
///
/// * `ctx` - The run's context
/// * `state` - The pass's state
async fn writer(ctx: &RunContext, state: &PassState) -> Result<Arc<Shared>, MigrateError> {
    // build it from the run's config and Redis pool once; an unreachable Scylla is worth retrying
    state
        .shared
        .get_or_try_init(|| async {
            Box::pin(Shared::for_offline_writer(
                (*ctx.backends.conf).clone(),
                ctx.backends.redis.clone(),
            ))
            .await
            .map(Arc::new)
            .map_err(|err| {
                MigrateError::Backend(format!("the reaction writer didn't start: {err}"))
            })
        })
        .await
        .cloned()
}

/// Get the current pass's state for the run's namespace
///
/// # Arguments
///
/// * `ctx` - The run's context
async fn pass(ctx: &RunContext) -> Result<Arc<PassState>, MigrateError> {
    // the state the pass's slices found
    PASSES
        .lock()
        .await
        .get(&ctx.backends.namespace)
        .cloned()
        .ok_or_else(|| MigrateError::Systematic("the pass's state wasn't found".to_owned()))
}

/// Run a Redis command on the run's pool
///
/// # Arguments
///
/// * `ctx` - The run's context
/// * `what` - What the command does, for its error
/// * `cmd` - The command
async fn query<T: redis::FromRedisValue>(
    ctx: &RunContext,
    what: &str,
    cmd: &redis::Cmd,
) -> Result<T, MigrateError> {
    // a failed connection or command may succeed on a retry
    let mut conn = ctx
        .backends
        .redis
        .get()
        .await
        .map_err(|err| backend_error(what, &err, false))?;
    cmd.query_async(&mut *conn)
        .await
        .map_err(|err| backend_error(what, &err, false))
}

/// Whether a reaction's data hash exists in a group
///
/// # Arguments
///
/// * `ctx` - The run's context
/// * `group` - The reaction's group
/// * `id` - The reaction's id
async fn reaction_exists(ctx: &RunContext, group: &str, id: &str) -> Result<bool, MigrateError> {
    // the hash every reaction keeps its fields in
    let key = format!("{}:reaction_data:{group}:{id}", ctx.backends.namespace);
    let found: u64 = query(ctx, "check a reaction", redis::cmd("EXISTS").arg(&key)).await?;
    Ok(found > 0)
}

/// A refusal that names a lasting reason, or a failure worth retrying
enum Stop {
    /// The request can't be resubmitted, for this reason
    Refuse(String),
    /// A backend failed; the row stays as it was for the next run
    Retry(MigrateError),
}

impl From<MigrateError> for Stop {
    /// A migration error leaves the row for the next run
    ///
    /// # Arguments
    ///
    /// * `err` - The error
    fn from(err: MigrateError) -> Self {
        Stop::Retry(err)
    }
}

/// Classify an API error: a refusal (4xx) names a lasting reason, anything else is retried
///
/// # Arguments
///
/// * `context` - What was being done
/// * `err` - The API's error
fn api_stop(context: &str, err: &ApiError) -> Stop {
    // a client error is the API refusing this request, which retrying can't change
    let status = err.status();
    if status.is_client_error() {
        let msg = err.msg().unwrap_or_else(|| status.to_string());
        Stop::Refuse(format!("{context}: {msg}"))
    } else {
        Stop::Retry(MigrateError::Backend(format!("{context}: {err:?}")))
    }
}

/// Write a row's new status and result, conditional on the status it was read with
///
/// Returns a write outcome when it applied, or `Current` when another writer moved the row first.
///
/// # Arguments
///
/// * `store` - The drain request rows
/// * `row` - The row as read
/// * `status` - The new status
/// * `result` - The new result
async fn settle(
    store: &DrainRequestStore,
    row: &DrainRequestRow,
    status: DrainStatus,
    result: String,
) -> Result<Outcome, MigrateError> {
    // the reserved id is kept through every status
    let mut next = row.clone();
    next.status = status;
    next.result = Some(result);
    next.finished = Some(Utc::now());
    next.updated = None;
    let applied = store.update_status_if(&next, row.status).await?;
    // the record is the row's key, since the work is already done
    Ok(if applied {
        Outcome::Write(Record::Scylla {
            key: row.id.clone(),
            columns: std::collections::BTreeMap::new(),
        })
    } else {
        Outcome::Current
    })
}

/// The pipeline's stage tool ids by stage, as a request's binding holds them, or `None` when a
/// stage has no stage tool
///
/// # Arguments
///
/// * `pipeline` - The pipeline
fn stage_binding(pipeline: &Pipeline) -> Option<Vec<Vec<String>>> {
    // each stage's tool id, in the pipeline's order
    pipeline
        .order
        .iter()
        .map(|stage| {
            stage
                .iter()
                .map(|name| pipeline.stage_tools.get(name).map(ToString::to_string))
                .collect()
        })
        .collect()
}

/// Whether a user holds the group's `owners`, `managers`, or `users` role; an admin's or an
/// analyst's bypass doesn't count
///
/// # Arguments
///
/// * `group` - The group
/// * `user` - The user's name
fn holds_role(group: &Group, user: &str) -> bool {
    // the combined sets include metagroup members
    group.owners.combined.contains(user)
        || group.managers.combined.contains(user)
        || group.users.combined.contains(user)
}

/// Append a status log line to the cancelled reaction naming its resubmission, when its hash
/// still exists
///
/// # Arguments
///
/// * `ctx` - The run's context
/// * `request` - The request
/// * `old` - The cancelled reaction's id
/// * `new` - The new reaction's id
async fn log_resubmission(
    ctx: &RunContext,
    request: &DrainRequest,
    old: &str,
    new: &str,
) -> Result<(), MigrateError> {
    // an expired reaction has no log to add to
    if !reaction_exists(ctx, &request.group, old).await? {
        return Ok(());
    }
    // the line, as the API's own status logs are written
    let update = StatusUpdate {
        group: request.group.clone(),
        pipeline: request.pipeline.clone(),
        reaction: old.to_owned(),
        action: Actions::ReactionFailed,
        timestamp: Utc::now(),
        msg: Some(format!("resubmitted by the upgrade to {REVISION} as {new}")),
        update: HashMap::new(),
    };
    // serialized as the API serializes it
    let line = serde_json::to_string(&update)
        .map_err(|err| MigrateError::Systematic(format!("a status log didn't serialize: {err}")))?;
    // appended to the reaction's status log list
    let key = format!("{}:logs:{}:{old}", ctx.backends.namespace, request.group);
    let _: u64 = query(
        ctx,
        "log a resubmission",
        redis::cmd("RPUSH").arg(&key).arg(line),
    )
    .await?;
    Ok(())
}

/// The reaction request a drain request is resubmitted as: its creator's request, tagged with
/// the cancelled reaction's id, with no parent and the pipeline's default SLA
///
/// # Arguments
///
/// * `request` - The drain request
/// * `old` - The cancelled reaction's id
/// * `args` - The request's args
fn reaction_request(request: &DrainRequest, old: &str, args: ReactionArgs) -> ReactionRequest {
    // the tag names the cancelled reaction, but is never what decides a resubmission
    let mut tags = request.tags.clone();
    tags.push(format!("resubmitted-from:{old}"));
    ReactionRequest {
        group: request.group.clone(),
        pipeline: request.pipeline.clone(),
        args,
        sla: None,
        tags,
        parent: None,
        samples: request.samples.clone(),
        buffers: HashMap::new(),
        repos: request.repos.clone(),
        trigger_depth: request.trigger_depth,
        cache: thorium::models::ReactionCache::default(),
    }
}

/// The runner of the `resubmit-reactions` migration
pub struct ResubmitReactions;

impl ResubmitReactions {
    /// Mark a row resubmitted with its new id, delete its args, and log the resubmission
    ///
    /// # Arguments
    ///
    /// * `ctx` - The run's context
    /// * `store` - The drain request rows
    /// * `row` - The row as read
    /// * `request` - The row's request
    /// * `entry` - The new reaction's id
    async fn resubmitted(
        ctx: &RunContext,
        store: &DrainRequestStore,
        row: &DrainRequestRow,
        request: &DrainRequest,
        entry: &str,
    ) -> Result<Outcome, MigrateError> {
        // the new id goes in the result as well as the reserved entry
        let mut row = row.clone();
        row.entry = Some(entry.to_owned());
        let outcome = settle(store, &row, DrainStatus::Resubmitted, entry.to_owned()).await?;
        if matches!(outcome, Outcome::Current) {
            return Ok(outcome);
        }
        // a resubmitted request's args are no longer needed
        store.delete_args(&row.id).await?;
        log_resubmission(ctx, request, &row.id, entry).await?;
        // the ids and the creator, never the args
        tracing::info!(
            old = %row.id,
            new = %entry,
            creator = %request.creator,
            "resubmitted a drained reaction as its creator"
        );
        Ok(outcome)
    }

    /// Load a request's creator and check they may run it in its group: not an account the
    /// operator makes, holding a role in the group now and when the request was recorded
    ///
    /// # Arguments
    ///
    /// * `shared` - The offline `Shared` to read with
    /// * `request` - The request
    async fn creator(shared: &Shared, request: &DrainRequest) -> Result<User, Stop> {
        // the creator as stored now, read the way the API reads it
        let user = match User::force_get(&request.creator, shared).await {
            Ok(user) => user,
            Err(err) if err.status().as_u16() == 404 => {
                return Err(Stop::Refuse("the creator no longer exists".to_owned()));
            }
            Err(err) => return Err(api_stop("the creator didn't load", &err)),
        };
        // an account the operator makes runs hunts, which aren't resubmitted
        if SERVICE_USERS.contains(&request.creator.as_str()) {
            return Err(Stop::Refuse(format!(
                "made by {}, a system account the operator manages",
                request.creator
            )));
        }
        // the creator must hold a role in the group now
        let group = match db::groups::get(&request.group, shared).await {
            Ok(group) => group,
            Err(err) if err.status().as_u16() == 404 => {
                return Err(Stop::Refuse("the group no longer exists".to_owned()));
            }
            Err(err) => return Err(api_stop("the group didn't load", &err)),
        };
        // in its owners, managers, or users; an admin's or analyst's bypass doesn't count
        if !holds_role(&group, &request.creator) {
            return Err(Stop::Refuse(format!(
                "the creator holds no owner, manager, or user role in group {}",
                request.group
            )));
        }
        // and must have held one when the request was recorded
        if !request.membership.held_role {
            return Err(Stop::Refuse(
                "the creator held no owner, manager, or user role in the group when the request \
                 was recorded"
                    .to_owned(),
            ));
        }
        Ok(user)
    }

    /// Run every check a request passes before it is created again, binding it first in the
    /// operator's run, and return the creator, group, pipeline, and args to create it with
    ///
    /// # Arguments
    ///
    /// * `ctx` - The run's context
    /// * `state` - The pass's state
    /// * `row` - The row as read, whose value takes the binding
    /// * `request` - The row's request, whose binding is set when it binds
    /// * `in_window` - Whether the row is of the window's attempt
    async fn checks(
        ctx: &RunContext,
        state: &PassState,
        row: &mut DrainRequestRow,
        request: &mut DrainRequest,
        in_window: bool,
    ) -> Result<(User, Group, Pipeline, ReactionArgs), Stop> {
        // a sub reaction whose parent was recorded too is made again by the parent's resubmission
        if let Some(parent) = request.parent
            && state.store.get(&parent.to_string()).await?.is_some()
        {
            return Err(Stop::Refuse(format!(
                "a sub reaction of {parent}, which the parent's resubmission creates again"
            )));
        }
        // the creator, who must still hold a role in the group
        let shared = writer(ctx, state).await?;
        let user = Self::creator(&shared, request).await?;
        // the pipeline must be the one the request ran
        let pipeline = match db::pipelines::get(&request.group, &request.pipeline, &shared).await {
            Ok(pipeline) => pipeline,
            Err(err) if err.status().as_u16() == 404 => {
                return Err(Stop::Refuse("the pipeline no longer exists".to_owned()));
            }
            Err(err) => return Err(api_stop("the pipeline didn't load", &err)),
        };
        // and neither recreated by someone else nor given other stages since the cancel
        let identity = PipelineIdentity::of(&pipeline);
        if identity.creator != request.identity.creator
            || identity.order_sha256 != request.identity.order_sha256
        {
            return Err(Stop::Refuse(
                "the pipeline was changed or recreated since the cancel".to_owned(),
            ));
        }
        // every stage must run a tool
        let Some(current) = stage_binding(&pipeline) else {
            let stages = pipeline
                .unconverted_stages()
                .into_iter()
                .cloned()
                .collect::<Vec<_>>();
            return Err(Stop::Refuse(format!(
                "the pipeline isn't converted to tools: stages {stages:?} have no stage tool"
            )));
        };
        // only the operator's run in the pause binds the attempt's rows to the stage tools
        if request.binding.is_none() && state.flagged && in_window {
            request.binding = Some(current.clone());
            let mut bound = row.clone();
            bound.value = request.to_json().map_err(|err| {
                MigrateError::Systematic(format!("a request didn't serialize: {err}"))
            })?;
            bound.updated = None;
            if !state.store.update_status_if(&bound, row.status).await? {
                return Err(Stop::Retry(MigrateError::Backend(
                    "the row changed while it was bound".to_owned(),
                )));
            }
            *row = bound;
        }
        // the request runs the tools it was bound to
        match &request.binding {
            None => {
                return Err(Stop::Refuse(
                    "the request was never bound to the pipeline's stage tools inside the \
                     conversion pause"
                        .to_owned(),
                ));
            }
            Some(binding) if *binding != current => {
                return Err(Stop::Refuse(
                    "the pipeline's stage tools changed since the request was bound".to_owned(),
                ));
            }
            Some(_) => (),
        }
        // buffers and caches aren't kept by the cancel
        if request.buffers || request.cache {
            return Err(Stop::Refuse(
                "the request used buffers or a cache, which aren't kept".to_owned(),
            ));
        }
        // the create route's checks
        let (group, pipeline) =
            Reaction::authorize_create(&user, &request.group, &request.pipeline, &shared)
                .await
                .map_err(|err| api_stop("the create route refused it", &err))?;
        // the args, when the request had any
        let args = Self::args(state, &row.id, request).await?;
        Ok((user, group, pipeline, args))
    }

    /// Read a request's args, refusing it when its args row is gone or doesn't match its hash
    ///
    /// # Arguments
    ///
    /// * `state` - The pass's state
    /// * `id` - The cancelled reaction's id
    /// * `request` - The request
    async fn args(
        state: &PassState,
        id: &str,
        request: &DrainRequest,
    ) -> Result<ReactionArgs, Stop> {
        // a request with no args hash had no args
        let Some(sha) = &request.args_sha256 else {
            return Ok(ReactionArgs::default());
        };
        // the args row expires, and a restore never brings it back
        let Some(json) = state.store.get_args(id).await? else {
            return Err(Stop::Refuse("arguments expired or not restored".to_owned()));
        };
        // the args must be the ones the cancel recorded
        if args_sha256(&json) != *sha {
            return Err(Stop::Refuse(
                "the arguments don't match the recorded hash".to_owned(),
            ));
        }
        // the args as the create route takes them
        serde_json::from_str(&json)
            .map_err(|err| Stop::Refuse(format!("the arguments don't parse: {err}")))
    }

    /// Reserve a row's new id, derived from its cancelled reaction and attempt, writing it
    /// `resubmitting` with the id, conditional on its status; a row that holds an id keeps it
    ///
    /// Returns the id, or `None` when another writer moved the row first.
    ///
    /// # Arguments
    ///
    /// * `state` - The pass's state
    /// * `row` - The row as read, which becomes the reserved row
    async fn reserve(
        state: &PassState,
        row: &mut DrainRequestRow,
    ) -> Result<Option<String>, MigrateError> {
        // an id reserved earlier is set once and kept
        if let Some(entry) = &row.entry {
            return Ok(Some(entry.clone()));
        }
        // the request's own id, written before anything is created; overlapping runs that both
        // read the row unreserved pick the same id, so the create's existence check lets only one
        // reaction be made even where the row's write isn't conditional
        let entry = reserved_id(&row.id, row.attempt.as_deref()).to_string();
        let mut reserving = row.clone();
        reserving.status = DrainStatus::Resubmitting;
        reserving.entry = Some(entry.clone());
        reserving.updated = None;
        if !state.store.update_status_if(&reserving, row.status).await? {
            return Ok(None);
        }
        *row = reserving;
        Ok(Some(entry))
    }

    /// Resubmit one row in a run, moving it forward one status at a time
    ///
    /// # Arguments
    ///
    /// * `ctx` - The run's context
    /// * `state` - The pass's state
    /// * `id` - The cancelled reaction's id
    async fn resubmit(
        ctx: &RunContext,
        state: &PassState,
        id: &str,
    ) -> Result<Outcome, MigrateError> {
        // the row as it is now, which another run may have moved
        let Some(mut row) = state.store.get(id).await? else {
            return Ok(Outcome::Current);
        };
        // a request that doesn't parse can never be created
        let Ok(mut request) = row.request() else {
            let reason = "the recorded request doesn't parse".to_owned();
            return settle(&state.store, &row, DrainStatus::Refused, reason).await;
        };
        // whether the row is of the attempt being resubmitted
        let in_window = state
            .window
            .is_some_and(|window| row.attempt.as_deref().and_then(parse_at) == Some(window));
        // finished rows stay as they are, and stale ones unless asked for
        let include_stale = ctx.flags.include_stale;
        match row.status {
            DrainStatus::Resubmitted | DrainStatus::Refused => return Ok(Outcome::Current),
            DrainStatus::Stale if !include_stale => return Ok(Outcome::Current),
            DrainStatus::Recorded if !in_window && !include_stale => {
                // another attempt's row, or one no recorded drain run made
                let reason = if state.window.is_some() {
                    "recorded outside the drain attempt being resubmitted; not resubmitted"
                } else {
                    "no finished drain attempt is open for resubmission (a stop, restore, or \
                     state reset came after the drain, or it hasn't finished); not resubmitted"
                };
                return settle(&state.store, &row, DrainStatus::Stale, reason.to_owned()).await;
            }
            _ => (),
        }
        // a reserved id is checked first: its reaction exists, or may be created again only by
        // the operator's run of the same attempt or by the run that reserved it
        if let Some(entry) = row.entry.clone() {
            if reaction_exists(ctx, &request.group, &entry).await? {
                return Self::resubmitted(ctx, &state.store, &row, &request, &entry).await;
            }
            let ours = state
                .reserved
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(id);
            let may_create = ours || (state.flagged && in_window);
            if !may_create {
                let reason = MISSING_RESERVATION.to_owned();
                return settle(&state.store, &row, DrainStatus::Refused, reason).await;
            }
        } else if !in_window && !include_stale {
            // nothing outside the attempt is created without a reservation
            return Ok(Outcome::Current);
        }
        // every check, binding first in the operator's run
        let (user, group, pipeline, args) =
            match Self::checks(ctx, state, &mut row, &mut request, in_window).await {
                Ok(checked) => checked,
                Err(Stop::Refuse(reason)) => {
                    return settle(&state.store, &row, DrainStatus::Refused, reason).await;
                }
                Err(Stop::Retry(err)) => return Err(err),
            };
        // a test's fault before anything is reserved
        fault(&ctx.backends.namespace, id, ResubmitFault::BeforeReserve)?;
        // reserve the new id before creating anything, unless the row holds one
        let Some(entry) = Self::reserve(state, &mut row).await? else {
            return Ok(Outcome::Current);
        };
        // remember the reservation, so a retry of this pass creates the reaction it reserved
        state
            .reserved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.to_owned());
        // a test's fault between the reservation and the create
        fault(&ctx.backends.namespace, id, ResubmitFault::AfterReserve)?;
        // the id the reaction is created under
        let reserved = Uuid::parse_str(&entry).map_err(|err| {
            MigrateError::Systematic(format!("the reserved id {entry} isn't an id: {err}"))
        })?;
        // the request as its creator made it, tagged with the old id, with no parent
        let create = reaction_request(&request, id, args);
        // create it as its creator, boxing the create's large future
        let shared = writer(ctx, state).await?;
        let created = Box::pin(Reaction::create_with_id(
            &user, &group, &pipeline, create, reserved, &shared,
        ))
        .await;
        match created {
            Ok(_) => (),
            // another run created it first, which the reservation check settles
            Err(err) if err.status().as_u16() == 409 => {
                if !reaction_exists(ctx, &request.group, &entry).await? {
                    return Err(MigrateError::Backend(format!(
                        "the reserved reaction {entry} conflicted but wasn't found"
                    )));
                }
            }
            Err(err) => match api_stop("the reaction wasn't created", &err) {
                Stop::Refuse(reason) => {
                    return settle(&state.store, &row, DrainStatus::Refused, reason).await;
                }
                Stop::Retry(err) => return Err(err),
            },
        }
        // a test's fault between the create and the row's update
        fault(&ctx.backends.namespace, id, ResubmitFault::AfterCreate)?;
        // the reaction exists under the reserved id
        Self::resubmitted(ctx, &state.store, &row, &request, &entry).await
    }

    /// Count every row by status and whether one of the window's attempt is still open
    ///
    /// # Arguments
    ///
    /// * `state` - The pass's state
    /// * `limit` - How many rows to read per page
    async fn tally(
        state: &PassState,
        limit: usize,
    ) -> Result<(HashMap<DrainStatus, u64>, bool), MigrateError> {
        // every row, a page at a time
        let mut counts = HashMap::new();
        let mut open = false;
        let mut after: Option<String> = None;
        loop {
            let page = state.store.list_page(after.as_deref(), limit).await?;
            for row in &page {
                *counts.entry(row.status).or_insert(0) += 1;
                let in_window = state.window.is_some_and(|window| {
                    row.attempt.as_deref().and_then(parse_at) == Some(window)
                });
                open |= in_window
                    && matches!(
                        row.status,
                        DrainStatus::Recorded | DrainStatus::Resubmitting
                    );
            }
            // a short page is the last
            if page.len() < limit {
                return Ok((counts, open));
            }
            after = page.last().map(|row| row.id.clone());
        }
    }
}

#[async_trait]
impl MigrationRunner for ResubmitReactions {
    /// Refuse before revision `2026-10-v04` or with an in-pause flag for another attempt, find the
    /// attempt window, and make one slice over every drain request row
    ///
    /// # Arguments
    ///
    /// * `n` - How many slices were asked for
    /// * `ctx` - The run's context
    async fn slices(&self, n: u32, ctx: &RunContext) -> Result<Vec<Slice>, MigrateError> {
        // the record's revision and history
        let _ = n;
        let session = &ctx.backends.scylla;
        let status = read_status(session, &ctx.backends.conf, &Catalog::PRODUCTION)
            .await
            .map_err(|err| MigrateError::Backend(format!("the record didn't read: {err}")))?;
        let RecordStatus::Recorded { record, .. } = status else {
            return Err(MigrateError::Systematic(format!(
                "no upgrade record; the resubmission runs once revision {REVISION} is recorded"
            )));
        };
        // the conversions must have finished, or a request would be refused for a passing reason
        let needed = REVISION
            .parse::<RevisionId>()
            .map_err(|err| MigrateError::Systematic(format!("{REVISION}: {err}")))?;
        if record.revision < needed {
            return Err(MigrateError::Systematic(format!(
                "the recorded revision is {}; the resubmission runs once revision {REVISION} is \
                 recorded",
                record.revision
            )));
        }
        // the in-pause flag must name the window's attempt
        let window = attempt_window(&record.history);
        let flagged = match &ctx.flags.in_pause {
            None => false,
            Some(attempt) if parse_at(attempt).is_some_and(|at| Some(at) == window) => true,
            Some(attempt) => {
                return Err(MigrateError::Systematic(format!(
                    "--in-pause {attempt} doesn't name the drain attempt open for resubmission \
                     ({})",
                    window.map_or_else(|| "none".to_owned(), |at| at.to_rfc3339())
                )));
            }
        };
        // this pass's state, replacing any earlier pass's
        let store = DrainRequestStore::prepare(session.clone(), &ctx.backends.namespace).await?;
        let state = PassState {
            store: Arc::new(store),
            window,
            flagged,
            reserved: std::sync::Mutex::new(HashSet::new()),
            shared: tokio::sync::OnceCell::new(),
        };
        PASSES
            .lock()
            .await
            .insert(ctx.backends.namespace.clone(), Arc::new(state));
        Ok(vec![Slice {
            index: 0,
            start: serde_json::Value::Null,
            end: serde_json::Value::Null,
        }])
    }

    /// Read the next page of rows by id, keeping the ones this pass may move
    ///
    /// # Arguments
    ///
    /// * `slice` - The slice to read
    /// * `after` - The last id read, or null
    /// * `ctx` - The run's context
    async fn next_batch(
        &self,
        slice: &Slice,
        after: &serde_json::Value,
        ctx: &RunContext,
    ) -> Result<Batch, MigrateError> {
        // a page of rows after the last id
        let _ = slice;
        let state = pass(ctx).await?;
        let limit = ctx.limits.batch_rows.max(1);
        let page = state.store.list_page(after.as_str(), limit).await?;
        // the unfinished rows, and stale ones when asked for
        let records = page
            .iter()
            .filter(|row| match row.status {
                DrainStatus::Recorded | DrainStatus::Resubmitting => true,
                DrainStatus::Stale => ctx.flags.include_stale,
                DrainStatus::Resubmitted | DrainStatus::Refused => false,
            })
            .map(|row| Record::Scylla {
                key: row.id.clone(),
                columns: std::collections::BTreeMap::new(),
            })
            .collect();
        // resume after the last row read, done after a short page
        let next = page.last().map_or_else(
            || after.clone(),
            |row| serde_json::Value::from(row.id.clone()),
        );
        Ok(Batch {
            records,
            next,
            done: page.len() < limit,
        })
    }

    /// Check mode counts each row that would move; a run resubmits, refuses, or marks it stale
    ///
    /// # Arguments
    ///
    /// * `record` - The row's key
    /// * `mode` - `Run` or `Check`
    /// * `ctx` - The run's context
    async fn process(
        &self,
        record: &Record,
        mode: RunMode,
        ctx: &RunContext,
    ) -> Result<Outcome, MigrateError> {
        // only rows' keys are read
        let Record::Scylla { key, .. } = record else {
            return Ok(Outcome::Current);
        };
        // a check writes nothing: each row it reads would move
        if mode == RunMode::Check {
            return Ok(Outcome::Write(record.clone()));
        }
        // the resubmission's future is large, so it lives on the heap
        let state = pass(ctx).await?;
        Box::pin(Self::resubmit(ctx, &state, key)).await
    }

    /// The work was done as each row was processed
    ///
    /// # Arguments
    ///
    /// * `record` - The row's key
    /// * `ctx` - The run's context
    async fn write(&self, record: &Record, ctx: &RunContext) -> Result<(), MigrateError> {
        // nothing is left to write
        let _ = (record, ctx);
        Ok(())
    }

    /// Record the resubmission's history entry once no row of the window's attempt is left
    /// recorded or resubmitting
    ///
    /// # Arguments
    ///
    /// * `ctx` - The run's context
    async fn finish(&self, ctx: &RunContext) -> Result<(), MigrateError> {
        // only a run of an open attempt records anything
        if ctx.mode != RunMode::Run {
            return Ok(());
        }
        // without an attempt window there is no attempt to record as resubmitted
        let state = pass(ctx).await?;
        let Some(window) = state.window else {
            return Ok(());
        };
        // every row as it is now
        let (counts, open) = Self::tally(&state, ctx.limits.batch_rows.max(1)).await?;
        if open {
            return Ok(());
        }
        // the entry the operator reads, with the counts
        let count = |status| counts.get(&status).copied().unwrap_or(0);
        let entry = AppliedStep {
            revision: REVISION.to_owned(),
            step: ID.to_owned(),
            outcome: StepState::Done,
            message: Some(format!(
                "resubmitted the requests of the drain attempt at {}",
                window.to_rfc3339()
            )),
            at: Some(Utc::now().to_rfc3339()),
            by: Some(super::run::ran_by()),
            work: Some(format!(
                "resubmitted {}, refused {}, stale {}",
                count(DrainStatus::Resubmitted),
                count(DrainStatus::Refused),
                count(DrainStatus::Stale)
            )),
            job: std::env::var(super::context::JOB_NAME_ENV).ok(),
            chain: vec![ID.to_owned()],
            ..AppliedStep::default()
        };
        append_history(&ctx.backends.scylla, &ctx.backends.conf, &[entry])
            .await
            .map_err(|err| MigrateError::Backend(format!("the history didn't record: {err}")))?;
        Ok(())
    }
}

/// The migration's registry entry: run by the operator inside the conversion pause, never as a
/// catalog step
pub const RESUBMIT_REACTIONS: MigrationDef = MigrationDef {
    id: ID,
    backend: Backend::Scylla,
    support: Support::Yes,
    runner: &ResubmitReactions,
    pass: ID,
    chain: &[ID],
    group_max: 5,
    families: &["drain requests"],
    has_views: false,
};

#[cfg(test)]
mod tests {
    use super::reserved_id;

    /// The reserved id is the same for the same request and attempt, differs for another attempt
    /// or reaction, and is a random-variant UUID
    #[test]
    fn the_reserved_id_is_derived_from_the_request() {
        // the same inputs give the same id
        let old = "0d9e3c52-6a8e-4c51-9b1f-2a7e5d4c3b21";
        let attempt = Some("2026-11-05T01:00:00.250+00:00");
        let id = reserved_id(old, attempt);
        assert_eq!(id, reserved_id(old, attempt));
        // another attempt, no attempt, or another reaction gives another id
        assert_ne!(id, reserved_id(old, Some("2026-11-04T01:00:00Z")));
        assert_ne!(id, reserved_id(old, None));
        assert_ne!(
            id,
            reserved_id("5b3f8e21-1c4d-4e8a-a2b7-9c6d0e1f2a34", attempt)
        );
        // the id is a version 4 UUID
        assert_eq!(id.get_version_num(), 4);
        assert_eq!(id.get_variant(), uuid::Variant::RFC4122);
    }
}
