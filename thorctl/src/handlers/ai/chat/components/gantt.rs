//! The gantt timeline view: the full lifetime of an analysis as a trace
//!
//! A session's activity is folded into swarm, agent, and tool-call spans laid
//! out Jaeger-style: one horizontal bar per span, sized by duration, with
//! concurrent work stacking vertically. Rows expand hierarchically (a swarm
//! into its member agents, an agent into its tool calls) and each row carries
//! a preview of counts, goals or arguments, tokens, and estimated cost. The
//! same pure reducer folds both live [`thorpal::ThorPalEvent`]s and persisted
//! [`thorpal::TranscriptLine`]s, so a resumed session rebuilds the same
//! timeline the live run produced (token buckets replay from durable per-turn
//! usage lines, topped up from the snapshot ledger's remainder for whatever
//! predates them).
//! The time axis is gap-compressed: idle stretches longer than five minutes
//! (a stop/restart downtime, most of all) collapse to a narrow break marker,
//! so a resumed run keeps its real proportions instead of being crushed into
//! the left margin by its own downtime.

mod log;
mod state;
mod timemap;
mod view;

pub use log::GanttLog;
pub use state::{ActivityIntervals, GanttEvent};
pub use timemap::TimeMap;
pub use view::GanttView;
