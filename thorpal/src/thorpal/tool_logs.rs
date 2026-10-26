//! Live log capture for long-running, streaming-capable tool calls.
//!
//! Some tools (today: everything backed by Ghidra) can run for minutes while
//! their only output is `tracing` log lines -- e.g. ghidra-cli re-emits every
//! analyzeHeadless stdout/stderr line as `tracing::info!("[Ghidra stdout] ...")`
//! from plain drain threads. Those threads carry no span context, so lines
//! cannot be correlated to a tool call through spans; instead a consumer
//! registers each running streaming call on a shared [`ToolLogRouter`] and a
//! [`ToolLogLayer`] installed in the consumer's tracing subscriber routes each
//! captured line, by its tracing target, to the oldest matching running call as
//! a [`ThorPalEvent::ToolLog`].
//!
//! A tool opts into streaming through the `logs = "..."` argument of its
//! `#[thorium_derive::thorpal_tool]` attribute, naming the tracing target
//! prefix its logs are emitted under; the route table here is built from the
//! tool registry those attributes populate. A future tool that streams a child
//! process's stdout gets there by logging each line via `tracing` under its
//! own target and declaring that target in its attribute.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use tracing::Subscriber;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::{Context, Layer};

use super::events::{ThorPalEvent, ThorPalEventSender};

/// The synthetic tool name the seed phase's initial Ghidra analysis runs under.
///
/// On a fresh session the slow Ghidra import/analysis happens during detector
/// seeding, outside any real tool call, so the seed phase emits a synthetic
/// tool call under this name for a consumer to stream logs onto.
pub const SEED_ANALYSIS_TOOL: &str = "initial_analysis";

/// The fabricated call id for the seed phase's synthetic tool call.
///
/// The seed runs at most once per session, so a constant id cannot collide
/// with rig's per-invocation `internal_call_id`s.
pub const SEED_ANALYSIS_CALL_ID: &str = "seed:initial_analysis";

