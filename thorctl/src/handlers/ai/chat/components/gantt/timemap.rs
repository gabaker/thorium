//! The piecewise map from real time onto the gap-compressed timeline axis
//!
//! A resumed session's downtime (a stop one day and a restart the next) would
//! otherwise dominate the window and crush every bar into the left margin, so
//! the axis collapses each idle gap longer than
//! [`GAP_THRESHOLD_MS`](super::state::GAP_THRESHOLD_MS) to a fixed, narrow
//! stride. Active time maps 1:1 — a run with no gaps renders exactly like an
//! uncompressed one, since the first segment maps onto itself — and the right
//! edge only tracks the wall clock while work is genuinely open, so a session
//! resumed just for review stays frozen in time. The map is rebuilt once per
//! frame from the state snapshot; compressed coordinates after a gap shift
//! when a *new* gap materializes mid-session, which only moves a manually
//! pinned window (auto-fit re-fits every frame).

use super::state::GanttState;

/// Each collapsed gap occupies this fraction of the total active time
///
/// A thirty-second of the active span is roughly two to three columns at
/// auto-fit on a typical bar area: visible as a break, never dominant.
const GAP_STRIDE_DIVISOR: u64 = 32;

/// One activity segment with its position on the compressed axis
#[derive(Debug, Clone, Copy, PartialEq)]
struct MapSegment {
    /// Milliseconds since the epoch when the segment starts
    start: u64,
    /// Milliseconds since the epoch when the segment ends
    end: u64,
    /// The segment start's position on the compressed axis
    cstart: u64,
}

impl MapSegment {
    /// The segment end's position on the compressed axis
    fn cend(&self) -> u64 {
        // active time maps 1:1, so the image is exactly as long as the segment
        self.cstart
            .saturating_add(self.end.saturating_sub(self.start))
    }
}

/// One collapsed idle gap on the compressed axis
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gap {
    /// The gap's left edge on the compressed axis
    pub c0: u64,
    /// The gap's right edge on the compressed axis
    pub c1: u64,
    /// The gap's real wall-clock length in milliseconds
    pub real_ms: u64,
}

/// The piecewise-linear map from epoch-ms onto the gap-compressed axis
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TimeMap {
    /// The activity segments with their compressed positions, in order
    segs: Vec<MapSegment>,
    /// The fixed compressed length every collapsed gap occupies
    stride: u64,
}

impl TimeMap {
    /// Builds the frame's map from the state's recorded activity
    ///
    /// In-flight calls count as activity up to now (a long call's quiet
    /// interior must not read as idle), and the last segment extends to now
    /// only while work is genuinely open, so a resumed session with nothing
    /// running keeps its historical right edge.
    ///
    /// # Arguments
    ///
    /// * `state` - The reduced timeline state to map
    /// * `now` - The current wall clock in milliseconds since the epoch
    pub fn build(state: &GanttState, now: Option<u64>) -> TimeMap {
        // fold any in-flight call intervals into a copy of the activity
        let mut activity = state.activity.clone();
        for call in &state.calls {
            if call.in_flight()
                && let (Some(start), Some(now)) = (call.start_ts, now)
            {
                activity.record(start, now.max(start));
            }
        }
        // the right edge only tracks the clock while work is genuinely open
        let extend_to = if state.has_open_work() { now } else { None };
        TimeMap::from_segments(activity.segments().to_vec(), extend_to)
    }

    /// Builds a map directly from pre-merged activity segments
    ///
    /// The segments must be ascending and disjoint -- the shape
    /// [`super::state::ActivityIntervals::segments`] produces. `extend_to`
    /// grows the last segment toward a clock, for runs with genuinely open
    /// work whose right edge should keep tracking now.
    ///
    /// # Arguments
    ///
    /// * `raw` - The merged `(start, end)` activity segments, in order
    /// * `extend_to` - A clock the last segment should grow toward, if any
    pub fn from_segments(mut raw: Vec<(u64, u64)>, extend_to: Option<u64>) -> TimeMap {
        // open work keeps the last segment growing toward the clock
        if let (Some(now), Some(last)) = (extend_to, raw.last_mut())
            && now > last.1
        {
            last.1 = now;
        }
        // each collapsed gap gets a fixed stride sized from the active total
        let total_active: u64 = raw.iter().map(|(start, end)| end - start).sum();
        let stride = (total_active / GAP_STRIDE_DIVISOR).max(1);
        // lay the segments out end to end, one stride apart; the first keeps
        // its real position so a gapless run maps onto itself
        let mut segs = Vec::with_capacity(raw.len());
        let mut cursor = raw.first().map(|(start, _)| *start).unwrap_or_default();
        for (start, end) in raw {
            segs.push(MapSegment {
                start,
                end,
                cstart: cursor,
            });
            cursor = cursor.saturating_add(end - start).saturating_add(stride);
        }
        TimeMap { segs, stride }
    }

