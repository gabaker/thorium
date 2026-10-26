//! Shared, live run-activity state for the chat TUI.
//!
//! The worker tasks that drive a chat (the ThorPal prompt/drain tasks and the
//! MCP `ask` loop) run in the background, while the render loop paints the frame
//! on the main task. This handle is the small piece of state they share so the
//! render loop can show a "thinking" throbber with the current agent/goal and a
//! live token-usage readout: the workers write it as events arrive, and
//! [`Chat::render`](crate::handlers::ai::chat::chat::Chat) snapshots it each frame.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};

/// The live state of one in-flight LLM retry episode, for the throbber line.
///
/// Only timestamps are stored -- the render loop computes "last try Xs ago"
/// and the countdown from them each frame, so the line ticks in place on the
/// app's periodic redraws without any further events arriving.
#[derive(Debug, Clone)]
pub struct RetrySnapshot {
    /// The display name of the agent whose request is retrying (e.g. `"main"`).
    pub agent: String,
    /// The attempt that most recently failed (1-based).
    pub attempt: u32,
    /// The attempt budget before the error surfaces as fatal.
    pub max_attempts: u32,
    /// A short summary of the fault (e.g. `"429 Too Many Requests"`).
    pub error: String,
    /// When the most recent attempt failed.
    pub last_attempt_at: DateTime<Utc>,
    /// When the next attempt will fire.
    pub next_attempt_at: DateTime<Utc>,
}

/// A point-in-time snapshot of the run's activity, cheap to clone for rendering.
#[derive(Debug, Clone, Default)]
pub struct ActivitySnapshot {
    /// Whether a turn is currently in flight (drives the throbber's visibility).
    pub busy: bool,
    /// The display name of the agent currently working, if one is known.
    pub agent: Option<String>,
    /// A short description of what that agent is doing (its goal or current tool).
    pub detail: Option<String>,
    /// Every in-flight retry episode, oldest last-attempt first, so the
    /// throbber can show one live backoff line (plus a count of the rest).
    pub retries: Vec<RetrySnapshot>,
    /// The input (prompt/context) tokens the most recent turn accounted for.
    pub last_input: u64,
    /// The output (completion) tokens the most recent turn produced.
    pub last_output: u64,
    /// The running sum of every turn's total tokens across the whole run.
    pub total_tokens: u64,
    /// The running sum of every turn's input tokens, as the provider reported
    /// them (uncached remainder on Anthropic/Bedrock, cache-inclusive on the
    /// OpenAI protocol).
    pub total_input: u64,
    /// The running sum of every turn's output tokens.
    pub total_output: u64,
    /// The running sum of input tokens read from the provider's prompt cache.
    pub total_cached: u64,
    /// The running sum of input tokens written into the provider's prompt cache.
    pub total_cache_creation: u64,
}

/// A shared, mutable handle to the run's [`ActivitySnapshot`].
///
/// Cloning shares the same underlying state (it is an `Arc` internally), so a
/// worker's clone and the render loop's clone always observe the same values.
#[derive(Debug, Clone, Default)]
pub struct RunActivity {
    /// The shared snapshot both the workers and the render loop touch.
    inner: Arc<Mutex<ActivitySnapshot>>,
    /// The in-flight retry episodes, keyed by their unique `retry_id` so
    /// concurrent agents (e.g. swarm members) update their own entry.
    retries: Arc<Mutex<HashMap<String, RetrySnapshot>>>,
}

impl RunActivity {
    /// Read a copy of the current activity snapshot for rendering
    pub fn snapshot(&self) -> ActivitySnapshot {
        // clone the guarded snapshot so the caller holds the lock only briefly
        let mut snapshot = self.inner.lock().unwrap().clone();
        // fold the live retry episodes in, oldest last-attempt first, so the
        // render loop sees a stable ordering across frames
        let mut retries: Vec<RetrySnapshot> =
            self.retries.lock().unwrap().values().cloned().collect();
        retries.sort_by_key(|retry| retry.last_attempt_at);
        snapshot.retries = retries;
        snapshot
    }