/// Maps each streaming-capable tool name to the tracing target prefixes its
/// logs are emitted under.
///
/// Built from the tool registry's `logs = "..."` declarations (e.g. the Ghidra
/// tools drive ghidra-cli, whose bridge drain threads re-emit
/// analyzeHeadless/bridge output under `ghidra_cli::...` targets), plus the
/// synthetic seed-phase row no real tool call backs.
static ROUTE_TABLE: LazyLock<Vec<(&'static str, &'static [&'static str])>> = LazyLock::new(|| {
    // start with every registered tool that declared streaming log targets
    let mut table: Vec<(&'static str, &'static [&'static str])> =
        crate::thorpal::tools::registry::streaming_routes().collect();
    // the synthetic seed-phase scope wrapping the Ghidra bootstrap
    table.push((SEED_ANALYSIS_TOOL, &["ghidra_cli"]));
    table
});

/// One registered running streaming tool call.
struct RunningCall {
    /// The ThorPal `call_id` captured log lines route to.
    call_id: String,
    /// The target prefixes this call's tool sources its logs from.
    prefixes: &'static [&'static str],
}

/// A registry of currently-running streaming tool calls.
///
/// Shared between a consumer's event-drain task (which registers a call when
/// it sees [`ThorPalEvent::ToolCallStarted`] and unregisters it on
/// [`ThorPalEvent::ToolCallFinished`]) and the [`ToolLogLayer`] (which routes
/// each captured log line to a registered call by its tracing target).
pub struct ToolLogRouter {
    /// The running streaming calls in registration order: oldest first, so a
    /// shared log source (e.g. one Ghidra bootstrap awaited by several calls)
    /// attributes to the call that has been waiting longest.
    running: Mutex<Vec<RunningCall>>,
    /// A lock-free count of registered calls, the layer's idle fast path.
    active: AtomicUsize,
}

impl ToolLogRouter {
    /// Builds a new, empty router.
    pub fn new() -> Arc<Self> {
        // wrap the router in an Arc so the layer and the drain task share it
        Arc::new(ToolLogRouter {
            running: Mutex::new(Vec::new()),
            active: AtomicUsize::new(0),
        })
    }

    /// Register a running tool call for log routing.
    ///
    /// A no-op for a tool with no [`ROUTE_TABLE`] row, so a consumer can call
    /// this for every tool call without knowing which tools stream.
    ///
    /// # Arguments
    ///
    /// * `call_id` - The ThorPal call id log lines should route to
    /// * `tool` - The name of the tool being called
    pub fn register(&self, call_id: &str, tool: &str) {
        // look the tool up in the route table; most tools do not stream
        let Some((_, prefixes)) = ROUTE_TABLE.iter().find(|(name, _)| *name == tool) else {
            return;
        };
        // append the call in registration order so the oldest routes first
        let mut running = self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        running.push(RunningCall {
            call_id: call_id.to_owned(),
            prefixes,
        });
        // publish the new count for the layer's lock-free idle check
        self.active.store(running.len(), Ordering::Release);
    }

    /// Unregister a finished tool call.
    ///
    /// A no-op for an unknown `call_id`, so a consumer can call this for every
    /// finished tool call without tracking which ones registered.
    ///
    /// # Arguments
    ///
    /// * `call_id` - The ThorPal call id of the call that finished
    pub fn unregister(&self, call_id: &str) {
        // drop the matching registration, keeping the rest in order
        let mut running = self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        running.retain(|call| call.call_id != call_id);
        // publish the new count for the layer's lock-free idle check
        self.active.store(running.len(), Ordering::Release);
    }

    /// Resolve a tracing target to the oldest registered matching call id.
    ///
    /// Returns fast (one atomic load, no lock) when nothing is registered,
    /// which is the common case -- this runs on every captured log event,
    /// including from non-async drain threads.
    ///
    /// # Arguments
    ///
    /// * `target` - The tracing target the log event was emitted under
    pub fn route(&self, target: &str) -> Option<String> {
        // skip the lock entirely while no streaming call is running
        if self.active.load(Ordering::Acquire) == 0 {
            return None;
        }
        // find the oldest registered call whose prefixes match this target
        let running = self
            .running
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        running
            .iter()
            .find(|call| {
                call.prefixes
                    .iter()
                    .any(|prefix| target.starts_with(prefix))
            })
            .map(|call| call.call_id.clone())
    }

    /// The static target filter covering every route-table prefix at INFO.
    ///
    /// A consumer attaches this to its [`ToolLogLayer`] (via
    /// `Layer::with_filter`) so the layer never widens the subscriber's
    /// callsite interest beyond the streamed targets -- idle runs then pay no
    /// event-dispatch cost for unrelated log statements.
    pub fn targets_filter() -> Targets {
        // union every prefix in the route table at the level the sources log at
        let mut targets = Targets::new();
        for (_, prefixes) in ROUTE_TABLE.iter() {
            for prefix in *prefixes {
                targets = targets.with_target(*prefix, tracing::Level::INFO);
            }
        }
        targets
    }
}

/// A field visitor that captures only an event's `message` field as a line.
#[derive(Default)]
struct MessageVisitor {
    /// The formatted message field, once seen.
    message: Option<String>,
}

impl tracing::field::Visit for MessageVisitor {
    /// Capture the `message` field's debug rendering as the log line
    ///
    /// `tracing` delivers a macro's format string as a pre-formatted
    /// `fmt::Arguments` under the field name `message`, whose `Debug` impl
    /// prints the formatted text unquoted -- exactly the line we want.
    ///
    /// # Arguments
    ///
    /// * `field` - The field being recorded
    /// * `value` - The field's value, rendered through `Debug`
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        // only the message field becomes the line; other fields are skipped
        if field.name() == "message" {
            self.message = Some(format!("{value:?}"));
        }
    }

    /// Capture a string-typed `message` field directly, avoiding Debug quoting
    ///
    /// # Arguments
    ///
    /// * `field` - The field being recorded
    /// * `value` - The field's string value
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        // only the message field becomes the line; other fields are skipped
        if field.name() == "message" {
            self.message = Some(value.to_owned());
        }
    }
}

/// A tracing layer that mirrors matching log events onto a ThorPal event sink
/// as [`ThorPalEvent::ToolLog`] lines, routed by a shared [`ToolLogRouter`].
///
/// Installed by a consumer (e.g. thorctl's chat TUI) alongside its normal
/// logging layer; the ThorPal library itself never installs a subscriber. The
/// layer deliberately has no `LookupSpan` bound: correlation is by tracing
/// target, not spans, because the log sources (ghidra-cli's stdout/stderr
/// drain threads) run outside any span.
pub struct ToolLogLayer {
    /// The shared registry of running streaming tool calls.
    router: Arc<ToolLogRouter>,
    /// The sink routed lines are emitted on (the consumer's event channel).
    events: ThorPalEventSender,
}

impl ToolLogLayer {
    /// Builds a new capture layer over a shared router and event sink.
    ///
    /// # Arguments
    ///
    /// * `router` - The shared registry of running streaming tool calls
    /// * `events` - The event sink routed log lines are emitted on
    pub fn new(router: Arc<ToolLogRouter>, events: ThorPalEventSender) -> Self {
        ToolLogLayer { router, events }
    }
}

