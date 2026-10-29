//! The events in Thorium for triggers and other things to act on

use chrono::prelude::*;
use futures::{StreamExt, stream};
use redis::cmd;
use std::collections::{BTreeMap, HashMap};
use strum::IntoEnumIterator;
use tracing::{Level, event, instrument};
use uuid::Uuid;

use super::keys::EventKeys;
use crate::conf::Events;
use crate::models::schema::{Bucket, scylla::ScanStatsTableRef};
use crate::models::{
    EntityKinds, Event, EventCacheStatus, EventDepths, EventStats, EventType, KindDepth, KindStats,
    ReapSummary, ScanStatsSnapshot, StatsHistoryParams, WorkerStatsReport,
};
use crate::utils::{ApiError, Shared, helpers};
use crate::{bad, conn, deserialize, internal_err, query, serialize};

pub mod shared;

/// Atomically pops mature events from a pending queue and claims them for a worker
///
/// The pop and the claim run in one script so a crash or dropped connection can never
/// leave an event popped from its pending queue without also being in its owners in
/// flight map and queue.
///
/// Keys: the pending queue to pop from, the owning worker's in flight map, the owning
/// worker's in flight queue, and the worker registry.
/// Args: the max number of events to pop, the current timestamp, the owning
/// worker's id, and the seconds events of this kind wait before they are mature.
///
/// Events younger than their kind's maturity delay are reset at their original score
/// so the DB has a chance to reach consistency (or a burst of changes can coalesce).
/// Mature events are claimed into the owners in flight map (id -> body) and in flight
/// queue (id scored by lease/pop time) and the owner's registry entry is upserted so
/// its keys are always discoverable by the reaper.
///
/// Returns the serialized mature events oldest first, since compacting events for the
/// same item relies on later events superseding earlier ones.
static POP_EVENTS_SCRIPT: std::sync::LazyLock<redis::Script> = std::sync::LazyLock::new(|| {
    redis::Script::new(
        r#"
        local popped = redis.call('zpopmin', KEYS[1], ARGV[1])
        local now = tonumber(ARGV[2])
        local delay = tonumber(ARGV[4])
        local mature = {}
        local i = 1
        while i < #popped do
            local member = popped[i]
            local score = tonumber(popped[i + 1])
            if score >= now - delay then
                redis.call('zadd', KEYS[1], score, member)
            else
                local body = cjson.decode(member)
                redis.call('hset', KEYS[2], body['id'], member)
                redis.call('zadd', KEYS[3], now, body['id'])
                mature[#mature + 1] = member
            end
            i = i + 2
        end
        redis.call('zadd', KEYS[4], now, ARGV[3])
        return mature
        "#,
    )
});

/// Atomically restore one stale in flight entry back to its pending queue
///
/// The restore only happens if the entry is still in the in flight queue at a
/// score below the cutoff it was selected by, so concurrent reapers move each
/// entry exactly once and an id the owner re-claimed with a fresh lease (or
/// cleared) between the listing and this claim is left alone.
///
/// Keys: the in flight queue, the in flight map, and the pending queue to
/// restore into.
/// Args: the event id, the cutoff score the entry was selected by, the score to
/// restore the event at, and the serialized event body to restore.
///
/// Returns 1 if this claim won and the entry was restored and 0 otherwise.
static RESTORE_STALE_SCRIPT: std::sync::LazyLock<redis::Script> = std::sync::LazyLock::new(|| {
    redis::Script::new(
        r#"
        local score = redis.call('zscore', KEYS[1], ARGV[1])
        if not score or tonumber(score) >= tonumber(ARGV[2]) then
            return 0
        end
        redis.call('zadd', KEYS[3], ARGV[3], ARGV[4])
        redis.call('zrem', KEYS[1], ARGV[1])
        redis.call('hdel', KEYS[2], ARGV[1])
        return 1
        "#,
    )
});

/// Atomically drop a dangling in flight queue entry that has no body
///
/// The entry is only removed while its score is still below the cutoff it was
/// selected by, so an owner that re-claimed this id with a fresh lease between
/// the body lookup and this drop keeps its live claim.
///
/// Keys: the in flight queue to drop the dangling entry from.
/// Args: the event id and the cutoff score the entry was selected by.
///
/// Returns 1 if the dangling entry was dropped and 0 otherwise.
static DROP_STALE_SCRIPT: std::sync::LazyLock<redis::Script> = std::sync::LazyLock::new(|| {
    redis::Script::new(
        r#"
        local score = redis.call('zscore', KEYS[1], ARGV[1])
        if score and tonumber(score) < tonumber(ARGV[2]) then
            redis.call('zrem', KEYS[1], ARGV[1])
            return 1
        end
        return 0
        "#,
    )
});

/// Make sure a worker id is safe to embed in redis key names
///
/// # Arguments
///
/// * `worker` - The worker id to validate
fn validate_worker_id(worker: &str) -> Result<(), ApiError> {
    // reject any worker id that could break out of a redis key
    if !crate::models::valid_worker_id(worker) {
        return bad!(format!(
            "worker ids must match ^[A-Za-z0-9._-]{{1,128}}$ but got '{worker}'"
        ));
    }
    Ok(())
}

/// Compute the heartbeat score below which a worker is presumed dead
///
/// # Arguments
///
/// * `now` - The current unix timestamp
/// * `worker_expiry_secs` - How many seconds after its last heartbeat a worker is dead
fn dead_worker_cutoff(now: i64, worker_expiry_secs: u64) -> i64 {
    // clamp the expiry into an i64 so a huge config value can't wrap the cutoff
    let expiry = i64::try_from(worker_expiry_secs).unwrap_or(i64::MAX);
    // a worker whose last beat is strictly below this cutoff is presumed dead
    now.saturating_sub(expiry)
}

/// Compute the lease score below which an in flight event is backstop reclaimed
///
/// # Arguments
///
/// * `now` - The current unix timestamp
/// * `in_flight_backstop_secs` - The lease age in seconds after which events reclaim
fn backstop_cutoff(now: i64, in_flight_backstop_secs: u64) -> i64 {
    // clamp the backstop into an i64 so a huge config value can't wrap the cutoff
    let backstop = i64::try_from(in_flight_backstop_secs).unwrap_or(i64::MAX);
    // an in flight event leased strictly below this cutoff is reclaimed
    now.saturating_sub(backstop)
}

/// Check whether a workers registry score marks it as alive
///
/// # Arguments
///
/// * `score` - This workers registry score if it is still registered
/// * `dead_cutoff` - The heartbeat score below which a worker is presumed dead
fn is_live(score: Option<f64>, dead_cutoff: i64) -> bool {
    // a worker is alive if it is registered at or above the dead cutoff
    score.is_some_and(|score| score >= dead_cutoff as f64)
}

/// Add a new event to its kind's event queue in Redis
///
/// # Arguments
///
/// * `event` - The event to add to its queue
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::events::create", skip_all, err(Debug))]
pub async fn create(event: &Event, shared: &Shared) -> Result<(), ApiError> {
    // get this events type
    let kind = EventType::from(&event.data);
    // get the key to the right event queue
    let key = EventKeys::queue(kind, shared);
    // serialize our event for our event stream
    let serialized = serialize!(event);
    // use our current time as a timestamp
    let now = Utc::now().timestamp();
    // build a pipeline to insert this event into
    let mut pipe = redis::pipe();
    // add this event
    pipe.cmd("zadd").arg(key).arg(now).arg(serialized);
    // execute this query
    let _: () = pipe.query_async(conn!(shared)).await?;
    Ok(())
}

/// Add some events to their queues in one pipeline
///
/// # Arguments
///
/// * `events` - The events to add
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::events::create_many", skip_all, fields(events = events.len()), err(Debug))]
pub async fn create_many(events: &[Event], shared: &Shared) -> Result<(), ApiError> {
    // nothing to add needs no round trip
    if events.is_empty() {
        return Ok(());
    }
    // every event is queued at the current time
    let now = Utc::now().timestamp();
    // add each event to its kind's queue in one pipeline
    let mut pipe = redis::pipe();
    for event in events {
        let key = EventKeys::queue(EventType::from(&event.data), shared);
        let serialized = serialize!(event);
        pipe.cmd("zadd").arg(key).arg(now).arg(serialized);
    }
    let _: () = pipe.query_async(conn!(shared)).await?;
    Ok(())
}

/// Clear some events from the in flight event queue
///
/// Owned clears also clear the same ids from the legacy un-owned keys: during a
/// rolling API upgrade a pop served by an old replica claims into the legacy
/// keys while the matching clear can land on a new replica, so clearing both
/// families keeps clears version agnostic (an id only ever lives in one family
/// so the extra removals are idempotent no-ops).
///
/// # Arguments
///
/// * `kind` - The kind of events to clear
/// * `ids` - The ids of the events to clear
/// * `owner` - The worker that owns these in flight events if an owner was set on pop
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::events::clear", skip_all, fields(clears = ids.len(), owner), err(Debug))]
pub async fn clear(
    kind: EventType,
    ids: &[Uuid],
    owner: Option<&str>,
    shared: &Shared,
) -> Result<(), ApiError> {
    // build the keys to the legacy un-owned in flight map and queue
    let legacy_map_key = EventKeys::in_flight_map(kind, shared);
    let legacy_queue_key = EventKeys::in_flight_queue(kind, shared);
    // build the keys to this owners in flight map and queue if an owner was set
    let owned_keys = match owner {
        // clear from this owners own in flight keys as well
        Some(owner) => {
            // make sure this worker id is safe to embed in redis keys
            validate_worker_id(owner)?;
            // build this owners in flight keys
            Some((
                EventKeys::in_flight_map_owned(kind, owner, shared),
                EventKeys::in_flight_queue_owned(kind, owner, shared),
            ))
        }
        // clear only the legacy un-owned in flight keys
        None => None,
    };
    // build a redis pipeline
    let mut pipe = redis::pipe();
    // clear each of these ids
    for id in ids {
        // clear this id from this owners own in flight keys if an owner was set
        if let Some((map_key, queue_key)) = &owned_keys {
            // remove this id from this owners in flight queue
            pipe.cmd("zrem").arg(queue_key).arg(id.to_string());
            // remove it from this owners in flight data map as well
            pipe.cmd("hdel").arg(map_key).arg(id.to_string());
        }
        // remove this id from the legacy in flight queue in case an old api
        // replica served the pop that claimed it mid rolling upgrade
        pipe.cmd("zrem").arg(&legacy_queue_key).arg(id.to_string());
        // remove it from the legacy in flight data map as well
        pipe.cmd("hdel").arg(&legacy_map_key).arg(id.to_string());
    }
    // execute this pipeline
    let _: () = pipe.query_async(conn!(shared)).await?;
    Ok(())
}

/// Get how many seconds events of one kind wait before they can be popped
///
/// Workflow graph events wait for the configured delay so a burst of changes is handled
/// together, model refresh events wait for nothing so a refresh runs on the worker's next pass,
/// and every other kind waits just long enough for the database to reach consistency.
///
/// # Arguments
///
/// * `kind` - The kind of events
/// * `conf` - The events settings
#[must_use]
pub fn maturity_delay(kind: EventType, conf: &Events) -> u64 {
    // workflow graph events wait longer than the consistency delay and refreshes don't wait
    match kind {
        EventType::WorkflowGraph => conf.workflow_graph_delay,
        EventType::ModelRefresh => 0,
        EventType::ReactionTrigger
        | EventType::SigmaScannableResults
        | EventType::YaraScannableSample
        | EventType::YaraHuntChunk => shared::CONSISTENCY_DELAY,
    }
}

/// Get some number of events to evaluate
///
/// Pops with an owner run as one atomic script that claims the popped events into the
/// owners own in flight keys so they can be reclaimed if the owner dies. Owner-less
/// pops keep the legacy two pipeline path and the shared in flight keys for older
/// clients.
///
/// # Arguments
///
/// * `kind` - The kind of events to pop
/// * `count` - The number of events to pop at most
/// * `owner` - The worker that will own the popped events if one was set
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::events::pop", skip(shared), err(Debug))]
pub async fn pop(
    kind: EventType,
    count: usize,
    owner: Option<&str>,
    shared: &Shared,
) -> Result<Vec<Event>, ApiError> {
    // pop into this owners own in flight keys or the legacy shared ones
    match owner {
        // pop and claim these events for this owner in one atomic script
        Some(owner) => pop_owned(kind, count, owner, shared).await,
        // pop with the legacy two pipeline path for older clients
        None => pop_legacy(kind, count, shared).await,
    }
}

/// Atomically pop some events and claim them into an owners in flight keys
///
/// # Arguments
///
/// * `kind` - The kind of events to pop
/// * `count` - The number of events to pop at most
/// * `owner` - The worker that will own the popped events
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::events::pop_owned", skip(shared), err(Debug))]
async fn pop_owned(
    kind: EventType,
    count: usize,
    owner: &str,
    shared: &Shared,
) -> Result<Vec<Event>, ApiError> {
    // make sure this worker id is safe to embed in redis keys
    validate_worker_id(owner)?;
    // build the key to the right event queue
    let queue_key = EventKeys::queue(kind, shared);
    // build the keys to this owners in flight map and in flight queue
    let map_key = EventKeys::in_flight_map_owned(kind, owner, shared);
    let owned_queue_key = EventKeys::in_flight_queue_owned(kind, owner, shared);
    // build the key to the worker registry
    let workers_key = EventKeys::workers(shared);
    // get our current timestamp to lease the popped events at
    let now = Utc::now().timestamp();
    // get how many seconds events of this kind wait before they are mature
    let delay = maturity_delay(kind, &shared.config.thorium.events);
    // atomically pop the mature events and claim them for this owner
    let serialized: Vec<String> = POP_EVENTS_SCRIPT
        .key(&queue_key)
        .key(&map_key)
        .key(&owned_queue_key)
        .key(&workers_key)
        .arg(count)
        .arg(now)
        .arg(owner)
        .arg(delay)
        .invoke_async(conn!(shared))
        .await?;
    // build a list of deserialized events
    let mut events = Vec::with_capacity(serialized.len());
    // deserialize each of the mature events we claimed
    for serial in &serialized {
        // deserialize this event
        let cast: Event = deserialize!(serial);
        // add this event to our list
        events.push(cast);
    }
    Ok(events)
}

/// Pop some events with the legacy two pipeline path and shared in flight keys
///
/// # Arguments
///
/// * `kind` - The kind of events to pop
/// * `count` - The number of events to pop at most
/// * `shared` - Shared Thorium objects
#[rustfmt::skip]
#[instrument(name = "db::events::pop_legacy", skip(shared), err(Debug))]
async fn pop_legacy(kind: EventType, count: usize, shared: &Shared) -> Result<Vec<Event>, ApiError> {
    // build the key to the right event queue
    let key = EventKeys::queue(kind, shared);
    // get our current timestamp
    let now = Utc::now();
    // try to pop some events from the right event queue
    let serialized: Vec<(String, f64)> = query!(cmd("zpopmin").arg(&key).arg(count), shared).await?;
    // filter out and reset any events that are not yet mature for this kind
    let delay = maturity_delay(kind, &shared.config.thorium.events);
    let (events, filtered) = shared::filter_immature::<Event>(&key, now, delay, serialized, shared).await?;
    // build the key to the in flight map and the in flight queue
    let map_key = EventKeys::in_flight_map(kind, shared);
    let queue_key = EventKeys::in_flight_queue(kind, shared);
    // build a redis pipeline to add these events to the in flight event queue
    let mut pipe = redis::pipe();
    // add all of our events to the in flight event queue
    for ((serial, timestamp), event) in filtered.iter().zip(events.iter()) {
        // save its data in a separate map so its easy to clean up later
        pipe.cmd("hset").arg(&map_key).arg(event.id.to_string()).arg(serial)
            // save this in flight event id to the in flight queue
            .cmd("zadd").arg(&queue_key).arg(timestamp).arg(event.id.to_string());
    }
    // execute this pipeline
    let _: () = pipe.query_async(conn!(shared)).await?;
    Ok(events)
}

/// Read the queued events of one kind without moving them in flight
///
/// Events are returned oldest first. The queue is scored by whole seconds, so events that share
/// a second are ordered by their own timestamps after they are read.
///
/// # Arguments
///
/// * `kind` - The kind of events to peek at
/// * `limit` - The number of events to return at most
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::events::peek", skip(shared), err(Debug))]
pub async fn peek(kind: EventType, limit: usize, shared: &Shared) -> Result<Vec<Event>, ApiError> {
    // a zero limit can return nothing, and redis would read a stop index of -1 as the whole queue
    if limit == 0 {
        return Ok(Vec::default());
    }
    // build the key to the right event queue
    let key = EventKeys::queue(kind, shared);
    // read the oldest events in this queue without removing them
    let serialized: Vec<String> =
        query!(cmd("zrange").arg(&key).arg(0).arg(limit - 1), shared).await?;
    // start a list of our deserialized events
    let mut events: Vec<Event> = Vec::with_capacity(serialized.len());
    // deserialize each of our queued events
    for serial in &serialized {
        events.push(deserialize!(serial));
    }
    // order events that share a second by their own timestamps
    events.sort_by_key(|event| event.timestamp);
    Ok(events)
}

/// Reset all events in our in flight event queue
#[instrument(name = "db::events::reset_all", skip(shared), err(Debug))]
pub async fn reset_all(kind: EventType, shared: &Shared) -> Result<(), ApiError> {
    // build the key to the inflight event map/queue
    let map_key = EventKeys::in_flight_map(kind, shared);
    let queue_key = EventKeys::in_flight_queue(kind, shared);
    // track the number of events we are resetting
    let mut total_reset = 0;
    // pop up to 10,000 items in our in flight event queue 1000 at a time
    loop {
        // try to pop some events from the right event queue
        let popped: Vec<(String, f64)> =
            query!(cmd("zpopmin").arg(&queue_key).arg(1000), shared).await?;
        // if we popped no data then we have no more events to reset
        if popped.is_empty() {
            break;
        }
        // build a redis pipeline
        let mut pipe = redis::pipe();
        // get these events data
        for (event_id, _) in &popped {
            // get this events data
            pipe.cmd("hget").arg(&map_key).arg(event_id);
        }
        // try to get all these events data
        // on failure try to reset our popped events so we don't leak them
        // if the reset fails then the events will be leaked
        let serialized: Vec<String> = match pipe.query_async(conn!(shared)).await {
            Ok(serialized) => serialized,
            Err(error) => {
                // log this error
                event!(Level::ERROR, error = error.to_string());
                // try to reset our popped events
                shared::reset_pops(&queue_key, popped, shared).await?;
                // return our original error if our reset didn't also fail
                return Err(ApiError::from(error));
            }
        };
        // get the key to the right event queue to reset our events into
        let key = EventKeys::queue(kind, shared);
        // build a redis pipeline to reset these events
        let mut pipe = redis::pipe();
        //  crawl over the events we retrieved and reset them
        for ((event_id, timestamp), data) in popped.iter().zip(&serialized) {
            // reset this event back into our event queue
            pipe.cmd("zadd").arg(&key).arg(timestamp).arg(data);
            // remove this events data from our in flight data map
            pipe.cmd("hdel").arg(&map_key).arg(event_id);
        }
        // execute this pipeline
        let _: () = pipe.query_async(conn!(shared)).await?;
        // increment the total number of events that we have reset
        total_reset += popped.len();
        // log the number of events we have reset so far
        event!(Level::INFO, total_reset);
    }
    Ok(())
}

/// The outcome of draining one kinds in flight events for a single owner
#[derive(Debug, Default)]
struct DrainOutcome {
    /// The number of events drained back to the pending queue
    drained: u64,
    /// Whether the owner came back to life mid drain
    owner_returned: bool,
}

/// A per page liveness re-check used when draining a presumed dead owner
#[derive(Debug)]
struct ZombieCheck<'a> {
    /// The key to the event handler worker registry
    workers_key: &'a str,
    /// The owner whose in flight events are being drained
    owner: &'a str,
    /// The heartbeat score below which the owner is still presumed dead
    dead_cutoff: i64,
}

