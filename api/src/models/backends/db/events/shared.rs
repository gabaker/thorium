//! Logic shared between multiple types of events

use chrono::{DateTime, Utc};
use tracing::{Level, event, instrument};

use crate::utils::{ApiError, Shared};
use crate::{conn, deserialize};

/// How many seconds events wait before they are mature, giving the database a chance to reach
/// consistency
pub const CONSISTENCY_DELAY: u64 = 3;

/// Classify popped events into mature (deserialized) events and immature resets
///
/// Events are popped oldest first and keep that order, since compacting events for the same
/// item relies on later events superseding earlier ones. Events are scored by the whole second
/// they were queued in, so an event is mature once its second is more than `delay` seconds
/// before `now`'s, which makes it at least `delay` seconds old. A delay of 0 makes every event
/// mature.
///
/// # Arguments
///
/// * `now` - A timestamp from before we pulled events
/// * `delay` - How many seconds events must wait before they are mature
/// * `serialized` - The serialized events to check for maturity
#[allow(
    clippy::type_complexity,
    reason = "the result tuple's three parts are named where it is destructured; an alias would hide them"
)]
fn classify_immature<D: for<'de> serde::de::Deserialize<'de>>(
    now: DateTime<Utc>,
    delay: u64,
    serialized: Vec<(String, f64)>,
) -> Result<(Vec<D>, Vec<(String, f64)>, Vec<(String, f64)>), ApiError> {
    // convert our datetime to a timestamp the delay in the past, or past every event without one
    #[allow(clippy::cast_possible_wrap, clippy::cast_precision_loss)]
    let now_ts = if delay == 0 {
        f64::INFINITY
    } else {
        now.timestamp().saturating_sub(delay as i64) as f64
    };
    // keep track of the events to reset and our deserialized events
    let mut resets = Vec::default();
    let mut events = Vec::with_capacity(serialized.len());
    let mut filtered_serial = Vec::with_capacity(serialized.len());
    // find first mature event that we retrieved
    for (serial, timestamp) in serialized {
        // check if this timestamp is mature or not yet
        if timestamp < now_ts {
            // try to deserialize this mature event
            let event: D = deserialize!(&serial);
            // add our deserialized event
            events.push(event);
            // add our still serialized but filtered info
            filtered_serial.push((serial, timestamp));
        } else {
            // this event is not yet mature so add it to the reste list
            resets.push((serial, timestamp));
        }
    }
    Ok((events, filtered_serial, resets))
}

/// Filter and reset any events that are younger than their queue's delay
///
/// Short delays give the DB a chance to reach consistency, and longer ones let a burst of
/// changes be handled together. Immature events are put back with their original scores.
///
/// # Arguments
///
/// * `key` - The sorted set we are filtering immature events from
/// * `now` - A timestamp from before we pulled events
/// * `delay` - How many seconds events must wait before they are mature
/// * `serialized` - The serialized events to check for maturity
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::events::shared::filter_immature", skip_all, fields(pre_filter = serialized.len()), err(Debug))]
#[allow(
    clippy::type_complexity,
    reason = "public thorium API; an alias for the result tuple adds a new public item"
)]
pub async fn filter_immature<D: for<'de> serde::de::Deserialize<'de>>(
    key: &String,
    now: DateTime<Utc>,
    delay: u64,
    serialized: Vec<(String, f64)>,
    shared: &Shared,
) -> Result<(Vec<D>, Vec<(String, f64)>), ApiError> {
    // classify our popped events into mature events and immature resets
    let (events, filtered_serial, resets) = classify_immature(now, delay, serialized)?;
    if !resets.is_empty() {
        // log the number of immature events that we found
        event!(Level::INFO, immature = resets.len());
        // try to reset these immature events
        reset_pops(key, resets, shared).await?;
    }
    Ok((events, filtered_serial))
}

