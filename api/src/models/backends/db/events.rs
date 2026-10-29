//! The events in Thorium for triggers and other things to act on

use chrono::prelude::*;
use redis::cmd;
use tracing::{Level, event, instrument};
use uuid::Uuid;

use super::keys::EventKeys;
use crate::conf::Events;
use crate::models::{Event, EventCacheStatus, EventType};
use crate::utils::{ApiError, Shared};
use crate::{conn, deserialize, query, serialize};

pub mod shared;

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
#[instrument(name = "db::events::clear", skip_all, fields(clears = ids.len()), err(Debug))]
pub async fn clear(kind: EventType, ids: &[Uuid], shared: &Shared) -> Result<(), ApiError> {
    // build the key to the in flight map and the in flight queue
    let map_key = EventKeys::in_flight_map(kind, shared);
    let queue_key = EventKeys::in_flight_queue(kind, shared);
    // build a redis pipeline
    let mut pipe = redis::pipe();
    // clear each of these ids
    for id in ids {
        // remove this id from our in flight queue
        pipe.cmd("zrem").arg(&queue_key).arg(id.to_string());
        // remove it from our our in flight data map as well
        pipe.cmd("hdel").arg(&map_key).arg(id.to_string());
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
        EventType::ReactionTrigger | EventType::SigmaScannableResults => shared::CONSISTENCY_DELAY,
    }
}

/// Get some number of events to evaluate
///
/// # Arguments
///
/// * `kind` - The kind of events to pop
/// * `count` - The number of events to pop at most
/// * `shared` - Shared Thorium objects
#[rustfmt::skip]
#[instrument(name = "db::events::pop", skip(shared), err(Debug))]
pub async fn pop(kind: EventType, count: usize, shared: &Shared) -> Result<Vec<Event>, ApiError> {
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
    // get our cache status
    pipe.atomic().cmd("hget").arg(&key).arg("status");
    // if we also need to clear our cache reset flag then add that
    let triggers = if clear {
        // clear our status flag
        pipe.cmd("hset").arg(&key).arg("status").arg(false);
        // send our query and drop our set response
        let (status, _): (bool, bool) = pipe.query_async(conn!(shared)).await?;
        status
    } else {
        // send our query
        let (status,): (bool,) = pipe.query_async(conn!(shared)).await?;
        status
    };
    // build our event cache status object
    let cache_status = EventCacheStatus { triggers };
    Ok(cache_status)
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
        for kind in [EventType::ReactionTrigger, EventType::SigmaScannableResults] {
            assert_eq!(maturity_delay(kind, &defaults), 3);
            assert_eq!(maturity_delay(kind, &configured), 3);
        }
    }
}