/// Drain one kinds in flight events for a single owner back to the pending queue
///
/// Restored events are scored by their own body timestamp, never their in flight
/// score, so reclaimed events keep their original queue position.
///
/// # Arguments
///
/// * `kind` - The kind of events to drain
/// * `map_key` - The in flight map to drain event bodies from
/// * `queue_key` - The in flight queue to drain event ids from
/// * `zombie_check` - A per page liveness re-check when draining a dead owner
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::events::drain_in_flight", skip_all, fields(kind = kind.as_str()), err(Debug))]
async fn drain_in_flight(
    kind: EventType,
    map_key: &String,
    queue_key: &String,
    zombie_check: Option<&ZombieCheck<'_>>,
    shared: &Shared,
) -> Result<DrainOutcome, ApiError> {
    // build the key to the pending queue these events restore into
    let pending_key = EventKeys::queue(kind, shared);
    // track the outcome of this drain
    let mut outcome = DrainOutcome::default();
    // drain this owners in flight queue in pages of 1000
    loop {
        // if we are draining a presumed dead owner then re-check its liveness once per page
        if let Some(check) = zombie_check {
            // get this owners current registry score
            let score: Option<f64> = query!(
                cmd("zscore").arg(check.workers_key).arg(check.owner),
                shared
            )
            .await?;
            // stop draining if this owner came back to life
            if is_live(score, check.dead_cutoff) {
                // log that this owner returned mid drain
                event!(
                    Level::WARN,
                    msg = "owner returned mid drain",
                    owner = check.owner
                );
                // flag that this owner returned so it is not removed from the registry
                outcome.owner_returned = true;
                break;
            }
        }
        // try to pop a page of in flight event ids
        let popped: Vec<(String, f64)> =
            query!(cmd("zpopmin").arg(queue_key).arg(1000), shared).await?;
        // if we popped no ids then this owners in flight queue is drained
        if popped.is_empty() {
            break;
        }
        // build a redis pipeline to get these events bodies
        let mut pipe = redis::pipe();
        // get each events body
        for (event_id, _) in &popped {
            // get this events body
            pipe.cmd("hget").arg(map_key).arg(event_id);
        }
        // try to get all these events bodies
        // on failure try to reset our popped ids so we don't leak them
        // if the reset fails then the events will be leaked
        let serialized: Vec<Option<String>> = match pipe.query_async(conn!(shared)).await {
            Ok(serialized) => serialized,
            Err(error) => {
                // log this error
                event!(Level::ERROR, error = error.to_string());
                // try to reset our popped ids
                shared::reset_pops(queue_key, popped, shared).await?;
                // return our original error if our reset didn't also fail
                return Err(ApiError::from(error));
            }
        };
        // build a redis pipeline to restore these events
        let mut pipe = redis::pipe();
        // crawl over the events we retrieved and restore them
        for ((event_id, score), serial) in popped.iter().zip(&serialized) {
            // skip any id whose body was already cleared or restored by a peer
            match serial {
                Some(serial) => {
                    // deserialize this event to recover its own timestamp; one
                    // undeserializable body (schema drift mid rolling upgrade or
                    // corruption) must never abort a page that is already popped
                    // or every other event on it would be silently lost
                    match serde_json::from_str::<Event>(serial) {
                        // restore this event scored by its own body timestamp
                        Ok(cast) => {
                            pipe.cmd("zadd")
                                .arg(&pending_key)
                                .arg(cast.timestamp.timestamp())
                                .arg(serial);
                        }
                        // restore the raw body at its in flight score so it is
                        // not lost and a newer api replica can still parse it
                        Err(error) => {
                            // warn that this events body could not be deserialized
                            event!(
                                Level::WARN,
                                msg = "restoring undeserializable in flight event body",
                                event_id,
                                error = error.to_string(),
                            );
                            // restore the raw body at this entries in flight score
                            pipe.cmd("zadd").arg(&pending_key).arg(*score).arg(serial);
                        }
                    }
                    // remove this events body from the in flight map
                    pipe.cmd("hdel").arg(map_key).arg(event_id);
                    // count this event as drained
                    outcome.drained += 1;
                }
                // this ids body is gone so just drop its already popped queue entry
                None => event!(Level::WARN, msg = "missing in flight event body", event_id),
            }
        }
        // execute this pipeline
        let _: () = pipe.query_async(conn!(shared)).await?;
        // log the number of events we have drained so far
        event!(Level::INFO, drained = outcome.drained);
    }
    Ok(outcome)
}

