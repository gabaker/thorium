//! The event queue inspection helper for integration tests
//!
//! Tests read queued events with the admin peek route, which leaves the queue untouched, and can
//! snapshot a queue's Redis state to prove a call did not move or remove any events.

use bb8_redis::{RedisConnectionManager, bb8::Pool};
use redis::cmd;
use std::collections::BTreeSet;
use tokio::sync::OnceCell;

use super::CONF;
use super::api::get_redis_client;
use crate::models::{Event, EventPeekOpts, EventType};
use crate::{Error, Thorium};

/// The most events a single inspection reads from a queue
const PEEK_LIMIT: usize = 100_000;

/// The Redis pool used to snapshot event queues
static REDIS: OnceCell<Pool<RedisConnectionManager>> = OnceCell::const_new();

/// The Redis state of one kind of event queue
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueState {
    /// The serialized events waiting in the queue with their scores, oldest first
    pub queued: Vec<(String, String)>,
    /// The ids of the events in the in flight queue
    pub in_flight: BTreeSet<String>,
    /// The ids of the events in the in flight data map
    pub in_flight_map: BTreeSet<String>,
}

/// Build the Redis key for one of an event kind's structures
///
/// These keys match the ones `EventKeys` builds in the API's Redis backend.
///
/// # Arguments
///
/// * `structure` - The structure to build a key for (`queue`, `in_flight_queue`, or `in_flight_map`)
/// * `kind` - The kind of events in this structure
fn event_key(structure: &str, kind: EventType) -> String {
    // build the key in the configured namespace
    format!(
        "{ns}:event-handler:{structure}:{kind}",
        ns = CONF.thorium.namespace
    )
}

/// Wrap a Redis error in a client error
///
/// # Arguments
///
/// * `error` - The error to wrap
#[allow(clippy::needless_pass_by_value)]
fn redis_error<E: std::fmt::Display>(error: E) -> Error {
    // describe the failed Redis call
    Error::new(format!("Failed to read an event queue from Redis: {error}"))
}

/// Snapshot the Redis state of one kind of event queue
///
/// Comparing snapshots taken before and after a call shows whether that call moved, removed,
/// or added any events of this kind.
///
/// # Arguments
///
/// * `kind` - The kind of event queue to snapshot
pub async fn queue_state(kind: EventType) -> Result<QueueState, Error> {
    // get or build our Redis pool
    let pool = REDIS.get_or_init(|| get_redis_client(&CONF)).await;
    // get a connection from our pool
    let mut conn = pool.get().await.map_err(redis_error)?;
    // build a pipeline reading the queue, the in flight queue, and the in flight map
    let mut pipe = redis::pipe();
    pipe.atomic()
        .add_command(
            cmd("zrange")
                .arg(event_key("queue", kind))
                .arg(0)
                .arg(-1)
                .arg("withscores")
                .clone(),
        )
        .add_command(
            cmd("zrange")
                .arg(event_key("in_flight_queue", kind))
                .arg(0)
                .arg(-1)
                .clone(),
        )
        .add_command(cmd("hkeys").arg(event_key("in_flight_map", kind)).clone());
    // read all three structures at once
    let (queued, in_flight, in_flight_map): (Vec<(String, String)>, Vec<String>, Vec<String>) =
        pipe.query_async(&mut *conn).await.map_err(redis_error)?;
    // build our snapshot
    Ok(QueueState {
        queued,
        in_flight: in_flight.into_iter().collect(),
        in_flight_map: in_flight_map.into_iter().collect(),
    })
}

/// Get every queued event of one kind, oldest first, without moving any of them
///
/// # Arguments
///
/// * `client` - An admin client to peek with
/// * `kind` - The kind of events to read
pub async fn queued(client: &Thorium, kind: EventType) -> Result<Vec<Event>, Error> {
    // peek at the whole queue
    let opts = EventPeekOpts::default().limit(PEEK_LIMIT);
    client.events.peek(kind, &opts).await
}

/// Get the queued events of one kind that match a filter, oldest first
///
/// # Arguments
///
/// * `client` - An admin client to peek with
/// * `kind` - The kind of events to read
/// * `filter` - The check an event must pass to be returned
pub async fn queued_matching<F>(
    client: &Thorium,
    kind: EventType,
    filter: F,
) -> Result<Vec<Event>, Error>
where
    F: Fn(&Event) -> bool,
{
    // read every queued event of this kind
    let mut events = queued(client, kind).await?;
    // keep only the events that match our filter
    events.retain(|event| filter(event));
    Ok(events)
}
