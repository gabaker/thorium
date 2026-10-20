use bb8_redis::redis::cmd;
use chrono::prelude::*;
use std::num::NonZeroU64;
use tracing::instrument;

use super::keys::streams::StreamKeys;
use crate::models::{StreamDepth, StreamObj};
use crate::utils::{ApiError, Shared};
use crate::{bad, conn, internal_err, not_found, query};

/// The most depth probes one depth request may ask for
pub const MAX_DEPTH_PROBES: i64 = 100_000;

/// Convert a stream timestamp in seconds to a date, refusing one no date can hold
///
/// # Arguments
///
/// * `timestamp` - The timestamp in seconds since the epoch
pub fn to_datetime(timestamp: i64) -> Result<DateTime<Utc>, ApiError> {
    match DateTime::from_timestamp(timestamp, 0) {
        Some(date) => Ok(date),
        None => bad!(format!("A timestamp of {timestamp} is out of range")),
    }
}

/// Build the inclusive ranges a depth request counts objects in
///
/// The ranges are `split` seconds long and cover `start` through `end`, with the last range
/// ending at `end`. A start or end that no date can hold, a span or split that doesn't fit in an
/// `i64`, or more than [`MAX_DEPTH_PROBES`] ranges, is refused before any range is built.
///
/// # Arguments
///
/// * `start` - The youngest point in the stream to use
/// * `end` - The oldest point in the stream to use
/// * `split` - The length of each depth probe in seconds
pub fn depth_probes(start: i64, end: i64, split: NonZeroU64) -> Result<Vec<(i64, i64)>, ApiError> {
    // refuse timestamps that no date can hold before counting anything
    to_datetime(start)?;
    to_datetime(end)?;
    // a split that doesn't fit in an i64 can't be added to a timestamp
    let Ok(split) = i64::try_from(split.get()) else {
        return bad!(format!("A split of {split} seconds is too large"));
    };
    // an end before the start leaves nothing to count
    if end < start {
        return Ok(Vec::new());
    }
    // get the length of our span without overflowing
    let Some(span) = end.checked_sub(start) else {
        return bad!(format!("The span from {start} to {end} is too large"));
    };
    // every range covers split seconds, so this is one less than the number of ranges
    let full_ranges = span / split;
    // refuse requests for too many ranges before building any of them
    if full_ranges >= MAX_DEPTH_PROBES {
        return bad!(format!(
            "cannot retrieve more than {MAX_DEPTH_PROBES} depths at once"
        ));
    }
    // build each range, ending the last one at our end
    let probes = (0..=full_ranges)
        .map(|index| {
            // this range starts at or before end, so it can't overflow
            let range_start = start + index * split;
            (range_start, range_start.saturating_add(split - 1).min(end))
        })
        .collect();
    Ok(probes)
}

/// Reads objects from a stream between a start and end timestamp
///
/// # Arguments
///
/// * `group` - The group this stream is in
/// * `namespace` - The namespace of this stream within this group
/// * `stream` - The name of the stream to read from
/// * `start_ts` - The youngest point in the stream to use
/// * `end_ts` - The oldest point in the stream to use
/// * `skip` - The number of entries in the stream to skip
/// * `limit` - The max number of objects to retrieve
/// * `shared` - The shared objects in Thorium
#[rustfmt::skip]
#[instrument(name = "db::streams::read", skip(shared), err(Debug))]
pub async fn read(
    group: &str,
    namespace: &str,
    stream: &str,
    start_ts: i64,
    end_ts: i64,
    skip: u64,
    limit: u64,
    shared: &Shared,
) -> Result<Vec<StreamObj>, ApiError> {
    // build stream keys
    let key = StreamKeys::stream(group, namespace, stream, shared);
    // read objects with their scores from the stream
    let raw: Vec<(String, i64)> = query!(
        cmd("zrangebyscore").arg(key).arg(start_ts).arg(end_ts)
            .arg("withscores").arg("LIMIT").arg(skip).arg(limit),
        shared).await?;
    let objs = raw.into_iter()
        .map(|raw| StreamObj::new(raw.1, raw.0))
        .collect();
    Ok(objs)
}

/// Reads objects from a stream between a start and end timestamp without scores for a prebuilt key
///
/// # Arguments
///
/// * `key` - The key in redis for this stream
/// * `start` - The youngest point in the stream to use
/// * `end` - The oldest point in the stream to use
/// * `limit` - The max number of objects to retrieve
/// * `shared` - The shared objects in Thorium
#[rustfmt::skip]
#[instrument(name = "db::streams::read_no_scores_by_key", skip(shared), err(Debug))]
pub async fn read_no_scores_by_key(
    key: &str,
    start: i64,
    end: i64,
    skip: u64,
    limit: u64,
    shared: &Shared,
) -> Result<Vec<String>, ApiError> {
    // read objects with their scores from the stream
    let raw: Vec<String> = query!(
        cmd("zrangebyscore").arg(key).arg(start).arg(end)
            .arg("LIMIT").arg(skip).arg(limit),
        shared).await?;
    Ok(raw)
}