    /// Whether the map holds no activity at all
    pub fn is_empty(&self) -> bool {
        self.segs.is_empty()
    }

    /// Maps a real timestamp onto the compressed axis (total and monotonic)
    ///
    /// Times inside a segment map linearly; times inside a gap land
    /// proportionally within its fixed stride; times outside the mapped run
    /// continue the identity off its nearest edge.
    ///
    /// # Arguments
    ///
    /// * `ts` - Milliseconds since the epoch to place on the axis
    pub fn compress(&self, ts: u64) -> u64 {
        // an empty map falls back to the identity
        let Some(first) = self.segs.first() else {
            return ts;
        };
        // times before the first segment keep their real distance to it
        if ts <= first.start {
            return first.cstart.saturating_sub(first.start - ts);
        }
        // find the last segment that starts at or before ts
        let index = self.segs.partition_point(|seg| seg.start <= ts) - 1;
        let seg = &self.segs[index];
        // inside the segment the map is linear
        if ts <= seg.end {
            return seg.cstart.saturating_add(ts - seg.start);
        }
        // past the last segment the identity continues off its end
        let Some(next) = self.segs.get(index + 1) else {
            return seg.cend().saturating_add(ts - seg.end);
        };
        // inside the gap: a proportional position within the fixed stride
        // (widened math so a years-long gap cannot overflow the product)
        let gap = next.start.saturating_sub(seg.end).max(1);
        let offset = u128::from(ts - seg.end) * u128::from(self.stride) / u128::from(gap);
        seg.cend().saturating_add(offset as u64)
    }

    /// The compressed axis position of the mapped run's right edge
    pub fn end(&self) -> Option<u64> {
        self.segs.last().map(MapSegment::cend)
    }

    /// The total active (worked) time across every segment, in milliseconds
    pub fn total_active(&self) -> u64 {
        self.segs.iter().map(|seg| seg.end - seg.start).sum()
    }

    /// The collapsed gaps between segments, in compressed-axis order
    pub fn gaps(&self) -> Vec<Gap> {
        self.segs
            .windows(2)
            .map(|pair| Gap {
                c0: pair[0].cend(),
                c1: pair[1].cstart,
                real_ms: pair[1].start - pair[0].end,
            })
            .collect()
    }

