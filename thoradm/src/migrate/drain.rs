//! The `drain-reactions` check and cancel
//!
//! Reactions that are still `Created` or `Started` hold queued jobs and running workers shaped for
//! the stage path, which the tool path can't read, so the conversions must not start while any
//! exist. The check only reads Redis and Scylla and asks the operator to wait while any reaction
//! is in flight or any stage-path worker is still registered. The wait it reports holds counts
//! only, since it reaches the cluster's status; the reactions and workers that hold the drain are
//! named on stderr, which reaches the admin's terminal or the check Job's logs but never the
//! stored report. The admin lets them finish or deletes the stuck ones with
//! `thorctl reactions delete` while the previous API still runs.
//!
//! The run, which the operator starts once an admin approves cancelling in-flight work, cancels
//! that work: for each in-flight reaction, children before parents, it records the reaction's
//! request as a `drain_request` row (its arguments in their own expiring row, named in the
//! request only by hash and size), deletes its jobs from every index the previous release kept
//! them in, and fails it with the reason. It then removes the stage-path workers' registrations.
//! A request already recorded is kept as it is, so an interrupted run finishes on the next one.
//! Its applied report, which is also the Job's result row, holds counts and the removed workers
//! the operator deletes pods for, and never a reaction id, creator, pipeline, or argument.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use redis::aio::MultiplexedConnection;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};
use thorium::models::backends::db;
use thorium::models::schema::drain_requests::{
    CreatorMembership, DrainRequest, DrainRequestRow, DrainRequestStore, DrainStatus, IDLE_DAYS,
    MAX_ARGS_BYTES, MAX_REQUESTS, PipelineIdentity, SERVICE_USERS, args_sha256,
};
use thorium::models::schema::{
    AppliedReport, Batch, CheckWait, DrainCancelReport, DrainWorker, IssueReport,
    MAX_DRAIN_WORKERS, MigrateError, MigrationRunner, ObjectClass, Outcome, PassCounts, RunContext,
    RunMode, Slice, backend_error, write_job_result,
};
use thorium::models::upgrades::{IssueCategory, IssueReason, Record};
use thorium::models::{
    ImageScaler, Reaction, ReactionStatus, RepoDependencyRequest, WorkerDeleteMap,
};
use thorium::utils::{ApiError, Shared};
use tracing::info;
use uuid::Uuid;

use super::registry::{Backend, MigrationDef, Support};
use crate::shared::termination::job_uid;

/// The id of this migration
pub const ID: &str = "drain-reactions";

/// How long the operator waits before it runs the check again, in seconds
pub const RETRY_AFTER_SECS: u64 = 15;

/// The statuses a reaction holds while its work is unfinished
const IN_FLIGHT: [&str; 2] = ["Created", "Started"];

/// What a cell shows when its value is unknown
const UNKNOWN: &str = "-";

/// The most rows each table of the named report shows, so a large backlog doesn't flood the check
/// Job's log on every check
pub const TABLE_ROWS: usize = 50;

/// Whether a stored reaction status is one of an unfinished reaction
///
/// A status is stored as a JSON string, so the quotes are dropped before it is compared.
///
/// # Arguments
///
/// * `stored` - The `status` field of a reaction's data hash
#[must_use]
pub fn is_in_flight(stored: &str) -> bool {
    // the quotes around a JSON string aren't part of the status
    IN_FLIGHT.contains(&unquote(stored))
}

/// A stored hash field without the quotes of a JSON string
///
/// # Arguments
///
/// * `stored` - The field as stored
fn unquote(stored: &str) -> &str {
    // a JSON string's quotes aren't part of its value
    stored.trim().trim_matches('"')
}

/// Build the text that says what the step waits for and how to clear it
///
/// The text holds counts only: it reaches the cluster's status, so it never names a reaction, a
/// group, or an argument. It points to the check, which names them.
///
/// # Arguments
///
/// * `reactions` - How many reactions are `Created` or `Started`
/// * `workers` - How many stage-path workers are still registered
#[must_use]
pub fn wait_reason(reactions: u64, workers: u64) -> Option<String> {
    // nothing in flight means nothing to wait for
    if reactions == 0 && workers == 0 {
        return None;
    }
    // say what is still in flight
    let mut found = Vec::new();
    if reactions > 0 {
        found.push(format!(
            "{reactions} reaction(s) are still Created or Started"
        ));
    }
    if workers > 0 {
        found.push(format!(
            "{workers} stage-path worker(s) are still registered"
        ));
    }
    // and where the admin sees which ones and how to clear them
    Some(format!(
        "{}. The conversion waits until none remain, with Thorium running normally. See which \
         ones with `thoradm migrate check --id {ID}`; let them finish, or delete a stuck one \
         with `thorctl reactions delete` while the previous API is still running (see \
         Upgrading to Agentic Workflows, \"The drain\"). Workers exit on their own once they \
         find no job.",
        found.join(" and ")
    ))
}

/// The fields of a reaction's data hash a row of the named report shows
const REACTION_FIELDS: [&str; 5] = ["status", "id", "group", "pipeline", "sla"];

/// The fields of a worker's data hash a row of the named report shows, and its tool instance
const WORKER_FIELDS: [&str; 7] = [
    "name",
    "cluster",
    "node",
    "scaler",
    "status",
    "heart_beat",
    "tool_instance",
];

/// How many keys one SCAN asks for, which also bounds how many hashes one pipeline reads
const SCAN_COUNT: usize = 500;

/// A Redis error as a migration error
///
/// # Arguments
///
/// * `err` - The error
fn redis_error(err: impl std::fmt::Display) -> MigrateError {
    // every Redis failure may pass on a retry
    MigrateError::Backend(err.to_string())
}

/// Read one page of the keys of a Redis family
///
/// # Arguments
///
/// * `conn` - The Redis connection
/// * `pattern` - The key pattern to scan for
/// * `cursor` - Where the scan stands, 0 to start
async fn scan_page(
    conn: &mut MultiplexedConnection,
    pattern: &str,
    cursor: u64,
) -> Result<(u64, Vec<String>), MigrateError> {
    // a cursor of 0 in the reply means the scan is done
    redis::cmd("SCAN")
        .arg(cursor)
        .arg("MATCH")
        .arg(pattern)
        .arg("COUNT")
        .arg(SCAN_COUNT)
        .query_async(conn)
        .await
        .map_err(redis_error)
}

/// A reaction that holds the drain, as the named report shows it
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockingReaction {
    /// The reaction's id
    pub id: String,
    /// The reaction's group
    pub group: String,
    /// The reaction's pipeline
    pub pipeline: String,
    /// The reaction's status, `Created` or `Started`
    pub status: String,
    /// When the reaction's first status log entry was written, which is when it was created
    pub created: Option<DateTime<Utc>>,
    /// When the reaction's newest status log entry was written
    pub last_activity: Option<DateTime<Utc>>,
    /// The reaction's SLA deadline, which stands in when it has no status log
    pub sla: Option<DateTime<Utc>>,
    /// Whether the pipeline's status set for its status lists it; an unlisted reaction is an
    /// orphan the status listings miss (`thorctl reactions get` or `delete` with `--status`),
    /// though the pipeline's listing without a status may still show it
    pub listed: bool,
}

/// A worker registered by the previous release, as the named report shows it
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageWorker {
    /// The worker's name
    pub name: String,
    /// The scaler that spawned it
    pub scaler: String,
    /// The cluster it runs in
    pub cluster: String,
    /// The node it runs on
    pub node: String,
    /// The worker's status
    pub status: String,
    /// When the worker last sent a heartbeat, if it ever did
    pub heart_beat: Option<DateTime<Utc>>,
    /// Whether its cluster and node are among the API's registered nodes
    pub node_registered: bool,
}

/// What holds the drain: every in-flight reaction and every stage-path worker
///
/// The named report shows at most [`TABLE_ROWS`] rows of each, orphans and workers on unregistered
/// nodes first, so those are never the ones left out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DrainReport {
    /// The reactions that are `Created` or `Started`, by group, pipeline, and id
    pub reactions: Vec<BlockingReaction>,
    /// The registered workers without a tool instance, by name
    pub workers: Vec<StageWorker>,
}