/// Reset a single workers in flight events for one kind back to the pending queue
///
/// Restored events are scored by their own body timestamp so a reset never reorders
/// the pending queue.
///
/// # Arguments
///
/// * `kind` - The kind of events to reset
/// * `worker` - The worker whose in flight events should be reset
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::events::reset_worker", skip(shared), err(Debug))]
pub async fn reset_worker(kind: EventType, worker: &str, shared: &Shared) -> Result<u64, ApiError> {
    // make sure this worker id is safe to embed in redis keys
    validate_worker_id(worker)?;
    // build the keys to this workers in flight map/queue
    let map_key = EventKeys::in_flight_map_owned(kind, worker, shared);
    let queue_key = EventKeys::in_flight_queue_owned(kind, worker, shared);
    // drain this workers in flight events back to the pending queue
    let outcome = drain_in_flight(kind, &map_key, &queue_key, None, shared).await?;
    Ok(outcome.drained)
}

/// Upsert a workers heartbeat in the worker registry
///
/// # Arguments
///
/// * `worker` - The worker that is heartbeating
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::events::heartbeat", skip(shared), err(Debug))]
pub async fn heartbeat(worker: &str, shared: &Shared) -> Result<(), ApiError> {
    // make sure this worker id is safe to embed in redis keys
    validate_worker_id(worker)?;
    // build the key to the worker registry
    let key = EventKeys::workers(shared);
    // get the current timestamp to score this workers heartbeat with
    let now = Utc::now().timestamp();
    // upsert this workers heartbeat in the registry
    let _: () = query!(cmd("zadd").arg(&key).arg(now).arg(worker), shared).await?;
    Ok(())
}

