//! Combines and reports on stats across all workers in Thorium

use chrono::prelude::*;
use kanal::{AsyncReceiver, AsyncSender};
use std::sync::Arc;
use std::time::Duration;
use thorium::models::WorkerStatsReport;
use thorium::{Conf, Error, Thorium};
use tokio::task::JoinHandle;
use tracing::{Level, event, instrument};

use crate::libs::workers::EventWorkerKinds;

use super::workers::{
    EventWorkerSupportCore, ReactionTriggerWorker, SigmaRuleWorker, WorkflowsWorker,
    YaraHuntWorker, YaraScannerWorker,
};

/// The different channels for for each event worker
pub struct EventWorkerStatChannels {
    /// The channel for reaction trigger workers
    pub reaction_trigger: (
        AsyncSender<<ReactionTriggerWorker as EventWorkerSupportCore>::Stats>,
        AsyncReceiver<<ReactionTriggerWorker as EventWorkerSupportCore>::Stats>,
    ),
    /// The channel for sigma rule workers
    pub sigma_rules: (
        AsyncSender<<SigmaRuleWorker as EventWorkerSupportCore>::Stats>,
        AsyncReceiver<<SigmaRuleWorker as EventWorkerSupportCore>::Stats>,
    ),
    /// The channel for yara scanner workers
    pub yara_scans: (
        AsyncSender<<YaraScannerWorker as EventWorkerSupportCore>::Stats>,
        AsyncReceiver<<YaraScannerWorker as EventWorkerSupportCore>::Stats>,
    ),
    /// The channel for yara hunt workers
    pub yara_hunts: (
        AsyncSender<<YaraHuntWorker as EventWorkerSupportCore>::Stats>,
        AsyncReceiver<<YaraHuntWorker as EventWorkerSupportCore>::Stats>,
    ),
    /// The channel for the workflows worker
    pub workflows: (
        AsyncSender<<WorkflowsWorker as EventWorkerSupportCore>::Stats>,
        AsyncReceiver<<WorkflowsWorker as EventWorkerSupportCore>::Stats>,
    ),
}

impl EventWorkerStatChannels {
    /// Build and spawn a stats worker for this kind of worker
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of event worker to spawn a stats worker for
    /// * `thorium` - A Thorium client
    /// * `worker` - The identity of this event handler process
    /// * `conf` - The Thorium config
    pub fn spawn_worker(
        &self,
        kind: EventWorkerKinds,
        thorium: &Arc<Thorium>,
        worker: &str,
        conf: &Conf,
    ) -> JoinHandle<Result<(), Error>> {
        // buld the correct worker
        match kind {
            EventWorkerKinds::ReactionTriggers => {
                // build the correct worker
                let stats_worker = EventWorkerStats::<ReactionTriggerWorker>::new(
                    &self.reaction_trigger.1,
                    thorium,
                    worker,
                    conf,
                );
                // spawn this stats worker
                tokio::task::spawn(stats_worker.start())
            }
            EventWorkerKinds::SigmaScanner => {
                // build the correct worker
                let stats_worker = EventWorkerStats::<SigmaRuleWorker>::new(
                    &self.sigma_rules.1,
                    thorium,
                    worker,
                    conf,
                );
                // spawn this stats worker
                tokio::task::spawn(stats_worker.start())
            }
            EventWorkerKinds::YaraScanner => {
                // build the correct worker
                let stats_worker = EventWorkerStats::<YaraScannerWorker>::new(
                    &self.yara_scans.1,
                    thorium,
                    worker,
                    conf,
                );
                // spawn this stats worker
                tokio::task::spawn(stats_worker.start())
            }
            EventWorkerKinds::YaraHunter => {
                // build the correct worker
                let stats_worker = EventWorkerStats::<YaraHuntWorker>::new(
                    &self.yara_hunts.1,
                    thorium,
                    worker,
                    conf,
                );
                // spawn this stats worker
                tokio::task::spawn(stats_worker.start())
            }
            EventWorkerKinds::Workflows => {
                // build the workflows worker's stats reporter
                let stats_worker = EventWorkerStats::<WorkflowsWorker>::new(
                    &self.workflows.1,
                    thorium,
                    worker,
                    conf,
                );
                // spawn this stats worker
                tokio::task::spawn(stats_worker.start())
            }
        }
    }
}

impl Default for EventWorkerStatChannels {
    /// Build the stats channels for every kind of event worker
    fn default() -> Self {
        EventWorkerStatChannels {
            reaction_trigger: kanal::unbounded_async(),
            sigma_rules: kanal::unbounded_async(),
            yara_scans: kanal::unbounded_async(),
            yara_hunts: kanal::unbounded_async(),
            workflows: kanal::unbounded_async(),
        }
    }
}