/// Parse a timestamp stored as a JSON string, or as a bare RFC 3339 string
///
/// # Arguments
///
/// * `stored` - The stored timestamp
fn parse_time(stored: &str) -> Option<DateTime<Utc>> {
    // a missing heartbeat is stored as JSON null, which parses as nothing
    DateTime::parse_from_rfc3339(unquote(stored))
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

/// The timestamp of a status log entry
///
/// # Arguments
///
/// * `entry` - The entry as stored, a JSON status update
fn log_time(entry: Option<&str>) -> Option<DateTime<Utc>> {
    // only the timestamp of the update is read, so an entry of any action parses
    let entry: serde_json::Value = serde_json::from_str(entry?).ok()?;
    entry.get("timestamp")?.as_str().and_then(parse_time)
}

/// Read the in-flight reactions among one page of reaction data keys
///
/// Only the status of each hash is read first; the fields a row shows are read for in-flight
/// reactions alone, with the status again in case the reaction finished in between, so the large
/// fields (arguments, samples, plan) are never read.
///
/// # Arguments
///
/// * `conn` - The Redis connection
/// * `ns` - The namespace the keys are in
/// * `keys` - The data hash keys of one scan page
async fn page_reactions(
    conn: &mut MultiplexedConnection,
    ns: &str,
    keys: &[String],
) -> Result<Vec<BlockingReaction>, MigrateError> {
    // the status of every hash on the page, in one round trip
    let mut pipe = redis::pipe();
    for key in keys {
        pipe.cmd("HGET").arg(key).arg("status");
    }
    let statuses: Vec<Option<String>> = pipe.query_async(conn).await.map_err(redis_error)?;
    // a finished reaction, or a hash without a status, holds nothing
    let in_flight = keys
        .iter()
        .zip(statuses)
        .filter(|(_, status)| status.as_deref().is_some_and(is_in_flight))
        .map(|(key, _)| key)
        .collect::<Vec<_>>();
    // a page of finished reactions needs no more reads
    if in_flight.is_empty() {
        return Ok(Vec::new());
    }
    // the fields each in-flight reaction's row shows
    let mut pipe = redis::pipe();
    for key in &in_flight {
        pipe.cmd("HMGET").arg(*key).arg(&REACTION_FIELDS[..]);
    }
    let fields: Vec<Vec<Option<String>>> = pipe.query_async(conn).await.map_err(redis_error)?;
    // a reaction deleted or finished since its status was read holds nothing; the key, which is
    // `<ns>:reaction_data:<group>:<id>`, stands in for a missing id or group
    let mut rows = Vec::with_capacity(in_flight.len());
    for (key, fields) in in_flight.into_iter().zip(fields) {
        let mut fields = fields.into_iter();
        let mut next = || fields.next().flatten();
        let (status, id, group, pipeline, sla) = (next(), next(), next(), next(), next());
        let Some(status) = status.filter(|status| is_in_flight(status)) else {
            continue;
        };
        let mut parts = key.rsplitn(3, ':');
        let key_id = parts.next().unwrap_or_default();
        let key_group = parts.next().unwrap_or_default();
        rows.push(BlockingReaction {
            id: id.unwrap_or_else(|| key_id.to_owned()),
            group: group.unwrap_or_else(|| key_group.to_owned()),
            pipeline: pipeline.unwrap_or_else(|| UNKNOWN.to_owned()),
            status: unquote(&status).to_owned(),
            created: None,
            last_activity: None,
            sla: sla.as_deref().and_then(parse_time),
            listed: false,
        });
    }
    // every one of them finished or went in between, so there is nothing to fill in
    if rows.is_empty() {
        return Ok(rows);
    }
    // the first and newest status log entries give each one's age and last activity
    let mut pipe = redis::pipe();
    for row in &rows {
        let logs = format!("{ns}:logs:{}:{}", row.group, row.id);
        pipe.cmd("LINDEX").arg(&logs).arg(0);
        pipe.cmd("LINDEX").arg(&logs).arg(-1);
    }
    let logs: Vec<Option<String>> = pipe.query_async(conn).await.map_err(redis_error)?;
    // a reaction the status listings show is in its pipeline's status set for its status
    let mut pipe = redis::pipe();
    for row in &rows {
        pipe.cmd("SISMEMBER")
            .arg(format!(
                "{ns}:reactions:{}:{}:{}",
                row.group, row.pipeline, row.status
            ))
            .arg(&row.id);
    }
    let listed: Vec<bool> = pipe.query_async(conn).await.map_err(redis_error)?;
    // fill each row in
    for ((row, pair), listed) in rows.iter_mut().zip(logs.chunks(2)).zip(listed) {
        row.created = log_time(pair.first().and_then(Option::as_deref));
        row.last_activity = log_time(pair.get(1).and_then(Option::as_deref));
        row.listed = listed;
    }
    Ok(rows)
}

/// Read the reactions that are still `Created` or `Started`
///
/// # Arguments
///
/// * `ctx` - The run's context
async fn blocking_reactions(ctx: &RunContext) -> Result<Vec<BlockingReaction>, MigrateError> {
    // walk the reaction data hashes a page at a time, keeping the in-flight ones
    let ns = &ctx.backends.namespace;
    let pattern = format!("{ns}:reaction_data:*");
    let mut conn = ctx.backends.redis.get().await.map_err(redis_error)?;
    let mut found = Vec::new();
    let mut cursor = 0_u64;
    loop {
        let (next, keys) = scan_page(&mut conn, &pattern, cursor).await?;
        if !keys.is_empty() {
            found.extend(page_reactions(&mut conn, ns, &keys).await?);
        }
        // a cursor back at the start ends the scan
        cursor = next;
        if cursor == 0 {
            break;
        }
    }
    // list them in a stable order
    found.sort_by(|a, b| (&a.group, &a.pipeline, &a.id).cmp(&(&b.group, &b.pipeline, &b.id)));
    Ok(found)
}

/// Whether a node is registered with the API, as a row of the `nodes` table
///
/// # Arguments
///
/// * `cluster` - The node's cluster
/// * `node` - The node's name
/// * `ctx` - The run's context
async fn node_registered(
    cluster: &str,
    node: &str,
    ctx: &RunContext,
) -> Result<bool, MigrateError> {
    // look the node up by its full primary key
    let ns = &ctx.backends.namespace;
    let rows = ctx
        .backends
        .scylla
        .query_unpaged(
            format!("SELECT node FROM {ns}.nodes WHERE cluster = ? AND node = ?"),
            (cluster, node),
        )
        .await
        .map_err(|err| backend_error("read a node", &err, false))?
        .into_rows_result()
        .map_err(|err| backend_error("read a node", &err, false))?;
    Ok(rows.rows_num() > 0)
}

/// Read the registered workers the tool path can't read: those without a tool instance
///
/// # Arguments
///
/// * `ctx` - The run's context
async fn stage_workers(ctx: &RunContext) -> Result<Vec<StageWorker>, MigrateError> {
    // walk the worker data hashes a page at a time
    let pattern = format!("{}:worker_data:*", ctx.backends.namespace);
    let mut conn = ctx.backends.redis.get().await.map_err(redis_error)?;
    let mut nodes: BTreeMap<(String, String), bool> = BTreeMap::new();
    let mut found = Vec::new();
    let mut cursor = 0_u64;
    loop {
        let (next, keys) = scan_page(&mut conn, &pattern, cursor).await?;
        cursor = next;
        // the fields each worker's row shows, and its tool instance, in one round trip
        let mut pipe = redis::pipe();
        for key in &keys {
            pipe.cmd("HMGET").arg(key).arg(&WORKER_FIELDS[..]);
        }
        let hashes: Vec<Vec<Option<String>>> = if keys.is_empty() {
            Vec::new()
        } else {
            pipe.query_async(&mut *conn).await.map_err(redis_error)?
        };
        // keep each worker the previous release registered, with its fields in `WORKER_FIELDS` order
        for (key, fields) in keys.iter().zip(hashes) {
            let [
                name,
                cluster,
                node,
                scaler,
                status,
                heart_beat,
                tool_instance,
            ]: [Option<String>; 7] = fields.try_into().unwrap_or_default();
            // a worker of the tool path, or one deleted since the scan, holds nothing; a worker the
            // previous release registered has no `tool_instance` field
            if tool_instance.is_some()
                || [&name, &cluster, &node, &scaler, &status, &heart_beat]
                    .iter()
                    .all(|field| field.is_none())
            {
                continue;
            }
            // a node without a name can't be registered; each node is looked up once
            let registered = match (&cluster, &node) {
                (Some(cluster), Some(node)) => {
                    let pair = (unquote(cluster).to_owned(), unquote(node).to_owned());
                    if let Some(registered) = nodes.get(&pair) {
                        *registered
                    } else {
                        let registered = node_registered(&pair.0, &pair.1, ctx).await?;
                        nodes.insert(pair, registered);
                        registered
                    }
                }
                _ => false,
            };
            // the key ends in the worker's name, which stands in for a missing field
            let shown = |field: Option<String>| {
                field.map_or_else(|| UNKNOWN.to_owned(), |value| unquote(&value).to_owned())
            };
            found.push(StageWorker {
                name: name.unwrap_or_else(|| key.rsplit(':').next().unwrap_or_default().to_owned()),
                scaler: shown(scaler),
                status: shown(status),
                heart_beat: heart_beat.as_deref().and_then(parse_time),
                cluster: shown(cluster),
                node: shown(node),
                node_registered: registered,
            });
        }
        // a cursor back at the start ends the scan
        if cursor == 0 {
            break;
        }
    }
    // list them in a stable order
    found.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(found)
}

/// Read every reaction and worker that holds the drain
///
/// # Arguments
///
/// * `ctx` - The run's context
pub async fn collect(ctx: &RunContext) -> Result<DrainReport, MigrateError> {
    // the reactions from Redis, then the workers with their nodes from Scylla
    Ok(DrainReport {
        reactions: blocking_reactions(ctx).await?,
        workers: stage_workers(ctx).await?,
    })
}

/// Format how long ago something happened in its two largest units, such as `3h12m`
///
/// # Arguments
///
/// * `secs` - How many seconds ago it happened
#[must_use]
pub fn short_age(secs: i64) -> String {
    // a time in the future, from a skewed clock, counts as now
    let secs = secs.max(0);
    let (days, hours, mins) = (secs / 86_400, secs % 86_400 / 3_600, secs % 3_600 / 60);
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{mins}m")
    } else if mins > 0 {
        format!("{mins}m{}s", secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// Format a timestamp to the second, or the unknown marker
///
/// # Arguments
///
/// * `time` - The timestamp
fn stamp(time: Option<DateTime<Utc>>) -> String {
    // RFC 3339 in UTC without fractions
    time.map_or_else(
        || UNKNOWN.to_owned(),
        |time| time.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    )
}

/// Lay rows out as a table whose columns are as wide as their widest cell
///
/// # Arguments
///
/// * `header` - The column titles
/// * `rows` - The cells of each row
fn table(header: &[&str], rows: &[Vec<String>]) -> String {
    // each column is as wide as its widest cell, and columns are two spaces apart
    let mut widths: Vec<usize> = header.iter().map(|title| title.len()).collect();
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.len());
        }
    }
    // a line pads each cell to its column's width
    let line = |cells: Vec<&str>| {
        let padded = cells
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect::<Vec<_>>();
        format!("{}\n", padded.join("  ").trim_end())
    };
    // the header, then each row
    let mut text = line(header.to_vec());
    for row in rows {
        text.push_str(&line(row.iter().map(String::as_str).collect()));
    }
    text
}

