//! The shared traversal log: an append-only event list plus the live graph
//!
//! Both the live event drain and the resume-time transcript replay push their
//! normalized [`TraversalEvent`]s here. The log keeps every event (the replay
//! source) and folds each one into a materialized live [`GraphState`] as it
//! arrives, so rendering the current state is a cheap snapshot rather than a
//! re-fold.

use std::collections::BTreeSet;
use std::ops::Range;
use std::sync::{Arc, Mutex};

use super::state::{GraphState, NodeId, TraversalEvent};

/// The guarded inner state of the traversal log
#[derive(Debug, Default)]
struct TraversalInner {
    /// Every traversal event so far, in arrival order (the replay source)
    events: Vec<TraversalEvent>,
    /// The materialized live graph, kept folded up to the last event
    live: GraphState,
    /// Enrichment edges learned from background Ghidra queries
    ///
    /// Static truth about the binary rather than traversal history, so they
    /// live outside the reduced state: the renderer overlays whichever of
    /// them touch nodes the current (live or replayed) state holds.
    enriched: BTreeSet<(NodeId, NodeId)>,
    /// The nodes already handed to the enrichment task, so each is queried once
    enrich_seen: BTreeSet<NodeId>,
}

/// The shared traversal log both worker tasks and the view hold
///
/// A `std::sync::Mutex` (not tokio's) guards the inner state: every critical
/// section is a short push-and-fold or a snapshot clone, and the render path
/// that reads it is synchronous.
#[derive(Debug, Clone, Default)]
pub struct TraversalLog {
    /// The guarded event list and materialized live graph
    inner: Arc<Mutex<TraversalInner>>,
}

impl TraversalLog {
    /// Appends an event and folds it into the live graph
    ///
    /// # Arguments
    ///
    /// * `event` - The traversal event to record
    pub fn push(&self, event: TraversalEvent) {
        // a poisoned lock only means a prior panic mid-fold; keep going with
        // the state as it stands rather than poisoning the whole TUI
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // fold the event into the live graph, then keep it for replay
        inner.live.apply(&event);
        inner.events.push(event);
    }

    /// The number of events recorded so far
    pub fn len(&self) -> usize {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.events.len()
    }

    /// Clones the live graph for rendering
    pub fn snapshot(&self) -> GraphState {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.live.clone()
    }

    /// Folds a range of recorded events into a caller-owned replay state
    ///
    /// The range is clamped to the events recorded so far, so a replay cursor
    /// pointing past the end of a still-growing log folds nothing rather than
    /// panicking.
    ///
    /// # Arguments
    ///
    /// * `state` - The replay state to fold the events into
    /// * `range` - The event index range to fold
    pub fn fold_into(&self, state: &mut GraphState, range: Range<usize>) {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // clamp both ends so a stale cursor cannot slice out of bounds
        let start = range.start.min(inner.events.len());
        let end = range.end.min(inner.events.len());
        // fold each event through the same reducer the live graph uses
        for event in &inner.events[start..end] {
            state.apply(event);
        }
    }

    /// The capture timestamp of one recorded event, if it carries one
    ///
    /// # Arguments
    ///
    /// * `index` - The index of the event to read
    pub fn ts_at(&self, index: usize) -> Option<u64> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.events.get(index).and_then(TraversalEvent::ts)
    }

    /// Records enrichment edges learned from a background Ghidra query
    ///
    /// # Arguments
    ///
    /// * `edges` - The `(caller, callee)` pairs to overlay on the graph
    pub fn add_enriched(&self, edges: impl IntoIterator<Item = (NodeId, NodeId)>) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // self-loops are annotated on nodes, never drawn as edges
        inner
            .enriched
            .extend(edges.into_iter().filter(|(from, to)| from != to));
    }

    /// Clones the enrichment edge overlay for rendering
    pub fn enriched(&self) -> BTreeSet<(NodeId, NodeId)> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.enriched.clone()
    }

    /// Drains the visited nodes not yet handed to the enrichment task
    ///
    /// Returns the visited (not merely discovered) node ids that have never
    /// been returned before, so the enrichment task queries each function once.
    pub fn drain_enrich_queue(&self) -> Vec<NodeId> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // collect the visited nodes the task has not seen yet
        let fresh: Vec<NodeId> = inner
            .live
            .nodes
            .values()
            .filter(|node| node.visits > 0 && !inner.enrich_seen.contains(&node.id))
            .map(|node| node.id.clone())
            .collect();
        // mark them seen so the next drain skips them
        inner.enrich_seen.extend(fresh.iter().cloned());
        fresh
    }

    /// The fraction of recorded events that carry a capture timestamp
    ///
    /// Replay uses this to decide whether proportional pacing is meaningful
    /// (an old, timestamp-less transcript replays at a fixed rate instead).
    pub fn ts_coverage(&self) -> f64 {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // an empty log has no coverage to speak of
        if inner.events.is_empty() {
            return 0.0;
        }
        // count the stamped events against the total
        let stamped = inner
            .events
            .iter()
            .filter(|event| event.ts().is_some())
            .count();
        stamped as f64 / inner.events.len() as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sized sequence of pushes folds live and replays identically
    #[test]
    fn replay_fold_matches_live_fold() {
        let log = TraversalLog::default();
        // record a short run of calls and results
        log.push(TraversalEvent::ToolCall {
            agent: "main".to_owned(),
            call_id: "c1".to_owned(),
            tool: "decomp_function".to_owned(),
            args: r#"{"target":"entry"}"#.to_owned(),
            ts: Some(10),
        });
        log.push(TraversalEvent::ToolResult {
            agent: "main".to_owned(),
            call_id: "c1".to_owned(),
            tool: "decomp_function".to_owned(),
            result: "ok".to_owned(),
            success: true,
            ts: Some(20),
        });
        assert_eq!(log.len(), 2);
        // folding the full range from scratch reproduces the live snapshot
        let mut replayed = GraphState::default();
        log.fold_into(&mut replayed, 0..log.len());
        assert_eq!(replayed, log.snapshot());
        // a stale cursor past the end folds nothing rather than panicking
        let mut unchanged = replayed.clone();
        log.fold_into(&mut unchanged, 10..20);
        assert_eq!(unchanged, replayed);
        // both events are stamped, so coverage is full
        assert!((log.ts_coverage() - 1.0).abs() < f64::EPSILON);
        assert_eq!(log.ts_at(0), Some(10));
    }
}
