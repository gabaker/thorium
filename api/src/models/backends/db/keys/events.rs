//! The keys related to events in Redis
use crate::{models::EventType, utils::Shared};

/// The keys related to events in Redis
#[derive(Debug)]
pub struct EventKeys {
    /// The key to our cache status
    #[allow(dead_code)]
    pub cache: String,
}

impl EventKeys {
    /// Build the keys for events
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    #[allow(dead_code)]
    pub fn new(shared: &Shared) -> Self {
        // build the key to our events cache
        let cache = Self::cache(shared);
        // build our keys
        EventKeys { cache }
    }

    /// Build the key for the event cache map
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    pub fn cache(shared: &Shared) -> String {
        format!(
            "{ns}:event-handler:cache",
            ns = shared.config.thorium.namespace
        )
    }

    /// The queue for a specific type of events
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of events to start or retrieve
    /// * `shared` - Shared Thorium objects
    pub fn queue(kind: EventType, shared: &Shared) -> String {
        format!(
            "{ns}:event-handler:queue:{kind}",
            ns = shared.config.thorium.namespace,
            kind = kind,
        )
    }

    /// The map of all events that are current in flight
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of events in this map
    /// * `shared` - Shared Thorium objects
    pub fn in_flight_map(kind: EventType, shared: &Shared) -> String {
        format!(
            "{ns}:event-handler:in_flight_map:{kind}",
            ns = shared.config.thorium.namespace,
            kind = kind,
        )
    }

    /// The queue of all events in the in flight queue
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of events in this queue
    /// * `shared` - Shared Thorium objects
    pub fn in_flight_queue(kind: EventType, shared: &Shared) -> String {
        format!(
            "{ns}:event-handler:in_flight_queue:{kind}",
            ns = shared.config.thorium.namespace,
            kind = kind,
        )
    }

    /// The registry of event handler workers scored by their last heartbeat
    ///
    /// # Arguments
    ///
    /// * `shared` - Shared Thorium objects
    pub fn workers(shared: &Shared) -> String {
        format!(
            "{ns}:event-handler:workers",
            ns = shared.config.thorium.namespace
        )
    }

    /// The map of in flight events owned by a single worker
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of events in this map
    /// * `worker` - The worker that owns these in flight events
    /// * `shared` - Shared Thorium objects
    pub fn in_flight_map_owned(kind: EventType, worker: &str, shared: &Shared) -> String {
        format!(
            "{ns}:event-handler:in_flight_map:{kind}:{worker}",
            ns = shared.config.thorium.namespace,
            kind = kind,
            worker = worker,
        )
    }

    /// The queue of in flight events owned by a single worker scored by lease time
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of events in this queue
    /// * `worker` - The worker that owns these in flight events
    /// * `shared` - Shared Thorium objects
    pub fn in_flight_queue_owned(kind: EventType, worker: &str, shared: &Shared) -> String {
        format!(
            "{ns}:event-handler:in_flight_queue:{kind}:{worker}",
            ns = shared.config.thorium.namespace,
            kind = kind,
            worker = worker,
        )
    }

    /// The TTL'd counter report last published by a single worker for one event kind
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of events these counters cover
    /// * `worker` - The worker that took the reading
    /// * `shared` - Shared Thorium objects
    pub fn stats(kind: EventType, worker: &str, shared: &Shared) -> String {
        format!(
            "{ns}:event-handler:stats:{kind}:{worker}",
            ns = shared.config.thorium.namespace,
            kind = kind,
            worker = worker,
        )
    }

    /// The gate marking one quantized snapshot window as already written
    ///
    /// The gate is keyed by the window it protects so a gate taken late in one
    /// window can never suppress the next windows only trigger. It is a cost
    /// optimization only: quantized row timestamps are the real cross replica
    /// dedup mechanism, so a lost gate just means doing the work twice.
    ///
    /// # Arguments
    ///
    /// * `window` - The quantized window this gate protects
    /// * `shared` - Shared Thorium objects
    pub fn snapshot_gate(window: i64, shared: &Shared) -> String {
        format!(
            "{ns}:event-handler:stats:snapshot-gate:{window}",
            ns = shared.config.thorium.namespace,
            window = window,
        )
    }
}
