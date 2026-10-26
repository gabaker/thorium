//! The agent traversal graph: how agents moved through a binary's functions
//!
//! A session's tool activity is folded into a growing call graph of the
//! functions agents touched: every function-scoped tool call upserts a node,
//! edge-bearing tool results (callers/callees/xrefs) link nodes together, and
//! per-agent cursors track where each agent currently is. The same pure
//! reducer folds both live [`thorpal::ThorPalEvent`]s and persisted
//! [`thorpal::TranscriptLine`]s, so a resumed or replayed session rebuilds the
//! identical graph the live run produced.

mod layout;
mod log;
mod replay;
mod state;
mod view;

pub use log::TraversalLog;
pub use state::{NodeId, TraversalEvent};
pub use view::{TraversalView, ViewOutcome};
