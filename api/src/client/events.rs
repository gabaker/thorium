//! Exposes events routes in Thorium

use chrono::prelude::*;

use crate::models::{
    Event, EventCacheStatus, EventCacheStatusOpts, EventDepths, EventIds, EventPeekOpts,
    EventPopOpts, EventStats, EventType, ReapSummary, ScanStatsSnapshot, WorkerStatsReport,
};
use crate::{Error, send, send_build};

// import our static runtime if we need a blocking client
#[cfg(feature = "sync")]
use super::RUNTIME;

#[cfg_attr(feature = "sync", thorium_derive::blocking_struct)]
#[derive(Clone)]
pub struct Events {
    host: String,
    /// token to use for auth
    token: String,
    client: reqwest::Client,
}

#[cfg(test)]
impl Events {
    /// The auth string this handler sends with its requests
    pub(crate) fn auth_token(&self) -> &str {
        &self.token
    }
}

#[cfg_attr(feature = "sync", thorium_derive::blocking_struct)]
impl Events {
    /// Creates a new events handler
    ///
    /// Instead of directly creating this handler you likely want to simply create a
    /// `thorium::Thorium` and use the handler within that instead.
    ///
    /// # Arguments
    ///
    /// * `host` - The url/ip of the Thorium api
    /// * `token` - The token used for authentication
    /// * `client` - The reqwest client to use
    ///
    /// # Examples
    ///
    /// ```
    /// use thorium::client::Events;
    ///
    /// let client = reqwest::Client::new();
    /// let events = Events::new("http://127.0.0.1", "token", &client);
    /// ```
    #[must_use]
    pub fn new(host: &str, token: &str, client: &reqwest::Client) -> Self {
        // build basic route handler
        Events {
            host: host.to_owned(),
            token: token.to_owned(),
            client: client.clone(),
        }
    }