/// Aggregates one worker kinds stats and reports them on a rate floored tick
pub struct EventWorkerStats<W: EventWorkerSupportCore> {
    /// The last reported stats
    last_reported: W::Stats,
    /// The current stats
    current: W::Stats,
    /// When stats can be reported again
    next_report: DateTime<Utc>,
    /// How many seconds sit between reports
    ///
    /// The servers counter keys expire on `events.worker_expiry_secs`, so the
    /// cadence must refresh them well inside that TTL or live workers would
    /// flicker out of the stats view between publishes; a third of the expiry
    /// leaves room for one slow or failed publish, floored at one second and
    /// capped at the old five second cadence.
    report_secs: i64,
    /// The channel to listen for stats on
    stats_rx: AsyncReceiver<W::Stats>,
    /// A client for Thorium
    thorium: Arc<Thorium>,
    /// The identity of this event handler process
    worker: String,
    /// Whether we already warned that the stats publish route is missing
    publish_missing: bool,
}

impl<W: EventWorkerSupportCore> EventWorkerStats<W> {
    /// Create a new event worker stats reporter
    ///
    /// # Arguments
    ///
    /// * `stats_rx` - The channel to receive stats over
    /// * `thorium` - A Thorium client
    /// * `worker` - The identity of this event handler process
    /// * `conf` - The Thorium config
    pub fn new(
        stats_rx: &AsyncReceiver<W::Stats>,
        thorium: &Arc<Thorium>,
        worker: &str,
        conf: &Conf,
    ) -> Self {
        // report at a third of the counter key TTL so keys never expire
        // between publishes, floored at 1s and capped at the 5s house cadence
        let report_secs =
            i64::try_from((conf.thorium.events.worker_expiry_secs / 3).clamp(1, 5)).unwrap_or(5);
        EventWorkerStats {
            last_reported: W::Stats::default(),
            current: W::Stats::default(),
            next_report: Utc::now() + chrono::Duration::seconds(report_secs),
            report_secs,
            stats_rx: stats_rx.clone(),
            thorium: thorium.clone(),
            worker: worker.to_owned(),
            publish_missing: false,
        }
    }

    /// Publish our cumulative counters to the API without ever being fatal
    ///
    /// The reading is published under every kind of event this worker handles
    /// so each kinds census sees the worker as live. A stats outage must never
    /// degrade scanning so failures warn and the next tick retries; 404s from
    /// an old API during a rolling upgrade are warned about once and then
    /// quieted (the `Heartbeat::warn_failure` behavior). The publish is also
    /// time boxed so a hung connection cannot freeze this reporter past its
    /// counter keys TTL.
    #[instrument(name = "EventWorkerStats::publish", skip_all)]
    async fn publish(&mut self) {
        // take a reading of our cumulative counters
        let report = WorkerStatsReport {
            counters: W::counter_map(&self.current),
            reported_at: Utc::now(),
        };
        // publish this reading under every kind of event this worker handles
        for kind in W::event_kinds() {
            // publish this reading under our process identity and this kind
            let publish = self
                .thorium
                .events
                .publish_worker_stats(kind, &self.worker, &report);
            // time box the publish so a hung connection cannot stall this reporter
            match tokio::time::timeout(Duration::from_secs(5), publish).await {
                // this reading was published
                Ok(Ok(_)) => (),
                // this publish failed so warn without ever being fatal
                Ok(Err(error)) => {
                    // a missing route on an old API is only warned about once
                    if error.status() == Some(reqwest::StatusCode::NOT_FOUND) {
                        // stay quiet if this missing route was already warned about
                        if self.publish_missing {
                            return;
                        }
                        // remember that this missing route has been warned about
                        self.publish_missing = true;
                    }
                    // warn about this failed publish
                    event!(
                        Level::WARN,
                        msg = "Worker stats publish failed",
                        kind = kind.as_str(),
                        error = error.to_string(),
                    );
                }
                // a publish that outlived its budget is skipped until the next tick
                Err(_) => event!(
                    Level::WARN,
                    msg = "Worker stats publish timed out",
                    kind = kind.as_str(),
                ),
            }
        }
    }

    /// Report our current stats
    ///
    /// The tracing event keeps its change gate while the counter publish is
    /// unconditional, so a counter keys TTL expiry means its worker is gone
    /// and not just idle. This will move the current stats into the last
    /// reported stats.
    #[instrument(name = "EventWorkerStats::report", skip_all)]
    async fn report(&mut self) {
        // only emit the tracing event when our stats changed since the last report
        if self.current != self.last_reported {
            // report this workers stats
            W::stats(&self.current, &self.last_reported);
        }
        // publish our cumulative counters even when they are unchanged
        self.publish().await;
        // update our last reported stats
        self.last_reported = self.current.clone();
        // don't report stats again until our TTL derived cadence allows it
        self.next_report = Utc::now() + chrono::Duration::seconds(self.report_secs);
    }

    /// Start tracking and reporting stats for this worker kind
    pub async fn start(mut self) -> Result<(), Error> {
        // always print out a zero stats message so we know things have started
        W::stats(&self.current, &self.last_reported);
        // publish our zero counters so this worker is visible before its first scan
        self.publish().await;
        // continue getting and reporting on stats forever
        loop {
            // get the next stats update
            if let Some(update) = self.stats_rx.try_recv()? {
                // update our current stats
                self.current += update;
            } else {
                // we didn't get any new stats updates so sleep for 2s
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            // report on every rate floored tick even when stats are unchanged
            // so our counter keys TTL expiry means this worker died, not idled
            if Utc::now() > self.next_report {
                self.report().await;
            }
        }
    }
}