impl DrainReport {
    /// Whether nothing holds the drain
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.reactions.is_empty() && self.workers.is_empty()
    }

    /// Render the named report: a summary line and a table of reactions and of workers, each
    /// capped at [`TABLE_ROWS`] rows with a line saying how many more there are
    ///
    /// # Arguments
    ///
    /// * `now` - The time ages are measured to
    #[must_use]
    pub fn render(&self, now: DateTime<Utc>) -> String {
        // the summary repeats the counts of the wait
        let mut text = format!(
            "{ID} waits: {} reaction(s) Created or Started, {} stage-path worker(s) registered\n",
            self.reactions.len(),
            self.workers.len()
        );
        // a table for each kind that holds the drain
        if !self.reactions.is_empty() {
            text.push_str(&self.reaction_table(now));
        }
        // and the stage-path workers after the reactions
        if !self.workers.is_empty() {
            text.push_str(&self.worker_table());
        }
        text
    }

    /// Render the table of reactions under its title, with the line saying how many more there are
    ///
    /// # Arguments
    ///
    /// * `now` - The time ages are measured to
    fn reaction_table(&self, now: DateTime<Utc>) -> String {
        // orphans first, then the rest in their order, up to the cap; a reaction without a status
        // log shows its SLA deadline in place of its activity
        let mut reactions = self.reactions.iter().collect::<Vec<_>>();
        reactions.sort_by_key(|reaction| reaction.listed);
        let rows = reactions
            .iter()
            .take(TABLE_ROWS)
            .map(|reaction| {
                let age = reaction.created.map_or_else(
                    || UNKNOWN.to_owned(),
                    |created| short_age((now - created).num_seconds()),
                );
                let activity = match (reaction.last_activity, reaction.sla) {
                    (Some(time), _) => stamp(Some(time)),
                    (None, Some(sla)) => format!("{} (sla)", stamp(Some(sla))),
                    (None, None) => UNKNOWN.to_owned(),
                };
                let listed = if reaction.listed {
                    "yes"
                } else {
                    "no (orphan)"
                };
                vec![
                    reaction.id.clone(),
                    reaction.group.clone(),
                    reaction.pipeline.clone(),
                    reaction.status.clone(),
                    age,
                    activity,
                    listed.to_owned(),
                ]
            })
            .collect::<Vec<_>>();
        // the title, the table, and how many more there are, orphans among them
        let mut text = String::from("\nReactions holding the drain\n");
        text.push_str(&table(
            &[
                "ID",
                "GROUP",
                "PIPELINE",
                "STATUS",
                "AGE",
                "LAST ACTIVITY",
                "LISTED",
            ],
            &rows,
        ));
        let orphans = reactions
            .iter()
            .skip(TABLE_ROWS)
            .filter(|reaction| !reaction.listed)
            .count();
        text.push_str(&more(reactions.len(), "reaction(s)", orphans, "orphans"));
        text
    }

    /// Render the table of stage-path workers under its title, with the line saying how many more
    /// there are
    fn worker_table(&self) -> String {
        // workers on unregistered nodes first, then the rest by name, up to the cap
        let mut workers = self.workers.iter().collect::<Vec<_>>();
        workers.sort_by_key(|worker| worker.node_registered);
        let rows = workers
            .iter()
            .take(TABLE_ROWS)
            .map(|worker| {
                let registered = if worker.node_registered { "yes" } else { "no" };
                vec![
                    worker.name.clone(),
                    worker.scaler.clone(),
                    worker.cluster.clone(),
                    worker.node.clone(),
                    worker.status.clone(),
                    stamp(worker.heart_beat),
                    registered.to_owned(),
                ]
            })
            .collect::<Vec<_>>();
        // the title, the table, and how many more there are, unregistered ones among them
        let mut text = String::from("\nStage-path workers\n");
        text.push_str(&table(
            &[
                "NAME",
                "SCALER",
                "CLUSTER",
                "NODE",
                "STATUS",
                "HEARTBEAT",
                "NODE REGISTERED",
            ],
            &rows,
        ));
        let unregistered = workers
            .iter()
            .skip(TABLE_ROWS)
            .filter(|worker| !worker.node_registered)
            .count();
        text.push_str(&more(
            workers.len(),
            "worker(s)",
            unregistered,
            "on unregistered nodes",
        ));
        text
    }
}

/// Say how many rows a table left out past its cap, and how many of those are marked, or nothing
/// when it left none out
///
/// # Arguments
///
/// * `total` - How many rows the table has
/// * `what` - What a row is, such as `reaction(s)`
/// * `marked` - How many of the rows left out are marked
/// * `mark` - What the marked rows are, such as `orphans`
fn more(total: usize, what: &str, marked: usize, mark: &str) -> String {
    // a table within its cap shows every row
    let hidden = total.saturating_sub(TABLE_ROWS);
    if hidden == 0 {
        return String::new();
    }
    // name the marked ones among them only when there are any
    if marked == 0 {
        format!("... and {hidden} more {what} not shown\n")
    } else {
        format!("... and {hidden} more {what} not shown, {marked} of them {mark}\n")
    }
}

/// Where the test fault hook stops a reaction's cancel
#[cfg(feature = "test-support")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelFault {
    /// A backend error after the reaction's request is recorded and before the reaction is failed
    AfterRecord,
}

/// The armed cancel faults: per namespace, the reaction's id and where to fail; each fires once
#[cfg(feature = "test-support")]
static CANCEL_FAULTS: std::sync::Mutex<BTreeMap<String, (String, CancelFault)>> =
    std::sync::Mutex::new(BTreeMap::new());

/// Arm a one-shot fault for one reaction's cancel in a namespace
///
/// # Arguments
///
/// * `ns` - The namespace the run works in
/// * `id` - The reaction's id
/// * `fault` - Where the cancel fails
#[cfg(feature = "test-support")]
pub fn arm_cancel_fault(ns: &str, id: &str, fault: CancelFault) {
    // a poisoned lock still holds the map
    CANCEL_FAULTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(ns.to_owned(), (id.to_owned(), fault));
}

/// Fire the fault armed for a reaction's cancel at a point, once
///
/// # Arguments
///
/// * `ns` - The namespace the run works in
/// * `id` - The reaction's id
/// * `at` - Where the cancel stands
#[cfg(feature = "test-support")]
fn fire_cancel_fault(ns: &str, id: &str, at: CancelFault) -> Result<(), MigrateError> {
    // take the namespace's fault only when it names this reaction and point
    let mut faults = CANCEL_FAULTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if faults.get(ns) != Some(&(id.to_owned(), at)) {
        return Ok(());
    }
    faults.remove(ns);
    // a systematic stop isn't retried, so the run ends with the cancel half done
    Err(MigrateError::Systematic(format!(
        "the armed {at:?} fault stopped the cancel"
    )))
}

/// The Job UIDs tests stand in for a pod's own, per namespace, so a run in a test leaves its result
/// row as a run in a Job does
#[cfg(feature = "test-support")]
static RESULT_UIDS: std::sync::Mutex<BTreeMap<String, String>> =
    std::sync::Mutex::new(BTreeMap::new());

/// Make the cancel runs in a namespace leave their result row under a UID, as a run in a Job does
///
/// # Arguments
///
/// * `ns` - The namespace the runs work in
/// * `uid` - The UID to write the result row under
#[cfg(feature = "test-support")]
pub fn set_result_uid(ns: &str, uid: &str) {
    // a poisoned lock still holds the map
    RESULT_UIDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(ns.to_owned(), uid.to_owned());
}

/// Get the UID of the Job a run is in, which its result row is written under; a run by hand has none
///
/// # Arguments
///
/// * `ns` - The namespace the run works in, which a test may have set a UID for
fn result_uid(ns: &str) -> Option<String> {
    // a test's stand-in for the namespace, when one is set
    #[cfg(feature = "test-support")]
    if let Some(uid) = RESULT_UIDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(ns)
        .cloned()
    {
        return Some(uid);
    }
    // the pod's own label, passed in by the Job's spec
    let _ = ns;
    job_uid()
}

/// The revision whose drain this is, named in each cancelled reaction's status log when the
/// catalog doesn't place the migration
const REVISION: &str = "2026-10-v04";