    /// Pop some events to handle
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of events to pop
    /// * `opts` - The parameters to use when popping events
    #[cfg_attr(
        feature = "trace",
        tracing::instrument(name = "Thorium::events::pop", skip(self, opts), err(Debug))
    )]
    pub async fn pop(&self, kind: EventType, opts: &EventPopOpts) -> Result<Vec<Event>, Error> {
        // build the url for listing events
        let url = format!("{}/api/events/pop/{}/", self.host, kind);
        // build our query opts
        let mut query = vec![("limit", opts.limit.to_string())];
        // claim the popped events for a worker if one was set
        if let Some(worker) = &opts.worker {
            query.push(("worker", worker.clone()));
        }
        // build our request
        let req = self
            .client
            .patch(&url)
            .query(&query)
            .header("authorization", &self.token);
        // send this request
        send_build!(self.client, req, Vec<Event>)
    }

    /// Peek at the queued events of one kind without moving them in flight
    ///
    /// Only admins can peek at events; this is meant for tests.
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of events to peek at
    /// * `opts` - The parameters to use when peeking at events
    #[cfg_attr(
        feature = "trace",
        tracing::instrument(name = "Thorium::events::peek", skip(self, opts), err(Debug))
    )]
    pub async fn peek(&self, kind: EventType, opts: &EventPeekOpts) -> Result<Vec<Event>, Error> {
        // build the url for peeking at events
        let url = format!("{}/api/events/peek/{}/", self.host, kind);
        // build our query opts
        let query = vec![("limit", opts.limit)];
        // build our request
        let req = self
            .client
            .get(&url)
            .query(&query)
            .header("authorization", &self.token);
        // send this request
        send_build!(self.client, req, Vec<Event>)
    }

    /// Clear some events
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of events to clear
    /// * `ids` - The events ids to clear
    /// * `worker` - The worker whose in flight keys these events live in if one was set on pop
    #[cfg_attr(
        feature = "trace",
        tracing::instrument(name = "Thorium::events::clear", skip(self, ids), err(Debug))
    )]
    pub async fn clear(
        &self,
        kind: EventType,
        ids: &EventIds,
        worker: Option<&str>,
    ) -> Result<reqwest::Response, Error> {
        // build the url for listing events
        let url = format!("{}/api/events/clear/{}/", self.host, kind);
        // build our request
        let mut req = self
            .client
            .delete(&url)
            .json(ids)
            .header("authorization", &self.token);
        // scope this clear to a workers own in flight keys if one was set
        if let Some(worker) = worker {
            req = req.query(&[("worker", worker)]);
        }
        // send this request
        send!(self.client, req)
    }

    /// Reset all currently in flight events for a specific event kind
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of events to clear
    #[cfg_attr(
        feature = "trace",
        tracing::instrument(name = "Thorium::events::reset_all", skip(self), err(Debug))
    )]
    pub async fn reset_all(&self, kind: EventType) -> Result<reqwest::Response, Error> {
        // build the url for listing events
        let url = format!("{}/api/events/reset/{}/", self.host, kind);
        // build our request
        let req = self.client.patch(&url).header("authorization", &self.token);
        // send this request
        send!(self.client, req)
    }

    /// Reset a single workers in flight events back to the pending queue
    ///
    /// This uses the worker scoped reset route so an old API without worker
    /// aware in flight tracking returns a clean 404 instead of silently
    /// resetting the whole kinds legacy in flight queue out from under every
    /// peer replica.
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of events to reset
    /// * `worker` - The worker whose in flight events should be reset
    #[cfg_attr(
        feature = "trace",
        tracing::instrument(name = "Thorium::events::reset_worker", skip(self), err(Debug))
    )]
    pub async fn reset_worker(
        &self,
        kind: EventType,
        worker: &str,
    ) -> Result<reqwest::Response, Error> {
        // build the url for resetting a single workers events
        let url = format!(
            "{}/api/events/workers/reset/{}/{}/",
            self.host, worker, kind
        );
        // build our request
        let req = self.client.patch(&url).header("authorization", &self.token);
        // send this request
        send!(self.client, req)
    }

    /// Upsert this workers heartbeat in the event handler worker registry
    ///
    /// # Arguments
    ///
    /// * `worker` - The worker that is heartbeating
    #[cfg_attr(
        feature = "trace",
        tracing::instrument(name = "Thorium::events::heartbeat", skip(self), err(Debug))
    )]
    pub async fn heartbeat(&self, worker: &str) -> Result<reqwest::Response, Error> {
        // build the url for heartbeating this worker
        let url = format!("{}/api/events/workers/heartbeat/{}/", self.host, worker);
        // build our request
        let req = self.client.patch(&url).header("authorization", &self.token);
        // send this request
        send!(self.client, req)
    }

    /// Publish this workers cumulative counters for one kind of event
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of events these counters cover
    /// * `worker` - The worker that took this reading
    /// * `report` - The counter report to publish
    #[cfg_attr(
        feature = "trace",
        tracing::instrument(
            name = "Thorium::events::publish_worker_stats",
            skip(self, report),
            err(Debug)
        )
    )]
    pub async fn publish_worker_stats(
        &self,
        kind: EventType,
        worker: &str,
        report: &WorkerStatsReport,
    ) -> Result<reqwest::Response, Error> {
        // build the url for publishing this workers counters
        let url = format!(
            "{}/api/events/stats/workers/{}/{}/",
            self.host, worker, kind
        );
        // build our request
        let req = self
            .client
            .post(&url)
            .json(report)
            .header("authorization", &self.token);
        // send this request
        send!(self.client, req)
    }

    /// Drain all of a workers in flight events and remove it from the registry
    ///
    /// # Arguments
    ///
    /// * `worker` - The worker to drain and deregister
    #[cfg_attr(
        feature = "trace",
        tracing::instrument(name = "Thorium::events::drain_worker", skip(self), err(Debug))
    )]
    pub async fn drain_worker(&self, worker: &str) -> Result<u64, Error> {
        // build the url for draining this worker
        let url = format!("{}/api/events/workers/{}/", self.host, worker);
        // build our request
        let req = self
            .client
            .delete(&url)
            .header("authorization", &self.token);
        // send this request and get the number of events that were drained
        send_build!(self.client, req, u64)
    }

    /// Reap dead event handler workers and over-aged in flight events
    #[cfg_attr(
        feature = "trace",
        tracing::instrument(name = "Thorium::events::reap", skip(self), err(Debug))
    )]
    pub async fn reap(&self) -> Result<ReapSummary, Error> {
        // build the url for reaping dead workers
        let url = format!("{}/api/events/workers/reap/", self.host);
        // build our request
        let req = self.client.post(&url).header("authorization", &self.token);
        // send this request and build the reap summary from the response
        send_build!(self.client, req, ReapSummary)
    }

    /// Get the queue depths for every kind of event plus the live worker count
    #[cfg_attr(
        feature = "trace",
        tracing::instrument(name = "Thorium::events::depths", skip(self), err(Debug))
    )]
    pub async fn depths(&self) -> Result<EventDepths, Error> {
        // build the url for getting event queue depths
        let url = format!("{}/api/events/depths/", self.host);
        // build our request
        let req = self.client.get(&url).header("authorization", &self.token);
        // send this request and build the depths from the response
        send_build!(self.client, req, EventDepths)
    }

    /// Get the live scan stats aggregate
    #[cfg_attr(
        feature = "trace",
        tracing::instrument(name = "Thorium::events::stats", skip(self), err(Debug))
    )]
    pub async fn stats(&self) -> Result<EventStats, Error> {
        // build the url for getting the live scan stats aggregate
        let url = format!("{}/api/events/stats/", self.host);
        // build our request
        let req = self.client.get(&url).header("authorization", &self.token);
        // send this request and build the stats aggregate from the response
        send_build!(self.client, req, EventStats)
    }

    /// Get a ranged, downsampled slice of the scan stats history
    ///
    /// The range is half open (`start <= snapshot_at < end`) and rows come
    /// back oldest first, downsampled to the newest row per stride.
    ///
    /// # Arguments
    ///
    /// * `start` - The oldest instant to return rows from (inclusive)
    /// * `end` - The newest instant to return rows up to (exclusive)
    /// * `step_secs` - The stride to downsample rows to in seconds
    #[cfg_attr(
        feature = "trace",
        tracing::instrument(name = "Thorium::events::stats_history", skip(self), err(Debug))
    )]
    pub async fn stats_history(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        step_secs: Option<u64>,
    ) -> Result<Vec<ScanStatsSnapshot>, Error> {
        // build the url for reading the scan stats history
        let url = format!("{}/api/events/stats/history/", self.host);
        // build our query params with the required half open range
        let mut query = vec![("start", start.to_rfc3339()), ("end", end.to_rfc3339())];
        // add the stride if one was requested
        if let Some(step_secs) = step_secs {
            query.push(("step_secs", step_secs.to_string()));
        }
        // build our request
        let req = self
            .client
            .get(&url)
            .query(&query)
            .header("authorization", &self.token);
        // send this request and build the history from the response
        send_build!(self.client, req, Vec<ScanStatsSnapshot>)
    }

    /// Trigger a quantized scan stats snapshot
    ///
    /// `None` means a peers trigger already persisted this quantized window,
    /// which is a success for the window and not an error.
    #[cfg_attr(
        feature = "trace",
        tracing::instrument(name = "Thorium::events::snapshot_stats", skip(self), err(Debug))
    )]
    pub async fn snapshot_stats(&self) -> Result<Option<ScanStatsSnapshot>, Error> {
        // build the url for triggering a scan stats snapshot
        let url = format!("{}/api/events/stats/snapshot/", self.host);
        // build our request
        let req = self.client.post(&url).header("authorization", &self.token);
        // send this request ourselves so a 204 can map to a gated skip
        let resp = match self.client.execute(req.build()?).await {
            Ok(resp) => resp,
            Err(error) => return Err(Error::from(error)),
        };
        // a 204 means a peer already wrote this window
        if resp.status() == reqwest::StatusCode::NO_CONTENT {
            return Ok(None);
        }
        // any other error status is a real failure
        if !resp.status().is_success() {
            return Err(Error::from(resp));
        }
        // parse the snapshot this trigger persisted
        match resp.json::<ScanStatsSnapshot>().await {
            Ok(snapshot) => Ok(Some(snapshot)),
            Err(error) => Err(Error::from(error)),
        }
    }

    /// Get a cached serialized yara ruleset if one exists
    ///
    /// A missing cache entry is returned as `None` instead of an error so
    /// callers can treat it as a clean cache miss and fall back to compiling.
    ///
    /// # Arguments
    ///
    /// * `engine_version` - The yara engine version this ruleset was serialized with
    /// * `ruleset_hash` - The content hash of the rules this ruleset was compiled from
    #[cfg_attr(
        feature = "trace",
        tracing::instrument(name = "Thorium::events::get_yara_ruleset", skip(self), err(Debug))
    )]
    pub async fn get_yara_ruleset(
        &self,
        engine_version: &str,
        ruleset_hash: &str,
    ) -> Result<Option<Vec<u8>>, Error> {
        // build the url for getting this cached ruleset
        let url = format!(
            "{}/api/events/yara/ruleset/{}/{}/",
            self.host, engine_version, ruleset_hash
        );
        // build our request
        let req = self.client.get(&url).header("authorization", &self.token);
        // send this request ourselves so a 404 can map to a clean miss
        let resp = match self.client.execute(req.build()?).await {
            Ok(resp) => resp,
            Err(error) => return Err(Error::from(error)),
        };
        // a missing cache entry is a clean miss not an error
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        // any other error status is a real failure
        if !resp.status().is_success() {
            return Err(Error::from(resp));
        }
        // buffer the serialized ruleset
        match resp.bytes().await {
            Ok(bytes) => Ok(Some(bytes.to_vec())),
            Err(error) => Err(Error::from(error)),
        }
    }

    /// Upload a serialized yara ruleset to the cache
    ///
    /// # Arguments
    ///
    /// * `engine_version` - The yara engine version this ruleset was serialized with
    /// * `ruleset_hash` - The content hash of the rules this ruleset was compiled from
    /// * `data` - The serialized ruleset bytes to cache
    #[cfg_attr(
        feature = "trace",
        tracing::instrument(
            name = "Thorium::events::upload_yara_ruleset",
            skip(self, data),
            err(Debug)
        )
    )]
    pub async fn upload_yara_ruleset(
        &self,
        engine_version: &str,
        ruleset_hash: &str,
        data: Vec<u8>,
    ) -> Result<reqwest::Response, Error> {
        // build the url for uploading this cached ruleset
        let url = format!(
            "{}/api/events/yara/ruleset/{}/{}/",
            self.host, engine_version, ruleset_hash
        );
        // build our request
        let req = self
            .client
            .post(&url)
            .body(data)
            .header("authorization", &self.token);
        // send this request
        send!(self.client, req)
    }

    /// Get the event queue cache status
    ///
    /// # Arguments
    ///
    /// * `params` - The params for getting the status of the event cache
    #[cfg_attr(
        feature = "trace",
        tracing::instrument(name = "Thorium::events::get_cache_status", skip(self), err(Debug))
    )]
    pub async fn get_cache_status(
        &self,
        opts: &EventCacheStatusOpts,
    ) -> Result<EventCacheStatus, Error> {
        // build the url for getting our event watermark
        let url = format!("{}/api/events/cache/status/", self.host);
        // build our query params
        let query = vec![("reset", opts.reset.to_string())];
        // build our request
        let req = self
            .client
            .get(&url)
            .header("authorization", &self.token)
            .query(&query);
        // send this request and build our event watermark from the response
        send_build!(self.client, req, EventCacheStatus)
    }
}
