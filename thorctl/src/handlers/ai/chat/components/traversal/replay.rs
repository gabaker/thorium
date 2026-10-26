//! Replay-from-the-start of the traversal log at an adjustable speed
//!
//! The controller owns a replay [`GraphState`] folded up to a cursor and
//! advances it on render ticks. Two pacing modes: a fixed events-per-second
//! rate (the only option for timestamp-less transcripts from older builds),
//! and proportional pacing that scales the real gaps between stamped events,
//! chosen automatically when nearly every event carries a timestamp. Reaching
//! the end of a still-growing log keeps following it, which is "catch up to
//! live" for free.

use std::time::Instant;

use super::log::TraversalLog;
use super::state::GraphState;

/// The default fixed replay rate in events per second
const DEFAULT_RATE: f64 = 10.0;

/// The slowest and fastest fixed replay rates the speed keys clamp to
const RATE_RANGE: (f64, f64) = (1.0, 512.0);

/// The default proportional speed multiplier over real time
const DEFAULT_MULTIPLIER: f64 = 20.0;

/// The slowest and fastest proportional multipliers the speed keys clamp to
const MULTIPLIER_RANGE: (f64, f64) = (1.0, 1024.0);

/// The longest real gap proportional pacing honors, in milliseconds
///
/// A session can idle for minutes between turns; clamping keeps the replay
/// moving instead of faithfully reproducing dead air.
const MAX_GAP_MS: f64 = 2_000.0;

/// How many events must be stamped for proportional pacing to engage
const TS_COVERAGE_THRESHOLD: f64 = 0.9;

/// How replay paces itself between events
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pacing {
    /// A fixed number of events per second
    Fixed {
        /// The events-per-second rate
        rate: f64,
    },
    /// Real inter-event gaps scaled down by a speed multiplier
    Proportional {
        /// The speed multiplier over real time
        multiplier: f64,
    },
}

/// Drives replay-from-the-start of the traversal log
#[derive(Debug)]
pub struct ReplayController {
    /// The replay graph, folded up to (but excluding) the cursor
    pub state: GraphState,
    /// The index of the next event to apply
    pub cursor: usize,
    /// Whether the replay is advancing on ticks
    pub playing: bool,
    /// How the replay paces itself
    pub pacing: Pacing,
    /// When the replay last advanced
    last_tick: Instant,
    /// The fractional event budget carried between ticks
    carry: f64,
}

impl ReplayController {
    /// Creates a replay controller starting from the beginning of a log
    ///
    /// # Arguments
    ///
    /// * `log` - The traversal log the replay will walk
    pub fn new(log: &TraversalLog) -> Self {
        // proportional pacing only makes sense when nearly every event is
        // stamped; older transcripts fall back to the fixed rate
        let pacing = if log.ts_coverage() >= TS_COVERAGE_THRESHOLD {
            Pacing::Proportional {
                multiplier: DEFAULT_MULTIPLIER,
            }
        } else {
            Pacing::Fixed { rate: DEFAULT_RATE }
        };
        ReplayController {
            state: GraphState::default(),
            cursor: 0,
            playing: true,
            pacing,
            last_tick: Instant::now(),
            carry: 0.0,
        }
    }

    /// Restarts the replay from the first event, playing
    pub fn restart(&mut self) {
        // drop the folded state and rewind the cursor to the very start
        self.state = GraphState::default();
        self.cursor = 0;
        self.playing = true;
        self.carry = 0.0;
        self.last_tick = Instant::now();
    }

    /// Advances the replay by the wall time elapsed since the last tick
    ///
    /// Returns whether any event was applied, so the caller knows a redraw
    /// is worthwhile.
    ///
    /// # Arguments
    ///
    /// * `log` - The traversal log to fold events from
    /// * `now` - The current instant
    pub fn tick(&mut self, log: &TraversalLog, now: Instant) -> bool {
        // a paused replay consumes no time budget
        if !self.playing {
            self.last_tick = now;
            return false;
        }
        // measure the elapsed time and reset the tick marker
        let elapsed = now.duration_since(self.last_tick).as_secs_f64();
        self.last_tick = now;
        // work out how many events the elapsed time pays for
        let count = match self.pacing {
            // a fixed rate owes elapsed * rate events, with fractional carry
            Pacing::Fixed { rate } => {
                let owed = elapsed * rate + self.carry;
                let count = owed.floor();
                self.carry = owed - count;
                count as usize
            }
            // proportional pacing walks the stamped gaps, scaled and clamped
            Pacing::Proportional { multiplier } => {
                // the elapsed real time buys this much transcript time
                let mut budget = elapsed * multiplier * 1_000.0 + self.carry;
                let mut count = 0;
                // consume stamped gaps until the budget runs out
                while self.cursor + count < log.len() {
                    let gap = self.gap_ms(log, self.cursor + count);
                    if gap > budget {
                        break;
                    }
                    budget -= gap;
                    count += 1;
                }
                // bank the leftover budget only while events remain, so a
                // long idle at the end cannot buy a burst later
                self.carry = if self.cursor + count < log.len() {
                    budget
                } else {
                    0.0
                };
                count
            }
        };
        if count == 0 {
            return false;
        }
        // fold the paid-for events into the replay state
        let end = (self.cursor + count).min(log.len());
        log.fold_into(&mut self.state, self.cursor..end);
        let advanced = end > self.cursor;
        self.cursor = end;
        advanced
    }