/// Upsert one workers TTL'd counter report for one event kind
///
/// The report expires after `events.worker_expiry_secs` so a dead workers
/// counters age out of the stats view on the same clock that declares it dead.
///
/// # Arguments
///
/// * `kind` - The kind of events these counters cover
/// * `worker` - The worker that took this reading
/// * `report` - The counter report to upsert
/// * `shared` - Shared Thorium objects
#[instrument(
    name = "db::events::publish_worker_stats",
    skip(report, shared),
    err(Debug)
)]
pub async fn publish_worker_stats(
    kind: EventType,
    worker: &str,
    report: &WorkerStatsReport,
    shared: &Shared,
) -> Result<(), ApiError> {
    // make sure this worker id is safe to embed in redis keys
    validate_worker_id(worker)?;
    // bound the counter map like the rest of the telemetry surface
    if report.counters.len() > WorkerStatsReport::MAX_COUNTERS {
        return bad!(format!(
            "worker stats reports are capped at {} counters but got {}",
            WorkerStatsReport::MAX_COUNTERS,
            report.counters.len()
        ));
    }
    // bound each counter name so one report cannot bloat its key space
    if report
        .counters
        .keys()
        .any(|name| name.len() > WorkerStatsReport::MAX_COUNTER_NAME_LEN)
    {
        return bad!(format!(
            "worker stats counter names are capped at {} bytes",
            WorkerStatsReport::MAX_COUNTER_NAME_LEN
        ));
    }
    // build the key to this workers counter report
    let key = EventKeys::stats(kind, worker, shared);
    // serialize this report for redis
    let serialized = serialize!(report);
    // expire this report on the same clock that declares its worker dead
    let expiry = shared.config.thorium.events.worker_expiry_secs;
    // upsert this workers TTL'd counter report
    let _: () = query!(
        cmd("set").arg(&key).arg(serialized).arg("EX").arg(expiry),
        shared
    )
    .await?;
    Ok(())
}

/// Drain all of a workers in flight events and remove it from the registry
///
/// The registry entry is only removed after every kind drains so a crash mid drain
/// leaves this worker discoverable by the reaper.
///
/// A straggling pop can land mid drain and atomically claim events into this
/// workers keys while re-upserting its registry entry, so after deregistering we
/// drain again until a pass finds nothing: any claim that landed before the final
/// deregistration gets drained by the pass that follows it, and any claim that
/// lands after it re-registers this worker where the reaper will recover it.
///
/// # Arguments
///
/// * `worker` - The worker to drain and deregister
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::events::drain_worker", skip(shared), err(Debug))]
pub async fn drain_worker(worker: &str, shared: &Shared) -> Result<u64, ApiError> {
    // make sure this worker id is safe to embed in redis keys
    validate_worker_id(worker)?;
    // build the key to the worker registry
    let key = EventKeys::workers(shared);
    // track the total number of events drained
    let mut total = 0;
    // drain this workers in flight events for every kind of event
    for kind in EventType::iter() {
        // drain this kinds in flight events back to the pending queue
        total += reset_worker(kind, worker, shared).await?;
    }
    // deregister then re-drain until a pass finds nothing so a straggling pop
    // can never strand events in keys the reaper will not look at; bound the
    // passes so a client that never stops popping cannot pin this request
    for _ in 0..10 {
        // remove this worker from the registry now that its keys look drained
        let _: () = query!(cmd("zrem").arg(&key).arg(worker), shared).await?;
        // track the number of events drained after deregistering
        let mut straggled = 0;
        // re-drain this workers in flight events for every kind of event
        for kind in EventType::iter() {
            // drain any events a straggling pop claimed mid drain
            straggled += reset_worker(kind, worker, shared).await?;
        }
        // if nothing straggled in then this worker is drained and deregistered
        if straggled == 0 {
            return Ok(total);
        }
        // count the straggled events we drained
        total += straggled;
        // log that a pop claimed events mid drain
        event!(Level::WARN, msg = "pop landed mid drain", worker, straggled);
    }
    // this worker kept claiming events mid drain; its last claims were drained
    // and any further claims re-register it for the reaper to recover
    event!(
        Level::WARN,
        msg = "worker still popping after drain",
        worker
    );
    Ok(total)
}

/// Restore the in flight events in a queue whose score is older than a cutoff
///
/// Entries are restored individually through a claim script that only moves an
/// entry while it is still in flight below the cutoff, so concurrent reapers
/// move each entry exactly once and an id its owner re-leased or cleared after
/// the listing is left alone. An undeserializable body is restored raw at its
/// stale score so one poison body can never wedge the reaper.
///
/// # Arguments
///
/// * `kind` - The kind of events to restore
/// * `map_key` - The in flight map to restore event bodies from
/// * `queue_key` - The in flight queue to restore event ids from
/// * `cutoff` - The score below which an in flight entry is restored
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::events::restore_aged", skip_all, fields(kind = kind.as_str(), cutoff), err(Debug))]
async fn restore_aged(
    kind: EventType,
    map_key: &String,
    queue_key: &String,
    cutoff: i64,
    shared: &Shared,
) -> Result<u64, ApiError> {
    // build the key to the pending queue these events restore into
    let pending_key = EventKeys::queue(kind, shared);
    // get the ids and scores of every entry older than the cutoff
    let stale: Vec<(String, f64)> = query!(
        cmd("zrangebyscore")
            .arg(queue_key)
            .arg("-inf")
            .arg(format!("({cutoff}"))
            .arg("withscores"),
        shared
    )
    .await?;
    // track the number of events we restore
    let mut restored = 0;
    // restore each stale entry individually
    for (event_id, stale_score) in &stale {
        // try to get this events body without removing anything yet
        let serial: Option<String> = query!(cmd("hget").arg(map_key).arg(event_id), shared).await?;
        // handle any id whose body was already cleared or restored by a peer
        let Some(serial) = serial else {
            // drop the dangling queue entry only while it is still stale so an
            // owner that just re-claimed this id keeps its fresh lease
            let _: i64 = DROP_STALE_SCRIPT
                .key(queue_key)
                .arg(event_id)
                .arg(cutoff)
                .invoke_async(conn!(shared))
                .await?;
            continue;
        };
        // recover this events own timestamp to restore it at; one
        // undeserializable body (schema drift mid rolling upgrade or
        // corruption) restores raw at its stale score instead of aborting so a
        // poison body can never block every later entry on every reap pass
        let restore_score = match serde_json::from_str::<Event>(&serial) {
            // restore this event scored by its own body timestamp
            Ok(cast) => cast.timestamp.timestamp(),
            // restore the raw body at this entries stale in flight score
            Err(error) => {
                // warn that this events body could not be deserialized
                event!(
                    Level::WARN,
                    msg = "restoring undeserializable in flight event body",
                    event_id,
                    error = error.to_string(),
                );
                *stale_score as i64
            }
        };
        // atomically restore this entry only if it is still in flight below the
        // cutoff so concurrent reapers move it exactly once and a re-leased or
        // cleared id is left alone
        let claimed: i64 = RESTORE_STALE_SCRIPT
            .key(queue_key)
            .key(map_key)
            .key(&pending_key)
            .arg(event_id)
            .arg(cutoff)
            .arg(restore_score)
            .arg(&serial)
            .invoke_async(conn!(shared))
            .await?;
        // count this event as restored if our claim won
        restored += u64::try_from(claimed).unwrap_or(0);
    }
    Ok(restored)
}

