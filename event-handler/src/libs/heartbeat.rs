//! The heartbeat task for this event handler process
//!
//! A dedicated tokio task beats this workers registry entry every
//! `thorium.events.heartbeat_secs` so peers can tell it is alive, reaps dead
//! workers and over aged in flight events, logs the event queue depths as a
//! gauge, and triggers a scan stats snapshot on the first beat inside each
//! new quantized snapshot window. It lives outside
//! the controller loop because that loop can block on multi second yara
//! compiles. Every failure here is non fatal: the beat just warns and tries
//! again next tick, and 404s from an old API during a rolling upgrade are
//! warned about once and then quieted.

use chrono::Utc;
use std::sync::Arc;
use std::time::Duration;
use thorium::models::ReapSummary;
use thorium::{Conf, Error, Thorium};
use tracing::{Level, event, instrument};

/// The heartbeat task for a single event handler process
pub struct Heartbeat {
    /// A client for Thorium
    thorium: Arc<Thorium>,
    /// The identity of this event handler process
    worker: String,
    /// How long to wait between beats
    interval: Duration,
    /// The max time one maintenance step may run before the next beat wins
    step_timeout: Duration,
    /// The scan stats snapshot window length in seconds
    snapshot_secs: u64,
    /// The last quantized window a snapshot trigger succeeded in
    ///
    /// Tracking the wall clock window instead of counting beats means a
    /// trigger fires on the first beat inside every new window no matter how
    /// long the beats themselves take, so windows are never skipped; failed
    /// triggers leave this unchanged and retry on the very next beat.
    last_snapshot_window: Option<i64>,
    /// Whether we already warned that the heartbeat route is missing
    heartbeat_missing: bool,
    /// Whether we already warned that the reap route is missing
    reap_missing: bool,
    /// Whether we already warned that the depths route is missing
    depths_missing: bool,
    /// Whether we already warned that the snapshot route is missing
    snapshot_missing: bool,
}

impl Heartbeat {
    /// Create a new heartbeat task
    ///
    /// # Arguments
    ///
    /// * `thorium` - A Thorium client
    /// * `worker` - The identity of this event handler process
    /// * `conf` - The Thorium config
    pub fn new(thorium: &Arc<Thorium>, worker: &str, conf: &Conf) -> Self {
        // get the worker reclaim knobs from our config
        let events_conf = &conf.thorium.events;
        // the maintenance steps run in the same loop as the beat so one slow
        // reap (the client allows requests to run for minutes) could otherwise
        // stall the next beat past the worker expiry and get this live worker
        // falsely reaped; budget the four steps (heartbeat, reap, depths, and
        // the periodic snapshot trigger) so a full loop always fits inside the
        // expiry window with room for the sleep between beats
        let step_secs = events_conf
            .worker_expiry_secs
            .saturating_sub(events_conf.heartbeat_secs)
            / 4;
        // build our heartbeat task
        Heartbeat {
            thorium: thorium.clone(),
            worker: worker.to_owned(),
            interval: Duration::from_secs(events_conf.heartbeat_secs),
            step_timeout: Duration::from_secs(step_secs.max(1)),
            snapshot_secs: events_conf.stats_snapshot_secs,
            // no window has been triggered yet so a boot writes this windows row
            last_snapshot_window: None,
            heartbeat_missing: false,
            reap_missing: false,
            depths_missing: false,
            snapshot_missing: false,
        }
    }

    /// Quantize a timestamp down to the start of its snapshot window
    ///
    /// This mirrors the API's window quantization so a trigger fires on the
    /// first beat inside every new window. A beat-counting cadence cannot do
    /// that: any rounding of window/beat makes the real trigger period at
    /// least the window length (beats also take time), so windows get skipped
    /// and their rows are lost; duplicate triggers inside one window instead
    /// just lose the servers window gate race, which is harmless.
    ///
    /// # Arguments
    ///
    /// * `ts` - The timestamp in epoch seconds to quantize
    /// * `snapshot_secs` - The snapshot cadence in seconds
    fn snapshot_window(ts: i64, snapshot_secs: u64) -> i64 {
        // clamp the window length so degenerate configs can never divide by zero
        let step = i64::try_from(snapshot_secs.max(1)).unwrap_or(i64::MAX);
        // floor this timestamp to the start of its window
        ts - ts.rem_euclid(step)
    }

    /// Check whether an error is a 404 from an API without this route
    ///
    /// # Arguments
    ///
    /// * `error` - The error to inspect
    fn is_missing_route(error: &Error) -> bool {
        // an old API 404s on routes it does not know about
        error.status() == Some(reqwest::StatusCode::NOT_FOUND)
    }

    /// Warn about a failed beat step without ever being fatal
    ///
    /// 404s from an old API during a rolling upgrade are warned about once and
    /// then quieted; every other error warns on every tick.
    ///
    /// # Arguments
    ///
    /// * `step` - The beat step that failed
    /// * `error` - The error that step failed with
    /// * `warned_missing` - Whether this steps missing route was already warned about
    fn warn_failure(step: &str, error: &Error, warned_missing: &mut bool) {
        // a missing route on an old API is only warned about once
        if Self::is_missing_route(error) {
            // stay quiet if this missing route was already warned about
            if *warned_missing {
                return;
            }
            // remember that this missing route has been warned about
            *warned_missing = true;
        }
        // warn about this failed beat step
        event!(
            Level::WARN,
            msg = "Heartbeat step failed",
            step = step,
            error = error.to_string(),
        );
    }