    /// Mark a turn as started or finished, driving the throbber's visibility
    ///
    /// Finishing a turn also clears the current agent/detail so a stale "thinking"
    /// line never lingers once the run goes idle.
    ///
    /// # Arguments
    ///
    /// * `busy` - Whether a turn is now in flight
    pub fn set_busy(&self, busy: bool) {
        // take the lock and flip the busy flag
        let mut guard = self.inner.lock().unwrap();
        guard.busy = busy;
        // clear the activity context when the run goes idle so nothing lingers
        if !busy {
            guard.agent = None;
            guard.detail = None;
            // drop any retry episode too, so a line whose resolution event was
            // lost (e.g. a cancelled turn) cannot outlive the run
            self.retries.lock().unwrap().clear();
        }
    }

    /// Record a failed attempt of an in-flight LLM retry episode
    ///
    /// # Arguments
    ///
    /// * `retry_id` - The unique key correlating this episode's attempts
    /// * `retry` - The episode's state after the attempt that just failed
    pub fn set_retry(&self, retry_id: impl Into<String>, retry: RetrySnapshot) {
        // take the lock and upsert this episode's entry
        self.retries.lock().unwrap().insert(retry_id.into(), retry);
    }

    /// Clear a retry episode once it resolves (success or final failure)
    ///
    /// # Arguments
    ///
    /// * `retry_id` - The unique key of the episode that resolved
    pub fn clear_retry(&self, retry_id: &str) {
        // take the lock and drop the resolved episode's entry
        self.retries.lock().unwrap().remove(retry_id);
    }

    /// Record which agent is working and what it is doing right now
    ///
    /// # Arguments
    ///
    /// * `agent` - The display name of the agent currently working
    /// * `detail` - A short description of its goal or current tool
    pub fn set_activity(&self, agent: impl Into<String>, detail: impl Into<String>) {
        // take the lock and overwrite the current activity context
        let mut guard = self.inner.lock().unwrap();
        guard.agent = Some(agent.into());
        guard.detail = Some(detail.into());
    }

    /// Fold a turn's token usage into the snapshot for the status-bar readout
    ///
    /// The latest turn's input/output are stored as-is (the context size), while
    /// every cumulative counter accumulates across the whole run so far.
    ///
    /// # Arguments
    ///
    /// * `input` - The input (context) tokens this turn accounted for
    /// * `output` - The output (completion) tokens this turn produced
    /// * `cached` - The input tokens this turn read from the provider's cache
    /// * `cache_creation` - The input tokens this turn wrote into the cache
    /// * `total` - The total tokens this turn accounted for
    pub fn record_usage(
        &self,
        input: u64,
        output: u64,
        cached: u64,
        cache_creation: u64,
        total: u64,
    ) {
        // take the lock and update the latest-turn counts and running totals
        let mut guard = self.inner.lock().unwrap();
        guard.last_input = input;
        guard.last_output = output;
        guard.total_tokens = guard.total_tokens.saturating_add(total);
        guard.total_input = guard.total_input.saturating_add(input);
        guard.total_output = guard.total_output.saturating_add(output);
        guard.total_cached = guard.total_cached.saturating_add(cached);
        guard.total_cache_creation = guard.total_cache_creation.saturating_add(cache_creation);
    }

    /// Seed the cumulative counters with a resumed session's saved usage
    ///
    /// Only the running totals are seeded -- the latest-turn context readout
    /// stays empty until the resumed run's first live turn reports in.
    ///
    /// # Arguments
    ///
    /// * `totals` - The saved run-wide usage tally to continue from
    pub fn seed_usage(&self, totals: &thorpal::UsageTotals) {
        // take the lock and fold the saved tallies into the cumulatives
        let mut guard = self.inner.lock().unwrap();
        guard.total_tokens = guard.total_tokens.saturating_add(totals.total_tokens);
        guard.total_input = guard.total_input.saturating_add(totals.input_tokens);
        guard.total_output = guard.total_output.saturating_add(totals.output_tokens);
        guard.total_cached = guard
            .total_cached
            .saturating_add(totals.cached_input_tokens);
        guard.total_cache_creation = guard
            .total_cache_creation
            .saturating_add(totals.cache_creation_input_tokens);
    }
}