/// Reads objects from a stream between a start and end timestamp without scores
///
/// # Arguments
///
/// * `group` - The group this stream is in
/// * `namespace` - The namespace of this stream within this group
/// * `stream` - The name of the stream to read from
/// * `start` - The youngest point in the stream to use
/// * `end` - The oldest point in the stream to use
/// * `limit` - The max number of objects to retrieve
/// * `shared` - The shared objects in Thorium
#[rustfmt::skip]
#[instrument(name = "db::streams::read_no_scores", skip(shared), err(Debug))]
pub async fn read_no_scores(
    group: &str,
    namespace: &str,
    stream: &str,
    start: i64,
    end: i64,
    limit: u64,
    shared: &Shared,
) -> Result<Vec<String>, ApiError> {
    // build stream keys
    let key = StreamKeys::stream(group, namespace, stream, shared);
    // read objects with their scores from the stream
    let raw: Vec<String> = query!(
        cmd("zrangebyscore").arg(key).arg(start).arg(end)
            .arg("LIMIT").arg(0).arg(limit),
        shared).await?;
    Ok(raw)
}

/// deletes a object from a stream
///
/// # Arguments
///
/// * `group` - The group this stream is in
/// * `namespace` - The namespace of this stream within this group
/// * `stream` - The name of the stream to delete an object from
/// * `start` - The stream to remove an object from
/// * `obj` - The object to remove
/// * `shared` - The shared objects in Thorium
pub async fn delete(
    group: &str,
    namespace: &str,
    stream: &str,
    obj: &str,
    shared: &Shared,
) -> Result<(), ApiError> {
    // build stream keys
    let key = StreamKeys::stream(group, namespace, stream, shared);
    // try to delete object from stream
    let deleted: bool = query!(cmd("zrem").arg(key).arg(obj), shared).await?;
    // error if object wasn't deleted
    if !deleted {
        return not_found!("Failed to remove object from stream".to_owned());
    }
    Ok(())
}

/// Count the number of objects in a stream between two points in time
///
/// # Arguments
///
/// * 'group - The group this stream is in
/// * `namespace` - The namespace of this stream within this group
/// * `stream` - The name of the stream to get the depth of
/// * `start` - The youngest point in the stream to use
/// * `end` - The oldest point in the stream to use
/// * `shared` - The shared objects in Thorium
#[instrument(name = "db::streams::depth", skip(shared), err(Debug))]
pub async fn depth(
    group: &str,
    namespace: &str,
    stream: &str,
    start: i64,
    end: i64,
    shared: &Shared,
) -> Result<StreamDepth, ApiError> {
    // refuse timestamps that no date can hold before counting anything
    to_datetime(start)?;
    to_datetime(end)?;
    // build stream keys
    let key = StreamKeys::stream(group, namespace, stream, shared);
    // count objects in a stream
    let depth = query!(cmd("zcount").arg(key).arg(start).arg(end), shared).await?;
    StreamDepth::new(depth, start, end)
}

/// Count the number of objects in a stream between two points in time
///
/// # Arguments
///
/// * 'group - The group this stream is in
/// * `namespace` - The namespace of this stream within this group
/// * `stream` - The name of the stream to get the depth of
/// * `start` - The youngest point in the stream to use
/// * `end` - The oldest point in the stream to use
/// * `split` - The length of each depth probe in seconds
/// * `shared` - The shared objects in Thorium
#[instrument(name = "db::streams::depth_range", skip(shared), err(Debug))]
pub async fn depth_range(
    group: &str,
    namespace: &str,
    stream: &str,
    start: i64,
    end: i64,
    split: NonZeroU64,
    shared: &Shared,
) -> Result<Vec<StreamDepth>, ApiError> {
    // build the inclusive range for each depth probe
    let depth_pairs = depth_probes(start, end, split)?;
    // build stream keys
    let key = StreamKeys::stream(group, namespace, stream, shared);
    // build zcount for each part of the stream
    let counts: Vec<i64> = depth_pairs
        .iter()
        .fold(redis::pipe().atomic(), |pipe, pair| {
            pipe.cmd("zcount").arg(&key).arg(pair.0).arg(pair.1)
        })
        .query_async(conn!(shared))
        .await?;
    // cast to vector of stream depths
    counts
        .into_iter()
        .enumerate()
        .map(|depth| StreamDepth::new(depth.1, depth_pairs[depth.0].0, depth_pairs[depth.0].1))
        .collect()
}

/// Get the earliest objects timestamp in a stream
///
/// # Arguments
///
/// * 'group - The group this stream is in
/// * `namespace` - The namespace of this stream within this group
/// * `stream` - The name of the stream to get the earliest timestamp from
/// * `shared` - The shared objects in Thorium
pub async fn earliest(
    group: &str,
    namespace: &str,
    stream: &str,
    shared: &Shared,
) -> Result<DateTime<Utc>, ApiError> {
    // build stream keys
    let key = StreamKeys::stream(group, namespace, stream, shared);
    // get earliest point in stream
    let raw: (String, i64) = query!(
        cmd("zrange").arg(key).arg(0).arg(0).arg("withscores"),
        shared
    )
    .await?;
    // convert to a timestamp
    match DateTime::from_timestamp(raw.1, 0) {
        Some(datetime) => Ok(datetime),
        None => {
            return internal_err!(format!(
                "Earliest point in stream does not have a valid timestamp - {}",
                raw.1
            ));
        }
    }
}

