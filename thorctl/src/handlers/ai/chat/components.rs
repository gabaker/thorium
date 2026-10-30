//! The different components in the ThorPal chat tui

mod activity;
mod agent_stats;
mod ask_modal;
mod banner;
mod chat_worker;
mod gantt;
mod markdown;
mod message_board;
mod message_log;
mod shortcut_menu;
mod sides;
mod slash_menu;
pub mod status_bar;
mod summary_modal;
mod tab_bar;
mod text_box;
mod text_wrap;
mod theme;
pub mod throbber;
mod traversal;

pub(super) use activity::{ActivitySnapshot, RunActivity};
// the stats/gantt views, their logs and folds, the replay helpers, and the
// shared formatting primitives are crate-visible so the standalone `analyze usage`
// TUI can reuse them through the re-exports in `chat.rs`
pub(crate) use agent_stats::{
    AgentStatsLog, AgentStatsView, estimate_tokens, fmt_cost, fmt_duration, now_ms,
};
pub(super) use ask_modal::{AskModal, AskModalState, AskOutcome};
pub(super) use banner::Banner;
pub(super) use chat_worker::{ReportSettings, ThorPalClient, WorkerRequest};
pub(crate) use chat_worker::{replay_transcript, replayed_usage, usage_remainder};
pub(crate) use gantt::{ActivityIntervals, GanttLog, GanttView, TimeMap};
pub(super) use message_board::{BoardOutcome, MessageBoard};
pub(super) use message_log::MessageLog;
// the telemetry TUIs reuse the shortcut palette and the text helpers, so
// these (plus text_wrap's word wrapper below) widen to crate visibility
// (re-exported through chat.rs)
pub(crate) use markdown::markdown_lines;
pub(crate) use shortcut_menu::ShortcutMenu;
pub(crate) use sides::SideRegistry;
pub(super) use slash_menu::SlashMenu;
pub(crate) use status_bar::with_thousands;
pub(super) use summary_modal::{SummaryModal, SummaryModalState, SummaryOutcome};
pub(super) use tab_bar::TabBar;
// crate-visible so the setup wizard TUI can reuse it through chat.rs's re-export
pub(crate) use text_box::TextBox;
// crate-visible for the telemetry TUIs' pop-out wrapping, like the pieces above
pub(crate) use text_wrap::wrap_words;
pub use theme::Theme;
pub(super) use traversal::TraversalView;
pub(crate) use traversal::{TraversalLog, ViewOutcome};