/// Reap dead workers and over-aged in flight events back to the pending queues
///
/// This runs in three passes:
///
/// 1. Workers whose last heartbeat is older than the worker expiry have their in
///    flight events drained back to the pending queues for every kind and are then
///    removed from the registry (drain first so a crash mid reap leaves the owner
///    discoverable). The owners registry score is re-checked once per drain page to
///    bound the zombie return race.
/// 2. Live owners in flight entries whose lease is older than the age backstop are
///    restored individually (catches wedged but still beating workers).
/// 3. Legacy un-owned in flight entries older than the age backstop are restored the
///    same way so events stranded by old replicas recover after a rolling upgrade.
///
/// Concurrent reaps are safe since every drained entry moves exactly once and
/// restoring an identical member into the pending sorted set is idempotent.
///
/// # Arguments
///
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::events::reap", skip_all, err(Debug))]
pub async fn reap(shared: &Shared) -> Result<ReapSummary, ApiError> {
    // get the worker reclaim knobs from our config
    let events_conf = &shared.config.thorium.events;
    // get the current timestamp to compute cutoffs from
    let now = Utc::now().timestamp();
    // compute the heartbeat score below which a worker is presumed dead
    let dead_cutoff = dead_worker_cutoff(now, events_conf.worker_expiry_secs);
    // compute the lease score below which an in flight event is reclaimed
    let backstop = backstop_cutoff(now, events_conf.in_flight_backstop_secs);
    // build the key to the worker registry
    let workers_key = EventKeys::workers(shared);
    // start a summary of this reap pass
    let mut summary = ReapSummary::default();
    // get every worker whose last heartbeat is older than the worker expiry
    let dead: Vec<String> = query!(
        cmd("zrangebyscore")
            .arg(&workers_key)
            .arg("-inf")
            .arg(format!("({dead_cutoff}")),
        shared
    )
    .await?;
    // reap each dead worker
    for owner in &dead {
        // build the per page liveness re-check for this owner
        let check = ZombieCheck {
            workers_key: &workers_key,
            owner,
            dead_cutoff,
        };
        // track whether this owner came back to life mid drain
        let mut returned = false;
        // drain this owners in flight events for every kind of event
        for kind in EventType::iter() {
            // build the keys to this owners in flight map/queue
            let map_key = EventKeys::in_flight_map_owned(kind, owner, shared);
            let queue_key = EventKeys::in_flight_queue_owned(kind, owner, shared);
            // drain this kinds in flight events back to the pending queue
            let outcome = drain_in_flight(kind, &map_key, &queue_key, Some(&check), shared).await?;
            // count the events we reclaimed from this dead owner
            summary.reclaimed += outcome.drained;
            // stop reaping this owner if it came back to life
            if outcome.owner_returned {
                returned = true;
                break;
            }
        }
        // skip deregistering this owner if it came back to life
        if returned {
            continue;
        }
        // remove this owner from the registry now that its keys are drained
        let _: () = query!(cmd("zrem").arg(&workers_key).arg(owner), shared).await?;
        // count this dead worker as reaped
        summary.dead_workers += 1;
    }
    // get every worker with a live heartbeat
    let live: Vec<String> = query!(
        cmd("zrangebyscore")
            .arg(&workers_key)
            .arg(dead_cutoff)
            .arg("+inf"),
        shared
    )
    .await?;
    // apply the age backstop to every live owners in flight events
    for owner in &live {
        // check every kind of event for this owner
        for kind in EventType::iter() {
            // build the keys to this owners in flight map/queue
            let map_key = EventKeys::in_flight_map_owned(kind, owner, shared);
            let queue_key = EventKeys::in_flight_queue_owned(kind, owner, shared);
            // restore any of this owners events whose lease is older than the backstop
            summary.backstopped +=
                restore_aged(kind, &map_key, &queue_key, backstop, shared).await?;
        }
    }
    // restore any legacy un-owned in flight entries older than the backstop
    for kind in EventType::iter() {
        // build the keys to the legacy in flight map/queue
        let map_key = EventKeys::in_flight_map(kind, shared);
        let queue_key = EventKeys::in_flight_queue(kind, shared);
        // restore any legacy entries older than the backstop
        summary.legacy_reclaimed +=
            restore_aged(kind, &map_key, &queue_key, backstop, shared).await?;
    }
    Ok(summary)
}

/// Get the queue depths for every kind of event plus the live worker count
///
/// In flight depth sums the legacy un-owned queue with every registered owners own
/// queue so dead-but-unreaped owners events still count as in flight.
///
/// # Arguments
///
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::events::depths", skip_all, err(Debug))]
pub async fn depths(shared: &Shared) -> Result<EventDepths, ApiError> {
    // get the worker reclaim knobs from our config
    let events_conf = &shared.config.thorium.events;
    // get the current timestamp to compute cutoffs from
    let now = Utc::now().timestamp();
    // compute the heartbeat score below which a worker is presumed dead
    let dead_cutoff = dead_worker_cutoff(now, events_conf.worker_expiry_secs);
    // build the key to the worker registry
    let workers_key = EventKeys::workers(shared);
    // get every registered owner so we can enumerate their in flight queues
    let owners: Vec<String> =
        query!(cmd("zrange").arg(&workers_key).arg(0).arg(-1), shared).await?;
    // count the workers with a live heartbeat
    let workers: u64 = query!(
        cmd("zcount").arg(&workers_key).arg(dead_cutoff).arg("+inf"),
        shared
    )
    .await?;
    // build a map of depths for every kind of event
    let mut kinds = HashMap::with_capacity(3);
    // get the depths for every kind of event
    for kind in EventType::iter() {
        // build a redis pipeline to get this kinds depths in one round trip
        let mut pipe = redis::pipe();
        // get the depth of this kinds pending queue
        pipe.cmd("zcard").arg(EventKeys::queue(kind, shared));
        // get the depth of this kinds legacy un-owned in flight queue
        pipe.cmd("zcard")
            .arg(EventKeys::in_flight_queue(kind, shared));
        // get the depth of each registered owners in flight queue
        for owner in &owners {
            // get this owners in flight depth for this kind
            pipe.cmd("zcard")
                .arg(EventKeys::in_flight_queue_owned(kind, owner, shared));
        }
        // execute this pipeline
        let counts: Vec<u64> = pipe.query_async(conn!(shared)).await?;
        // the first count is the pending queue depth
        let queued = counts.first().copied().unwrap_or(0);
        // every other count is in flight (legacy + per owner)
        let in_flight = counts.iter().skip(1).sum();
        // save this kinds depths
        kinds.insert(kind, KindDepth { queued, in_flight });
    }
    Ok(EventDepths { kinds, workers })
}

/// Assemble the live scan stats aggregate from the registry, depths, and counters
///
/// Reads intersect the worker registry with the TTL'd counter keys so the live
/// view and the stats view agree on who exists: a wedged-but-still-beating
/// worker stays visible with frozen counters while a dead workers keys age out
/// on the same clock that declares it dead.
///
/// # Arguments
///
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::events::stats", skip_all, err(Debug))]
pub async fn stats(shared: &Shared) -> Result<EventStats, ApiError> {
    // get the worker reclaim knobs from our config
    let events_conf = &shared.config.thorium.events;
    // get the instant this aggregate covers
    let taken_at = Utc::now();
    // compute the heartbeat score below which a worker is presumed dead
    let dead_cutoff = dead_worker_cutoff(taken_at.timestamp(), events_conf.worker_expiry_secs);
    // build the key to the worker registry
    let workers_key = EventKeys::workers(shared);
    // get every worker with a live heartbeat
    let live: Vec<String> = query!(
        cmd("zrangebyscore")
            .arg(&workers_key)
            .arg(dead_cutoff)
            .arg("+inf"),
        shared
    )
    .await?;
    // get the queue depths these stats ride on
    let depths = depths(shared).await?;
    // seed every kinds stats from its depths so the key set is stable
    let mut kinds: HashMap<EventType, KindStats> = EventType::iter()
        .map(|kind| {
            // get this kinds depths defaulting to empty queues
            let depth = depths.kinds.get(&kind).copied().unwrap_or_default();
            // start this kinds stats with its depths and no counters yet
            let stats = KindStats {
                queued: depth.queued,
                in_flight: depth.in_flight,
                counters: BTreeMap::new(),
            };
            (kind, stats)
        })
        .collect();
    // build the per worker breakdown behind the per kind sums
    let mut per_worker: HashMap<String, HashMap<EventType, WorkerStatsReport>> =
        HashMap::with_capacity(live.len());
    // only read counter keys when workers are live since a zero key mget errors
    if !live.is_empty() {
        // build the (kind, worker) pairs and their counter keys in lock step
        let mut pairs = Vec::with_capacity(live.len() * kinds.len());
        let mut keys = Vec::with_capacity(live.len() * kinds.len());
        for kind in EventType::iter() {
            for worker in &live {
                pairs.push((kind, worker));
                keys.push(EventKeys::stats(kind, worker, shared));
            }
        }
        // get every live workers counter reports in one round trip
        let raw: Vec<Option<String>> = query!(cmd("mget").arg(&keys), shared).await?;
        // fold each published report into its kinds sums and the per worker map
        for ((kind, worker), raw) in pairs.into_iter().zip(raw.into_iter()) {
            // skip workers that have not published counters for this kind
            if let Some(raw) = raw {
                // deserialize this workers counter report
                let report: WorkerStatsReport = deserialize!(&raw);
                // sum this reports counters into its kinds totals
                if let Some(kind_stats) = kinds.get_mut(&kind) {
                    for (name, count) in &report.counters {
                        // saturate so huge counter values can never wrap the sums
                        let total = kind_stats.counters.entry(name.clone()).or_insert(0);
                        *total = total.saturating_add(*count);
                    }
                }
                // save this report under its worker for wedge debugging
                per_worker
                    .entry(worker.clone())
                    .or_default()
                    .insert(kind, report);
            }
        }
    }
    // count live workers from the same read the per worker view is built from
    let workers = live.len() as u64;
    Ok(EventStats {
        taken_at,
        workers,
        kinds,
        per_worker,
    })
}