impl<S: Subscriber> Layer<S> for ToolLogLayer {
    /// Route a captured log event to its running tool call, if any
    ///
    /// # Arguments
    ///
    /// * `event` - The log event being dispatched
    /// * `_ctx` - The layer context (unused; no span lookups happen here)
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        // route by target first: a lock-free no-op when nothing is registered
        let Some(call_id) = self.router.route(event.metadata().target()) else {
            return;
        };
        // visit only the message field into a single owned line
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        // emit the line; a closed channel means the consumer quit, so ignore it
        if let Some(line) = visitor.message {
            let _ = self.events.send(ThorPalEvent::ToolLog { call_id, line });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A registered streaming call routes its targets and unregisters cleanly
    #[test]
    fn register_route_unregister_round_trip() {
        // build an empty router; nothing routes while idle
        let router = ToolLogRouter::new();
        assert_eq!(router.route("ghidra_cli::ghidra::bridge"), None);
        // register a streaming tool call and route a matching target to it
        router.register("call-1", "list_funcs");
        assert_eq!(
            router.route("ghidra_cli::ghidra::bridge"),
            Some("call-1".to_string())
        );
        // a non-matching target routes nowhere
        assert_eq!(router.route("hyper::client"), None);
        // unregistering returns the router to its idle state
        router.unregister("call-1");
        assert_eq!(router.route("ghidra_cli::ghidra::bridge"), None);
    }

    /// With two registered calls the oldest matching registrant wins
    #[test]
    fn route_prefers_oldest_registrant() {
        // register two concurrent ghidra calls in order
        let router = ToolLogRouter::new();
        router.register("older", "list_funcs");
        router.register("newer", "decomp_function");
        // the shared source attributes to the call registered first
        assert_eq!(
            router.route("ghidra_cli::ghidra::bridge"),
            Some("older".to_string())
        );
        // once the older call finishes, the newer one takes over
        router.unregister("older");
        assert_eq!(
            router.route("ghidra_cli::ghidra::bridge"),
            Some("newer".to_string())
        );
    }

    /// Registering a tool with no route-table row is a no-op
    #[test]
    fn non_streaming_tool_registration_is_noop() {
        // register a tool that does not stream logs
        let router = ToolLogRouter::new();
        router.register("call-1", "create_post");
        // nothing routes because nothing registered
        assert_eq!(router.route("ghidra_cli::ghidra::bridge"), None);
    }

    /// The synthetic seed scope routes ghidra targets like a real tool call
    #[test]
    fn seed_scope_routes_ghidra_targets() {
        // register the synthetic seed-phase analysis scope
        let router = ToolLogRouter::new();
        router.register(SEED_ANALYSIS_CALL_ID, SEED_ANALYSIS_TOOL);
        // ghidra bridge output routes onto the synthetic call id
        assert_eq!(
            router.route("ghidra_cli::ghidra::bridge"),
            Some(SEED_ANALYSIS_CALL_ID.to_string())
        );
    }

    /// The layer captures a routed event's formatted message end to end
    #[test]
    fn layer_captures_routed_message() {
        use tracing_subscriber::layer::SubscriberExt;
        // build a router with a registered ghidra call and a capture channel
        let router = ToolLogRouter::new();
        router.register("call-1", "list_funcs");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        // compose a registry holding just the capture layer
        let subscriber =
            tracing_subscriber::registry().with(ToolLogLayer::new(Arc::clone(&router), tx));
        // emit one routed and one unrouted event under that subscriber
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "ghidra_cli::ghidra::bridge", "[Ghidra stdout] {}", "hello");
            tracing::info!(target: "hyper::client", "not captured");
        });
        // the routed line arrives formatted, correlated to its call id
        match rx.try_recv() {
            Ok(ThorPalEvent::ToolLog { call_id, line }) => {
                assert_eq!(call_id, "call-1");
                assert_eq!(line, "[Ghidra stdout] hello");
            }
            other => panic!("expected a captured ToolLog, got {other:?}"),
        }
        // the unrouted event was never mirrored onto the channel
        assert!(rx.try_recv().is_err());
    }

    /// The targets filter covers the route table's prefixes at INFO
    #[test]
    fn targets_filter_covers_route_prefixes() {
        // the filter enables the ghidra prefix at INFO
        let targets = ToolLogRouter::targets_filter();
        assert!(targets.would_enable("ghidra_cli::ghidra::bridge", &tracing::Level::INFO));
        // unrelated targets stay disabled so idle runs pay no dispatch cost
        assert!(!targets.would_enable("hyper::client", &tracing::Level::INFO));
        // the filter never widens interest past INFO for routed targets
        assert!(!targets.would_enable("ghidra_cli::ghidra::bridge", &tracing::Level::DEBUG));
    }
}