/// Why a request whose creator is a system account or holds no role in the group is refused
const NO_ROLE: &str = "created by a system account or by a user without a role in the group; \
                       re-run the hunt or resubmit by hand";

/// The group roles a creator's membership is read from, in order, with the name a request
/// records for each; only the first [`HELD_ROLES`] hold a role
const ROLES: [(&str, &str); 4] = [
    ("owners", "owner"),
    ("managers", "manager"),
    ("users", "user"),
    ("monitors", "monitor"),
];

/// How many of [`ROLES`] hold a role in the group
const HELD_ROLES: usize = 3;

/// The record field naming what a record is
const KIND: &str = "kind";

/// The record kind of an in-flight reaction
const KIND_REACTION: &str = "reaction";

/// The record kind of a stage-path worker
const KIND_WORKER: &str = "worker";

/// The record field saying whether a reaction's parent is in flight too, so failing the
/// reaction doesn't proceed it
const PARENT_IN_SET: &str = "parent_in_set";

/// What a cancel pass builds once per namespace
struct PassTools {
    /// The offline `Shared` the API's own reaction and worker writes go through
    shared: Arc<Shared>,
    /// The drain request rows
    store: Arc<DrainRequestStore>,
}

/// The tools each namespace's pass built, dropped when the pass finishes
static TOOLS: LazyLock<tokio::sync::Mutex<HashMap<String, Arc<PassTools>>>> =
    LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));

/// What a cancel pass did so far
#[derive(Default)]
struct Progress {
    /// The counts and worker list the applied report carries
    report: DrainCancelReport,
    /// The reactions whose request this pass inserted, so a retried cancel isn't counted as
    /// keeping its own row
    inserted: HashSet<String>,
    /// The reactions this pass cancelled, so the requests an earlier run of the attempt recorded
    /// are counted once
    cancelled: HashSet<String>,
}

/// What each namespace's cancel pass did so far, reset when a pass starts
static PROGRESS: LazyLock<Mutex<HashMap<String, Progress>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Change what a namespace's cancel pass did so far
///
/// # Arguments
///
/// * `ns` - The namespace
/// * `change` - The change to make
fn with_progress<T>(ns: &str, change: impl FnOnce(&mut Progress) -> T) -> T {
    // a poisoned lock still holds the counts
    let mut all = PROGRESS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    change(all.entry(ns.to_owned()).or_default())
}

/// Get the tools a namespace's cancel works with, building them on first use
///
/// # Arguments
///
/// * `ctx` - The run's context
async fn tools(ctx: &RunContext) -> Result<Arc<PassTools>, MigrateError> {
    // reuse the ones built for this namespace, holding the lock so only one set is ever built
    let mut all = TOOLS.lock().await;
    let ns = &ctx.backends.namespace;
    if let Some(tools) = all.get(ns) {
        return Ok(tools.clone());
    }
    // the writer from the run's config and Redis pool; an unreachable Scylla is worth retrying
    let shared = Box::pin(Shared::for_offline_writer(
        (*ctx.backends.conf).clone(),
        ctx.backends.redis.clone(),
    ))
    .await
    .map_err(|err| MigrateError::Backend(format!("the reaction writer didn't start: {err}")))?;
    // and the drain request statements
    let store = DrainRequestStore::prepare(ctx.backends.scylla.clone(), ns).await?;
    let tools = Arc::new(PassTools {
        shared: Arc::new(shared),
        store: Arc::new(store),
    });
    all.insert(ns.clone(), tools.clone());
    Ok(tools)
}

/// An in-flight reaction as the cancel's listing sees it
struct InFlight {
    /// The key of its data hash
    key: String,
    /// Its parent's id, if it is a sub reaction
    parent: Option<String>,
}

/// Read every in-flight reaction's data key and parent, by reaction id
///
/// # Arguments
///
/// * `ctx` - The run's context
async fn in_flight(ctx: &RunContext) -> Result<HashMap<String, InFlight>, MigrateError> {
    // walk the reaction data hashes a page at a time, reading each one's status and parent
    let pattern = format!("{}:reaction_data:*", ctx.backends.namespace);
    let mut conn = ctx.backends.redis.get().await.map_err(redis_error)?;
    let mut found = HashMap::new();
    let mut cursor = 0_u64;
    loop {
        let (next, keys) = scan_page(&mut conn, &pattern, cursor).await?;
        cursor = next;
        let mut pipe = redis::pipe();
        for key in &keys {
            pipe.cmd("HMGET").arg(key).arg(&["status", "parent"][..]);
        }
        let fields: Vec<(Option<String>, Option<String>)> = if keys.is_empty() {
            Vec::new()
        } else {
            pipe.query_async(&mut *conn).await.map_err(redis_error)?
        };
        // keep the unfinished ones, by the id that ends their key
        for (key, (status, parent)) in keys.into_iter().zip(fields) {
            if status.as_deref().is_some_and(is_in_flight) {
                let id = key.rsplit(':').next().unwrap_or_default().to_owned();
                let parent = parent
                    .map(|parent| unquote(&parent).to_owned())
                    .filter(|parent| !parent.is_empty() && parent != "null");
                found.insert(id, InFlight { key, parent });
            }
        }
        // a cursor back at the start ends the scan
        if cursor == 0 {
            break;
        }
    }
    Ok(found)
}

/// How many in-flight ancestors a reaction has, so children are cancelled before their parents
///
/// # Arguments
///
/// * `id` - The reaction's id
/// * `found` - Every in-flight reaction by id
fn depth(id: &str, found: &HashMap<String, InFlight>) -> usize {
    // follow the parents while they are in flight, stopping at a cycle
    let mut seen = HashSet::from([id]);
    let mut at = found
        .get(id)
        .and_then(|reaction| reaction.parent.as_deref());
    let mut depth = 0;
    while let Some(parent) = at {
        if !seen.insert(parent) {
            break;
        }
        let Some(reaction) = found.get(parent) else {
            break;
        };
        depth += 1;
        at = reaction.parent.as_deref();
    }
    depth
}

/// Read the keys of every registered worker the tool path can't read
///
/// # Arguments
///
/// * `ctx` - The run's context
async fn stage_worker_keys(ctx: &RunContext) -> Result<Vec<String>, MigrateError> {
    // walk the worker data hashes a page at a time
    let pattern = format!("{}:worker_data:*", ctx.backends.namespace);
    let mut conn = ctx.backends.redis.get().await.map_err(redis_error)?;
    let mut found = Vec::new();
    let mut cursor = 0_u64;
    loop {
        let (next, keys) = scan_page(&mut conn, &pattern, cursor).await?;
        cursor = next;
        let mut pipe = redis::pipe();
        for key in &keys {
            pipe.cmd("HMGET")
                .arg(key)
                .arg(&["tool_instance", "name", "cluster"][..]);
        }
        let fields: Vec<(Option<String>, Option<String>, Option<String>)> = if keys.is_empty() {
            Vec::new()
        } else {
            pipe.query_async(&mut *conn).await.map_err(redis_error)?
        };
        // a worker the previous release registered has no tool instance; one deleted since the
        // scan has no fields
        for (key, (tool_instance, name, cluster)) in keys.into_iter().zip(fields) {
            if tool_instance.is_none() && (name.is_some() || cluster.is_some()) {
                found.push(key);
            }
        }
        // a cursor back at the start ends the scan
        if cursor == 0 {
            break;
        }
    }
    Ok(found)
}

/// Read the next batch of what the cancel works on: in-flight reactions, children first, then
/// stage-path workers, each after the position a batch ended at
///
/// Everything is listed again for each batch, so a reaction finished or a worker gone since the
/// last batch is never read, and the position only keeps a record left as an issue from being
/// read twice.
///
/// # Arguments
///
/// * `after` - The position the last batch ended at, the sort key of its last record
/// * `ctx` - The run's context
async fn cancel_batch(after: &serde_json::Value, ctx: &RunContext) -> Result<Batch, MigrateError> {
    // the in-flight reactions, each sorted by how deep it is so children come first
    let found = in_flight(ctx).await?;
    let mut sorted = found
        .iter()
        .map(|(id, reaction)| {
            let sort = format!(
                "r:{:04}:{}",
                9999_usize.saturating_sub(depth(id, &found)),
                reaction.key
            );
            let parent_in_set = reaction
                .parent
                .as_ref()
                .is_some_and(|parent| found.contains_key(parent));
            let fields = BTreeMap::from([
                (KIND.to_owned(), KIND_REACTION.to_owned()),
                (PARENT_IN_SET.to_owned(), parent_in_set.to_string()),
            ]);
            (
                sort,
                Record::Redis {
                    key: reaction.key.clone(),
                    fields,
                },
            )
        })
        .collect::<Vec<_>>();
    // then the stage-path workers, once every reaction is failed
    for key in stage_worker_keys(ctx).await? {
        let fields = BTreeMap::from([(KIND.to_owned(), KIND_WORKER.to_owned())]);
        sorted.push((format!("w:{key}"), Record::Redis { key, fields }));
    }
    // keep what comes after the last batch, in order, up to a batch
    let after = after.as_str().unwrap_or_default();
    sorted.retain(|(sort, _)| sort.as_str() > after);
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let done = sorted.len() <= ctx.limits.batch_rows;
    sorted.truncate(ctx.limits.batch_rows);
    let next = sorted.last().map_or_else(
        || serde_json::Value::String(after.to_owned()),
        |(sort, _)| serde_json::Value::String(sort.clone()),
    );
    Ok(Batch {
        records: sorted.into_iter().map(|(_, record)| record).collect(),
        next,
        done,
    })
}

