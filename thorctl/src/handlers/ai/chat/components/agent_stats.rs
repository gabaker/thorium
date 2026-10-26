//! The sub-agent inspection/stats view: who spent what, and on which work
//!
//! A session's activity is folded into per-agent tallies (cost, tool calls,
//! token buckets, lifetimes, turns), per-swarm groupings, and a session-wide
//! context-spend breakdown. The same pure reducer folds both live
//! [`thorpal::ThorPalEvent`]s and persisted [`thorpal::TranscriptLine`]s, so a
//! resumed session rebuilds the same tallies the live run produced (token
//! buckets replay from durable per-turn usage lines, topped up from the
//! snapshot ledger's remainder for whatever predates them).

mod log;
mod state;
mod view;

pub use log::AgentStatsLog;
pub use state::{StatsEvent, estimate_tokens, fmt_cost, fmt_duration, now_ms};
pub use view::AgentStatsView;