/// Get the latest objects timestamp in a stream
///
/// # Arguments
///
/// * 'group - The group this stream is in
/// * `namespace` - The namespace of this stream within this group
/// * `stream` - The name of the stream to get the latest timestamp from
/// * `shared` - The shared objects in Thorium
pub async fn latest(
    group: &str,
    namespace: &str,
    stream: &str,
    shared: &Shared,
) -> Result<DateTime<Utc>, ApiError> {
    // build stream keys
    let key = StreamKeys::stream(group, namespace, stream, shared);
    // get earliest point in stream
    let raw: (String, i64) = query!(
        cmd("zrange").arg(key).arg(-1).arg(-1).arg("withscores"),
        shared
    )
    .await?;
    // convert to a timestamp
    match DateTime::from_timestamp(raw.1, 0) {
        Some(datetime) => Ok(datetime),
        None => {
            return internal_err!(format!(
                "Earliest point in stream does not have a valid timestamp - {}",
                raw.1
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_DEPTH_PROBES, depth_probes, to_datetime};
    use chrono::prelude::*;
    use std::num::NonZeroU64;

    /// Build a non zero split for a test
    ///
    /// # Arguments
    ///
    /// * `split` - The split to build
    fn split(split: u64) -> NonZeroU64 {
        // tests only use splits above zero
        NonZeroU64::new(split).unwrap()
    }

    /// The ranges cover every second from start through end exactly once, in order, and match
    /// the ranges that stepping from `start - 1` by the split gives
    #[test]
    fn depth_probes_cover_the_span() {
        // check spans wider and narrower than their split, and spans crossing zero
        for (start, end, size) in [
            (0, 100, 1000),
            (1, 50, 20),
            (5, 5, 3),
            (-10, 10, 7),
            (0, 9, 10),
        ] {
            // build the ranges for this span
            let probes = depth_probes(start, end, split(size)).unwrap();
            // the ranges that stepping from start - 1 by the split gives
            let expected: Vec<(i64, i64)> = ((start - 1)..end)
                .step_by(usize::try_from(size).unwrap())
                .map(|probe| (probe + 1, (probe + size.cast_signed()).min(end)))
                .collect();
            assert_eq!(probes, expected, "probing {start}..={end} by {size}");
        }
    }

    /// An end before the start has nothing to count
    #[test]
    fn depth_probes_empty_when_reversed() {
        // an end five seconds before the start builds no ranges
        assert_eq!(depth_probes(10, 5, split(1)).unwrap(), Vec::new());
    }

    /// Spans and splits that don't fit in an i64, and too many ranges, are refused
    #[test]
    fn depth_probes_refuse_overflows() {
        // spans too wide for an i64 are refused
        assert!(
            depth_probes(
                -4_611_686_018_427_387_904,
                4_611_686_018_427_387_905,
                split(1000)
            )
            .is_err()
        );
        assert!(depth_probes(i64::MIN, i64::MAX, split(1)).is_err());
        // a split too large for an i64 is refused
        assert!(depth_probes(0, 100, split(u64::MAX)).is_err());
        // one range more than the most allowed is refused, and the most allowed is built
        assert!(depth_probes(0, MAX_DEPTH_PROBES, split(1)).is_err());
        assert_eq!(
            depth_probes(1, MAX_DEPTH_PROBES, split(1)).unwrap().len(),
            100_000
        );
    }

    /// A split as large as an i64 allows ends at the last range without overflowing
    #[test]
    fn depth_probes_largest_split() {
        // probe up to the latest timestamp a date can hold
        let last = DateTime::<Utc>::MAX_UTC.timestamp();
        let probes = depth_probes(last - 5, last, split(i64::MAX.cast_unsigned())).unwrap();
        assert_eq!(probes, vec![(last - 5, last)]);
    }

    /// Starts and ends that no date can hold are refused
    #[test]
    fn depth_probes_refuse_dates_out_of_range() {
        // get the earliest and latest timestamps a date can hold
        let first = DateTime::<Utc>::MIN_UTC.timestamp();
        let last = DateTime::<Utc>::MAX_UTC.timestamp();
        assert!(depth_probes(first, first + 10, split(1)).is_ok());
        assert!(depth_probes(last - 10, last, split(1)).is_ok());
        assert!(depth_probes(first - 1, first + 10, split(1)).is_err());
        assert!(depth_probes(last - 10, last + 1, split(100)).is_err());
        assert!(depth_probes(9_000_000_000_000, 9_000_000_000_010, split(1)).is_err());
        assert!(to_datetime(last + 1).is_err());
    }
}