/// Quantize a timestamp down to the start of its snapshot window
///
/// Quantization is the cross replica dedup mechanism: two replicas triggering
/// in the same window write the same clustering key and last write wins.
///
/// # Arguments
///
/// * `ts` - The unix timestamp to quantize
/// * `step` - The snapshot cadence in seconds
fn quantize_window(ts: i64, step: u64) -> i64 {
    // clamp the step into a nonzero i64 so a huge or zero config value can
    // never wrap or divide-by-zero the window math
    let step = i64::try_from(step).unwrap_or(i64::MAX).max(1);
    // floor this timestamp to the start of its window
    ts - ts.rem_euclid(step)
}

/// Persist one quantized scan stats snapshot unless a peer already has
///
/// The window gate is checked first so racing triggers skip the assembly work,
/// but it is only a cost optimization: a replica that loses the gate race and
/// writes anyway collapses onto the same quantized clustering key. The gate is
/// only taken after the row lands so a failed or client-cancelled attempt can
/// never claim a window without writing it — peers (or the next beat) just
/// retry the window instead of leaving a permanent gap in the series.
///
/// # Arguments
///
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::events::snapshot_stats", skip_all, err(Debug))]
pub async fn snapshot_stats(shared: &Shared) -> Result<Option<ScanStatsSnapshot>, ApiError> {
    // get the events knobs from our config
    let events_conf = &shared.config.thorium.events;
    // quantize now down to the window this snapshot covers
    let window = quantize_window(Utc::now().timestamp(), events_conf.stats_snapshot_secs);
    // build the key to this windows snapshot gate
    let gate = EventKeys::snapshot_gate(window, shared);
    // a peer already wrote this window so skip the work
    let written: Option<String> = query!(cmd("get").arg(&gate), shared).await?;
    if written.is_some() {
        return Ok(None);
    }
    // assemble the live aggregate this snapshot persists
    let stats = stats(shared).await?;
    // rebuild the quantized window as the rows clustering timestamp
    let Some(snapshot_at) = DateTime::from_timestamp(window, 0) else {
        return internal_err!(format!("Snapshot window {window} is out of range"));
    };
    // persist only the anonymous aggregate, never the per worker breakdown
    let snapshot = ScanStatsSnapshot {
        snapshot_at,
        workers: stats.workers,
        kinds: stats.kinds,
    };
    // compute the partition this snapshot lands in
    let year = snapshot_at.year();
    let bucket = helpers::partition(snapshot_at, year, events_conf.stats_partition_size);
    // serialize this snapshot for scylla
    let serialized = serialize!(&snapshot);
    // build this windows full row borrowing the serialized payload
    let row = ScanStatsTableRef {
        year,
        bucket,
        snapshot_at,
        data: &serialized,
    };
    // write this windows snapshot row; the tables TTL expires it whole
    shared
        .scylla
        .queries
        .scan_stats
        .insert(&shared.scylla.session, &row)
        .await?;
    // take the gate only now that the row landed so a failure above never
    // claims the window; racing writers collapse onto the same clustering
    // key so a lost race here is still a written window
    let acquired: Option<String> = query!(
        cmd("set")
            .arg(&gate)
            .arg(1)
            .arg("NX")
            .arg("EX")
            .arg(events_conf.stats_snapshot_secs),
        shared
    )
    .await?;
    // a peer that raced us wrote the same quantized row so report theirs won
    if acquired.is_none() {
        return Ok(None);
    }
    Ok(Some(snapshot))
}

/// The hard cap on points one stats history response may carry
///
/// Thirty days at the default 30s cadence is 86,400 rows; no chart wants
/// them, so strides are clamped to keep responses at or under this cap.
const MAX_HISTORY_POINTS: u64 = 5_000;

/// How many buckets one ranged scan stats pull may bind at once
const HISTORY_BUCKETS_PER_PULL: usize = 32;

/// How many stride window reads may be in flight at once
const HISTORY_STRIDE_CONCURRENCY: usize = 16;

/// Compute the effective downsample stride for a stats history read
///
/// # Arguments
///
/// * `range_secs` - How many seconds the requested range covers
/// * `requested` - The stride the caller asked for if any
/// * `snapshot_secs` - The snapshot cadence in seconds
fn effective_step(range_secs: u64, requested: Option<u64>, snapshot_secs: u64) -> u64 {
    // default to a chart friendly ~1,000 points across the range
    let wanted = requested.unwrap_or(range_secs / 1_000);
    // cap the stride at the range itself so an absurd request can never
    // overflow the window math downstream (the stride is cast to an i64
    // chrono duration; an unclamped u64::MAX would wrap negative and walk
    // the window loop backwards forever)
    let wanted = wanted.min(range_secs.max(1));
    // floor the stride at the snapshot cadence since finer strides are empty,
    // and clamp it so a response can never exceed the point cap
    wanted
        .max(snapshot_secs)
        .max(1)
        .max(range_secs.div_ceil(MAX_HISTORY_POINTS))
}

/// Enumerate the scan stats partitions a time range touches
///
/// Partitions are enumerated arithmetically since scan stats keep no census of
/// populated buckets; empty partitions just return no rows.
///
/// # Arguments
///
/// * `start` - The oldest instant the range covers
/// * `end` - The newest instant the range covers
/// * `chunk` - The scan stats partition size in seconds
fn scan_stats_partitions(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    chunk: u16,
) -> Vec<(i32, Vec<i32>)> {
    // build the per year bucket lists this range touches
    let mut partitions = Vec::with_capacity((end.year() - start.year() + 1).max(1) as usize);
    // walk every year between our endpoints
    for year in start.year()..=end.year() {
        // interior years start at their first bucket
        let first = if year == start.year() {
            u32::try_from(helpers::partition(start, year, chunk)).unwrap_or_default()
        } else {
            0
        };
        // interior years run through their final bucket
        let last = if year == end.year() {
            u32::try_from(helpers::partition(end, year, chunk)).unwrap_or_default()
        } else {
            Bucket::last_of_year(year, chunk)
                .map(Bucket::get)
                .unwrap_or_default()
        };
        // collect this years buckets as the i32s scylla stores
        let buckets = (first..=last).map(|bucket| bucket as i32).collect();
        partitions.push((year, buckets));
    }
    partitions
}