/// An issue for a reaction hash the API can't read
///
/// # Arguments
///
/// * `key` - The hash's key
fn unreadable(key: &str) -> Outcome {
    // the reaction is left as it is, and counted in its own group
    Outcome::Issue(IssueReport {
        key: key.to_owned(),
        transform: ID.to_owned(),
        class: ObjectClass::Error,
        reason: IssueReason::new(IssueCategory::MalformedJson, "unreadable-reaction"),
    })
}

/// Read the creator's role in the reaction's group from the group's combined role sets
///
/// An admin's or analyst's bypass and the creator's own list of groups are never read.
///
/// # Arguments
///
/// * `group` - The reaction's group
/// * `creator` - The reaction's creator
/// * `ctx` - The run's context
async fn membership(
    group: &str,
    creator: &str,
    ctx: &RunContext,
) -> Result<CreatorMembership, MigrateError> {
    // whether each role's combined set, which holds metagroup members too, has the creator
    let ns = &ctx.backends.namespace;
    let mut pipe = redis::pipe();
    for (set, _) in ROLES {
        pipe.cmd("SISMEMBER")
            .arg(format!("{ns}:groups_combined:{group}:{set}"))
            .arg(creator);
    }
    let mut conn = ctx.backends.redis.get().await.map_err(redis_error)?;
    let held: Vec<bool> = pipe.query_async(&mut *conn).await.map_err(redis_error)?;
    // the first role held names it
    let first = held.iter().position(|held| *held);
    Ok(CreatorMembership {
        held_role: first.is_some_and(|index| index < HELD_ROLES),
        role: first.map_or("none", |index| ROLES[index].1).to_owned(),
    })
}

/// The identity of the pipeline a reaction runs, or why its request can't be resubmitted when
/// the pipeline can't be read
///
/// # Arguments
///
/// * `reaction` - The reaction
/// * `shared` - The offline writer
async fn identity(
    reaction: &Reaction,
    shared: &Shared,
) -> Result<(PipelineIdentity, Option<String>), MigrateError> {
    // the pipeline as the API reads it
    match db::pipelines::get(&reaction.group, &reaction.pipeline, shared).await {
        Ok(pipeline) => Ok((PipelineIdentity::of(&pipeline), None)),
        Err(ApiError::Redis(err)) => Err(redis_error(err)),
        Err(err) => {
            // a pipeline that is gone or unreadable can't be checked again at resubmission
            let reason = if err.status().as_u16() == 404 {
                "the pipeline no longer exists"
            } else {
                "the pipeline can't be read"
            };
            let identity = PipelineIdentity {
                group: reaction.group.clone(),
                name: reaction.pipeline.clone(),
                creator: String::new(),
                order_sha256: String::new(),
            };
            Ok((identity, Some(reason.to_owned())))
        }
    }
}

/// Build the request row a reaction is recorded as, classified as recorded, refused, or stale
///
/// # Arguments
///
/// * `reaction` - The reaction
/// * `raw_args` - Its argument JSON exactly as its hash stores it
/// * `ctx` - The run's context
/// * `shared` - The offline writer
async fn request_row(
    reaction: &Reaction,
    raw_args: Option<&str>,
    ctx: &RunContext,
    shared: &Shared,
) -> Result<DrainRequestRow, MigrateError> {
    // its newest activity is its newest status log entry, or its SLA when it has none, and an
    // orphan is missing from its pipeline's status set
    let ns = &ctx.backends.namespace;
    let id = reaction.id.to_string();
    let status = unquote(&serde_json::to_string(&reaction.status).unwrap_or_default()).to_owned();
    let mut conn = ctx.backends.redis.get().await.map_err(redis_error)?;
    let (newest, listed): (Option<String>, bool) = redis::pipe()
        .cmd("LINDEX")
        .arg(format!("{ns}:logs:{}:{id}", reaction.group))
        .arg(-1)
        .cmd("SISMEMBER")
        .arg(format!(
            "{ns}:reactions:{}:{}:{status}",
            reaction.group, reaction.pipeline
        ))
        .arg(&id)
        .query_async(&mut *conn)
        .await
        .map_err(redis_error)?;
    drop(conn);
    let last_activity = log_time(newest.as_deref()).or(Some(reaction.sla));
    // the args are named only by the hash and size of their stored text
    let raw_args = raw_args.filter(|raw| !matches!(raw.trim(), "" | "{}" | "null"));
    let args_bytes = raw_args.map_or(0, str::len);
    // who created it, and the pipeline it runs
    let membership = membership(&reaction.group, &reaction.creator, ctx).await?;
    let (identity, pipeline_refused) = identity(reaction, shared).await?;
    let buffers = !reaction.ephemeral.is_empty();
    // classify it: refused for what a resubmission can never carry, stale when idle too long
    let idle = last_activity.is_some_and(|at| at < Utc::now() - chrono::Duration::days(IDLE_DAYS));
    let (row_status, result) = if args_bytes > MAX_ARGS_BYTES {
        (
            DrainStatus::Refused,
            Some(format!(
                "arguments too large to record ({args_bytes} bytes)"
            )),
        )
    } else if SERVICE_USERS.contains(&reaction.creator.as_str()) || !membership.held_role {
        (DrainStatus::Refused, Some(NO_ROLE.to_owned()))
    } else if buffers || reaction.has_cache {
        let reason = "the reaction had files uploaded with it or a cache, which a resubmission \
                      can't carry; resubmit by hand";
        (DrainStatus::Refused, Some(reason.to_owned()))
    } else if let Some(reason) = pipeline_refused {
        (DrainStatus::Refused, Some(reason))
    } else if idle {
        let reason =
            format!("idle for more than {IDLE_DAYS} days; resubmit by hand if still wanted");
        (DrainStatus::Stale, Some(reason))
    } else {
        (DrainStatus::Recorded, None)
    };
    // the request holds the reaction as it was, never its args
    let request = DrainRequest {
        group: reaction.group.clone(),
        pipeline: reaction.pipeline.clone(),
        samples: reaction.samples.clone(),
        repos: reaction
            .repos
            .iter()
            .map(|repo| RepoDependencyRequest {
                url: repo.url.clone(),
                commitish: repo.commitish.clone(),
                kind: repo.kind,
            })
            .collect(),
        tags: reaction.tags.clone(),
        parent: reaction.parent,
        trigger_depth: reaction.trigger_depth,
        creator: reaction.creator.clone(),
        old_status: reaction.status.clone(),
        last_activity,
        orphan: !listed,
        args_sha256: raw_args.map(args_sha256),
        args_bytes: u64::try_from(args_bytes).unwrap_or(u64::MAX),
        buffers,
        cache: reaction.has_cache,
        identity,
        membership,
        binding: None,
    };
    let value = request
        .to_json()
        .map_err(|err| MigrateError::Systematic(format!("a drain request didn't render: {err}")))?;
    // the row carries the drain step's attempt, and a final status is finished now
    let now = Utc::now();
    Ok(DrainRequestRow {
        id,
        value,
        attempt: ctx.flags.step_attempt.clone(),
        status: row_status,
        entry: None,
        result,
        revision: Some(super::revision_of(ID).unwrap_or_else(|| REVISION.to_owned())),
        started: Some(now),
        finished: (row_status != DrainStatus::Recorded).then_some(now),
        updated: Some(now),
    })
}

/// Remove a reaction's jobs from the indexes the previous release kept them in that the API's
/// job deletion no longer names: the status queue by pipeline and stage, and the scaler's
/// deadline stream entry without a tool instance
///
/// # Arguments
///
/// * `reaction` - The reaction whose jobs to remove
/// * `ctx` - The run's context
async fn remove_old_job_entries(reaction: &Reaction, ctx: &RunContext) -> Result<(), MigrateError> {
    // a reaction without jobs holds no entries
    if reaction.jobs.is_empty() {
        return Ok(());
    }
    // the fields each entry is built from, in one round trip
    let ns = &ctx.backends.namespace;
    let mut conn = ctx.backends.redis.get().await.map_err(redis_error)?;
    let mut pipe = redis::pipe();
    for job in &reaction.jobs {
        pipe.cmd("HMGET")
            .arg(format!("{ns}:job_data:{job}"))
            .arg(&["group", "pipeline", "stage", "creator", "status", "scaler"][..]);
    }
    let jobs: Vec<Vec<Option<String>>> = pipe.query_async(&mut *conn).await.map_err(redis_error)?;
    // each entry exactly as the previous release formatted it
    let rid = reaction.id;
    let mut pipe = redis::pipe();
    for (job, fields) in reaction.jobs.iter().zip(jobs) {
        let [group, pipeline, stage, creator, status, scaler]: [Option<String>; 6] =
            fields.try_into().unwrap_or_default();
        let (Some(group), Some(pipeline), Some(stage), Some(creator)) =
            (group, pipeline, stage, creator)
        else {
            continue;
        };
        if let Some(status) = status {
            pipe.cmd("ZREM")
                .arg(format!(
                    "{ns}:job_queue:{group}:{pipeline}:{stage}:{creator}:{}",
                    unquote(&status)
                ))
                .arg(format!("{{\"job\":\"{job}\",\"reaction\":\"{rid}\"}}"));
        }
        if let Some(scaler) = scaler {
            pipe.cmd("ZREM")
                .arg(format!(
                    "{ns}:streams:system:{}:deadlines",
                    unquote(&scaler)
                ))
                .arg(format!(
                    "{{\"group\":\"{group}\",\"pipeline\":\"{pipeline}\",\"stage\":\"{stage}\",\
                     \"creator\":\"{creator}\",\"job_id\":\"{job}\",\"reaction\":\"{rid}\"}}"
                ));
        }
    }
    // remove them together
    let _: () = pipe
        .atomic()
        .query_async(&mut *conn)
        .await
        .map_err(redis_error)?;
    Ok(())
}