    /// The active time covered by a compressed-axis window, in milliseconds
    ///
    /// This is the axis tick labeler: ticks count cumulative worked time from
    /// the window's left edge, so the collapsed gaps never inflate them.
    ///
    /// # Arguments
    ///
    /// * `c0` - The window's left edge on the compressed axis
    /// * `c1` - The window's right edge on the compressed axis
    pub fn active_between(&self, c0: u64, c1: u64) -> u64 {
        // sum each segment image's overlap with the window
        self.segs
            .iter()
            .map(|seg| seg.cend().min(c1).saturating_sub(seg.cstart.max(c0)))
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::state::GanttEvent;

    /// Builds a tool call event for an agent at a stamp
    fn call(agent: &str, call_id: &str, ts: u64) -> GanttEvent {
        GanttEvent::ToolCall {
            agent: agent.to_owned(),
            call_id: call_id.to_owned(),
            tool: "decomp_function".to_owned(),
            args: "{}".to_owned(),
            ts: Some(ts),
        }
    }

    /// Builds a tool result event for an agent at a stamp
    fn result(agent: &str, call_id: &str, ts: u64) -> GanttEvent {
        GanttEvent::ToolResult {
            agent: agent.to_owned(),
            call_id: call_id.to_owned(),
            tool: "decomp_function".to_owned(),
            result: "ok".to_owned(),
            success: true,
            ts: Some(ts),
        }
    }

    /// Folds a closed two-call cluster into a state, 24 hours apart
    fn two_cluster_state() -> GanttState {
        let mut state = GanttState::default();
        // cluster one: a minute of work starting at an arbitrary epoch point
        state.apply(&call("main", "call-1", 1_000_000));
        state.apply(&result("main", "call-1", 1_060_000));
        // cluster two: another minute of work a day later
        state.apply(&call("main", "call-2", 1_000_000 + 86_400_000));
        state.apply(&result("main", "call-2", 1_060_000 + 86_400_000));
        state
    }

    /// A single-segment map is the identity over its whole run
    #[test]
    fn identity_when_single_segment() {
        let mut state = GanttState::default();
        state.apply(&call("main", "call-1", 5_000));
        state.apply(&result("main", "call-1", 65_000));
        let map = TimeMap::build(&state, Some(1_000_000));
        // closed work freezes the right edge at the last stamp, not now
        assert_eq!(map.end(), Some(65_000));
        assert_eq!(map.total_active(), 60_000);
        assert!(map.gaps().is_empty());
        // the map is the identity inside (and around) the one segment
        assert_eq!(map.compress(5_000), 5_000);
        assert_eq!(map.compress(42_000), 42_000);
        assert_eq!(map.compress(65_000), 65_000);
        assert_eq!(map.active_between(5_000, 42_000), 37_000);
    }

    /// Open work extends the last segment to the current wall clock
    #[test]
    fn open_work_tracks_now() {
        let mut state = GanttState::default();
        state.apply(&call("main", "call-1", 5_000));
        let map = TimeMap::build(&state, Some(90_000));
        // the in-flight call keeps the run growing toward now
        assert_eq!(map.end(), Some(90_000));
        assert!(map.gaps().is_empty());
    }

    /// A day-long gap collapses to one stride between the two clusters
    #[test]
    fn gap_compresses_to_stride() {
        let state = two_cluster_state();
        let map = TimeMap::build(&state, Some(1_060_001 + 86_400_000));
        // two minutes of active work and one collapsed day between them
        assert_eq!(map.total_active(), 120_000);
        let gaps = map.gaps();
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].real_ms, 86_400_000 - 60_000);
        // the compressed span is both segments plus one fixed stride
        let stride = (120_000_u64 / GAP_STRIDE_DIVISOR).max(1);
        assert_eq!(map.end(), Some(1_000_000 + 120_000 + stride));
        assert_eq!(gaps[0].c1 - gaps[0].c0, stride);
        // active time excludes the stride no matter where the window sits
        assert_eq!(
            map.active_between(1_000_000, map.end().expect("a mapped end")),
            120_000
        );
    }

    /// Building from raw segments matches the state-driven build exactly
    #[test]
    fn from_segments_matches_build() {
        let state = two_cluster_state();
        // closed work: neither path extends toward a clock
        let built = TimeMap::build(&state, Some(2_000_000 + 86_400_000));
        let direct = TimeMap::from_segments(state.activity.segments().to_vec(), None);
        assert_eq!(built, direct);
        // an explicit clock grows the last segment the same way open work does
        let extended = TimeMap::from_segments(
            state.activity.segments().to_vec(),
            Some(1_100_000 + 86_400_000),
        );
        assert_eq!(
            extended.end(),
            Some(extended.compress(1_100_000 + 86_400_000))
        );
        assert_eq!(extended.total_active(), 120_000 + (1_100_000 - 1_060_000));
    }

    /// The map is total and monotonic across every region
    #[test]
    fn compress_is_total_and_monotonic() {
        let state = two_cluster_state();
        let map = TimeMap::build(&state, None);
        // probe before, inside, between, and after the mapped segments
        let probes = [
            0,
            999_999,
            1_000_000,
            1_030_000,
            1_060_000,
            44_000_000,
            1_000_000 + 86_400_000,
            1_030_000 + 86_400_000,
            1_060_000 + 86_400_000,
            2_000_000 + 86_400_000,
        ];
        let mut previous = None;
        for ts in probes {
            let compressed = map.compress(ts);
            // each probe lands at or after the one before it
            if let Some(previous) = previous {
                assert!(compressed >= previous, "map went backward at {ts}");
            }
            previous = Some(compressed);
        }
        // the identity holds before the first segment and off the last end
        assert_eq!(map.compress(999_000), 999_000);
        let end = map.end().expect("a mapped end");
        assert_eq!(map.compress(1_060_000 + 86_400_000 + 500), end + 500);
    }
}
