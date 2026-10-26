//! The shared agent stats log: an append-only event list plus the live state
//!
//! Both the live event drain and the resume-time transcript replay push their
//! normalized [`StatsEvent`]s here. The log keeps every event and folds each
//! one into a materialized live [`StatsState`] as it arrives, so rendering the
//! current tallies is a cheap snapshot rather than a re-fold.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use super::state::{StatsEvent, StatsState};

/// The guarded inner state of the agent stats log
#[derive(Debug, Default)]
struct StatsInner {
    /// Every stats event so far, in arrival order
    events: Vec<StatsEvent>,
    /// The materialized live state, kept folded up to the last event
    live: StatsState,
}

/// The shared agent stats log both worker tasks and the view hold
///
/// A `std::sync::Mutex` (not tokio's) guards the inner state: every critical
/// section is a short push-and-fold or a snapshot clone, and the render path
/// that reads it is synchronous.
#[derive(Debug, Clone, Default)]
pub struct AgentStatsLog {
    /// The guarded event list and materialized live state
    inner: Arc<Mutex<StatsInner>>,
}

impl AgentStatsLog {
    /// Appends an event and folds it into the live state
    ///
    /// # Arguments
    ///
    /// * `event` - The stats event to record
    pub fn push(&self, event: StatsEvent) {
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
    pub fn snapshot(&self) -> StatsState {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.live.clone()
    }

    /// Seeds a resumed session's per-agent token buckets from its snapshot
    ///
    /// The snapshot's usage ledger carries accumulated buckets but no turn
    /// counts, so each agent is seeded rather than given invented turns.
    ///
    /// # Arguments
    ///
    /// * `usage` - The snapshot's usage ledger, keyed by agent display name
    pub fn seed_usage(&self, usage: &BTreeMap<String, thorpal::UsageTotals>) {
        // push one seed per agent so the fold stays the single reducer
        for (agent, totals) in usage {
            self.push(StatsEvent::SeedUsage {
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
        let log = AgentStatsLog::default();
        // record a short run of calls and a usage checkpoint
        log.push(StatsEvent::ToolCall {
            agent: "main".to_owned(),
            tool: "decomp_function".to_owned(),
            args_est: 4,
            ts: Some(10),
        });
        log.push(StatsEvent::ToolResult {
            agent: "main".to_owned(),
            tool: "decomp_function".to_owned(),
            success: true,
            result_est: 100,
            ts: Some(20),
        });
        log.push(StatsEvent::Usage {
            agent: "main".to_owned(),
            input: 1_000,
            output: 50,
            cached: 0,
            cache_creation: 0,
            ts: Some(30),
        });
        assert_eq!(log.len(), 3);
        // folding the same events from scratch reproduces the live snapshot
        let mut refolded = StatsState::default();
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
        let log = AgentStatsLog::default();
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
        // both agents got rows with seeded buckets and no invented turns
        let state = log.snapshot();
        assert_eq!(state.agents.len(), 2);
        let main = state.agents.get("main").expect("main row");
        assert_eq!(main.input, 1_000);
        assert!(main.seeded);
        assert_eq!(main.turns, 0);
    }
}
