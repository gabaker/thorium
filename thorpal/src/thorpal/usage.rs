//! An instance-scoped token-usage ledger.
//!
//! Every agent in one ThorPal instance -- the main agent, the goal-extraction
//! agent, and each sub-agent run through
//! [`crate::thorpal::agents::tool::into_goal_tool`] -- reports the token usage
//! `rig` aggregates for its run into this shared ledger. Because sub-agents
//! execute as tool calls nested inside the main agent's loop, the main agent's
//! own usage covers only its own model turns; recording each agent separately
//! therefore sums to a true grand total without double-counting. The ledger is
//! held as an `Arc<UsageLedger>` on the `ThorPal` instance and shared with its
//! sub-agent hooks, so two ThorPal instances keep independent tallies. At the end
//! of a run [`UsageLedger::log_summary`] emits a per-agent breakdown and a grand
//! total.

use std::collections::BTreeMap;

use rig::completion::Usage;
use tokio::sync::Mutex;
use tracing::instrument;

use super::resume::UsageTotals;

/// An instance-scoped ledger of per-agent token usage.
///
/// Keyed by agent name in a `BTreeMap` so the summary prints in a stable,
/// name-sorted order regardless of the order agents happened to run in.
#[derive(Default)]
pub struct UsageLedger {
    /// The per-agent usage tally, guarded so concurrent sub-agents can record.
    inner: Mutex<BTreeMap<String, Usage>>,
}

impl UsageLedger {
    /// Creates a new, empty usage ledger
    pub fn new() -> Self {
        UsageLedger {
            inner: Mutex::new(BTreeMap::new()),
        }
    }

    /// Records a completed agent run's token usage into this ledger
    ///
    /// Repeated calls for the same agent accumulate, so an agent invoked several
    /// times over a run reports the sum of all of its invocations.
    ///
    /// # Arguments
    ///
    /// * `agent` - The name of the agent this usage belongs to
    /// * `usage` - The token usage `rig` aggregated for the agent's run
    #[instrument(skip_all)]
    pub async fn record(&self, agent: &str, usage: Usage) {
        // accumulate this run's usage under the agent's name, starting from zero if
        // this is the first time we have seen this agent
        let mut guard = self.inner.lock().await;
        *guard.entry(agent.to_owned()).or_default() += usage;
    }

    /// Takes a persistable copy of the per-agent tallies
    ///
    /// Used by session snapshotting so a resume can rebuild the ledger and the
    /// consumer's running token readout.
    #[instrument(skip_all)]
    pub async fn snapshot(&self) -> BTreeMap<String, UsageTotals> {
        // copy the tallies out under the lock, converting to the persistable mirror
        let guard = self.inner.lock().await;
        guard
            .iter()
            .map(|(agent, usage)| (agent.clone(), UsageTotals::from(*usage)))
            .collect()
    }

    /// Seeds this ledger with a saved per-agent tally
    ///
    /// Each saved tally folds into any existing entry rather than overwriting
    /// it, so restoring is safe even after the resumed run has already recorded.
    ///
    /// # Arguments
    ///
    /// * `saved` - The persisted per-agent tallies to fold in
    #[instrument(skip_all)]
    pub async fn restore(&self, saved: BTreeMap<String, UsageTotals>) {
        // fold each saved tally into the live map under the lock
        let mut guard = self.inner.lock().await;
        for (agent, totals) in saved {
            *guard.entry(agent).or_default() += Usage::from(totals);
        }
    }