    /// Steps the replay forward by a number of events while paused
    ///
    /// # Arguments
    ///
    /// * `log` - The traversal log to fold events from
    /// * `count` - How many events to apply
    pub fn step(&mut self, log: &TraversalLog, count: usize) {
        // fold the requested events directly, ignoring pacing
        let end = (self.cursor + count).min(log.len());
        log.fold_into(&mut self.state, self.cursor..end);
        self.cursor = end;
    }

    /// Doubles the replay speed, up to the clamp
    pub fn faster(&mut self) {
        match &mut self.pacing {
            Pacing::Fixed { rate } => *rate = (*rate * 2.0).min(RATE_RANGE.1),
            Pacing::Proportional { multiplier } => {
                *multiplier = (*multiplier * 2.0).min(MULTIPLIER_RANGE.1);
            }
        }
    }

    /// Halves the replay speed, down to the clamp
    pub fn slower(&mut self) {
        match &mut self.pacing {
            Pacing::Fixed { rate } => *rate = (*rate / 2.0).max(RATE_RANGE.0),
            Pacing::Proportional { multiplier } => {
                *multiplier = (*multiplier / 2.0).max(MULTIPLIER_RANGE.0);
            }
        }
    }

    /// A short human-readable speed label for the footer
    pub fn speed_label(&self) -> String {
        match self.pacing {
            Pacing::Fixed { rate } => format!("{rate:.0} ev/s"),
            Pacing::Proportional { multiplier } => format!("{multiplier:.0}x"),
        }
    }

    /// The clamped transcript-time gap before one event, in milliseconds
    ///
    /// # Arguments
    ///
    /// * `log` - The traversal log the gaps come from
    /// * `index` - The index of the event whose leading gap to measure
    fn gap_ms(&self, log: &TraversalLog, index: usize) -> f64 {
        // the very first event plays immediately
        if index == 0 {
            return 0.0;
        }
        // measure the stamped gap, treating missing stamps as no gap
        let (Some(prev), Some(next)) = (log.ts_at(index - 1), log.ts_at(index)) else {
            return 0.0;
        };
        // clamp long idles so dead air between turns stays short on replay
        (next.saturating_sub(prev) as f64).min(MAX_GAP_MS)
    }
}

#[cfg(test)]
mod tests {
    use super::super::state::TraversalEvent;
    use super::*;
    use std::time::Duration;

    /// Builds a log of stamped no-target tool calls, one per given timestamp
    fn stamped_log(stamps: &[u64]) -> TraversalLog {
        let log = TraversalLog::default();
        for (i, ts) in stamps.iter().enumerate() {
            log.push(TraversalEvent::ToolCall {
                agent: "main".to_owned(),
                call_id: format!("c{i}"),
                tool: "list_strings".to_owned(),
                args: "{}".to_owned(),
                ts: Some(*ts),
            });
        }
        log
    }

    /// Fixed pacing applies elapsed * rate events including fractional carry
    #[test]
    fn fixed_pacing_counts_events_with_carry() {
        let log = stamped_log(&[0; 20]);
        let mut replay = ReplayController::new(&log);
        // force the fixed mode at a known rate
        replay.pacing = Pacing::Fixed { rate: 10.0 };
        let start = Instant::now();
        replay.last_tick = start;
        // 150ms at 10 ev/s pays for 1 event with 0.5 carried
        assert!(replay.tick(&log, start + Duration::from_millis(150)));
        assert_eq!(replay.cursor, 1);
        // another 150ms pays the carried half plus one more: 2 events total
        assert!(replay.tick(&log, start + Duration::from_millis(300)));
        assert_eq!(replay.cursor, 3);
    }

    /// A fully stamped log selects proportional pacing and honors gaps
    #[test]
    fn proportional_pacing_walks_stamped_gaps() {
        // three events: a 100ms gap then a huge gap that gets clamped
        let log = stamped_log(&[0, 100, 60_100]);
        let mut replay = ReplayController::new(&log);
        assert!(matches!(replay.pacing, Pacing::Proportional { .. }));
        replay.pacing = Pacing::Proportional { multiplier: 10.0 };
        let start = Instant::now();
        replay.last_tick = start;
        // 20ms real time buys 200ms of transcript: the first two events
        assert!(replay.tick(&log, start + Duration::from_millis(20)));
        assert_eq!(replay.cursor, 2);
        // the 60s gap clamps to 2s, so 210ms real time (2.1s scaled) covers it
        assert!(replay.tick(&log, start + Duration::from_millis(230)));
        assert_eq!(replay.cursor, 3);
    }

    /// Restart rewinds to an empty state and run-to-end matches the snapshot
    #[test]
    fn restart_and_run_to_end_match_snapshot() {
        let log = stamped_log(&[0, 10, 20]);
        let mut replay = ReplayController::new(&log);
        // step through everything manually
        replay.step(&log, log.len());
        assert_eq!(replay.cursor, 3);
        assert_eq!(replay.state, log.snapshot());
        // restart rewinds cleanly
        replay.restart();
        assert_eq!(replay.cursor, 0);
        assert_eq!(replay.state, GraphState::default());
    }
}