/// Try to reset popped events in the case of an error or other failure
///
/// If this reset fails then these events will simply be lost.
///
/// # Arguments
///
/// * `key` - The sorted set to restore these popped events too
/// * `pops` - The popped events to restore
/// * `shared` - Shared Thorium objects
#[instrument(name = "db::events::shared::reset_pops", skip_all, fields(pops = pops.len()), err(Debug))]
pub async fn reset_pops(
    key: &String,
    pops: Vec<(String, f64)>,
    shared: &Shared,
) -> Result<(), ApiError> {
    // build a pipe to reset any immature events
    let mut pipe = redis::pipe();
    // add all of our immature event reset commands to this pipeline
    for (serial, timestamp) in pops {
        pipe.cmd("zadd").arg(key).arg(timestamp).arg(serial);
    }
    // execute this redis pipeline
    let _: () = pipe.query_async(conn!(shared)).await?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Build popped events of some ages, oldest first, whose payloads are their ages
    ///
    /// # Arguments
    ///
    /// * `now` - The time the events are popped at
    /// * `ages` - The age in seconds of each event, oldest first
    #[allow(clippy::cast_precision_loss)]
    fn aged(now: DateTime<Utc>, ages: &[i64]) -> Vec<(String, f64)> {
        // score each event by the second it was queued in
        ages.iter()
            .map(|age| (age.to_string(), (now.timestamp() - age) as f64))
            .collect()
    }

    /// A delay splits popped events at exactly that age: an event is mature once it's older
    /// than the delay, and a delay of 0 makes every event mature
    #[test]
    fn delay_splits_at_its_age() {
        // events from 12 seconds old down to queued this second
        let now = DateTime::from_timestamp(1_000_000, 0).unwrap();
        let ages = [12, 11, 10, 9, 4, 3, 2, 0];
        // each delay keeps exactly the events older than it, oldest first
        for (delay, mature) in [
            (0, vec![12, 11, 10, 9, 4, 3, 2, 0]),
            (3, vec![12, 11, 10, 9, 4]),
            (10, vec![12, 11]),
        ] {
            let (events, filtered, resets): (Vec<i64>, _, _) =
                classify_immature(now, delay, aged(now, &ages)).unwrap();
            assert_eq!(events, mature, "mature events with a {delay}s delay");
            // the serialized forms of the mature events are kept with their scores
            assert_eq!(filtered.len(), mature.len());
            // the rest are put back with their original scores, oldest first
            let expected = aged(now, &ages[mature.len()..]);
            assert_eq!(resets, expected, "immature events with a {delay}s delay");
        }
    }

    /// `filter_immature` returns mature events in the oldest-first order `ZPOPMIN` produced.
    /// Downstream compaction (`CompactEvent::append`) assumes events arrive oldest-first ("the
    /// delete event will come last"), so preserving that order keeps compaction from keeping the
    /// OLDEST event's type, losing deletes and leaving ghost documents in Elasticsearch.
    #[test]
    fn mature_events_preserve_oldest_first_order() {
        // pin "now" far enough ahead that every event below is mature (timestamp < now - 3)
        let now = DateTime::from_timestamp(1_000_000, 0).unwrap();
        // ZPOPMIN returns members ascending by score, so the popped list is oldest-first: the
        // payloads (100, 200, 300) double as their scores so the intended order is unambiguous
        let serialized = vec![
            ("100".to_owned(), 100.0),
            ("200".to_owned(), 200.0),
            ("300".to_owned(), 300.0),
        ];
        // classify the popped events
        let (events, _filtered, resets): (Vec<u64>, _, _) =
            classify_immature(now, 3, serialized).unwrap();
        // all three events are mature so none should be reset
        assert!(resets.is_empty(), "no mature event should be reset");
        // the mature events must come back oldest-first, matching ZPOPMIN's ascending order
        assert_eq!(
            events,
            vec![100, 200, 300],
            "mature events must preserve oldest-first order, not be reversed to newest-first"
        );
    }
}