/// The status log message of a cancelled reaction
///
/// # Arguments
///
/// * `row` - Its request row as stored
fn cancel_reason(row: &DrainRequestRow) -> String {
    // the revision, and whether the request will be resubmitted
    let revision = super::revision_of(ID).unwrap_or_else(|| REVISION.to_owned());
    match (row.status, &row.result) {
        (DrainStatus::Recorded | DrainStatus::Resubmitting, _) => format!(
            "cancelled by the upgrade to {revision}; recorded to be resubmitted once the upgrade \
             finishes"
        ),
        (_, Some(result)) => {
            format!("cancelled by the upgrade to {revision}; not resubmitted: {result}")
        }
        (_, None) => format!("cancelled by the upgrade to {revision}; not resubmitted"),
    }
}

/// Cancel one in-flight reaction: record its request and args, delete its jobs, and fail it
///
/// Every step can be repeated, so a cancel interrupted at any point finishes on the next run: a
/// request already recorded is kept as it is.
///
/// # Arguments
///
/// * `record` - The reaction's record
/// * `ctx` - The run's context
async fn cancel_reaction(record: &Record, ctx: &RunContext) -> Result<Outcome, MigrateError> {
    // the group and id end the data key
    let Record::Redis { key, fields } = record else {
        return Ok(Outcome::Current);
    };
    let ns = &ctx.backends.namespace;
    let mut parts = key.rsplitn(3, ':');
    let (Some(id), Some(group)) = (parts.next(), parts.next()) else {
        return Ok(unreadable(key));
    };
    let Ok(uuid) = Uuid::parse_str(id) else {
        return Ok(unreadable(key));
    };
    // the reaction as the API reads it; one gone or finished since it was listed is left alone
    let tools = Box::pin(tools(ctx)).await?;
    let reaction = match db::reactions::get(group, &uuid, &tools.shared).await {
        Ok(reaction) => reaction,
        Err(err) if err.status().as_u16() == 404 => return Ok(Outcome::Current),
        Err(ApiError::Redis(err)) => return Err(redis_error(err)),
        Err(_) => return Ok(unreadable(key)),
    };
    if !matches!(
        reaction.status,
        ReactionStatus::Created | ReactionStatus::Started
    ) {
        return Ok(Outcome::Current);
    }
    // its args exactly as stored, never a re-rendering, so a rerun hashes the same text
    let mut conn = ctx.backends.redis.get().await.map_err(redis_error)?;
    let raw_args: Option<String> = redis::cmd("HGET")
        .arg(key)
        .arg("args")
        .query_async(&mut *conn)
        .await
        .map_err(redis_error)?;
    drop(conn);
    // record its request; a row already there is kept as it is
    let row = request_row(&reaction, raw_args.as_deref(), ctx, &tools.shared).await?;
    let inserted = tools.store.insert_if_absent(&row).await?;
    let stored = if inserted {
        with_progress(ns, |progress| progress.inserted.insert(row.id.clone()));
        row
    } else {
        tools.store.get(id).await?.ok_or_else(|| {
            MigrateError::Backend("a drain request row vanished after its insert".to_owned())
        })?
    };
    let kept = !with_progress(ns, |progress| progress.inserted.contains(id));
    // its args go in their own expiring row when the stored request names exactly these args
    let stored_sha = stored
        .request()
        .ok()
        .and_then(|request| request.args_sha256);
    if let Some(raw) = raw_args.as_deref()
        && matches!(stored.status, DrainStatus::Recorded | DrainStatus::Stale)
        && raw.len() <= MAX_ARGS_BYTES
        && stored_sha.as_deref() == Some(args_sha256(raw).as_str())
    {
        tools.store.put_args(id, raw).await?;
    }
    // the test fault hook may stop the cancel between the record and the cleanup
    #[cfg(feature = "test-support")]
    fire_cancel_fault(ns, id, CancelFault::AfterRecord)?;
    // its jobs leave the previous release's indexes, then the API deletes them
    remove_old_job_entries(&reaction, ctx).await?;
    db::reactions::delete_jobs(&reaction, &tools.shared)
        .await
        .map_err(|err| MigrateError::Backend(format!("a reaction's jobs didn't delete: {err}")))?;
    // fail it with the reason, proceeding its parent only when the parent isn't cancelled too
    let parent_in_set = fields
        .get(PARENT_IN_SET)
        .is_some_and(|value| value == "true");
    let reason = cancel_reason(&stored);
    db::reactions::fail_with_reason(reaction, &reason, !parent_in_set, &tools.shared)
        .await
        .map_err(|err| MigrateError::Backend(format!("a reaction didn't fail: {err}")))?;
    info!(
        reaction = id,
        group,
        status = stored.status.as_str(),
        kept,
        "cancelled an in-flight reaction"
    );
    // count it
    with_progress(ns, |progress| {
        progress.cancelled.insert(id.to_owned());
        let report = &mut progress.report;
        report.cancelled += 1;
        match (kept, stored.status) {
            (false, DrainStatus::Recorded) => report.recorded += 1,
            (false, DrainStatus::Refused) => report.refused += 1,
            (false, DrainStatus::Stale) => report.stale += 1,
            _ => report.kept += 1,
        }
    });
    Ok(Outcome::Write(record.clone()))
}

