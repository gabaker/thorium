//! The event queue inspection helper for integration tests
//!
//! Tests read queued events with the admin peek route, which leaves the queue untouched, and can
//! snapshot a queue's Redis state to prove a call did not move or remove any events.

use bb8_redis::{RedisConnectionManager, bb8::Pool};
use redis::cmd;
use std::collections::BTreeSet;
use tokio::sync::OnceCell;
use uuid::Uuid;

use super::CONF;
use super::api::get_redis_client;
use crate::models::{
    Event, EventData, EventPeekOpts, EventType, ModelRefreshEvent, WorkflowGraphChange,
    WorkflowGraphEvent,
};
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

/// Queue events straight into Redis, each scored by the second of its own timestamp
///
/// This is how the API queues an event, so tests can queue many events at once or events that
/// no route emits.
///
/// # Arguments
///
/// * `events` - The events to queue
pub async fn push(events: &[Event]) -> Result<(), Error> {
    // get or build our Redis pool
    let pool = REDIS.get_or_init(|| get_redis_client(&CONF)).await;
    // get a connection from our pool
    let mut conn = pool.get().await.map_err(redis_error)?;
    // add each event to its kind's queue in one pipeline
    let mut pipe = redis::pipe();
    for event in events {
        let serialized = serde_json::to_string(event)
            .map_err(|err| Error::new(format!("Failed to serialize an event: {err}")))?;
        pipe.cmd("zadd")
            .arg(event_key("queue", EventType::from(&event.data)))
            .arg(event.timestamp.timestamp())
            .arg(serialized);
    }
    let _: () = pipe.query_async(&mut *conn).await.map_err(redis_error)?;
    Ok(())
}

/// Drop every queued and in flight event of one kind
///
/// Tests that run a worker over a whole queue start from an empty one this way.
///
/// # Arguments
///
/// * `kind` - The kind of events to drop
pub async fn discard(kind: EventType) -> Result<(), Error> {
    // get or build our Redis pool
    let pool = REDIS.get_or_init(|| get_redis_client(&CONF)).await;
    // get a connection from our pool
    let mut conn = pool.get().await.map_err(redis_error)?;
    // delete the queue, the in flight queue, and the in flight map
    let _: () = cmd("del")
        .arg(event_key("queue", kind))
        .arg(event_key("in_flight_queue", kind))
        .arg(event_key("in_flight_map", kind))
        .query_async(&mut *conn)
        .await
        .map_err(redis_error)?;
    Ok(())
}

/// Count the commands Redis has run since it started, across every client
///
/// Comparing counts taken before and after some work shows how many Redis operations it cost.
pub async fn redis_commands() -> Result<u64, Error> {
    // get or build our Redis pool
    let pool = REDIS.get_or_init(|| get_redis_client(&CONF)).await;
    // get a connection from our pool
    let mut conn = pool.get().await.map_err(redis_error)?;
    // read the server's stats
    let info: String = cmd("info")
        .arg("stats")
        .query_async(&mut *conn)
        .await
        .map_err(redis_error)?;
    // find the processed command count
    info.lines()
        .find_map(|line| line.strip_prefix("total_commands_processed:"))
        .and_then(|count| count.trim().parse().ok())
        .ok_or_else(|| Error::new("Redis INFO has no total_commands_processed"))
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

/// Get the workflow graph change an event carries, if it's a workflow graph event
///
/// # Arguments
///
/// * `event` - The event to read
#[must_use]
pub fn graph_event(event: &Event) -> Option<&WorkflowGraphEvent> {
    // only workflow graph events carry a change
    match &event.data {
        EventData::WorkflowGraph(graph) => Some(graph),
        _ => None,
    }
}

/// Get the model refresh an event carries, if it's a model refresh event
///
/// # Arguments
///
/// * `event` - The event to read
#[must_use]
pub fn refresh_event(event: &Event) -> Option<&ModelRefreshEvent> {
    // only model refresh events carry a refresh
    match &event.data {
        EventData::ModelRefresh(refresh) => Some(refresh),
        _ => None,
    }
}

/// Whether an event is a model refresh whose target names an endpoint
///
/// # Arguments
///
/// * `event` - The event to check
/// * `endpoint` - The endpoint the refresh must name
#[must_use]
pub fn refresh_targets(event: &Event, endpoint: Uuid) -> bool {
    // only endpoint targets name an endpoint
    refresh_event(event).is_some_and(|refresh| refresh.target.endpoint() == Some(endpoint))
}

/// Get the queued model refresh events whose target names an endpoint, oldest first
///
/// # Arguments
///
/// * `client` - An admin client to peek with
/// * `endpoint` - The endpoint the refreshes must name
pub async fn refresh_events(client: &Thorium, endpoint: Uuid) -> Result<Vec<Event>, Error> {
    // keep the refreshes of this endpoint
    queued_matching(client, EventType::ModelRefresh, |event| {
        refresh_targets(event, endpoint)
    })
    .await
}

/// Get the entities a workflow graph change names
///
/// # Arguments
///
/// * `change` - The change to read
#[must_use]
pub fn changed_ids(change: &WorkflowGraphChange) -> Vec<Uuid> {
    // list every id the change carries
    match change {
        WorkflowGraphChange::WorkflowUpdated { workflow }
        | WorkflowGraphChange::ManualInstance { workflow } => vec![*workflow],
        WorkflowGraphChange::ToolUpdated {
            tool,
            tool_instance,
        } => vec![*tool, *tool_instance],
        WorkflowGraphChange::AgentUpdated { agent } => vec![*agent],
        WorkflowGraphChange::EdgeChanged { source, target, .. } => vec![*source, *target],
        WorkflowGraphChange::ToolSetChanged { tool_set }
        | WorkflowGraphChange::ManualToolSetInstance { tool_set } => vec![*tool_set],
        WorkflowGraphChange::ToolSetUpdated {
            tool_set,
            tool_set_instance,
        } => vec![*tool_set, *tool_set_instance],
        WorkflowGraphChange::ToolTagsChanged { tool, .. } => vec![*tool],
    }
}

/// Get every queued workflow graph event whose change names an entity, oldest first
///
/// Events are only ever added to the queue while no event handler runs, so the events a call
/// queued are the ones after the count read before it.
///
/// # Arguments
///
/// * `client` - An admin client to peek with
/// * `id` - The entity the changes must name
pub async fn graph_changes(client: &Thorium, id: Uuid) -> Result<Vec<Event>, Error> {
    // keep the workflow graph events that name this entity
    queued_matching(client, EventType::WorkflowGraph, |event| {
        graph_event(event).is_some_and(|graph| changed_ids(&graph.change).contains(&id))
    })
    .await
}