/// Fetch the newest persisted snapshot inside one stride window
///
/// A window smaller than a partition can straddle at most one partition or
/// year boundary, so this is one or two bounded single-row-per-partition reads.
///
/// # Arguments
///
/// * `window` - The half open window to fetch the newest row from
/// * `chunk` - The scan stats partition size in seconds
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::events::newest_in_window", skip_all, err(Debug))]
async fn newest_in_window(
    window: (DateTime<Utc>, DateTime<Utc>),
    chunk: u16,
    shared: &Shared,
) -> Result<Option<ScanStatsSnapshot>, ApiError> {
    // track the newest row seen across this windows partitions
    let mut newest: Option<(DateTime<Utc>, String)> = None;
    // read the newest row from each slice of this windows partitions
    for (year, buckets) in scan_stats_partitions(window.0, window.1, chunk) {
        // bind a bounded number of buckets per pull so a wide stride over
        // small partitions can never exceed scylla's partition key IN cap
        for bucket_chunk in buckets.chunks(HISTORY_BUCKETS_PER_PULL) {
            // read at most one row per partition inside this windows slice
            let query = shared
                .scylla
                .session
                .execute_unpaged(
                    &shared.scylla.queries.scan_stats.pull,
                    (year, bucket_chunk, window.1, window.0, 1_i32),
                )
                .await?;
            // make sure we got rows
            let query_rows = query.into_rows_result()?;
            // keep the newest row across this windows partitions
            for cast in query_rows.rows::<(DateTime<Utc>, String)>()? {
                // make sure the row is valid
                let (snapshot_at, data) = cast?;
                // newer rows win across partition slices
                if newest.as_ref().is_none_or(|(ts, _)| snapshot_at > *ts) {
                    newest = Some((snapshot_at, data));
                }
            }
        }
    }
    // parse the newest rows payload if this window holds one
    match newest {
        Some((_, data)) => {
            // deserialize this windows newest snapshot
            let snapshot: ScanStatsSnapshot = deserialize!(&data);
            Ok(Some(snapshot))
        }
        None => Ok(None),
    }
}

/// Read a ranged, downsampled slice of the scan stats history oldest first
///
/// Two read paths keep this bounded: when the range could only hold at most
/// the point cap of rows at the snapshot cadence, whole partitions are pulled
/// in bounded chunks and strided in memory; wider ranges instead issue one
/// single-row read per stride window so reads scale with the points returned,
/// never with the rows persisted.
///
/// # Arguments
///
/// * `params` - The half open range and stride to read
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::events::stats_history", skip_all, err(Debug))]
pub async fn stats_history(
    params: &StatsHistoryParams,
    shared: &Shared,
) -> Result<Vec<ScanStatsSnapshot>, ApiError> {
    // get the events knobs from our config
    let events_conf = &shared.config.thorium.events;
    // reject inverted or empty ranges outright
    if params.start >= params.end {
        return bad!(format!(
            "stats history start ({}) must be before end ({})",
            params.start, params.end
        ));
    }
    // compute how many seconds this range covers
    let range_secs = u64::try_from((params.end - params.start).num_seconds()).unwrap_or_default();
    // bound the range by the retention window since nothing older can exist
    if range_secs > events_conf.stats_retention {
        return bad!(format!(
            "stats history ranges are capped at the retention window of {} seconds but got {}",
            events_conf.stats_retention, range_secs
        ));
    }
    // compute the effective downsample stride for this read
    let step = effective_step(
        range_secs,
        params.step_secs,
        events_conf.stats_snapshot_secs,
    );
    // get the partition size these rows are bucketed by
    let chunk = events_conf.stats_partition_size;
    // estimate the raw rows a full pull would read at the snapshot cadence
    let estimated = range_secs / events_conf.stats_snapshot_secs.max(1);
    // narrow ranges pull whole partitions and stride in memory
    if estimated <= MAX_HISTORY_POINTS {
        // collect every row this range holds from bounded partition chunks
        let mut rows: Vec<(DateTime<Utc>, String)> = Vec::new();
        for (year, buckets) in scan_stats_partitions(params.start, params.end, chunk) {
            // bind a bounded number of buckets per pull
            for bucket_chunk in buckets.chunks(HISTORY_BUCKETS_PER_PULL) {
                // pull this chunks rows inside our half open range
                let query = shared
                    .scylla
                    .session
                    .execute_unpaged(
                        &shared.scylla.queries.scan_stats.pull,
                        (
                            year,
                            bucket_chunk,
                            params.end,
                            params.start,
                            i32::try_from(MAX_HISTORY_POINTS).unwrap_or(i32::MAX),
                        ),
                    )
                    .await?;
                // make sure we got rows
                let query_rows = query.into_rows_result()?;
                // collect this chunks rows
                for cast in query_rows.rows::<(DateTime<Utc>, String)>()? {
                    rows.push(cast?);
                }
            }
        }
        // iterate newest first so the first row seen per stride window is the
        // newest one, matching the stride paths single row reads
        rows.sort_unstable_by(|left, right| right.0.cmp(&left.0));
        // keep the newest row per stride window keyed by window index
        let mut by_window: BTreeMap<i64, String> = BTreeMap::new();
        for (snapshot_at, data) in rows {
            // compute the stride window this row falls in
            let index = (snapshot_at - params.start).num_seconds() / step.max(1) as i64;
            // the first row seen per window is its newest
            by_window.entry(index).or_insert(data);
        }
        // parse the kept rows into snapshots oldest first
        let mut history = Vec::with_capacity(by_window.len());
        for data in by_window.into_values() {
            // deserialize this windows newest snapshot
            let snapshot: ScanStatsSnapshot = deserialize!(&data);
            history.push(snapshot);
        }
        Ok(history)
    } else {
        // wide ranges read one row per stride window so reads scale with the
        // points returned; build the half open windows oldest first
        let mut windows =
            Vec::with_capacity(usize::try_from(range_secs.div_ceil(step)).unwrap_or_default());
        let mut cursor = params.start;
        while cursor < params.end {
            // cap the final window at the ranges exclusive end
            let window_end = (cursor + chrono::Duration::seconds(step as i64)).min(params.end);
            windows.push((cursor, window_end));
            cursor = window_end;
        }
        // fetch each windows newest row with bounded concurrency; buffered
        // streams keep their inputs oldest first order
        let mut fetches = stream::iter(windows)
            .map(|window| newest_in_window(window, chunk, shared))
            .buffered(HISTORY_STRIDE_CONCURRENCY);
        // collect the windows that hold a row; empty windows stay honest gaps
        let mut history = Vec::new();
        while let Some(fetched) = fetches.next().await {
            // raise an error if any window read failed
            if let Some(snapshot) = fetched? {
                history.push(snapshot);
            }
        }
        Ok(history)
    }
}

/// The raw cache status fields read from redis in one hmget
///
/// Redis nils for unset staleness flags deserialize to false and unset generation
/// counters come back as None so fresh installs and rolling upgrades stay safe.
type RawCacheStatus = (
    bool,
    bool,
    bool,
    bool,
    Option<u64>,
    Option<u64>,
    Option<u64>,
    Option<u64>,
);

/// get the event cache status
///
/// # Arguments
///
/// * `clear` - Whether to clear the event cache statuses or not
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::events::get_cache_status", skip(shared), err(Debug))]
pub async fn get_cache_status(clear: bool, shared: &Shared) -> Result<EventCacheStatus, ApiError> {
    // get our event handler cache key
    let key = EventKeys::cache(shared);
    // build a redis pipleline
    let mut pipe = redis::pipe();
    // get each cache components staleness flag and generation counter in one
    // round trip; redis nils for unset fields deserialize to false/None which
    // keeps fresh installs and rolling upgrades from the legacy fields safe
    pipe.atomic()
        .cmd("hmget")
        .arg(&key)
        .arg("triggers")
        .arg("users")
        .arg("sigma_rules")
        .arg("yara_rules")
        .arg("triggers_gen")
        .arg("users_gen")
        .arg("sigma_rules_gen")
        .arg("yara_rules_gen");
    // if we also need to clear our cache staleness flags then add that
    // the generation counters are monotonic and are never cleared
    let fields = if clear {
        // clear every per component staleness flag
        pipe.cmd("hset")
            .arg(&key)
            .arg("triggers")
            .arg(false)
            .arg("users")
            .arg(false)
            .arg("sigma_rules")
            .arg(false)
            .arg("yara_rules")
            .arg(false);
        // drop the legacy single status flag if an old api replica wrote one
        pipe.cmd("hdel").arg(&key).arg("status");
        // send our query and drop our set/del responses
        let (fields, _, _): (RawCacheStatus, u64, u64) = pipe.query_async(conn!(shared)).await?;
        fields
    } else {
        // send our query
        let (fields,): (RawCacheStatus,) = pipe.query_async(conn!(shared)).await?;
        fields
    };
    // unpack the raw cache status fields
    let (triggers, users, sigma_rules, yara_rules, triggers_gen, users_gen, sigma_gen, yara_gen) =
        fields;
    // build our event cache status object defaulting unset generations to 0
    let cache_status = EventCacheStatus {
        triggers,
        users,
        sigma_rules,
        yara_rules,
        triggers_gen: triggers_gen.unwrap_or(0),
        users_gen: users_gen.unwrap_or(0),
        sigma_rules_gen: sigma_gen.unwrap_or(0),
        yara_rules_gen: yara_gen.unwrap_or(0),
    };
    Ok(cache_status)
}