    /// Logs the final token usage: one line per agent, a subtotal per multi-agent
    /// role, and a grand total
    ///
    /// Intended to be called once, after the run finishes, so the operator can see
    /// where the run spent its tokens. Roles with two or more named invocations
    /// (e.g. several `ReverseEngineer(...)` agents) also get a subtotal line;
    /// single-instance roles like `main` are skipped since their subtotal would
    /// just repeat their per-agent line.
    #[instrument(skip_all)]
    pub async fn log_summary(&self) {
        // take a snapshot of the ledger to log without holding the lock across the logs
        let guard = self.inner.lock().await;
        // start a fresh grand total to accumulate every agent's usage into
        let mut total = Usage::new();
        // accumulate a per-role subtotal and invocation count, keyed by role so the
        // subtotals print in a stable, name-sorted order like the per-agent lines
        let mut per_role: BTreeMap<String, (Usage, usize)> = BTreeMap::new();
        // print a header so the summary stands out among the run's other log lines
        tracing::info!("──────────── TOKEN USAGE ────────────");
        // emit one line per agent, folding each into the grand total and its role
        for (agent, usage) in guard.iter() {
            tracing::info!(
                agent = %agent,
                input = %with_thousands(usage.input_tokens),
                output = %with_thousands(usage.output_tokens),
                total = %with_thousands(usage.total_tokens),
                "agent token usage"
            );
            total += *usage;
            // fold this agent's usage into its role's running subtotal and count
            let entry = per_role.entry(role_of(agent).to_owned()).or_default();
            entry.0 += *usage;
            entry.1 += 1;
        }
        // emit a subtotal line for each role that had more than one named invocation
        for (role, (usage, count)) in per_role.iter().filter(|(_, (_, count))| *count > 1) {
            tracing::info!(
                role = %role,
                agents = %count,
                input = %with_thousands(usage.input_tokens),
                output = %with_thousands(usage.output_tokens),
                total = %with_thousands(usage.total_tokens),
                "role token usage across all agents"
            );
        }
        // emit the grand total across every agent that ran
        tracing::info!(
            input = %with_thousands(total.input_tokens),
            output = %with_thousands(total.output_tokens),
            total = %with_thousands(total.total_tokens),
            "TOTAL token usage across all agents"
        );
    }
}

/// Formats an unsigned integer with `,` grouping every three digits (e.g. 5481169 -> "5,481,169")
///
/// # Arguments
///
/// * `value` - The token count to render
pub(crate) fn with_thousands(value: u64) -> String {
    // render the raw digits so we can walk them and place separators
    let digits = value.to_string();
    // preallocate room for the digits plus the commas we'll insert between groups
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    // total digit count, used to detect where each group of three begins
    let len = digits.len();
    // walk the digits left-to-right, inserting a comma ahead of every fresh group
    for (idx, ch) in digits.chars().enumerate() {
        // a separator precedes any non-leading digit that starts a group of three
        if idx != 0 && (len - idx) % 3 == 0 {
            out.push(',');
        }
        // emit this digit after any separator that belonged before it
        out.push(ch);
    }
    out
}

/// Extracts the role prefix from a per-invocation agent display name
///
/// Sub-agent names are minted as `Role(Moon)` (e.g. `ReverseEngineer(Europa)`),
/// so the role is everything before the first `(`. Names without a `(` (like the
/// main agent's `main`) are their own role and are returned unchanged.
///
/// # Arguments
///
/// * `name` - The agent display name to pull the role prefix out of
fn role_of(name: &str) -> &str {
    // split at the first `(`; the head is the role, or the whole name if absent
    match name.split_once('(') {
        // a `Role(Moon)` name: keep just the role prefix
        Some((role, _)) => role,
        // a bare name with no moon suffix: it is its own role
        None => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Role(Moon)` name yields just the role, however the moon is suffixed.
    #[test]
    fn role_of_strips_the_moon_suffix() {
        // a plain role + moon reduces to the bare role prefix
        assert_eq!(role_of("ReverseEngineer(Europa)"), "ReverseEngineer");
        // a wrapped moon with a (n) pass suffix still yields the same role
        assert_eq!(role_of("ReverseEngineer(Europa (2))"), "ReverseEngineer");
    }

    /// A bare name with no moon suffix is its own role.
    #[test]
    fn role_of_passes_through_bare_names() {
        // the main agent has no `(`, so it is returned unchanged
        assert_eq!(role_of("main"), "main");
    }

    /// A ledger's tallies survive a snapshot/restore round-trip into a fresh ledger.
    #[tokio::test]
    async fn ledger_round_trips_through_snapshot_and_restore() {
        // record some usage for two agents, with a repeat to prove accumulation
        let ledger = UsageLedger::new();
        let usage = Usage {
            input_tokens: 10,
            output_tokens: 20,
            total_tokens: 42,
            cached_input_tokens: 8,
            cache_creation_input_tokens: 4,
            tool_use_prompt_tokens: 0,
            reasoning_tokens: 2,
        };
        ledger.record("main", usage).await;
        ledger.record("main", usage).await;
        ledger.record("ReverseEngineer(Europa)", usage).await;
        // snapshot the tallies and restore them into a fresh ledger
        let saved = ledger.snapshot().await;
        let restored = UsageLedger::new();
        restored.restore(saved.clone()).await;
        // the restored ledger snapshots back identical tallies
        assert_eq!(restored.snapshot().await, saved);
        // the repeated agent accumulated across both records
        assert_eq!(saved["main"].total_tokens, 84);
    }
}