/// Whether two attempt times name the same instant, comparing them as times when both parse
///
/// # Arguments
///
/// * `left` - One attempt's text
/// * `right` - The other attempt's text
fn same_attempt(left: &str, right: &str) -> bool {
    // the operator writes RFC 3339 times, which may be spelled differently for the same instant
    match (
        DateTime::parse_from_rfc3339(left),
        DateTime::parse_from_rfc3339(right),
    ) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

/// Count the requests still `recorded` under an attempt whose reactions this pass didn't cancel:
/// those an earlier run of the attempt recorded before failing their reactions
///
/// # Arguments
///
/// * `ctx` - The run's context
/// * `store` - The drain request rows
/// * `attempt` - The step attempt this run records under
async fn earlier_requests(
    ctx: &RunContext,
    store: &DrainRequestStore,
    attempt: &str,
) -> Result<u64, MigrateError> {
    // the reactions this pass cancelled, which it counted already
    let cancelled = with_progress(&ctx.backends.namespace, |progress| {
        progress.cancelled.clone()
    });
    // page through every row, counting the attempt's recorded ones this pass didn't see
    let limit = ctx.limits.batch_rows.max(1);
    let mut after: Option<String> = None;
    let mut earlier = 0;
    loop {
        let page = store.list_page(after.as_deref(), limit).await?;
        let found = page
            .iter()
            .filter(|row| {
                row.status == DrainStatus::Recorded
                    && !cancelled.contains(&row.id)
                    && row
                        .attempt
                        .as_deref()
                        .is_some_and(|row_attempt| same_attempt(row_attempt, attempt))
            })
            .count();
        earlier += u64::try_from(found).unwrap_or(u64::MAX);
        if page.len() < limit {
            return Ok(earlier);
        }
        after = page.last().map(|row| row.id.clone());
    }
}

/// Remove one stage-path worker's registration and list it for the operator's pod deletion
///
/// # Arguments
///
/// * `record` - The worker's record
/// * `ctx` - The run's context
async fn remove_worker(record: &Record, ctx: &RunContext) -> Result<Outcome, MigrateError> {
    // the name ends the data key
    let Record::Redis { key, .. } = record else {
        return Ok(Outcome::Current);
    };
    let ns = &ctx.backends.namespace;
    let prefix = format!("{ns}:worker_data:");
    let name = key.strip_prefix(&prefix).unwrap_or(key);
    // the fields the operator needs to find its pod
    let mut conn = ctx.backends.redis.get().await.map_err(redis_error)?;
    let fields: Vec<Option<String>> = redis::cmd("HMGET")
        .arg(key)
        .arg(&["group", "cluster", "node", "scaler", "tool_instance"][..])
        .query_async(&mut *conn)
        .await
        .map_err(redis_error)?;
    let [group, cluster, node, scaler, tool_instance]: [Option<String>; 5] =
        fields.try_into().unwrap_or_default();
    drop(conn);
    // a worker gone since it was listed, or of the tool path, is left alone
    let (Some(cluster), Some(node), None) = (cluster, node, tool_instance) else {
        return Ok(Outcome::Current);
    };
    let Some(scaler_text) = scaler else {
        return Ok(Outcome::Current);
    };
    let Ok(scaler) = serde_json::from_str::<ImageScaler>(&scaler_text)
        .or_else(|_| unquote(&scaler_text).parse::<ImageScaler>())
    else {
        return Ok(Outcome::Issue(IssueReport {
            key: key.clone(),
            transform: ID.to_owned(),
            class: ObjectClass::Error,
            reason: IssueReason::new(IssueCategory::InvalidValue, "scaler"),
        }));
    };
    // remove its registration the way the API does; one removed meanwhile is done already
    let tools = Box::pin(tools(ctx)).await?;
    let deletes = WorkerDeleteMap {
        workers: vec![name.to_owned()],
    };
    match db::system::delete_workers(scaler, deletes, &tools.shared).await {
        Ok(()) => {}
        Err(err) if err.status().as_u16() == 404 => return Ok(Outcome::Current),
        Err(err) => {
            return Err(MigrateError::Backend(format!(
                "a worker registration didn't delete: {err}"
            )));
        }
    }
    // list it, up to the cap, and count the rest
    let worker = DrainWorker {
        group: group.map_or_else(String::new, |group| unquote(&group).to_owned()),
        name: name.to_owned(),
        scaler: unquote(&scaler_text).to_owned(),
        cluster: unquote(&cluster).to_owned(),
        node: unquote(&node).to_owned(),
    };
    with_progress(ns, |progress| {
        let report = &mut progress.report;
        report.workers_removed += 1;
        if report.workers.len() < MAX_DRAIN_WORKERS {
            report.workers.push(worker);
        } else {
            report.workers_not_listed += 1;
        }
    });
    Ok(Outcome::Write(record.clone()))
}

/// The runner of `drain-reactions`: its check waits while work is in flight, and its run cancels
/// that work, recording each reaction's request for the resubmission
pub struct DrainReactions;

#[async_trait]
impl MigrationRunner for DrainReactions {
    /// Make one slice, since the drain reads its own keys; a run first refuses more in-flight
    /// reactions than its cap, before anything is touched
    ///
    /// # Arguments
    ///
    /// * `n` - How many slices were asked for
    /// * `ctx` - The run's context
    async fn slices(&self, n: u32, ctx: &RunContext) -> Result<Vec<Slice>, MigrateError> {
        // one slice, so children are always failed before their parents
        let _ = n;
        let slice = Slice {
            index: 0,
            start: serde_json::Value::Null,
            end: serde_json::Value::Null,
        };
        // a check classifies no record
        if ctx.mode == RunMode::Check {
            return Ok(vec![slice]);
        }
        // a run starts its counts afresh and counts what it would cancel
        let ns = &ctx.backends.namespace;
        with_progress(ns, |progress| *progress = Progress::default());
        let count = in_flight(ctx).await?.len();
        let cap = ctx.flags.max_requests.unwrap_or(MAX_REQUESTS);
        if count > cap {
            return Err(MigrateError::Systematic(format!(
                "{count} reactions are Created or Started, more than the {cap} one drain cancels; \
                 nothing was cancelled. Let them finish, delete stuck ones with `thorctl \
                 reactions delete` while the previous API still runs, or run the cancel by hand \
                 with `thoradm migrate run --id {ID} --max-requests <n>`"
            )));
        }
        Ok(vec![slice])
    }

    /// Read the next batch: nothing in a check, and in a run the in-flight reactions, children
    /// first, then the stage-path workers
    ///
    /// # Arguments
    ///
    /// * `slice` - The slice to read
    /// * `after` - The last position done
    /// * `ctx` - The run's context
    async fn next_batch(
        &self,
        slice: &Slice,
        after: &serde_json::Value,
        ctx: &RunContext,
    ) -> Result<Batch, MigrateError> {
        // a check's counts are read when it decides whether to wait, not record by record
        let _ = slice;
        if ctx.mode == RunMode::Check {
            return Ok(Batch {
                records: Vec::new(),
                next: after.clone(),
                done: true,
            });
        }
        // a run reads what it cancels
        cancel_batch(after, ctx).await
    }

    /// Cancel a reaction or remove a worker's registration; a check classifies nothing
    ///
    /// # Arguments
    ///
    /// * `record` - The stored record
    /// * `mode` - `Run` or `Check`
    /// * `ctx` - The run's context
    async fn process(
        &self,
        record: &Record,
        mode: RunMode,
        ctx: &RunContext,
    ) -> Result<Outcome, MigrateError> {
        // a check never reads a record, and changes nothing
        if mode == RunMode::Check {
            return Ok(Outcome::Current);
        }
        // the record's kind says what to do
        let kind = match record {
            Record::Redis { fields, .. } => fields.get(KIND).map(String::as_str),
            _ => None,
        };
        match kind {
            Some(KIND_REACTION) => Box::pin(cancel_reaction(record, ctx)).await,
            Some(KIND_WORKER) => Box::pin(remove_worker(record, ctx)).await,
            _ => Ok(Outcome::Current),
        }
    }

    /// Nothing is left to write: the cancel did its work as it processed each record
    ///
    /// # Arguments
    ///
    /// * `record` - The record processed
    /// * `ctx` - The run's context
    async fn write(&self, record: &Record, ctx: &RunContext) -> Result<(), MigrateError> {
        // the record only counts what was done
        let _ = (record, ctx);
        Ok(())
    }

    /// Release the writer and statements the pass held, and count the requests an earlier run of
    /// the step's attempt recorded for reactions it already failed as kept
    ///
    /// # Arguments
    ///
    /// * `ctx` - The run's context
    async fn finish(&self, ctx: &RunContext) -> Result<(), MigrateError> {
        // they are only valid for this pass
        let ns = &ctx.backends.namespace;
        let tools = TOOLS.lock().await.remove(ns);
        // a run of a step attempt counts the requests a failed earlier run of it left, which the
        // resubmission of the attempt creates though this run never saw their reactions
        if ctx.mode == RunMode::Run
            && let Some(attempt) = ctx.flags.step_attempt.as_deref()
        {
            let store = match tools {
                Some(tools) => tools.store.clone(),
                None => {
                    Arc::new(DrainRequestStore::prepare(ctx.backends.scylla.clone(), ns).await?)
                }
            };
            let earlier = earlier_requests(ctx, &store, attempt).await?;
            with_progress(ns, |progress| progress.report.kept += earlier);
        }
        Ok(())
    }

    /// Put what the cancel did in the applied report, drop the skipped samples and notes that
    /// would name reactions, and leave the report as the Job's result for the operator
    ///
    /// # Arguments
    ///
    /// * `report` - The report the runner built
    /// * `ctx` - The run's context
    async fn finish_report(
        &self,
        report: &mut AppliedReport,
        ctx: &RunContext,
    ) -> Result<(), MigrateError> {
        // only a run cancels anything
        if ctx.mode != RunMode::Run {
            return Ok(());
        }
        // the counts and worker list, and no list that names a reaction
        let ns = &ctx.backends.namespace;
        let progress = PROGRESS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(ns)
            .unwrap_or_default();
        report.drain = Some(progress.report);
        report.skipped.clear();
        report.notes.clear();
        // the operator reads the worker list from the Job's result row
        if let Some(uid) = result_uid(ns) {
            let mut result = report.clone();
            result.bound();
            let value = serde_json::to_value(&result).map_err(|err| {
                MigrateError::Systematic(format!("the drain report didn't render: {err}"))
            })?;
            write_job_result(&ctx.backends.scylla, &ctx.backends.conf, &uid, &value)
                .await
                .map_err(|err| {
                    MigrateError::Backend(format!("the Job's result didn't write: {err}"))
                })?;
        }
        Ok(())
    }

    /// Wait while a reaction is in flight or a stage-path worker is registered, naming each one
    /// on stderr
    ///
    /// # Arguments
    ///
    /// * `counts` - What the check classified, which is nothing
    /// * `ctx` - The run's context
    async fn check_wait(
        &self,
        counts: &PassCounts,
        ctx: &RunContext,
    ) -> Result<Option<CheckWait>, MigrateError> {
        // read what is still in flight
        let _ = counts;
        let report = collect(ctx).await?;
        // name each one on stderr only, so the stored report and the cluster status never do
        if !report.is_empty() {
            eprint!("{}", report.render(Utc::now()));
        }
        // wait with the counts and the way to clear them, or let the step skip
        let reactions = u64::try_from(report.reactions.len()).unwrap_or(u64::MAX);
        let workers = u64::try_from(report.workers.len()).unwrap_or(u64::MAX);
        Ok(wait_reason(reactions, workers).map(|reason| CheckWait {
            reason,
            retry_after_secs: RETRY_AFTER_SECS,
        }))
    }
}

/// The migration's registry entry: the check waits for in-flight work, and the run the operator
/// starts on approval cancels it
pub const DRAIN_REACTIONS: MigrationDef = MigrationDef {
    id: ID,
    backend: Backend::Redis,
    support: Support::Yes,
    runner: &DrainReactions,
    pass: ID,
    chain: &[ID],
    group_max: 5,
    families: &["reactions", "workers"],
    has_views: false,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a created or started reaction is in flight, quoted or not
    #[test]
    fn only_unfinished_statuses_are_in_flight() {
        // the statuses of unfinished reactions, as stored (a JSON string) and bare
        for status in ["\"Created\"", "\"Started\"", "Created", "Started"] {
            assert!(is_in_flight(status), "{status}");
        }
        // finished reactions, and text that isn't a status, are not
        for status in [
            "\"Completed\"",
            "\"Failed\"",
            "Completed",
            "\"created\"",
            "",
        ] {
            assert!(!is_in_flight(status), "{status}");
        }
    }

    /// Nothing in flight means no wait
    #[test]
    fn nothing_in_flight_does_not_wait() {
        assert_eq!(wait_reason(0, 0), None);
    }

    /// The wait names the counts and the command that clears a reaction, and nothing else
    #[test]
    fn the_wait_names_the_counts_and_the_delete_command() {
        // reactions alone
        let reason = wait_reason(130, 0).expect("a wait");
        assert!(reason.contains("130 reaction(s)"), "{reason}");
        assert!(
            !reason.contains("worker(s) are still registered"),
            "{reason}"
        );
        assert!(reason.contains("thorctl reactions delete"), "{reason}");
        // workers alone
        let reason = wait_reason(0, 2).expect("a wait");
        assert!(reason.contains("2 stage-path worker(s)"), "{reason}");
        assert!(!reason.contains("reaction(s) are still"), "{reason}");
        // both, in one reason
        let reason = wait_reason(3, 4).expect("a wait");
        assert!(
            reason.contains("3 reaction(s)") && reason.contains("4 stage-path"),
            "{reason}"
        );
    }

    /// The wait points to the check that names what holds the drain, and names no reaction
    #[test]
    fn the_wait_points_to_the_named_report() {
        // the pointer is in every wait, and the text holds only the counts
        let reason = wait_reason(2, 1).expect("a wait");
        assert!(
            reason.contains("`thoradm migrate check --id drain-reactions`"),
            "{reason}"
        );
        assert!(reason.contains("\"The drain\""), "{reason}");
        assert!(!reason.contains("thorctl reactions get"), "{reason}");
        assert!(!reason.contains("--group"), "{reason}");
    }

    /// Ages show their two largest units
    #[test]
    fn ages_show_two_units() {
        // seconds, minutes, hours, and days, with a future time as now
        assert_eq!(short_age(-5), "0s");
        assert_eq!(short_age(45), "45s");
        assert_eq!(short_age(12 * 60 + 3), "12m3s");
        assert_eq!(short_age(3 * 3_600 + 12 * 60 + 9), "3h12m");
        assert_eq!(short_age(6 * 86_400 + 2 * 3_600 + 59), "6d2h");
    }

    /// The named report lays out a row per reaction and worker, marks orphans and unregistered
    /// nodes, and lets the SLA stand in for a missing status log
    #[test]
    fn the_report_names_each_row() {
        // a listed reaction with logs, an orphan without, and a worker on an unknown node
        let at = |text: &str| parse_time(text).expect("a time");
        let report = DrainReport {
            reactions: vec![
                BlockingReaction {
                    id: "r1".to_owned(),
                    group: "corn".to_owned(),
                    pipeline: "unpack".to_owned(),
                    status: "Started".to_owned(),
                    created: Some(at("2026-11-04T07:00:00Z")),
                    last_activity: Some(at("2026-11-04T10:02:11Z")),
                    sla: None,
                    listed: true,
                },
                BlockingReaction {
                    id: "r2".to_owned(),
                    group: "corn".to_owned(),
                    pipeline: "unpack".to_owned(),
                    status: "Created".to_owned(),
                    created: None,
                    last_activity: None,
                    sla: Some(at("\"2026-10-29T08:00:03Z\"")),
                    listed: false,
                },
            ],
            workers: vec![StageWorker {
                name: "corn-unpack-7f9c2".to_owned(),
                scaler: "K8s".to_owned(),
                cluster: "prod".to_owned(),
                node: "node-3".to_owned(),
                status: "Running".to_owned(),
                heart_beat: None,
                node_registered: false,
            }],
        };
        let text = report.render(at("2026-11-04T10:12:00Z"));
        // the summary, the headers, and each row, the orphan first
        let lines = text.lines().collect::<Vec<_>>();
        assert_eq!(
            lines[0],
            "drain-reactions waits: 2 reaction(s) Created or Started, 1 stage-path worker(s) \
             registered"
        );
        assert_eq!(lines[2], "Reactions holding the drain");
        assert_eq!(
            lines[3],
            "ID  GROUP  PIPELINE  STATUS   AGE    LAST ACTIVITY               LISTED"
        );
        assert_eq!(
            lines[4],
            "r2  corn   unpack    Created  -      2026-10-29T08:00:03Z (sla)  no (orphan)"
        );
        assert_eq!(
            lines[5],
            "r1  corn   unpack    Started  3h12m  2026-11-04T10:02:11Z        yes"
        );
        assert_eq!(lines[7], "Stage-path workers");
        assert_eq!(
            lines[9],
            "corn-unpack-7f9c2  K8s     prod     node-3  Running  -          no"
        );
        // nothing holds an empty report
        assert!(DrainReport::default().is_empty() && !report.is_empty());
    }

    /// Make a reaction row for a report
    ///
    /// # Arguments
    ///
    /// * `id` - The reaction's id
    /// * `listed` - Whether its status set lists it
    fn reaction(id: &str, listed: bool) -> BlockingReaction {
        // a created reaction with no status log or SLA
        BlockingReaction {
            id: id.to_owned(),
            group: "corn".to_owned(),
            pipeline: "unpack".to_owned(),
            status: "Created".to_owned(),
            created: None,
            last_activity: None,
            sla: None,
            listed,
        }
    }

    /// Make a worker row for a report
    ///
    /// # Arguments
    ///
    /// * `name` - The worker's name
    /// * `node_registered` - Whether its node is registered
    fn worker(name: &str, node_registered: bool) -> StageWorker {
        // a running worker that never sent a heartbeat
        StageWorker {
            name: name.to_owned(),
            scaler: "K8s".to_owned(),
            cluster: "prod".to_owned(),
            node: "node-3".to_owned(),
            status: "Running".to_owned(),
            heart_beat: None,
            node_registered,
        }
    }

    /// The first cell of each row of the table under a title, and the line after the table
    ///
    /// # Arguments
    ///
    /// * `text` - The rendered report
    /// * `title` - The line above the table's header
    fn table_rows<'a>(text: &'a str, title: &str) -> (Vec<&'a str>, Option<&'a str>) {
        // the rows run from after the header to the first line that isn't a row
        let mut lines = text.lines().skip_while(|line| *line != title).skip(2);
        let mut firsts = Vec::new();
        let mut after = None;
        for line in lines.by_ref() {
            if line.is_empty() || line.starts_with("...") {
                after = Some(line);
                break;
            }
            firsts.push(line.split_whitespace().next().unwrap_or_default());
        }
        (firsts, after)
    }

    /// Each table shows at most its cap of rows, orphans and workers on unregistered nodes first,
    /// and says how many more there are and how many of those are orphans or unregistered
    #[test]
    fn the_tables_are_capped_with_the_actionable_rows_first() {
        // more reactions than the cap, with the orphans sorted last by id
        let total = TABLE_ROWS + 10;
        let mut reactions = (0..total)
            .map(|n| reaction(&format!("r{n:03}"), n < total - 3))
            .collect::<Vec<_>>();
        // and more workers than the cap, with the unregistered ones last by name
        let workers = (0..TABLE_ROWS + 2)
            .map(|n| worker(&format!("w{n:03}"), n < TABLE_ROWS))
            .collect::<Vec<_>>();
        let report = DrainReport {
            reactions: reactions.clone(),
            workers,
        };
        let text = report.render(Utc::now());
        // the summary counts every row
        assert!(
            text.starts_with(&format!(
                "drain-reactions waits: {total} reaction(s) Created or Started, {} stage-path \
                 worker(s) registered",
                TABLE_ROWS + 2
            )),
            "{text}"
        );
        // the orphans come first, then the listed reactions in order, up to the cap
        let (rows, after) = table_rows(&text, "Reactions holding the drain");
        assert_eq!(rows.len(), TABLE_ROWS, "{text}");
        assert_eq!(rows[..3], ["r057", "r058", "r059"], "{text}");
        assert_eq!(rows[3], "r000", "{text}");
        assert_eq!(
            after,
            Some("... and 10 more reaction(s) not shown"),
            "{text}"
        );
        // the unregistered workers come first too
        let (rows, after) = table_rows(&text, "Stage-path workers");
        assert_eq!(rows.len(), TABLE_ROWS, "{text}");
        assert_eq!(rows[..3], ["w050", "w051", "w000"], "{text}");
        assert_eq!(after, Some("... and 2 more worker(s) not shown"), "{text}");
        // orphans past the cap are counted in the line
        for listed in reactions.iter_mut().take(TABLE_ROWS + 4) {
            listed.listed = false;
        }
        let text = DrainReport {
            reactions,
            workers: Vec::new(),
        }
        .render(Utc::now());
        let (rows, after) = table_rows(&text, "Reactions holding the drain");
        assert_eq!(rows.len(), TABLE_ROWS, "{text}");
        assert_eq!(
            after,
            Some("... and 10 more reaction(s) not shown, 7 of them orphans"),
            "{text}"
        );
        // a report within the cap has no such line
        let text = report_of(3).render(Utc::now());
        assert!(!text.contains("..."), "{text}");
    }

    /// A report of listed reactions and registered workers
    ///
    /// # Arguments
    ///
    /// * `rows` - How many of each
    fn report_of(rows: usize) -> DrainReport {
        // listed reactions and workers on registered nodes
        DrainReport {
            reactions: (0..rows)
                .map(|n| reaction(&format!("r{n}"), true))
                .collect(),
            workers: (0..rows).map(|n| worker(&format!("w{n}"), true)).collect(),
        }
    }
}