/// Mark an entity rule cache component as stale after a rule entity mutation
///
/// This is a no-op for entity kinds that no event handler cache is built from.
///
/// # Arguments
///
/// * `kind` - The kind of entity that was mutated
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::events::set_rules_stale", skip(shared), err(Debug))]
pub async fn set_rules_stale(kind: EntityKinds, shared: &Shared) -> Result<(), ApiError> {
    // map this entity kind to its staleness flag and generation counter
    let (field, gen_field) = match kind {
        EntityKinds::SigmaRule => ("sigma_rules", "sigma_rules_gen"),
        EntityKinds::YaraRule => ("yara_rules", "yara_rules_gen"),
        // no event handler cache is built from this entity kind
        _ => return Ok(()),
    };
    // get our event handler cache key
    let key = EventKeys::cache(shared);
    // build a redis pipeline to mark this cache component as stale
    let mut pipe = redis::pipe();
    // mark this cache component as stale for older event handlers
    pipe.cmd("hset")
        .arg(&key)
        .arg(field)
        .arg(true)
        // bump this components generation counter for replica safe polling
        .cmd("hincrby")
        .arg(&key)
        .arg(gen_field)
        .arg(1);
    // execute this pipeline
    let _: () = pipe.query_async(conn!(shared)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Model refresh events wait for nothing, whatever delay is configured for workflow graph
    /// events
    #[test]
    fn model_refresh_has_no_delay() {
        // the default and a configured workflow graph delay
        let defaults = Events::default();
        let configured = Events {
            workflow_graph_delay: 2,
            ..Events::default()
        };
        // a refresh is popped on the worker's next pass either way
        assert_eq!(maturity_delay(EventType::ModelRefresh, &defaults), 0);
        assert_eq!(maturity_delay(EventType::ModelRefresh, &configured), 0);
    }

    /// Workflow graph events wait for the configured delay and every other kind waits 3 seconds
    #[test]
    fn delay_per_queue() {
        // the default config waits 10 seconds for workflow graph events
        let defaults = Events::default();
        assert_eq!(maturity_delay(EventType::WorkflowGraph, &defaults), 10);
        // a configured delay is used instead
        let configured = Events {
            workflow_graph_delay: 2,
            ..Events::default()
        };
        assert_eq!(maturity_delay(EventType::WorkflowGraph, &configured), 2);
        // the other kinds keep their 3 second delay whatever is configured
        for kind in [
            EventType::ReactionTrigger,
            EventType::SigmaScannableResults,
            EventType::YaraScannableSample,
            EventType::YaraHuntChunk,
        ] {
            assert_eq!(maturity_delay(kind, &defaults), 3);
            assert_eq!(maturity_delay(kind, &configured), 3);
        }
    }

    /// The dead worker cutoff must classify heartbeat scores correctly
    #[test]
    fn dead_worker_cutoff_classifies_scores() {
        // pin a current timestamp and the default 90s expiry
        let now = 1_000_000;
        let cutoff = dead_worker_cutoff(now, 90);
        // the cutoff must sit exactly one expiry in the past
        assert_eq!(cutoff, 999_910);
        // a worker that beat exactly at the cutoff is still alive (strictly below is dead)
        assert!(is_live(Some(cutoff as f64), cutoff));
        // a worker that beat after the cutoff is alive
        assert!(is_live(Some((cutoff + 1) as f64), cutoff));
        // a worker that last beat before the cutoff is dead
        assert!(!is_live(Some((cutoff - 1) as f64), cutoff));
        // an unregistered worker is not alive
        assert!(!is_live(None, cutoff));
    }

    /// The backstop cutoff must be strictly older than the dead worker cutoff
    /// under any validated config so death detection always fires first
    #[test]
    fn backstop_cutoff_is_more_patient_than_expiry() {
        // pin a current timestamp and the default knobs
        let now = 1_000_000;
        // compute both cutoffs with the default 90s expiry and 3600s backstop
        let dead = dead_worker_cutoff(now, 90);
        let backstop = backstop_cutoff(now, 3600);
        // the backstop cutoff must be further in the past than the dead cutoff
        assert!(backstop < dead);
        // the backstop must sit exactly one backstop window in the past
        assert_eq!(backstop, 996_400);
    }

    /// Huge config values must clamp instead of wrapping the cutoffs
    #[test]
    fn cutoffs_saturate_on_huge_configs() {
        // a u64 expiry larger than i64::MAX must clamp to i64::MAX and not wrap
        assert_eq!(dead_worker_cutoff(0, u64::MAX), -i64::MAX);
        // a u64 backstop larger than i64::MAX must clamp and not wrap as well
        assert_eq!(backstop_cutoff(0, u64::MAX), -i64::MAX);
        // a cutoff already at the minimum must saturate instead of overflowing
        assert_eq!(dead_worker_cutoff(i64::MIN, 90), i64::MIN);
    }

    /// Snapshot windows must floor to their start and survive degenerate steps
    #[test]
    fn quantize_window_floors_to_window_start() {
        // mid window timestamps floor to their windows start
        assert_eq!(quantize_window(1_000_031, 30), 1_000_020);
        // window boundaries are already quantized
        assert_eq!(quantize_window(1_000_020, 30), 1_000_020);
        // a zero step can never divide by zero
        assert_eq!(quantize_window(1_000_031, 0), 1_000_031);
        // a step beyond i64 clamps instead of wrapping
        assert_eq!(quantize_window(10, u64::MAX), 0);
    }

    /// The effective stride must default, floor, and clamp correctly
    #[test]
    fn effective_step_defaults_and_caps() {
        // no requested stride defaults to ~1,000 points across the range
        assert_eq!(effective_step(100_000, None, 30), 100);
        // the default stride floors at the snapshot cadence on narrow ranges
        assert_eq!(effective_step(1_000, None, 30), 30);
        // a requested stride below the cadence clamps up to it
        assert_eq!(effective_step(1_000, Some(5), 30), 30);
        // a requested stride above the cadence is honored
        assert_eq!(effective_step(1_000, Some(120), 30), 120);
        // the stride clamps so a response can never exceed the point cap
        let range = 2_592_000;
        let step = effective_step(range, Some(30), 30);
        assert_eq!(step, range.div_ceil(MAX_HISTORY_POINTS));
        assert!(range.div_ceil(step) <= MAX_HISTORY_POINTS);
        // an absurd requested stride caps at the range so the i64 window
        // math downstream can never wrap negative or overflow chrono
        assert_eq!(effective_step(2_592_000, Some(u64::MAX), 30), 2_592_000);
        // a sub second range still yields a positive stride
        assert_eq!(effective_step(0, Some(u64::MAX), 30), 30);
    }

    /// Partition enumeration must cover ranges inside and across years
    #[test]
    fn scan_stats_partitions_spans_years() {
        // a 12 hour chunk mirrors the production default
        let chunk = 43_200;
        // a range inside one partition touches exactly that partition
        let start = Utc.with_ymd_and_hms(2026, 6, 1, 1, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2026, 6, 1, 2, 0, 0).unwrap();
        let same = scan_stats_partitions(start, end, chunk);
        assert_eq!(same.len(), 1);
        assert_eq!(same[0].0, 2026);
        assert_eq!(same[0].1.len(), 1);
        // a multi day range touches two buckets per day
        let end = Utc.with_ymd_and_hms(2026, 6, 3, 1, 0, 0).unwrap();
        let multi = scan_stats_partitions(start, end, chunk);
        assert_eq!(multi[0].1.len(), 5);
        // a december to january range spans both years buckets
        let start = Utc.with_ymd_and_hms(2026, 12, 31, 23, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2027, 1, 1, 1, 0, 0).unwrap();
        let crossed = scan_stats_partitions(start, end, chunk);
        assert_eq!(crossed.len(), 2);
        // the old year runs through its final bucket
        assert_eq!(crossed[0].0, 2026);
        assert_eq!(
            *crossed[0].1.last().unwrap(),
            Bucket::last_of_year(2026, chunk).unwrap().get() as i32
        );
        // the new year starts at its first bucket
        assert_eq!(crossed[1].0, 2027);
        assert_eq!(crossed[1].1.first(), Some(&0));
    }
}