    /// Perform a single beat: heartbeat, reap, log queue depths, then trigger
    /// a scan stats snapshot on every Nth beat
    #[instrument(name = "Heartbeat::beat", skip_all)]
    async fn beat(&mut self) {
        // upsert this workers heartbeat in the registry
        if let Err(error) = self.thorium.events.heartbeat(&self.worker).await {
            // a failed beat is retried next tick
            Self::warn_failure("heartbeat", &error, &mut self.heartbeat_missing);
        }
        // reap dead workers and over aged in flight events; the reap is time
        // boxed so one huge reclaim can never stall our next beat past the
        // worker expiry, and a timed out reap just keeps running server side
        match tokio::time::timeout(self.step_timeout, self.thorium.events.reap()).await {
            Ok(Ok(summary)) => {
                // only log reap passes that actually moved something
                if summary != ReapSummary::default() {
                    // log what this reap pass reclaimed
                    event!(
                        Level::INFO,
                        msg = "Reaped event handler workers",
                        dead_workers = summary.dead_workers,
                        reclaimed = summary.reclaimed,
                        backstopped = summary.backstopped,
                        legacy_reclaimed = summary.legacy_reclaimed,
                    );
                }
            }
            // a failed reap is retried next tick
            Ok(Err(error)) => Self::warn_failure("reap", &error, &mut self.reap_missing),
            // a reap that outlived its budget finishes server side
            Err(_) => event!(Level::WARN, msg = "Heartbeat step timed out", step = "reap"),
        }
        // log the event queue depths as a gauge for operators; depths are time
        // boxed for the same reason the reap is
        match tokio::time::timeout(self.step_timeout, self.thorium.events.depths()).await {
            Ok(Ok(depths)) => {
                // log each kinds depth as a gauge event
                for (kind, depth) in &depths.kinds {
                    // log this kinds queue depth
                    event!(
                        Level::INFO,
                        msg = "Event queue depth",
                        kind = kind.to_string(),
                        queued = depth.queued,
                        in_flight = depth.in_flight,
                        workers = depths.workers,
                    );
                }
            }
            // failed depth gauges are retried next tick
            Ok(Err(error)) => Self::warn_failure("depths", &error, &mut self.depths_missing),
            // a depth gauge that outlived its budget is just skipped this tick
            Err(_) => event!(
                Level::WARN,
                msg = "Heartbeat step timed out",
                step = "depths"
            ),
        }
        // trigger a scan stats snapshot on the first beat inside each new
        // quantized window; the API gates each window so racing replica
        // triggers collapse to at most one row per window
        let window = Self::snapshot_window(Utc::now().timestamp(), self.snapshot_secs);
        if self.last_snapshot_window != Some(window) {
            // the snapshot is time boxed for the same reason the reap is
            match tokio::time::timeout(self.step_timeout, self.thorium.events.snapshot_stats())
                .await
            {
                // winning the window and losing the gate to a peer are both
                // fine; either way this window is written so stand down until
                // the next one opens
                Ok(Ok(_)) => self.last_snapshot_window = Some(window),
                // a failed snapshot is retried on the very next beat
                Ok(Err(error)) => {
                    Self::warn_failure("snapshot", &error, &mut self.snapshot_missing);
                }
                // a snapshot that outlived its budget is retried on the next
                // beat; if it did land server side the window gate makes the
                // retry a cheap no-op
                Err(_) => event!(
                    Level::WARN,
                    msg = "Heartbeat step timed out",
                    step = "snapshot"
                ),
            }
        }
    }

    /// Start beating until this task is aborted
    ///
    /// The first beat (and so the first reap and snapshot trigger) fires
    /// immediately at startup.
    pub async fn start(mut self) {
        // beat forever; the controller aborts this task on shutdown
        loop {
            // perform a single beat
            self.beat().await;
            // sleep until the next beat is due
            tokio::time::sleep(self.interval).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Window quantization must floor to window starts so the first beat in
    /// every new window triggers exactly once no matter how long beats take
    #[test]
    fn snapshot_window_floors_to_window_start() {
        // mid window timestamps floor to their windows start
        assert_eq!(Heartbeat::snapshot_window(1_000_031, 30), 1_000_020);
        // window boundaries are already quantized
        assert_eq!(Heartbeat::snapshot_window(1_000_020, 30), 1_000_020);
        // consecutive beats inside one window share its quantized start
        assert_eq!(
            Heartbeat::snapshot_window(95, 30),
            Heartbeat::snapshot_window(119, 30)
        );
        // the first beat past a boundary lands in a new window even when the
        // beats themselves drifted (the old beat-counting cadence missed this)
        assert_ne!(
            Heartbeat::snapshot_window(119, 30),
            Heartbeat::snapshot_window(121, 30)
        );
        // a zero cadence clamps to one second windows instead of dividing by zero
        assert_eq!(Heartbeat::snapshot_window(1_000_031, 0), 1_000_031);
    }
}
