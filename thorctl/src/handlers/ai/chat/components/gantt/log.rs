//! The shared gantt log: an append-only event list plus the live state
//!
//! Both the live event drain and the resume-time transcript replay push their
//! normalized [`GanttEvent`]s here. The log keeps every event and folds each
//! one into a materialized live [`GanttState`] as it arrives, so rendering the
//! current timeline is a cheap snapshot rather than a re-fold.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use super::state::{GanttEvent, GanttState};

/// The guarded inner state of the gantt log
#[derive(Debug, Default)]
struct GanttInner {
    /// Every gantt event so far, in arrival order
    events: Vec<GanttEvent>,
    /// The materialized live state, kept folded up to the last event
    live: GanttState,
}

/// The shared gantt log both worker tasks and the view hold
///
/// A `std::sync::Mutex` (not tokio's) guards the inner state: every critical
/// section is a short push-and-fold or a snapshot clone, and the render path
/// that reads it is synchronous.
#[derive(Debug, Clone, Default)]
pub struct GanttLog {
    /// The guarded event list and materialized live state
    inner: Arc<Mutex<GanttInner>>,
}

impl GanttLog {
    /// Appends an event and folds it into the live state
    ///
    /// # Arguments
    ///
    /// * `event` - The gantt event to record
    pub fn push(&self, event: GanttEvent) {
        // a poisoned lock only means a prior panic mid-fold; keep going with
        // the state as it stands rather than poisoning the whole TUI
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // fold the event into the live state, then keep it for inspection
        inner.live.apply(&event);
        inner.events.push(event);
    }

    /// The number of events recorded so far
    #[cfg(test)]
    pub fn len(&self) -> usize {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.events.len()
    }

    /// Clones the live state for rendering
    pub fn snapshot(&self) -> GanttState {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.live.clone()
    }

    /// Seeds a resumed session's per-agent token buckets from its snapshot
    ///
    /// The snapshot's usage ledger carries accumulated buckets but no capture
    /// times, so a seed fills each agent's tokens without opening any window.
    ///
    /// # Arguments
    ///
    /// * `usage` - The snapshot's usage ledger, keyed by agent display name
    pub fn seed_usage(&self, usage: &BTreeMap<String, thorpal::UsageTotals>) {
        // push one seed per agent so the fold stays the single reducer
        for (agent, totals) in usage {
            self.push(GanttEvent::SeedUsage {
                agent: agent.clone(),
                input: totals.input_tokens,
                output: totals.output_tokens,
                cached: totals.cached_input_tokens,
                cache_creation: totals.cache_creation_input_tokens,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pushed sequence folds live identically to a from-scratch re-fold
    #[test]
    fn live_fold_matches_refold() {
        let log = GanttLog::default();
        // record a short run of a call, its result, and a usage checkpoint
        log.push(GanttEvent::ToolCall {
            agent: "main".to_owned(),
            call_id: "call-1".to_owned(),
            tool: "decomp_function".to_owned(),
            args: "{\"name\": \"entry\"}".to_owned(),
            ts: Some(10),
        });
        log.push(GanttEvent::ToolResult {
            agent: "main".to_owned(),
            call_id: "call-1".to_owned(),
            tool: "decomp_function".to_owned(),
            result: "void entry(void) {}".to_owned(),
            success: true,
            ts: Some(20),
        });
        log.push(GanttEvent::Usage {
            agent: "main".to_owned(),
            input: 1_000,
            output: 50,
            cached: 0,
            cache_creation: 0,
            ts: Some(30),
        });
        assert_eq!(log.len(), 3);
        // folding the same events from scratch reproduces the live snapshot
        let mut refolded = GanttState::default();
        let inner = log.inner.lock().expect("the log lock is healthy");
        for event in &inner.events {
            refolded.apply(event);
        }
        drop(inner);
        assert_eq!(refolded, log.snapshot());
    }

    /// Seeding pushes one seed event per agent in the ledger
    #[test]
    fn seed_usage_feeds_every_agent() {
        let log = GanttLog::default();
        // a two-agent ledger from a resumed snapshot
        let mut usage = BTreeMap::new();
        usage.insert(
            "main".to_owned(),
            thorpal::UsageTotals {
                input_tokens: 1_000,
                output_tokens: 100,
                ..Default::default()
            },
        );
        usage.insert(
            "ReverseEngineer(Europa)".to_owned(),
            thorpal::UsageTotals {
                input_tokens: 500,
                output_tokens: 50,
                ..Default::default()
            },
        );
        log.seed_usage(&usage);
        // both agents got spans with seeded buckets and no opened windows
        let state = log.snapshot();
        assert_eq!(state.agents.len(), 2);
        let main = state.agents.get("main").expect("main span");
        assert_eq!(main.input, 1_000);
        assert!(main.seeded);
        assert_eq!(main.first_ts, None);
    }
}
