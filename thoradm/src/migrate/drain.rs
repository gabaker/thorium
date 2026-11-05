//! The `drain-reactions` check
//!
//! Reactions that are still `Created` or `Started` hold queued jobs and running workers shaped for
//! the stage path, which the tool path can't read, so the conversions must not start while any
//! exist. This migration only checks: it reads Redis and Scylla, writes nothing, never deletes a
//! reaction, and asks the operator to wait while any reaction is in flight or any stage-path worker
//! is still registered. The wait it reports holds counts only, since it reaches the cluster's
//! status; the reactions and workers that hold the drain are named on stderr, which reaches the
//! admin's terminal or the check Job's logs but never the stored report. The admin lets them
//! finish or deletes the stuck ones with `thorctl reactions delete` while the previous API still
//! runs.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use redis::aio::MultiplexedConnection;
use std::collections::BTreeMap;
use thorium::models::schema::{
    Batch, CheckWait, MigrateError, MigrationRunner, Outcome, PassCounts, RunContext, RunMode,
    Slice, backend_error,
};
use thorium::models::upgrades::Record;

use super::registry::{Backend, MigrationDef, Support};

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

/// The runner of the `drain-reactions` check, which converts and writes nothing
pub struct DrainReactions;

#[async_trait]
impl MigrationRunner for DrainReactions {
    /// Make one empty slice, since the check reads its own keys
    ///
    /// # Arguments
    ///
    /// * `n` - How many slices were asked for
    /// * `ctx` - The run's context
    async fn slices(&self, n: u32, ctx: &RunContext) -> Result<Vec<Slice>, MigrateError> {
        // one slice is enough for a check that classifies no record
        let _ = (n, ctx);
        Ok(vec![Slice {
            index: 0,
            start: serde_json::Value::Null,
            end: serde_json::Value::Null,
        }])
    }

    /// Read no records
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
        // the counts are read when the check decides whether to wait, not record by record
        let _ = (slice, ctx);
        Ok(Batch {
            records: Vec::new(),
            next: after.clone(),
            done: true,
        })
    }

    /// Leave a record as it is
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
        // nothing is ever converted
        let _ = (record, mode, ctx);
        Ok(Outcome::Current)
    }

    /// Nothing to record once every slice is done
    ///
    /// # Arguments
    ///
    /// * `ctx` - The run's context
    async fn finish(&self, ctx: &RunContext) -> Result<(), MigrateError> {
        // the check leaves nothing behind
        let _ = ctx;
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

/// The migration's registry entry: checked, never run, since it only waits
pub const DRAIN_REACTIONS: MigrationDef = MigrationDef {
    id: ID,
    backend: Backend::Redis,
    support: Support::CheckOnly,
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
