//! A ThorPal-backed chat worker for Thorctl
//!
//! This drives a ThorPal [`Session`] over a local binary and mirrors the run's
//! activity into the [`ChatContext`] the ratatui shell renders: a prompt task
//! runs one conversational turn per user message, and a drain task turns
//! [`ThorPalEvent`]s into chat entries.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};

use kanal::{AsyncReceiver, AsyncSender};
use thorpal::{
    AnalystAnswer, AnalystLink, AskOption, ForumState, PendingAsk, Session, ThorPalEvent,
    ToolLogRouter, TranscriptItem, TranscriptLine, UsageTotals,
};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::Error;
use crate::handlers::ai::chat::{AppEvent, ChatContext};
use crate::handlers::ai::pricing::CostModel;

use super::RunActivity;
use super::activity::RetrySnapshot;
use super::agent_stats::{AgentStatsLog, StatsEvent};
use super::gantt::{GanttEvent, GanttLog};
use super::sides::{SideHandle, SideRegistry, is_side_root, scope_of_agent};
use super::traversal::{NodeId, TraversalEvent, TraversalLog};

/// A request the TUI sends to the background prompt task
pub enum WorkerRequest {
    /// A user chat message driving one conversational turn
    Chat(String),
    /// Generate a markdown report and write it to a path
    Report {
        /// Where to write the generated report
        path: PathBuf,
        /// An explicit template path, outranking the configured default
        template: Option<PathBuf>,
    },
    /// Promote a session script into the persistent cross-session library
    Promote {
        /// The bare name of the session script to promote
        name: String,
        /// What the script does and when a future run should use it
        description: String,
    },
    /// Fork (or switch to) a named side conversation off the main agent
    BtwFork {
        /// The side conversation's name
        name: String,
    },
    /// One conversational turn inside a named side conversation
    BtwChat {
        /// The side conversation to run the turn in
        name: String,
        /// The user's message for this turn
        msg: String,
    },
    /// Generate (or revise) the /done summary for a side conversation
    BtwSummarize {
        /// The side conversation to summarize
        name: String,
        /// The previously generated summary being revised, if any
        prior: Option<String>,
        /// The analyst's feedback steering the revision, if any
        feedback: Option<String>,
    },
    /// Accept a /done summary: inject it into the main chat and close the side
    BtwAccept {
        /// The side conversation whose summary is being accepted
        name: String,
        /// The (possibly analyst-edited) summary to bring back
        summary: String,
    },
    /// Close a side conversation without returning anything
    BtwClose {
        /// The side conversation to close
        name: String,
    },
}

/// The report settings resolved from the chat flags and the Thorctl config
#[derive(Clone, Default)]
pub struct ReportSettings {
    /// Auto-generate a report here once the agent requests to finish
    pub auto_path: Option<PathBuf>,
    /// The configured default template path (`--report-template`, then the `ai`
    /// config's `report_template`); `None` falls back to the builtin template
    pub template: Option<PathBuf>,
    /// Defang IOCs in reports written to disk (`--defang`); the session
    /// store's `report.md` copy always stays pristine
    pub defang: bool,
}

/// The live pause banner the pages render while the session is paused
///
/// Written by the event drain from [`ThorPalEvent::SessionPaused`] (and
/// cleared on [`ThorPalEvent::SessionResumed`]); session-wide, so every tab
/// renders the same banner.
pub struct PauseBanner {
    /// The short pause-kind label (e.g. "provider auth failed").
    pub kind: String,
    /// A short human-readable summary of the fault.
    pub error: String,
    /// How many automatic probes have failed since the pause began.
    pub probe_attempts: u32,
    /// When the next automatic probe fires.
    pub next_probe_at: DateTime<Utc>,
}

/// A chat client driving a ThorPal session in the background
pub struct ThorPalClient {
    /// A channel to send requests to the AI
    pub to_chat: AsyncSender<WorkerRequest>,
    /// The chat history shared with our background tasks
    pub context: ChatContext,
    /// The live run-activity the render loop reads for the throbber and usage
    pub activity: RunActivity,
    /// The cost estimator for this session's model, or `None` when the model
    /// is unknown and no pricing overrides were configured
    pub cost: Option<CostModel>,
    /// The session's store name, shown in the dead-worker banner's resume hint
    pub session_name: String,
    /// The reason the prompt worker died, set at most once by its supervisor
    ///
    /// A set value means the worker can no longer take requests: the pages
    /// render a persistent banner from it and refuse new submits instead of
    /// panicking on a closed channel. The session store was already autosaved
    /// turn by turn, so the banner points at `--resume`.
    pub worker_dead: Arc<std::sync::OnceLock<String>>,
    /// The controller for the session's provider pause state (/retry, /abort)
    pub pause: thorpal::PauseController,
    /// The analyst link ask_analyst waits on while this TUI is attached
    ///
    /// The question modal answers through it, and the quit path detaches it so
    /// an in-flight turn blocked on an ask resolves to its recommended default
    /// instead of wedging the bounded worker join.
    pub analyst: Arc<AnalystLink>,
    /// The live pause banner, raised and cleared by the event drain
    pub pause_banner: Arc<std::sync::Mutex<Option<PauseBanner>>>,
    /// The join handle for our supervised background prompt task
    ///
    /// Taken (once) by the TUI's quit path so it can await the worker's
    /// best-effort [`Session::finish`] -- the Ghidra flush and analysis-dir
    /// rename -- instead of letting process exit cancel it mid-write.
    join_handle: std::sync::Mutex<Option<JoinHandle<Result<(), Error>>>>,
    /// The ThorPal forum agents discuss on
    pub forum: Arc<ForumState>,
    /// Whether this client was resumed from a saved session
    ///
    /// A resumed client opens directly on the chat page (its history is already
    /// in the context) instead of the home page's new-analysis prompt.
    pub resumed: bool,
    /// The traversal log the graph view renders, fed by both worker tasks
    pub traversal: TraversalLog,
    /// The agent stats log the stats view renders, fed by both worker tasks
    pub agent_stats: AgentStatsLog,
    /// The gantt log the timeline view renders, fed by both worker tasks
    pub gantt: GanttLog,
    /// The shared registry of side conversations the btw tabs render from
    pub sides: SideRegistry,
    /// The session-wide usage bucket summing the main and every side's spend
    pub session_total: RunActivity,
}

impl ThorPalClient {
    /// Builds a chat client driven by a ThorPal [`Session`]
    ///
    /// This spawns two background tasks that share the client's context: a prompt
    /// task that seeds the analysis once and then runs one [`Session::chat`] turn
    /// per user message (a persistent, multi-turn conversation), and a drain task
    /// that folds the run's [`ThorPalEvent`]s into the shared context so the TUI
    /// renders tool calls, sub-agent activity, and progress live. A resumed
    /// session passes its saved conversation as `restored`, which is replayed
    /// into the context (before the tasks spawn, so ordering is deterministic)
    /// so the TUI opens showing the prior chat.
    ///
    /// # Arguments
    ///
    /// * `session` - The built ThorPal session driving this conversation
    /// * `session_name` - The session's store name, shown so it can be resumed
    /// * `events_rx` - The receiver end of the session's event channel
    /// * `to_app` - A channel to send app events (redraws, failures) over
    /// * `restored` - The saved conversation to replay, or `None` for a fresh run
    /// * `restored_usage` - A resumed session's saved per-agent usage ledger to continue from
    /// * `restored_sides` - A resumed session's saved side-conversation names
    /// * `cost` - The cost estimator for this session's model, if it is priceable
    /// * `report` - The resolved report settings for this chat
    /// * `tool_logs` - The shared log router streaming tool calls register on
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session: Session,
        session_name: String,
        events_rx: UnboundedReceiver<ThorPalEvent>,
        to_app: &AsyncSender<AppEvent>,
        restored: Option<Vec<TranscriptLine>>,
        restored_usage: Option<BTreeMap<String, thorpal::UsageTotals>>,
        restored_sides: Vec<String>,
        cost: Option<CostModel>,
        report: ReportSettings,
        tool_logs: Arc<ToolLogRouter>,
    ) -> Self {
        // build the channel the TUI sends user messages to the prompt task over
        let (to_chat, from_user) = kanal::unbounded_async();
        // the shell renders from this shared context; both tasks mutate it
        let context = ChatContext::default();
        // the traversal log the graph view renders; the transcript replay
        // seeds it and the drain task keeps feeding it live
        let traversal = TraversalLog::default();
        // the agent stats log the stats view renders; fed the same dual way
        let agent_stats = AgentStatsLog::default();
        // the gantt log the timeline view renders; fed the same dual way
        let gantt = GanttLog::default();
        // the registry of side conversations the btw tabs render from;
        // pre-create a resumed session's sides so the replay and the tab
        // builder both find them
        let sides = SideRegistry::default();
        for name in &restored_sides {
            sides.handle(name);
        }
        // remember whether this is a resume before the transcript is consumed
        let resumed = restored.is_some();
        // tell the user what this session is called and how to pick it up later
        context.add_ai(format!(
            "Session '{session_name}' — resume later with `thorctl analyze chat --resume {session_name}`"
        ));
        // sum any durable per-turn usage lines before the replay consumes the
        // transcript, so the seeding below can subtract what the stats and
        // gantt folds already counted
        let replayed = restored.as_deref().map(replayed_usage).unwrap_or_default();
        // replay a resumed session's prior conversation before the worker tasks
        // spawn, so the saved history always renders above the new activity;
        // the carry hands the dead run's open calls to the live drain so a
        // resumed delegation's result can close its replayed bubble
        let delegation_tools = session.delegation_tool_names();
        let carry = match restored {
            Some(transcript) => replay_transcript(
                transcript,
                &context,
                &sides,
                &traversal,
                &agent_stats,
                &gantt,
                &delegation_tools,
            ),
            None => ReplayCarry::default(),
        };
        // the main conversation's run-activity the render loop reads for the
        // throbber and the status-bar usage readout
        let activity = RunActivity::default();
        // the session-wide bucket summing the main and every side's spend
        let session_total = RunActivity::default();
        // seed a resumed session's saved usage so the running totals continue
        // instead of restarting at zero: main-scope keys sum into the main
        // readout, each side's keys into its own bucket, everything into the
        // session total, and the stats view keeps the full per-agent split
        if let Some(usage) = &restored_usage {
            // reconcile the snapshot ledger with the replayed usage lines
            // first: the remainder is whatever the lines did not cover, and
            // replayed + remainder is the per-field max of the two sources,
            // so a crash that left either side ahead never undercounts any
            // readout (and every pane agrees with `thorctl analyze usage`)
            let remainder = usage_remainder(usage, &replayed);
            let mut reconciled = replayed.clone();
            for (agent, rem) in &remainder {
                *reconciled.entry(agent.clone()).or_default() += *rem;
            }
            // fold the reconciled per-agent map into the per-scope tallies
            let mut main_summed = thorpal::UsageTotals::default();
            let mut session_summed = thorpal::UsageTotals::default();
            let mut side_summed: BTreeMap<String, thorpal::UsageTotals> = BTreeMap::new();
            for (agent, totals) in &reconciled {
                session_summed += *totals;
                match scope_of_agent(agent) {
                    // a scoped agent's spend lands in its side's bucket
                    Some(scope) => *side_summed.entry(scope.to_owned()).or_default() += *totals,
                    // everything else is the main conversation's spend
                    None => main_summed += *totals,
                }
            }
            activity.seed_usage(&main_summed);
            session_total.seed_usage(&session_summed);
            for (scope, totals) in side_summed {
                sides.handle(&scope).activity.seed_usage(&totals);
            }
            // the stats and gantt folds already counted the replayed usage
            // lines during the replay above, so they seed only the remainder
            // (pre-upgrade spend, hook-suppressed turns)
            agent_stats.seed_usage(&remainder);
            gantt.seed_usage(&remainder);
        }
        // grab the forum handle before the session moves into the prompt task so
        // the message board and the drain task can keep reading it
        let forum = session.forum();
        // grab the pause controller for the /retry and /abort commands, and
        // create the banner slot the drain task raises on a session pause
        let pause = session.pause_controller();
        let pause_banner = Arc::new(std::sync::Mutex::new(None));
        // grab and attach the analyst link so ask_analyst waits on this TUI
        // (headless runs never attach, so their asks auto-default instead)
        let analyst = session.analyst();
        analyst.attach();
        // spawn the best-effort edge-enrichment task over the session's shared
        // Ghidra connection; it idles silently when Ghidra is unavailable. the
        // handle rides into the prompt task so teardown can abort the poll
        // loop before it races the session's Ghidra shutdown
        let enrich = tokio::task::spawn(enrich_edges(
            session.ghidra(),
            traversal.clone(),
            to_app.clone(),
        ));
        // spawn the drain task first so no early event is missed; it owns its own
        // context, activity, and app-event clones and runs detached in the background
        tokio::task::spawn(drain_events(
            events_rx,
            context.clone(),
            activity.clone(),
            sides.clone(),
            session_total.clone(),
            forum.clone(),
            to_app.clone(),
            tool_logs,
            traversal.clone(),
            agent_stats.clone(),
            gantt.clone(),
            Arc::clone(&pause_banner),
            carry,
        ));
        // share the session so concurrent side turns can run alongside the
        // serial main loop; the prompt task holds one clone as the owner
        let session = Arc::new(session);
        // the write-once death note the supervisor sets and the pages render
        let worker_dead = Arc::new(std::sync::OnceLock::new());
        // spawn the prompt task; it owns the session for the client's lifetime
        let worker = tokio::task::spawn(run_prompts(
            session,
            resumed,
            from_user,
            context.clone(),
            activity.clone(),
            sides.clone(),
            to_app.clone(),
            report,
            enrich,
        ));
        // supervise the prompt task so a panic becomes a rendered dead-worker
        // state instead of a closed channel that panics the TUI in raw mode
        let join_handle = tokio::task::spawn(supervise_worker(
            worker,
            Arc::clone(&worker_dead),
            context.clone(),
            session_name.clone(),
            to_app.clone(),
        ));
        ThorPalClient {
            to_chat,
            context,
            activity,
            cost,
            session_name,
            worker_dead,
            pause,
            pause_banner,
            analyst,
            join_handle: std::sync::Mutex::new(Some(join_handle)),
            forum,
            resumed,
            traversal,
            agent_stats,
            gantt,
            sides,
            session_total,
        }
    }

    /// Take the supervised worker's join handle, once, for the quit path
    ///
    /// The TUI's teardown awaits this handle (bounded) after dropping its last
    /// request sender, so the worker's final [`Session::finish`] completes
    /// before the process exits. Returns `None` on a second take.
    pub fn take_join_handle(&self) -> Option<JoinHandle<Result<(), Error>>> {
        // a poisoned lock only means a panicking taker; the slot is plain data
        self.join_handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

/// Supervises the prompt task, turning a panic into a rendered dead state
///
/// The prompt task only ever returns once the TUI closes its channel, so a
/// join error (in practice: a panic somewhere inside a turn) is the one way
/// the worker dies mid-session. The supervisor records the reason in the
/// client's write-once death note, drops a visible error bubble into the main
/// log with the resume hint, and asks for a redraw -- the pages then refuse
/// new submits instead of panicking on the closed request channel. A clean
/// return (the quit path, including its best-effort `finish`) passes through
/// unchanged for the caller that awaits the join handle on quit.
///
/// # Arguments
///
/// * `worker` - The prompt task's join handle to supervise
/// * `worker_dead` - The client's write-once death note to set on a panic
/// * `context` - The main conversation's shared context for the error bubble
/// * `session_name` - The session's store name for the resume hint
/// * `to_app` - The channel to request a redraw over
async fn supervise_worker(
    worker: JoinHandle<Result<(), Error>>,
    worker_dead: Arc<std::sync::OnceLock<String>>,
    context: ChatContext,
    session_name: String,
    to_app: AsyncSender<AppEvent>,
) -> Result<(), Error> {
    match worker.await {
        // a clean return is the quit path; hand its result to the quit awaiter
        Ok(result) => result,
        // the worker died mid-session: render the reason instead of panicking
        Err(join_error) => {
            // pull the panic payload's message out when there is one
            let reason = match join_error.try_into_panic() {
                Ok(payload) => payload
                    .downcast_ref::<&str>()
                    .map(|msg| (*msg).to_owned())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "the worker panicked".to_owned()),
                Err(join_error) => format!("{join_error}"),
            };
            // record the death once; later channel failures keep the first note
            let _ = worker_dead.set(reason.clone());
            // one loud bubble in the main log says what happened and what to do
            context.add_ai(format!(
                "⚠ the chat worker stopped: {reason}\nThe session is saved — quit \
                 and resume with `thorctl analyze chat --resume {session_name}`"
            ));
            // ask for a redraw so the banner and bubble appear immediately
            let _ = to_app.send(AppEvent::Redraw).await;
            Err(Error::new(format!("the chat worker stopped: {reason}")))
        }
    }
}

/// Parses a ThorPal tool call's raw JSON arguments for display
///
/// Anything that is not a valid JSON object renders with no arguments rather
/// than failing.
///
/// # Arguments
///
/// * `args` - The raw JSON argument string ThorPal reported
fn parse_tool_args(args: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    // keep the arguments only when they are a JSON object
    match serde_json::from_str::<serde_json::Value>(args) {
        Ok(serde_json::Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// Renders the display phrase for one turn-budget extension trigger
///
/// Shared by the live event drain and the transcript replay so the two
/// renderings can never drift apart.
///
/// # Arguments
///
/// * `trigger` - What caused the extension to be granted or recorded
/// * `granted` - The turns granted (or reserved, for the reserved trigger)
fn extension_phrase(trigger: thorpal::ExtensionTrigger, granted: usize) -> String {
    // name the mechanism so the log reads unambiguously at a glance
    match trigger {
        thorpal::ExtensionTrigger::Requested => format!("granted +{granted} turn(s)"),
        thorpal::ExtensionTrigger::Reserved => {
            format!("reserved +{granted} turn(s) (applies at the cap)")
        }
        thorpal::ExtensionTrigger::ReservationApplied => {
            format!("reservation applied: +{granted} turn(s)")
        }
        thorpal::ExtensionTrigger::Auto => format!("auto-extended +{granted} turn(s) at the cap"),
    }
}

/// Renders the display label for a context-pressure level
///
/// # Arguments
///
/// * `level` - The pressure level: 1 = warn, 2 = critical
fn context_level_label(level: u8) -> &'static str {
    // the two levels the budget state reports, plus a defensive fallback
    match level {
        1 => "warning (flush findings to the forum)",
        2 => "CRITICAL (forum tools only)",
        _ => "pressure",
    }
}

/// The correlation state a transcript replay hands to the live event drain
///
/// Carries the replayed tool calls whose results never landed (the dead run's
/// open calls), so a resumed session's live events can close those bubbles:
/// the resume answers an interrupted delegation under the dead run's own call
/// id, and seeding the drain's live map with these entries lets that result
/// merge into the replayed call instead of being dropped as unmatched.
pub(crate) struct ReplayCarry {
    /// The replayed calls still awaiting results, keyed by ThorPal call_id
    open_calls: HashMap<String, (Uuid, ChatContext)>,
}

impl Default for ReplayCarry {
    /// An empty carry for fresh sessions with nothing replayed
    fn default() -> Self {
        ReplayCarry {
            open_calls: HashMap::new(),
        }
    }
}

/// Computes the transcript lines a replay hides as resolved failure debris
///
/// A dead run can leave terminal failure records behind (the error that
/// killed it, the sub-agents it took down); once a resume barrier shows the
/// session was picked back up -- and the failed work continued -- replaying
/// that debris would spam the chat log with errors the resume made moot. Per
/// barrier, three families of lines before it are hidden:
///
/// - the trailing cluster of main-conversation `turn_failed` lines with no
///   later main-scope activity before the barrier (the terminal failure;
///   mid-run failures the model recovered from stay visible). This hides the
///   terminal error of ANY failure class once the session was resumed --
///   deliberately, per the design decision that a resumed-after-failure
///   session reads as never-failed; the durable line survives for audits,
/// - a `sub_agent_terminated` whose agent was started again as resumed after
///   the barrier (per-invocation display names make the pairing exact),
/// - a failed delegation `tool_result` whose call id is re-answered after the
///   barrier (hiding it leaves the call open so the real result closes it).
///
/// The lines stay in the durable transcript -- audit tooling still sees them
/// -- only the replay rendering skips them.
///
/// # Arguments
///
/// * `transcript` - The saved transcript lines, in original render order
/// * `delegation_tools` - The workflow's delegation tool names
fn replay_suppressions(
    transcript: &[TranscriptLine],
    delegation_tools: &BTreeSet<String>,
) -> std::collections::HashSet<usize> {
    let mut hidden = std::collections::HashSet::new();
    // the barrier indices split the transcript into per-process segments
    let barriers: Vec<usize> = transcript
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            matches!(line.item, TranscriptItem::SessionResumed { .. }).then_some(index)
        })
        .collect();
    if barriers.is_empty() {
        return hidden;
    }
    // index what the post-barrier processes resumed/re-answered, so segment
    // debris can be paired exactly: resumed display names and re-answered
    // call ids anywhere after the first barrier that follows them
    for (window, &barrier) in barriers.iter().enumerate() {
        // the segment is everything after the previous barrier
        let start = window
            .checked_sub(1)
            .map(|prev| barriers[prev] + 1)
            .unwrap_or(0);
        let segment = &transcript[start..barrier];
        let after = &transcript[barrier + 1..];
        // (a) the trailing cluster of main turn failures: every main-scope
        // turn_failed after the segment's last main-scope user/assistant item
        let last_activity = segment.iter().rposition(|line| {
            matches!(
                &line.item,
                TranscriptItem::User { scope: None, .. }
                    | TranscriptItem::Assistant { scope: None, .. }
            )
        });
        for (offset, line) in segment.iter().enumerate() {
            if last_activity.is_some_and(|last| offset <= last) {
                continue;
            }
            if matches!(&line.item, TranscriptItem::TurnFailed { agent, .. } if agent == "main") {
                hidden.insert(start + offset);
            }
        }
        // (b) a terminated sub-agent that a later process resumed by name
        for (offset, line) in segment.iter().enumerate() {
            let TranscriptItem::SubAgentTerminated { name, .. } = &line.item else {
                continue;
            };
            let resumed_later = after.iter().any(|later| {
                matches!(
                    &later.item,
                    TranscriptItem::SubAgentStarted { name: resumed_name, resumed: true, .. }
                        if resumed_name == name
                )
            });
            if resumed_later {
                hidden.insert(start + offset);
            }
        }
        // (c) a failed delegation result whose call a later process re-answered
        for (offset, line) in segment.iter().enumerate() {
            let TranscriptItem::ToolResult {
                call_id,
                tool,
                success: false,
                ..
            } = &line.item
            else {
                continue;
            };
            if !delegation_tools.contains(tool) {
                continue;
            }
            let reanswered = after.iter().any(|later| {
                matches!(
                    &later.item,
                    TranscriptItem::ToolResult { call_id: later_id, .. } if later_id == call_id
                )
            });
            if reanswered {
                hidden.insert(start + offset);
            }
        }
    }
    hidden
}

/// Replays a saved session transcript into the shared chat context
///
/// Mirrors [`drain_events`]' rendering arms exactly, so a resumed chat log
/// looks the same as it did live: the items were persisted in render order
/// from the same event stream, so replaying the file top-to-bottom rebuilds
/// the original interleaving of user, main-agent, and sub-agent entries.
/// Tool calls and their results merge on a freshly minted Uuid per persisted
/// `call_id`, exactly as the live drain does. Lines carrying a side scope --
/// `btw:`-prefixed agents or scope-tagged user/assistant lines -- are routed
/// into that side's own context, so each btw tab's log rebuilds alongside the
/// main one. Each line is also folded into the traversal, stats, and gantt
/// logs, so those views open showing the whole prior run; a replay-ended
/// marker then settles anything a torn transcript left open.
///
/// # Arguments
///
/// * `transcript` - The saved transcript lines, in original render order
/// * `context` - The main conversation's shared context
/// * `sides` - The shared registry scoped lines are routed through
/// * `traversal` - The traversal log the graph view renders
/// * `agent_stats` - The agent stats log the stats view renders
/// * `gantt` - The gantt log the timeline view renders
/// * `delegation_tools` - The workflow's delegation tool names, for pairing
///   resumed delegations with the failure debris they supersede
pub(crate) fn replay_transcript(
    transcript: Vec<TranscriptLine>,
    context: &ChatContext,
    sides: &SideRegistry,
    traversal: &TraversalLog,
    agent_stats: &AgentStatsLog,
    gantt: &GanttLog,
    delegation_tools: &BTreeSet<String>,
) -> ReplayCarry {
    // route an explicit scope tag to its side's context (main when untagged)
    let ctx_for_scope = |scope: &Option<String>| match scope {
        Some(scope) => sides.handle(scope).context,
        None => context.clone(),
    };
    // route an agent's lines to its scope's context, exactly like the live drain
    let ctx_for = |agent: &str| match scope_of_agent(agent) {
        Some(scope) => sides.handle(scope).context,
        None => context.clone(),
    };
    // map a persisted call_id to the Uuid the message log merges a call+result
    // on, plus the context the call replayed into so its result follows it
    let mut call_ids: HashMap<String, (Uuid, ChatContext)> = HashMap::new();
    // resolve the failure debris a later resume made moot, so it never renders
    let hidden = replay_suppressions(&transcript, delegation_tools);
    // replay every item through the same rendering the live drain applies
    for (index, line) in transcript.into_iter().enumerate() {
        // skip debris a resume superseded: no bubble, no stats, no gantt bar
        if hidden.contains(&index) {
            continue;
        }
        // fold the line into the traversal graph before the item moves below
        if let Some(event) = TraversalEvent::from_transcript(&line) {
            traversal.push(event);
        }
        // fold the line into the per-agent stats the stats view renders
        if let Some(event) = StatsEvent::from_transcript(&line) {
            agent_stats.push(event);
        }
        // fold the line into the timeline the gantt view renders
        if let Some(event) = GanttEvent::from_transcript(&line) {
            gantt.push(event);
        }
        match line.item {
            // the user's prior messages come back as user bubbles in their scope
            TranscriptItem::User { text, scope } => ctx_for_scope(&scope).add_user(text),
            // a conversation's prior answers come back attributed to its root
            TranscriptItem::Assistant { text, scope } => {
                // a side's answers carry its root name, main's carry "main"
                let agent = match &scope {
                    Some(scope) => format!("btw:{scope}"),
                    None => "main".to_string(),
                };
                ctx_for_scope(&scope).add_ai_from(text, Some(agent));
            }
            // a sub-agent's text comes back attributed to it, in its scope
            TranscriptItem::SubAgentMessage { agent, text } => {
                ctx_for(&agent).add_ai_from(text, Some(agent));
            }
            // a sub-agent delegation breadcrumb, attributed to the sub-agent;
            // a resumed invocation renders exactly like the live bubble did
            TranscriptItem::SubAgentStarted {
                name,
                goal,
                resumed,
                ..
            } => {
                let glyph = if resumed { "↻" } else { "▶" };
                let verb = if resumed { " resumed" } else { "" };
                ctx_for(&name)
                    .add_ai_from(format!("{glyph} {name}{verb} — goal: {goal}"), Some(name));
            }
            // a sub-agent's turn-extension breadcrumb, attributed to the agent
            TranscriptItem::SubAgentExtended {
                name,
                granted,
                remaining_allowance,
                justification,
                trigger,
            } => {
                ctx_for(&name).add_ai_from(
                    format!(
                        "⏱ {name} {} ({remaining_allowance} left): {justification}",
                        extension_phrase(trigger, granted),
                    ),
                    Some(name),
                );
            }
            // a context-pressure breadcrumb, attributed to the agent
            TranscriptItem::ContextPressure {
                name,
                level,
                used_tokens,
                context_window,
            } => {
                ctx_for(&name).add_ai_from(
                    format!(
                        "⚠ {name} context {}: ~{used_tokens} of {context_window} tokens",
                        context_level_label(level),
                    ),
                    Some(name),
                );
            }
            // a sub-agent death breadcrumb, with its salvage post when one landed
            TranscriptItem::SubAgentTerminated {
                name,
                reason,
                salvage_post,
                ..
            } => {
                let posted = match &salvage_post {
                    Some(post) => format!(" (handoff posted as #{post})"),
                    None => String::new(),
                };
                ctx_for(&name).add_ai_from(
                    format!("✖ {name} terminated early — {reason}{posted}"),
                    Some(name),
                );
            }
            // a swarm's start and finish breadcrumbs, in the swarm's scope;
            // a resumed swarm renders exactly like the live bubble did
            TranscriptItem::SwarmStarted {
                name,
                goal,
                members,
                resumed,
            } => {
                let (glyph, verb) = if resumed {
                    ("↻", "resumed")
                } else {
                    ("▶", "started")
                };
                ctx_for(&name).add_ai(format!(
                    "{glyph} {name} {verb} {} agents ({}): {goal}",
                    members.len(),
                    members.join(", "),
                ));
            }
            TranscriptItem::SwarmFinished {
                name,
                succeeded,
                failed,
            } => {
                ctx_for(&name).add_ai(format!(
                    "■ {name} finished: {succeeded} succeeded, {failed} failed"
                ));
            }
            // a tool call: mint a Uuid and remember it (plus its scope's
            // context) against the call_id
            TranscriptItem::ToolCall {
                agent,
                call_id,
                tool,
                args,
            } => {
                let id = Uuid::new_v4();
                let call_context = ctx_for(&agent);
                call_ids.insert(call_id, (id, call_context.clone()));
                call_context.add_tool_call(id, Some(agent), tool, parse_tool_args(&args));
            }
            // a tool result merges into its call's entry in the same context;
            // an unmatched result (its call line was torn by a crash) is skipped
            TranscriptItem::ToolResult {
                call_id,
                result,
                success,
                ..
            } => {
                if let Some((id, call_context)) = call_ids.remove(&call_id) {
                    call_context.add_tool_result(id, result, Some(success));
                }
            }
            // a failed turn comes back as the same inline error bubble
            TranscriptItem::TurnFailed {
                agent,
                error,
                request_id,
            } => {
                // replay the failure as the same compact, click-to-expand
                // entry the live run showed, structured request id included
                // (older lines without one degrade to the embedded id in the
                // error text behind the click)
                let summary = failure_summary(&error);
                let detail = (summary != error).then_some(error);
                ctx_for(&agent).add_failure(
                    format!("{agent}: {summary}"),
                    Some(agent),
                    request_id,
                    detail,
                );
            }
            // a scrubbed retry comes back as the same breadcrumb it was shown
            // as (the episode and request ids ride the line for offline
            // telemetry, not for the chat log)
            TranscriptItem::LlmScrubbed { agent, level, .. } => {
                ctx_for(&agent).add_ai_from(
                    format!(
                        "⚠ {agent}: content filter blocked a request; retried \
                         at scrub level {level}"
                    ),
                    Some(agent),
                );
            }
            // a report degradation comes back on the main context
            TranscriptItem::ReportDegraded { rung, detail } => {
                ctx_for_scope(&None).add_ai(format!("⚠ report degraded to rung {rung}: {detail}"));
            }
            // a resolved analyst ask comes back as the same decision bubble
            TranscriptItem::AnalystAsk {
                agent,
                question,
                chosen,
                source,
            } => {
                ctx_for(&agent).add_ai_from(
                    format!("❓ {question} → {chosen:?} ({source})"),
                    Some(agent),
                );
            }
            // a raised review flag comes back as the same queue-entry bubble
            TranscriptItem::ReviewFlag { agent, title } => {
                ctx_for(&agent).add_ai_from(
                    format!("⚑ flagged for analyst review: {title}"),
                    Some(agent),
                );
            }
            // a usage checkpoint is not a chat bubble; the stats and gantt
            // folds above already consumed it
            TranscriptItem::Usage { .. } => {}
            // a resume barrier is replay machinery, never a chat bubble: it
            // marks where a later process picked the session back up (the
            // suppression pass above already consumed it)
            TranscriptItem::SessionResumed { .. } => {}
            // retry attempts and episode closes are offline telemetry; live
            // rendering showed only a transient throbber line, never a chat
            // bubble, so replay mirrors that silence
            TranscriptItem::LlmRetry { .. } | TranscriptItem::LlmRetryResolved { .. } => {}
            // an offered-tool manifest is offline telemetry with no live
            // rendering at all, so replay shows nothing either
            TranscriptItem::ToolsOffered { .. } => {}
            // a detector roster is offline telemetry the same way
            TranscriptItem::DetectorsRun { .. } => {}
            // a pause's first landing comes back as the same log line the
            // live run added under its banner (the banner itself is live-only)
            TranscriptItem::ProviderPaused {
                pause,
                agent,
                error,
                request_id,
            } => {
                ctx_for_scope(&None).add_failure(
                    format!(
                        "session paused — {pause}: {error} (first hit by {agent}). \
                         Probing automatically; /retry probes now, /abort fails the \
                         parked requests instead."
                    ),
                    Some(agent),
                    request_id,
                    None,
                );
            }
            // a pause's end comes back as the same one-line outcome the live
            // run showed when the banner cleared
            TranscriptItem::ProviderResumed {
                pause, recovered, ..
            } => {
                if recovered {
                    ctx_for_scope(&None).add_ai(format!("▶ session resumed — {pause} recovered"));
                } else {
                    ctx_for_scope(&None).add_ai(
                        "⚠ provider pause aborted — the parked requests surface \
                         their errors"
                            .to_owned(),
                    );
                }
            }
        }
    }
    // the session that wrote this transcript is gone, so anything still open
    // when it ended (a torn tool call, a running swarm) was interrupted --
    // settle it so no view keeps a bar growing toward now forever
    agent_stats.push(StatsEvent::ReplayEnded);
    gantt.push(GanttEvent::ReplayEnded);
    // hand the still-open calls to the live drain: a resumed delegation's
    // result arrives under the dead run's call id and closes its bubble
    ReplayCarry {
        open_calls: call_ids,
    }
}

/// Sums the per-turn usage checkpoint lines a transcript carries, per agent
///
/// Sessions saved before usage lines existed yield an empty map; the caller
/// then seeds the full snapshot ledger instead.
///
/// # Arguments
///
/// * `transcript` - The saved transcript lines to sum
pub(crate) fn replayed_usage(transcript: &[TranscriptLine]) -> BTreeMap<String, UsageTotals> {
    // fold every usage line into its agent's accumulated buckets
    let mut summed: BTreeMap<String, UsageTotals> = BTreeMap::new();
    for line in transcript {
        if let TranscriptItem::Usage {
            agent,
            input,
            output,
            cached,
            cache_creation,
            total,
        } = &line.item
        {
            // mirror the line's buckets into the persistable shape and fold;
            // the buckets a usage line does not carry stay in the remainder
            *summed.entry(agent.clone()).or_default() += UsageTotals {
                input_tokens: *input,
                output_tokens: *output,
                total_tokens: *total,
                cached_input_tokens: *cached,
                cache_creation_input_tokens: *cache_creation,
                tool_use_prompt_tokens: 0,
                reasoning_tokens: 0,
            };
        }
    }
    summed
}

/// A saved usage ledger minus what a transcript replay already counted
///
/// Replayed usage lines and the ledger can disagree in both directions: the
/// ledger also tallies turns whose response hook rig suppressed, while a crash
/// between a transcript append and the next autosave leaves the lines ahead of
/// the ledger. Each field subtracts saturating so neither skew double-counts,
/// and an agent whose remainder is all zeros across the line-carried buckets
/// is dropped entirely so its exact replayed turn count survives (a seed
/// would mark it unknown). The reasoning and tool-use counters are
/// informational sub-breakdowns usage lines never carry and no seed or view
/// renders, so they never block the drop.
///
/// # Arguments
///
/// * `ledger` - The snapshot's accumulated per-agent usage
/// * `replayed` - The per-agent sums the transcript's usage lines already fed
pub(crate) fn usage_remainder(
    ledger: &BTreeMap<String, UsageTotals>,
    replayed: &BTreeMap<String, UsageTotals>,
) -> BTreeMap<String, UsageTotals> {
    // subtract each agent's replayed sums from its ledger entry
    let mut remainder = BTreeMap::new();
    for (agent, totals) in ledger {
        // an agent with no replayed lines keeps its whole ledger entry
        let seen = replayed.get(agent).copied().unwrap_or_default();
        // saturate per field so a transcript ahead of the ledger yields zero
        let rem = UsageTotals {
            input_tokens: totals.input_tokens.saturating_sub(seen.input_tokens),
            output_tokens: totals.output_tokens.saturating_sub(seen.output_tokens),
            total_tokens: totals.total_tokens.saturating_sub(seen.total_tokens),
            cached_input_tokens: totals
                .cached_input_tokens
                .saturating_sub(seen.cached_input_tokens),
            cache_creation_input_tokens: totals
                .cache_creation_input_tokens
                .saturating_sub(seen.cache_creation_input_tokens),
            tool_use_prompt_tokens: totals
                .tool_use_prompt_tokens
                .saturating_sub(seen.tool_use_prompt_tokens),
            reasoning_tokens: totals
                .reasoning_tokens
                .saturating_sub(seen.reasoning_tokens),
        };
        // an all-zero remainder across the buckets usage lines actually carry
        // would only flag the agent as seeded; skip it (the reasoning and
        // tool-use sub-breakdowns can never be covered by lines, so counting
        // them here would mislabel every fully-checkpointed session)
        let covered = rem.input_tokens == 0
            && rem.output_tokens == 0
            && rem.total_tokens == 0
            && rem.cached_input_tokens == 0
            && rem.cache_creation_input_tokens == 0;
        if !covered {
            remainder.insert(agent.clone(), rem);
        }
    }
    remainder
}

/// Drives the persistent ThorPal conversation, one turn per user message
///
/// Seeds the analysis once (surfacing a "starting analysis" bubble; a resumed
/// session skips seeding and says so), then loops receiving requests: a chat
/// message is added to the context as a user bubble and run through
/// [`Session::chat`], whose answer is added back as an assistant bubble, while
/// a report request generates and writes a report. A failed turn is only
/// logged; the event drain renders the engine's `TurnFailed` bubble for it.
/// When an auto-report path was configured, a
/// report is generated exactly once after the first settled turn in which the
/// agent has requested to finish. When the user channel closes (the TUI quit),
/// the session is finished best-effort so Ghidra flushes.
///
/// # Arguments
///
/// * `session` - The ThorPal session this conversation drives
/// * `resumed` - Whether this session was resumed from a saved store
/// * `from_user` - The channel user requests arrive on
/// * `context` - The shared context this task writes user/assistant bubbles into
/// * `activity` - The shared run-activity this task flips busy while it works
/// * `sides` - The shared registry of side conversations btw requests target
/// * `to_app` - The channel to request redraws and report failures over
/// * `report` - The resolved report settings for this chat
/// * `enrich` - The edge-enrichment poller, aborted before the session finishes
#[allow(clippy::too_many_arguments)]
async fn run_prompts(
    session: Arc<Session>,
    resumed: bool,
    from_user: AsyncReceiver<WorkerRequest>,
    context: ChatContext,
    activity: RunActivity,
    sides: SideRegistry,
    to_app: AsyncSender<AppEvent>,
    report: ReportSettings,
    enrich: JoinHandle<()>,
) -> Result<(), Error> {
    // let the user know the (potentially slow) Ghidra analysis is starting; a
    // resumed session skips the detector seed and continues where it left off
    if resumed {
        context.add_ai("Resuming analysis…");
    } else {
        context.add_ai("Starting analysis…");
    }
    // the seed pass can be slow, so show the throbber while it runs
    activity.set_busy(true);
    let _ = to_app.send(AppEvent::Redraw).await;
    // seed the forum with the static detectors' findings before the first turn
    // (a no-op for a resumed session, whose config disables seeding)
    session.seed().await;
    // the seed is done and no turn is in flight yet, so idle the throbber
    activity.set_busy(false);
    let _ = to_app.send(AppEvent::Redraw).await;
    // a restored session may hold an interrupted turn whose sub-agents were
    // mid-flight when the last process died; confirm before spending tokens
    // on it, so opening a session just to inspect it stays free -- declining
    // (or quitting) holds the work, which then resumes right before the next
    // message instead
    if resumed && session.has_interrupted_turn().await {
        let delegations = session.interrupted_delegations().await;
        let question = format!(
            "This session was interrupted mid-turn with {delegations} sub-agent \
             delegation(s) outstanding. Resume the interrupted work now? \
             (Enter resumes; Esc holds it until your next message.)"
        );
        let options = vec![
            AskOption {
                label: "Resume now".to_owned(),
                description: "restart the interrupted sub-agents and finish the turn immediately"
                    .to_owned(),
            },
            AskOption {
                label: "Hold".to_owned(),
                description: "keep the work frozen; it resumes right before your next message"
                    .to_owned(),
            },
        ];
        let answer = session
            .analyst()
            .pose("main", question, options, "Resume now")
            .await;
        // only an explicit pick resumes now: declining (Esc) and teardown
        // both hold, keeping resume-and-quit token-free
        if matches!(&answer, AnalystAnswer::Picked { label } if label == "Resume now") {
            run_resume_turn(&session, &context, &activity, &to_app).await;
        } else {
            context.add_ai(
                "Interrupted work held — it resumes automatically before your next message",
            );
            let _ = to_app.send(AppEvent::Redraw).await;
        }
    }
    // grab a forum handle so the auto-report check can observe the finish flag
    let forum = session.forum();
    // latch so a configured auto-report is only ever generated once per run
    let mut auto_reported = false;
    // handle user requests until the TUI closes the channel
    loop {
        // wait for the next user request; a closed channel means the TUI quit
        let msg = match from_user.recv().await {
            // run a single conversational turn for a chat message
            Ok(WorkerRequest::Chat(msg)) => msg,
            // generate and write a report for an explicit /report request
            Ok(WorkerRequest::Report { path, template }) => {
                run_report(
                    &session,
                    &path,
                    template.as_ref(),
                    &report,
                    &context,
                    &activity,
                    &to_app,
                )
                .await;
                continue;
            }
            // promote a session script for an explicit /promote request; every
            // outcome lands as a bubble rather than failing the chat loop
            Ok(WorkerRequest::Promote { name, description }) => {
                match session.promote_script(&name, &description).await {
                    Ok(message) => context.add_ai(message),
                    Err(err) => context.add_ai(format!("⚠ failed to promote '{name}': {err:#}")),
                }
                let _ = to_app.send(AppEvent::Redraw).await;
                continue;
            }
            // fork (or switch to) a named side conversation off the main agent
            Ok(WorkerRequest::BtwFork { name }) => {
                match session.fork_side(&name).await {
                    // a fresh fork tells the side's tab where it came from
                    Ok(true) => sides.handle(&name).context.add_ai(format!(
                        "Side conversation '{name}' forked from the main chat — \
                         /done summarizes it back, /close discards it"
                    )),
                    // an existing side is just switched to, nothing to add
                    Ok(false) => (),
                    // a failed fork surfaces in the main chat, where it was typed
                    Err(err) => {
                        context.add_ai(format!("⚠ failed to fork '{name}': {err:#}"));
                    }
                }
                let _ = to_app.send(AppEvent::Redraw).await;
                continue;
            }
            // run a side conversation's turn concurrently with the main loop;
            // the side's own lock serializes turns within one side
            Ok(WorkerRequest::BtwChat { name, msg }) => {
                tokio::spawn(run_side_turn(
                    Arc::clone(&session),
                    name.clone(),
                    msg,
                    sides.handle(&name),
                    to_app.clone(),
                ));
                continue;
            }
            // generate (or revise) a /done summary concurrently too, so a slow
            // summarizer never blocks the main conversation
            Ok(WorkerRequest::BtwSummarize {
                name,
                prior,
                feedback,
            }) => {
                tokio::spawn(run_side_summary(
                    Arc::clone(&session),
                    name.clone(),
                    prior,
                    feedback,
                    sides.handle(&name),
                    to_app.clone(),
                ));
                continue;
            }
            // accept a /done summary: inject it into the main conversation and
            // close the side; the injection is fast, so it runs inline
            Ok(WorkerRequest::BtwAccept { name, summary }) => {
                match session.accept_side(&name, &summary).await {
                    Ok(()) => {
                        // mirror the injected note as a visible main-chat bubble,
                        // matching what the transcript replays on resume
                        context.add_user(format!(
                            "[Summary returned from side conversation '{name}']\n\n{summary}"
                        ));
                        // the side is gone, so drop its registry handle and tab
                        sides.remove(&name);
                        let _ = to_app.send(AppEvent::CloseBtwTab { name }).await;
                    }
                    // a failed accept surfaces in the side's own tab, which stays open
                    Err(err) => {
                        sides
                            .handle(&name)
                            .context
                            .add_ai(format!("⚠ failed to accept the summary: {err:#}"));
                    }
                }
                let _ = to_app.send(AppEvent::Redraw).await;
                continue;
            }
            // close a side conversation without returning anything
            Ok(WorkerRequest::BtwClose { name }) => {
                // a failed close still drops the tab; the error lands in main
                if let Err(err) = session.close_side(&name).await {
                    context.add_ai(format!("⚠ failed to close '{name}': {err:#}"));
                }
                sides.remove(&name);
                let _ = to_app.send(AppEvent::CloseBtwTab { name }).await;
                let _ = to_app.send(AppEvent::Redraw).await;
                continue;
            }
            Err(_) => break,
        };
        // an interrupted turn held at open (or by a prior decline) completes
        // right before the new message, so its delegations' real results
        // re-enter the conversation first
        if session.has_interrupted_turn().await {
            run_resume_turn(&session, &context, &activity, &to_app).await;
        }
        // show the user's message as a bubble and redraw immediately
        context.add_user(msg.clone());
        // mark the run busy for this turn so the throbber shows while it works
        activity.set_busy(true);
        let _ = to_app.send(AppEvent::Redraw).await;
        // run a single conversational turn against the persistent session
        let result = session.chat(msg).await;
        // the turn is done, so idle the throbber regardless of the outcome
        activity.set_busy(false);
        match result {
            // add the turn's answer as an assistant entry attributed to the main agent
            Ok(outcome) => {
                context.add_ai_from(outcome.response, Some("main".to_string()));
                let _ = to_app.send(AppEvent::Redraw).await;
            }
            // the drain task already rendered the engine's TurnFailed bubble
            // for this failure, so adding another here would double-bubble
            // every failed turn; the file log keeps the full detail
            Err(err) => {
                tracing::warn!(error = %format!("{err:#}"), "chat turn failed");
                let _ = to_app.send(AppEvent::Redraw).await;
            }
        }
        // once the agent has requested to finish, auto-generate the configured
        // report exactly once; the flag is observed without clearing it so the
        // persisted forum state stays untouched
        if let Some(auto) = &report.auto_path
            && !auto_reported
            && forum.finish_requested()
        {
            // latch before generating so a failure is not retried every turn
            auto_reported = true;
            run_report(&session, auto, None, &report, &context, &activity, &to_app).await;
        }
    }
    // the TUI has gone away: stop the edge-enrichment poller first so its
    // periodic Ghidra queries cannot race the shutdown inside the finish below
    enrich.abort();
    // finish the session best-effort so Ghidra flushes. the terminal is being
    // torn down, so a failure here is swallowed rather than printed (which
    // would corrupt the restoring screen) and surfaced as the task's returned
    // error for a caller that later inspects the join handle
    session
        .finish()
        .await
        .map_err(|err| Error::new(format!("{err:#}")))
}

/// Completes a restored interrupted turn, rendering it like a live chat turn
///
/// The resumed delegations stream their activity through the normal event
/// drain (started/resumed bubbles, tool calls, usage); this helper owns the
/// busy throbber and renders the completed turn's answer as a main bubble,
/// exactly like a chat turn's. A failure is logged -- the engine's TurnFailed
/// bubble covers rendering it.
///
/// # Arguments
///
/// * `session` - The session holding the interrupted turn
/// * `context` - The main conversation's shared context
/// * `activity` - The main conversation's run-activity bucket
/// * `to_app` - The channel to request redraws over
async fn run_resume_turn(
    session: &Session,
    context: &ChatContext,
    activity: &RunActivity,
    to_app: &AsyncSender<AppEvent>,
) {
    // mark the resumption so the operator sees why work is starting
    context.add_ai("↻ Resuming interrupted work…");
    activity.set_busy(true);
    let _ = to_app.send(AppEvent::Redraw).await;
    // complete the held turn: delegations re-run, then the turn continues
    let result = session.resume_interrupted().await;
    activity.set_busy(false);
    match result {
        // render the completed turn's answer exactly like a chat turn's
        Ok(outcome) => {
            // an empty answer means another path already completed the turn
            if !outcome.response.is_empty() {
                context.add_ai_from(outcome.response, Some("main".to_string()));
            }
        }
        // the engine's TurnFailed bubble (via the drain) renders the failure;
        // the file log keeps the full detail
        Err(err) => {
            tracing::warn!(error = %format!("{err:#}"), "failed to resume the interrupted turn");
        }
    }
    let _ = to_app.send(AppEvent::Redraw).await;
}

/// Generates a report over the session and writes it to a path
///
/// Resolves the template (the explicit argument, then the configured default,
/// then the builtin), generates the report with [`Session::generate_report`]
/// (which also saves the session store's `report.md`), and writes it to the
/// target path. Every failure is surfaced as a `⚠`-prefixed assistant bubble
/// rather than failing the chat loop.
///
/// # Arguments
///
/// * `session` - The ThorPal session the report covers
/// * `path` - Where to write the generated report
/// * `template_arg` - An explicit template path, outranking the configured default
/// * `settings` - The resolved report settings for this chat
/// * `context` - The shared context progress and errors are bubbled into
/// * `activity` - The shared run-activity flipped busy while generating
/// * `to_app` - The channel to request redraws over
async fn run_report(
    session: &Session,
    path: &Path,
    template_arg: Option<&PathBuf>,
    settings: &ReportSettings,
    context: &ChatContext,
    activity: &RunActivity,
    to_app: &AsyncSender<AppEvent>,
) {
    // tell the user the report is being written and show the throbber
    context.add_ai(format!("Writing report to {}…", path.display()));
    activity.set_busy(true);
    activity.set_activity("report", "writing report");
    let _ = to_app.send(AppEvent::Redraw).await;
    // resolve the template: the explicit argument outranks the configured default
    let template = match template_arg.or(settings.template.as_ref()) {
        // read the resolved template off disk, surfacing a failure as a bubble
        Some(template_path) => match tokio::fs::read_to_string(template_path).await {
            Ok(template) => Some(template),
            Err(err) => {
                context.add_ai(format!(
                    "⚠ failed to read report template '{}': {err}",
                    template_path.display()
                ));
                activity.set_busy(false);
                let _ = to_app.send(AppEvent::Redraw).await;
                return;
            }
        },
        // no template was resolved, so the builtin default is used
        None => None,
    };
    // generate the report, which also saves the session store's report.md
    let report = match session.generate_report(template.as_deref()).await {
        Ok(report) => report,
        Err(err) => {
            context.add_ai(format!("⚠ failed to generate report: {err:#}"));
            activity.set_busy(false);
            let _ = to_app.send(AppEvent::Redraw).await;
            return;
        }
    };
    // defang the written copy when asked; the store copy is already saved raw
    let report = if settings.defang {
        crate::handlers::ai::defang::defang(&report)
    } else {
        report
    };
    // write the report to the path the user chose
    match tokio::fs::write(path, &report).await {
        // tell the user where both copies of the report landed
        Ok(()) => context.add_ai(format!(
            "Report written to {} (also saved in the session store as report.md)",
            path.display()
        )),
        // the session store copy already landed, so say so alongside the failure
        Err(err) => context.add_ai(format!(
            "⚠ failed to write report to {}: {err} (the report was still saved \
             in the session store as report.md)",
            path.display()
        )),
    }
    // the report is done, so idle the throbber and redraw
    activity.set_busy(false);
    let _ = to_app.send(AppEvent::Redraw).await;
}

/// Runs one side-conversation turn concurrently with the main prompt loop
///
/// Mirrors a main turn's shape against the side's own handle: the user bubble
/// and busy flag land in the side's context and activity, the turn runs
/// through [`Session::side_chat`] (the side's own lock serializes turns within
/// one side), and the answer -- or a routed `⚠` bubble on failure -- lands back
/// in the side's context: a side turn's failure belongs in its own tab, not
/// the main one.
///
/// # Arguments
///
/// * `session` - The shared ThorPal session the side belongs to
/// * `name` - The side conversation to run the turn in
/// * `msg` - The user's message for this turn
/// * `handle` - The side's shared display handle (context plus activity)
/// * `to_app` - The channel to request redraws over
async fn run_side_turn(
    session: Arc<Session>,
    name: String,
    msg: String,
    handle: SideHandle,
    to_app: AsyncSender<AppEvent>,
) {
    // show the user's message in the side's own log and start its throbber
    handle.context.add_user(msg.clone());
    handle.activity.set_busy(true);
    let _ = to_app.send(AppEvent::Redraw).await;
    // run a single conversational turn against the side's forked history
    let result = session.side_chat(&name, msg).await;
    // the turn is done, so idle the side's throbber regardless of the outcome
    handle.activity.set_busy(false);
    match result {
        // add the turn's answer attributed to the side's root agent
        Ok(response) => {
            handle
                .context
                .add_ai_from(response, Some(format!("btw:{name}")));
        }
        // a failed side turn surfaces in the side's own tab
        Err(err) => {
            handle.context.add_ai(format!("⚠ {err:#}"));
        }
    }
    let _ = to_app.send(AppEvent::Redraw).await;
}

/// Generates (or revises) a /done summary for a side conversation
///
/// Runs concurrently with the main prompt loop so a slow summarizer never
/// blocks other turns. The outcome is routed back to the side's tab as an
/// [`AppEvent::BtwSummaryReady`] or [`AppEvent::BtwSummaryFailed`], which the
/// app delivers to that tab's summary modal.
///
/// # Arguments
///
/// * `session` - The shared ThorPal session the side belongs to
/// * `name` - The side conversation to summarize
/// * `prior` - The previously generated summary being revised, if any
/// * `feedback` - The analyst's feedback steering the revision, if any
/// * `handle` - The side's shared display handle (context plus activity)
/// * `to_app` - The channel the outcome and redraws are sent over
async fn run_side_summary(
    session: Arc<Session>,
    name: String,
    prior: Option<String>,
    feedback: Option<String>,
    handle: SideHandle,
    to_app: AsyncSender<AppEvent>,
) {
    // show the side's throbber while the summarizer works
    handle.activity.set_busy(true);
    handle
        .activity
        .set_activity(format!("btw:{name}"), "summarizing");
    let _ = to_app.send(AppEvent::Redraw).await;
    // run the one-shot summarizer over the side's history
    let result = session
        .summarize_side(&name, prior.as_deref(), feedback.as_deref())
        .await;
    // the summary is done, so idle the side's throbber
    handle.activity.set_busy(false);
    // route the outcome to the side's tab; the app may be shutting down, so a
    // failed send is ignored
    let event = match result {
        Ok(summary) => AppEvent::BtwSummaryReady { name, summary },
        Err(err) => AppEvent::BtwSummaryFailed {
            name,
            error: format!("{err:#}"),
        },
    };
    let _ = to_app.send(event).await;
    let _ = to_app.send(AppEvent::Redraw).await;
}

/// Enriches the traversal graph with call edges the agents never queried
///
/// Agents reveal edges only when they run `callers`/`callees`/xref tools, so a
/// decompile-only exploration would render as isolated nodes. This task
/// periodically drains the traversal log's queue of newly-visited functions
/// and asks Ghidra for each one's direct callers and callees, overlaying the
/// learned edges on the graph. Everything is best-effort: a session without a
/// usable Ghidra (e.g. a resume without an install) idles silently, a failed
/// query is skipped, and queries run one at a time so the agents' own tool
/// calls always come first.
///
/// # Arguments
///
/// * `ghidra` - The session's shared, lazily-opened Ghidra connection
/// * `traversal` - The traversal log to drain visits from and feed edges into
/// * `to_app` - The channel to request redraws over (its closure ends the task)
async fn enrich_edges(
    ghidra: Arc<thorpal::GhidraSession>,
    traversal: TraversalLog,
    to_app: AsyncSender<AppEvent>,
) {
    // a session without an install can never answer, so do not bother polling
    if !ghidra.has_install() {
        return;
    }
    // consecutive failures end the task: a persistently broken Ghidra (dead
    // bridge, failed bootstrap) would otherwise be re-polled every 2s forever
    const MAX_CONSECUTIVE_FAILURES: u32 = 10;
    let mut consecutive_failures = 0u32;
    loop {
        // debounce: let a burst of visits accumulate before querying
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        // pick up the functions visited since the last pass
        let fresh = traversal.drain_enrich_queue();
        let mut learned = false;
        for id in fresh {
            // ask Ghidra for this function's direct call-graph neighbors,
            // skipping it quietly on any failure (bogus target, Ghidra down)
            let Ok(edges) = ghidra.function_edges(&id.target_string()).await else {
                // count the failure and give up for good on a streak; this is
                // best-effort enrichment, invisible to the model, so quietly
                // stopping beats hammering a broken bridge for the whole run
                consecutive_failures += 1;
                if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                    tracing::warn!(
                        failures = consecutive_failures,
                        "edge enrichment stopping after consecutive ghidra failures"
                    );
                    return;
                }
                continue;
            };
            // an answer resets the streak; only unbroken failures stop the task
            consecutive_failures = 0;
            // orient the learned edges around the visited node
            let callers = edges
                .callers
                .iter()
                .map(|edge| (NodeId::addr(&edge.address), id.clone()));
            let callees = edges
                .callees
                .iter()
                .map(|edge| (id.clone(), NodeId::addr(&edge.address)));
            traversal.add_enriched(callers.chain(callees));
            learned = true;
        }
        // ask for a redraw only when something new landed; a closed channel
        // means the app is gone, so the task ends with it
        if learned && to_app.send(AppEvent::Redraw).await.is_err() {
            return;
        }
    }
}

/// The most live calls kept correlated at once; far above any real burst
const MAX_LIVE_CALLS: usize = 512;

/// The live tool calls still awaiting a result, correlated by ThorPal call_id
///
/// Maps a ThorPal call_id to the [`Uuid`] the message log merges a call+result
/// on, plus the context the call rendered into so its result follows it. Calls
/// whose results never arrive (an agent killed mid-call) are evicted
/// oldest-first once the map outgrows [`MAX_LIVE_CALLS`] instead of leaking.
#[derive(Default)]
struct LiveCalls {
    /// The live calls' render ids and contexts, keyed by ThorPal call_id
    calls: HashMap<String, (Uuid, ChatContext)>,
    /// The insertion order of live calls, so stale ones evict oldest-first
    order: VecDeque<String>,
}

impl LiveCalls {
    /// Registers a started call, evicting the oldest live calls past the cap
    ///
    /// # Arguments
    ///
    /// * `call_id` - The ThorPal call_id the call's result will arrive under
    /// * `entry` - The render id and context the call's result merges into
    fn insert(&mut self, call_id: String, entry: (Uuid, ChatContext)) {
        // remember the insertion order so eviction can drop oldest-first
        self.order.push_back(call_id.clone());
        self.calls.insert(call_id, entry);
        // evict the oldest live calls once the map outgrows the cap;
        // already-finished ids in the order queue are free to pop
        while self.calls.len() > MAX_LIVE_CALLS {
            match self.order.pop_front() {
                Some(stale) => {
                    self.calls.remove(&stale);
                }
                None => break,
            }
        }
        // compact the order queue when finished-call tombstones pile up
        if self.order.len() > MAX_LIVE_CALLS * 4 {
            let calls = &self.calls;
            self.order.retain(|call| calls.contains_key(call));
        }
    }

    /// Seeds the map with a replay's still-open calls
    ///
    /// A resumed delegation answers under the dead run's call id, so seeding
    /// the replayed open calls lets that live result close the replayed
    /// bubble exactly as if the call had been made by this process.
    ///
    /// # Arguments
    ///
    /// * `carry` - The correlation state the transcript replay handed over
    fn seed(&mut self, carry: ReplayCarry) {
        // insert through the normal path so eviction bookkeeping stays uniform
        for (call_id, entry) in carry.open_calls {
            self.insert(call_id, entry);
        }
    }

    /// Looks a live call's render id and context up without finishing it
    ///
    /// # Arguments
    ///
    /// * `call_id` - The ThorPal call_id to look up
    fn get(&self, call_id: &str) -> Option<&(Uuid, ChatContext)> {
        self.calls.get(call_id)
    }

    /// Removes a finished call, handing back its render id and context
    ///
    /// The order queue keeps the finished id as a tombstone eviction pops for
    /// free; compaction clears any pile-up on the next insert.
    ///
    /// # Arguments
    ///
    /// * `call_id` - The ThorPal call_id that finished
    fn finish(&mut self, call_id: &str) -> Option<(Uuid, ChatContext)> {
        self.calls.remove(call_id)
    }
}

/// Folds a ThorPal run's events into the shared contexts as renderable bubbles
///
/// Correlates a tool call with its result by [`ThorPalEvent`]'s `call_id`, minting
/// a [`Uuid`] per call so the message log can merge the call and result into one
/// bubble. Every agent-attributed event is routed by its display name's scope:
/// main-conversation activity lands in the main context and activity bucket,
/// while a `btw:<name>`-scoped agent's activity lands in that side's own handle.
/// A conversation root's assistant text and final response are skipped here --
/// the prompt and side tasks already add the final answers, so re-adding them
/// would duplicate the bubble. Usage checkpoints feed the owning scope's token
/// readout plus the session-wide total. A forum change is turned into a fresh
/// [`thorpal::ForumView`] for the message board rather than a bubble.
///
/// # Arguments
///
/// * `events_rx` - The receiver the session emits its events on
/// * `context` - The main conversation's shared context
/// * `activity` - The main conversation's run-activity bucket
/// * `sides` - The shared registry scoped events are routed through
/// * `session_total` - The session-wide usage bucket every scope sums into
/// * `forum` - The session's forum, read for a fresh view on every change
/// * `to_app` - The channel to request a redraw after each event
/// * `tool_logs` - The shared log router streaming tool calls register on
/// * `traversal` - The traversal log the graph view renders
/// * `agent_stats` - The agent stats log the stats view renders
/// * `gantt` - The gantt log the timeline view renders
/// * `pause_banner` - The shared pause banner raised while the session pauses
/// * `carry` - A resumed session's replayed open calls, seeded so live
///   results can close the dead run's bubbles
/// Flatten a full error onto one compact line for a failure entry's summary
///
/// The full text stays behind the entry's click-to-expand detail; the summary
/// just has to name the fault without ballooning the chat log (provider error
/// bodies run to kilobytes).
///
/// # Arguments
///
/// * `error` - The full error text to summarize
fn failure_summary(error: &str) -> String {
    // flatten the error text onto one line and keep it short
    let flat = error.replace('\n', " ");
    if flat.chars().count() > 120 {
        let truncated: String = flat.chars().take(117).collect();
        format!("{truncated}...")
    } else {
        flat
    }
}

#[allow(clippy::too_many_arguments)]
async fn drain_events(
    mut events_rx: UnboundedReceiver<ThorPalEvent>,
    context: ChatContext,
    activity: RunActivity,
    sides: SideRegistry,
    session_total: RunActivity,
    forum: Arc<ForumState>,
    to_app: AsyncSender<AppEvent>,
    tool_logs: Arc<ToolLogRouter>,
    traversal: TraversalLog,
    agent_stats: AgentStatsLog,
    gantt: GanttLog,
    pause_banner: Arc<std::sync::Mutex<Option<PauseBanner>>>,
    carry: ReplayCarry,
) {
    // route an agent's bubbles to its scope's context (main or a side's)
    let ctx_for = |agent: &str| match scope_of_agent(agent) {
        Some(scope) => sides.handle(scope).context,
        None => context.clone(),
    };
    // route an agent's throbber and usage to its scope's activity bucket
    let act_for = |agent: &str| match scope_of_agent(agent) {
        Some(scope) => sides.handle(scope).activity,
        None => activity.clone(),
    };
    // the live tool calls awaiting results, correlated by ThorPal call_id;
    // a resumed session's replayed open calls seed the map so a resumed
    // delegation's result closes the dead run's bubble
    let mut live_calls = LiveCalls::default();
    live_calls.seed(carry);
    // process events until the session drops its sender
    while let Some(event) = events_rx.recv().await {
        // fold the event into the traversal graph before rendering consumes it
        if let Some(traversal_event) = TraversalEvent::from_event(&event) {
            traversal.push(traversal_event);
        }
        // fold the event into the per-agent stats the stats view renders (this
        // is also what keeps UsageUpdated's agent attribution, which the
        // per-scope readouts below sum away)
        if let Some(stats_event) = StatsEvent::from_event(&event) {
            agent_stats.push(stats_event);
        }
        // fold the event into the timeline the gantt view renders
        if let Some(gantt_event) = GanttEvent::from_event(&event) {
            gantt.push(gantt_event);
        }
        match event {
            // a conversation root's turn text and final answer are added by the
            // prompt/side tasks from the turn's return value, so skip them here
            // to avoid dupes; a sub-agent's text is shown as progress in its
            // scope's log, tagged with its name
            ThorPalEvent::AgentMessage { agent, text } => {
                if agent != "main" && !is_side_root(&agent) {
                    // attribute the sub-agent's text to it so the UI can label who
                    // spoke rather than folding it into an anonymous prefix
                    ctx_for(&agent).add_ai_from(text, Some(agent));
                }
            }
            // announce a sub-agent delegation as an informational breadcrumb and
            // surface its goal in its scope's throbber so the user sees what it
            // is doing
            ThorPalEvent::SubAgentStarted {
                name,
                goal,
                resumed,
                ..
            } => {
                act_for(&name).set_activity(name.clone(), goal.clone());
                // attribute the breadcrumb to the sub-agent being started; a
                // resumed invocation is marked so the operator can tell its
                // earlier work continues rather than restarting
                let glyph = if resumed { "↻" } else { "▶" };
                let verb = if resumed { " resumed" } else { "" };
                ctx_for(&name)
                    .add_ai_from(format!("{glyph} {name}{verb} — goal: {goal}"), Some(name));
            }
            // announce a sub-agent's self-granted turn extension with its
            // justification, attributed to the agent that asked for it
            ThorPalEvent::SubAgentExtended {
                name,
                granted,
                remaining_allowance,
                justification,
                trigger,
            } => {
                ctx_for(&name).add_ai_from(
                    format!(
                        "⏱ {name} {} ({remaining_allowance} left): {justification}",
                        extension_phrase(trigger, granted),
                    ),
                    Some(name),
                );
            }
            // surface a sub-agent's context pressure as a warning bubble
            ThorPalEvent::SubAgentContextPressure {
                name,
                level,
                used_tokens,
                context_window,
            } => {
                ctx_for(&name).add_ai_from(
                    format!(
                        "⚠ {name} context {}: ~{used_tokens} of {context_window} tokens",
                        context_level_label(level),
                    ),
                    Some(name),
                );
            }
            // surface a sub-agent death with its class and salvage post
            ThorPalEvent::SubAgentTerminated {
                name,
                reason,
                salvage_post,
                ..
            } => {
                let posted = match &salvage_post {
                    Some(post) => format!(" (handoff posted as #{post})"),
                    None => String::new(),
                };
                ctx_for(&name).add_ai_from(
                    format!("✖ {name} terminated early — {reason}{posted}"),
                    Some(name),
                );
            }
            // announce a swarm of concurrent sub-agents as an informational
            // bubble; a resumed swarm continues its interrupted members' work
            ThorPalEvent::SwarmStarted {
                name,
                goal,
                members,
                resumed,
            } => {
                let (glyph, verb) = if resumed {
                    ("↻", "resumed")
                } else {
                    ("▶", "started")
                };
                ctx_for(&name).add_ai(format!(
                    "{glyph} {name} {verb} {} agents ({}): {goal}",
                    members.len(),
                    members.join(", "),
                ));
            }
            // report how a swarm's members fared once they have all finished
            ThorPalEvent::SwarmFinished {
                name,
                succeeded,
                failed,
            } => {
                ctx_for(&name).add_ai(format!(
                    "■ {name} finished: {succeeded} succeeded, {failed} failed"
                ));
            }
            // a tool call is starting: mint a Uuid, remember it (and its scope's
            // context) against the ThorPal call_id, add a tool-call bubble to
            // render, and note the activity so the throbber shows the tool
            ThorPalEvent::ToolCallStarted {
                agent,
                call_id,
                tool,
                args,
            } => {
                act_for(&agent).set_activity(agent.clone(), format!("calling {tool}"));
                let id = Uuid::new_v4();
                // register this call for live log routing (a no-op for tools
                // that do not stream) before the call_id moves into the map
                tool_logs.register(&call_id, &tool);
                let call_context = ctx_for(&agent);
                live_calls.insert(call_id, (id, call_context.clone()));
                // attribute the tool call to the agent that made it
                call_context.add_tool_call(id, Some(agent), tool, parse_tool_args(&args));
            }
            // a tool call finished: look up its Uuid and add the result to the
            // matching entry in the same context the call rendered into; an
            // unmatched result is skipped since it has no call entry to merge into
            ThorPalEvent::ToolCallFinished {
                call_id,
                result,
                success,
                ..
            } => {
                // stop routing log lines to this call now that it is done
                tool_logs.unregister(&call_id);
                if let Some((id, call_context)) = live_calls.finish(&call_id) {
                    // carry the observed success status so the UI can flag failures
                    call_context.add_tool_result(id, result, Some(success));
                }
            }
            // a running streaming tool produced a log line: append it to the
            // call's entry; a line for an already-finished call is dropped
            ThorPalEvent::ToolLog { call_id, line } => {
                if let Some((id, call_context)) = live_calls.get(&call_id) {
                    call_context.add_tool_log(*id, line);
                }
            }
            // fold this turn's token usage into its scope's snapshot (so each
            // tab's status bar shows its own totals) plus the session-wide total
            ThorPalEvent::UsageUpdated {
                agent,
                input,
                output,
                cached,
                cache_creation,
                total,
            } => {
                act_for(&agent).record_usage(input, output, cached, cache_creation, total);
                session_total.record_usage(input, output, cached, cache_creation, total);
            }
            // the user's message is added by the prompt/side task when it is
            // sent, so skip it here (it exists on the stream for the transcript)
            ThorPalEvent::UserMessage { .. } => {}
            // the final response is added by the prompt/side task, so skip it here
            ThorPalEvent::RunFinished { .. } => {}
            // a turn failed after exhausting its retries: surface it inline as
            // a compact, click-to-expand error attributed to the agent, so the
            // run reports the failure without flooding the log -- the provider
            // request id and the full error text sit behind the click
            ThorPalEvent::TurnFailed {
                agent,
                error,
                request_id,
            } => {
                // keep the full text behind the click only when it says more
                // than the compact summary already shows
                let summary = failure_summary(&error);
                let detail = (summary != error).then_some(error);
                ctx_for(&agent).add_failure(
                    format!("{agent}: {summary}"),
                    Some(agent),
                    request_id,
                    detail,
                );
            }
            // the forum changed: hand the message board a fresh view of it
            ThorPalEvent::ForumUpdated { target } => {
                let view = forum.view().await;
                // the app may be shutting down, so a failed send is ignored
                let _ = to_app.send(AppEvent::ForumUpdated { view, target }).await;
            }
            // a completion request hit a transient fault and is backing off:
            // update the episode's live throbber line in its scope's bucket
            // instead of adding a bubble, so retries never spam the chat log
            ThorPalEvent::LlmRetrying {
                agent,
                retry_id,
                attempt,
                max_attempts,
                error,
                // the one-row throbber strip has no room for a request id; a
                // retried fault's ids live in the thorctl-ai log, and a fatal
                // outcome surfaces its id on the TurnFailed entry
                request_id: _,
                last_attempt_at,
                next_attempt_at,
            } => {
                act_for(&agent).set_retry(
                    retry_id,
                    RetrySnapshot {
                        agent,
                        attempt,
                        max_attempts,
                        error,
                        last_attempt_at,
                        next_attempt_at,
                    },
                );
            }
            // the retry episode ended (recovered or surfaced as fatal): clear
            // its live line; a final failure still arrives through the normal
            // error paths (a TurnFailed bubble or a failed tool result)
            ThorPalEvent::LlmRetryResolved {
                agent, retry_id, ..
            } => {
                act_for(&agent).clear_retry(&retry_id);
            }
            // a content filter killed a request and a scrubbed retry fired:
            // one compact chat-log line per escalation keeps the recovery
            // visible, with the killed request's id behind a click
            ThorPalEvent::LlmScrubbed {
                agent,
                level,
                error,
                request_id,
                ..
            } => {
                context.add_failure(
                    format!(
                        "{agent}: content filter blocked a request ({error}); \
                         retrying at scrub level {level}"
                    ),
                    Some(agent),
                    request_id,
                    None,
                );
            }
            // report generation stepped down its content-filter ladder: one
            // chat-log line per degradation keeps the loss visible
            ThorPalEvent::ReportDegraded { rung, detail } => {
                context.add_ai(format!("⚠ report degraded to rung {rung}: {detail}"));
            }
            // the whole session paused on a provider fault: raise (or refresh)
            // the banner, adding a log line only when the pause first lands so
            // repeated probe updates never spam the chat
            ThorPalEvent::SessionPaused {
                kind,
                agent,
                error,
                request_id,
                probe_attempts,
                next_probe_at,
                ..
            } => {
                let mut banner = pause_banner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if banner.is_none() {
                    context.add_failure(
                        format!(
                            "session paused — {kind}: {error} (first hit by {agent}). \
                             Probing automatically; /retry probes now, /abort fails the \
                             parked requests instead."
                        ),
                        Some(agent),
                        request_id,
                        None,
                    );
                }
                *banner = Some(PauseBanner {
                    kind: kind.to_string(),
                    error,
                    probe_attempts,
                    next_probe_at,
                });
            }
            // the pause ended: clear the banner and say how it ended (the
            // duration and probe tally ride the event for the transcript, not
            // the chat log)
            ThorPalEvent::SessionResumed {
                kind, recovered, ..
            } => {
                *pause_banner
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                if recovered {
                    context.add_ai(format!("▶ session resumed — {kind} recovered"));
                } else {
                    context.add_ai(
                        "⚠ provider pause aborted — the parked requests surface \
                         their errors"
                            .to_owned(),
                    );
                }
            }
            // the main agent asked the analyst a question: explain the stall
            // in its scope's throbber and hand the pending ask to the app so
            // the chat page can open its question modal
            ThorPalEvent::AnalystAskPending {
                id,
                agent,
                question,
                options,
                recommended,
            } => {
                act_for(&agent).set_activity(
                    agent.clone(),
                    "waiting for the analyst — answer the question modal".to_owned(),
                );
                let _ = to_app
                    .send(AppEvent::AnalystAsk {
                        ask: PendingAsk {
                            id,
                            agent,
                            question,
                            options,
                            recommended,
                        },
                    })
                    .await;
            }
            // an ask resolved: show the taken fork inline, close a matching
            // still-open modal (covers a detach race; the normal path already
            // closed it on answer), and refresh the board without a jump
            ThorPalEvent::AnalystAskResolved {
                id,
                agent,
                question,
                chosen,
                source,
                ..
            } => {
                // replace the waiting line so the status no longer points at
                // a modal that just closed
                act_for(&agent)
                    .set_activity(agent.clone(), "continuing with the answer".to_owned());
                ctx_for(&agent).add_ai_from(
                    format!("❓ {question} → {chosen:?} ({source})"),
                    Some(agent),
                );
                let _ = to_app.send(AppEvent::AnalystAskResolved { id }).await;
                let view = forum.view().await;
                let _ = to_app.send(AppEvent::ForumRefreshed { view }).await;
            }
            // an agent flagged something for review: one visible line in its
            // scope plus a board refresh so the queue entry appears
            ThorPalEvent::ReviewFlagRaised { agent, title, .. } => {
                ctx_for(&agent).add_ai_from(
                    format!("⚑ flagged for analyst review: {title}"),
                    Some(agent),
                );
                let view = forum.view().await;
                let _ = to_app.send(AppEvent::ForumRefreshed { view }).await;
            }
        }
        // redraw after every event so the TUI reflects the run's progress live
        let _ = to_app.send(AppEvent::Redraw).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::ai::chat::ChatEntry;

    /// Wraps items as timestamp-less transcript lines for the replay tests
    fn as_lines(items: Vec<TranscriptItem>) -> Vec<TranscriptLine> {
        items
            .into_iter()
            .map(|item| TranscriptLine { ts: None, item })
            .collect()
    }

    /// A replayed transcript renders the same entries, in order, as the live drain
    #[test]
    fn replay_transcript_matches_live_rendering() {
        // build a context and replay a small interleaved transcript into it
        let context = ChatContext::default();
        let transcript = as_lines(vec![
            TranscriptItem::User {
                text: "analyze this".to_owned(),
                scope: None,
            },
            TranscriptItem::SubAgentStarted {
                name: "ReverseEngineer(Europa)".to_owned(),
                role: Some("ReverseEngineer".to_owned()),
                goal: "triage".to_owned(),
                swarm: None,
                resumed: false,
            },
            TranscriptItem::ToolCall {
                agent: "ReverseEngineer(Europa)".to_owned(),
                call_id: "call-1".to_owned(),
                tool: "memory_map".to_owned(),
                args: "{}".to_owned(),
            },
            TranscriptItem::ToolResult {
                agent: "ReverseEngineer(Europa)".to_owned(),
                call_id: "call-1".to_owned(),
                tool: "memory_map".to_owned(),
                result: "ok".to_owned(),
                success: true,
            },
            TranscriptItem::SubAgentExtended {
                name: "ReverseEngineer(Europa)".to_owned(),
                granted: 2,
                remaining_allowance: 1,
                justification: "reserved early".to_owned(),
                trigger: thorpal::ExtensionTrigger::Reserved,
            },
            TranscriptItem::ContextPressure {
                name: "ReverseEngineer(Europa)".to_owned(),
                level: 2,
                used_tokens: 185_000,
                context_window: 200_000,
            },
            TranscriptItem::SubAgentTerminated {
                name: "ReverseEngineer(Europa)".to_owned(),
                reason: "turn budget exhausted (10 model calls used)".to_owned(),
                class: "max_turns".to_owned(),
                salvage_post: Some("salvage-reverseengineer-europa".to_owned()),
            },
            TranscriptItem::Assistant {
                text: "done".to_owned(),
                scope: None,
            },
        ]);
        // the replay also seeds the traversal, stats, and gantt logs alongside
        // the chat
        let traversal = TraversalLog::default();
        let agent_stats = AgentStatsLog::default();
        let gantt = GanttLog::default();
        let sides = SideRegistry::default();
        replay_transcript(
            transcript,
            &context,
            &sides,
            &traversal,
            &agent_stats,
            &gantt,
            &BTreeSet::new(),
        );
        // the agent breadcrumb plus the call and its result were folded in
        assert_eq!(traversal.len(), 3);
        // the stats log also saw the user message, the assistant answer, and
        // the replay-ended marker
        assert_eq!(agent_stats.len(), 6);
        // the gantt log saw everything but the user message, plus the marker
        assert_eq!(gantt.len(), 5);
        // the sub-agent's span kept its goal and closed tool interval
        let timeline = gantt.snapshot();
        let span = timeline
            .agents
            .get("ReverseEngineer(Europa)")
            .expect("sub-agent span");
        assert_eq!(span.goal.as_deref(), Some("triage"));
        assert_eq!(span.calls.len(), 1);
        assert_eq!(timeline.calls[0].success, Some(true));
        // the sub-agent's row tallied its call, result, and swarm-less start
        let stats = agent_stats.snapshot();
        let sub = stats
            .agents
            .get("ReverseEngineer(Europa)")
            .expect("sub-agent row");
        assert_eq!(sub.tool_calls, 1);
        assert_eq!(sub.tool_ok, 1);
        assert_eq!(sub.role.as_deref(), Some("ReverseEngineer"));
        assert_eq!(sub.swarm, None);
        // the entries come back in original order with the same shapes the
        // live drain produces: user bubble, breadcrumb, call+result, the
        // budget/context/death breadcrumbs, answer
        let entries = context.access();
        assert_eq!(entries.len(), 8);
        assert!(matches!(&entries[0], ChatEntry::User(text) if text == "analyze this"));
        assert!(
            matches!(&entries[1], ChatEntry::Ai { agent: Some(agent), text }
                if agent == "ReverseEngineer(Europa)" && text.contains("goal: triage"))
        );
        // the call and its result merge on the same freshly minted Uuid
        let call_id = match &entries[2] {
            ChatEntry::ToolCall { id, name, .. } if name == "memory_map" => *id,
            other => panic!("expected a tool call entry, got {other:?}"),
        };
        assert!(
            matches!(&entries[3], ChatEntry::ToolResult { id, success: Some(true), .. }
                if *id == call_id)
        );
        // the reservation renders with its trigger phrasing, the context
        // pressure with its level, and the death with its salvage post
        assert!(matches!(&entries[4], ChatEntry::Ai { text, .. }
                if text.contains("reserved +2 turn(s)") && text.contains("reserved early")));
        assert!(matches!(&entries[5], ChatEntry::Ai { text, .. }
                if text.contains("context CRITICAL") && text.contains("185000")));
        assert!(matches!(&entries[6], ChatEntry::Ai { text, .. }
                if text.contains("terminated early")
                    && text.contains("#salvage-reverseengineer-europa")));
        assert!(
            matches!(&entries[7], ChatEntry::Ai { agent: Some(agent), text }
                if agent == "main" && text == "done")
        );
    }

    /// Provider-stall lines replay the way live rendering showed them: retry
    /// attempts and episode closes stay out of the chat log, a pause's first
    /// landing comes back as its click-to-expand failure entry, and the
    /// pause's end comes back as its one-line outcome
    #[test]
    fn replay_renders_stall_lines_like_live() {
        // replay a transcript holding the whole stall lifecycle
        let context = ChatContext::default();
        replay_transcript(
            as_lines(vec![
                TranscriptItem::User {
                    text: "analyze this".to_owned(),
                    scope: None,
                },
                TranscriptItem::LlmRetry {
                    agent: "main".to_owned(),
                    retry_id: "main#1".to_owned(),
                    attempt: 1,
                    max_attempts: 12,
                    error: "429 Too Many Requests".to_owned(),
                    request_id: Some("req-13bb".to_owned()),
                    backoff_ms: 2_000,
                },
                TranscriptItem::LlmRetryResolved {
                    agent: "main".to_owned(),
                    retry_id: "main#1".to_owned(),
                    attempts: 2,
                    succeeded: true,
                },
                TranscriptItem::ProviderPaused {
                    pause: thorpal::PauseKind::Auth,
                    agent: "main".to_owned(),
                    error: "401 Unauthorized".to_owned(),
                    request_id: Some("req-90dd".to_owned()),
                },
                TranscriptItem::ProviderResumed {
                    pause: thorpal::PauseKind::Auth,
                    recovered: true,
                    paused_ms: 60_000,
                    probe_attempts: 1,
                },
                TranscriptItem::TurnFailed {
                    agent: "main".to_owned(),
                    error: "boom".to_owned(),
                    request_id: Some("req-77aa".to_owned()),
                },
            ]),
            &context,
            &SideRegistry::default(),
            &TraversalLog::default(),
            &AgentStatsLog::default(),
            &GanttLog::default(),
            &BTreeSet::new(),
        );
        // the retry lines added nothing; the pause, its end, and the turn
        // failure rendered exactly like their live counterparts
        let entries = context.access();
        assert_eq!(entries.len(), 4);
        assert!(matches!(&entries[0], ChatEntry::User(text) if text == "analyze this"));
        assert!(
            matches!(&entries[1], ChatEntry::Failure { summary, request_id: Some(id), .. }
                if summary.contains("session paused — provider auth failed: 401 Unauthorized")
                    && id == "req-90dd")
        );
        assert!(matches!(&entries[2], ChatEntry::Ai { text, .. }
                if text.contains("session resumed — provider auth failed recovered")));
        assert!(
            matches!(&entries[3], ChatEntry::Failure { summary, request_id: Some(id), .. }
                if id == "req-77aa" && summary.starts_with("main: "))
        );
    }

    /// An unmatched tool result (torn call line) is skipped rather than panicking
    #[test]
    fn replay_transcript_skips_unmatched_results() {
        // replay a result whose call line was lost to a crash
        let context = ChatContext::default();
        replay_transcript(
            as_lines(vec![TranscriptItem::ToolResult {
                agent: "main".to_owned(),
                call_id: "orphan".to_owned(),
                tool: "list_funcs".to_owned(),
                result: "ok".to_owned(),
                success: true,
            }]),
            &context,
            &SideRegistry::default(),
            &TraversalLog::default(),
            &AgentStatsLog::default(),
            &GanttLog::default(),
            &BTreeSet::new(),
        );
        // nothing renders since there is no call entry to merge into
        assert!(context.access().is_empty());
    }

    /// Scoped lines replay into their side's context, not the main one
    #[test]
    fn replay_transcript_routes_scoped_lines() {
        // replay a side turn interleaved with a main-conversation line
        let context = ChatContext::default();
        let sides = SideRegistry::default();
        replay_transcript(
            as_lines(vec![
                TranscriptItem::User {
                    text: "main question".to_owned(),
                    scope: None,
                },
                TranscriptItem::User {
                    text: "side question".to_owned(),
                    scope: Some("crypto".to_owned()),
                },
                TranscriptItem::ToolCall {
                    agent: "btw:crypto/ReverseEngineer(Europa)".to_owned(),
                    call_id: "call-1".to_owned(),
                    tool: "memory_map".to_owned(),
                    args: "{}".to_owned(),
                },
                TranscriptItem::ToolResult {
                    agent: "btw:crypto/ReverseEngineer(Europa)".to_owned(),
                    call_id: "call-1".to_owned(),
                    tool: "memory_map".to_owned(),
                    result: "ok".to_owned(),
                    success: true,
                },
                TranscriptItem::Assistant {
                    text: "side answer".to_owned(),
                    scope: Some("crypto".to_owned()),
                },
            ]),
            &context,
            &sides,
            &TraversalLog::default(),
            &AgentStatsLog::default(),
            &GanttLog::default(),
            &BTreeSet::new(),
        );
        // only the main-conversation line landed in the main context
        let entries = context.access();
        assert_eq!(entries.len(), 1);
        assert!(matches!(&entries[0], ChatEntry::User(text) if text == "main question"));
        drop(entries);
        // the side's question, tool call+result, and answer landed in its log
        let side = sides.handle("crypto");
        let entries = side.context.access();
        assert_eq!(entries.len(), 4);
        assert!(matches!(&entries[0], ChatEntry::User(text) if text == "side question"));
        assert!(
            matches!(&entries[3], ChatEntry::Ai { agent: Some(agent), text }
                if agent == "btw:crypto" && text == "side answer")
        );
    }

    /// A panicking worker flips the death note and drops a resume-hint bubble
    #[tokio::test]
    async fn supervisor_renders_a_worker_panic() {
        // stage the shared pieces the supervisor writes into
        let context = ChatContext::default();
        let dead = Arc::new(std::sync::OnceLock::new());
        let (to_app, _from_app) = kanal::unbounded_async();
        // a worker that dies mid-session only ever does so by panicking
        let worker = tokio::task::spawn(async { panic!("turn exploded") });
        let result = supervise_worker(
            worker,
            Arc::clone(&dead),
            context.clone(),
            "corn-sess".to_owned(),
            to_app,
        )
        .await;
        // the death surfaces as an error for the quit path's awaiter
        assert!(result.is_err());
        // the write-once note carries the panic's own message
        assert_eq!(dead.get().map(String::as_str), Some("turn exploded"));
        // the main log got one loud bubble with the resume hint
        let entries = context.access();
        assert!(
            entries
                .iter()
                .any(|entry| matches!(entry, ChatEntry::Ai { text, .. }
            if text.contains("turn exploded") && text.contains("--resume corn-sess")))
        );
    }

    /// A cleanly returning worker passes its result through untouched
    #[tokio::test]
    async fn supervisor_passes_a_clean_return_through() {
        // stage a worker that returns the quit path's clean result
        let context = ChatContext::default();
        let dead = Arc::new(std::sync::OnceLock::new());
        let (to_app, _from_app) = kanal::unbounded_async();
        let worker = tokio::task::spawn(async { Ok(()) });
        let result = supervise_worker(
            worker,
            Arc::clone(&dead),
            context.clone(),
            "corn-sess".to_owned(),
            to_app,
        )
        .await;
        // nothing died: no error, no death note, no bubble
        assert!(result.is_ok());
        assert!(dead.get().is_none());
        assert!(context.access().is_empty());
    }

    /// Live calls cap the map, evict oldest-first, and compact tombstones
    #[test]
    fn live_calls_evict_finish_and_compact() {
        let context = ChatContext::default();
        // fill one past the cap: the oldest unfinished call is evicted
        let mut live = LiveCalls::default();
        for i in 0..=MAX_LIVE_CALLS {
            live.insert(format!("call-{i}"), (Uuid::new_v4(), context.clone()));
        }
        assert_eq!(live.calls.len(), MAX_LIVE_CALLS);
        assert_eq!(live.order.len(), MAX_LIVE_CALLS);
        assert!(live.get("call-0").is_none());
        assert!(live.get("call-1").is_some());
        // finishing a call removes it exactly once
        assert!(live.finish("call-1").is_some());
        assert!(live.get("call-1").is_none());
        assert!(live.finish("call-1").is_none());
        // churn insert+finish cycles until the tombstones trip compaction
        let mut live = LiveCalls::default();
        for i in 0..=MAX_LIVE_CALLS * 4 {
            live.insert(format!("churn-{i}"), (Uuid::new_v4(), context.clone()));
            assert!(live.finish(&format!("churn-{i}")).is_some());
        }
        // the compacting insert dropped every tombstone but its own, which the
        // trailing finish then left behind as the queue's only entry
        assert!(live.calls.is_empty());
        assert_eq!(live.order.len(), 1);
    }

    /// Builds a stamped usage checkpoint line for an agent
    fn usage_line(agent: &str, input: u64, output: u64, ts: u64) -> TranscriptLine {
        TranscriptLine {
            ts: Some(ts),
            item: TranscriptItem::Usage {
                agent: agent.to_owned(),
                input,
                output,
                cached: input * 2,
                cache_creation: 10,
                total: input * 3 + output + 10,
            },
        }
    }

    /// Usage lines sum per agent and every other line kind is ignored
    #[test]
    fn replayed_usage_sums_per_agent() {
        // two turns for main, one for a sub-agent, plus unrelated lines
        let transcript = vec![
            TranscriptLine {
                ts: Some(5),
                item: TranscriptItem::User {
                    text: "analyze this".to_owned(),
                    scope: None,
                },
            },
            usage_line("main", 100, 10, 10),
            usage_line("ReverseEngineer(Europa)", 50, 5, 20),
            usage_line("main", 200, 20, 30),
        ];
        let summed = replayed_usage(&transcript);
        // main's two turns folded into one accumulated entry
        let main = summed.get("main").expect("main sums");
        assert_eq!(main.input_tokens, 300);
        assert_eq!(main.output_tokens, 30);
        assert_eq!(main.cached_input_tokens, 600);
        assert_eq!(main.cache_creation_input_tokens, 20);
        assert_eq!(main.total_tokens, 100 * 3 + 10 + 10 + 200 * 3 + 20 + 10);
        // the sub-agent kept its own bucket and nothing else appeared
        assert_eq!(summed.len(), 2);
        // a transcript with no usage lines sums to nothing at all
        assert!(replayed_usage(&transcript[..1]).is_empty());
    }

    /// The remainder covers pre-upgrade spend, drops exactly-replayed agents,
    /// and saturates when the transcript ran ahead of the ledger
    #[test]
    fn usage_remainder_handles_both_skews() {
        // the ledger knows three agents; the transcript replayed two of them
        let mut ledger: BTreeMap<String, UsageTotals> = BTreeMap::new();
        ledger.insert(
            "old-only".to_owned(),
            UsageTotals {
                input_tokens: 1_000,
                output_tokens: 100,
                reasoning_tokens: 7,
                ..Default::default()
            },
        );
        ledger.insert(
            "exact".to_owned(),
            UsageTotals {
                input_tokens: 500,
                output_tokens: 50,
                ..Default::default()
            },
        );
        ledger.insert(
            "mixed".to_owned(),
            UsageTotals {
                input_tokens: 800,
                output_tokens: 80,
                ..Default::default()
            },
        );
        let mut replayed: BTreeMap<String, UsageTotals> = BTreeMap::new();
        replayed.insert(
            "exact".to_owned(),
            UsageTotals {
                input_tokens: 500,
                output_tokens: 50,
                ..Default::default()
            },
        );
        replayed.insert(
            "mixed".to_owned(),
            UsageTotals {
                input_tokens: 300,
                output_tokens: 30,
                ..Default::default()
            },
        );
        // a crash left this agent's lines ahead of the lagging snapshot
        replayed.insert(
            "ahead".to_owned(),
            UsageTotals {
                input_tokens: 999,
                ..Default::default()
            },
        );
        let remainder = usage_remainder(&ledger, &replayed);
        // the never-replayed agent keeps its full ledger entry (reasoning too)
        let old = remainder.get("old-only").expect("old-only remainder");
        assert_eq!(old.input_tokens, 1_000);
        assert_eq!(old.reasoning_tokens, 7);
        // the exactly-replayed agent dropped out so its turn count stays exact
        assert!(!remainder.contains_key("exact"));
        // a leftover reasoning sub-breakdown alone never blocks the drop:
        // usage lines cannot carry it, so it would mislabel exact sessions
        let mut reasoning_ledger = BTreeMap::new();
        reasoning_ledger.insert(
            "exact".to_owned(),
            UsageTotals {
                input_tokens: 500,
                output_tokens: 50,
                reasoning_tokens: 42,
                ..Default::default()
            },
        );
        assert!(usage_remainder(&reasoning_ledger, &replayed).is_empty());
        // the partially-replayed agent keeps only its pre-upgrade spend
        let mixed = remainder.get("mixed").expect("mixed remainder");
        assert_eq!(mixed.input_tokens, 500);
        assert_eq!(mixed.output_tokens, 50);
        // the transcript-ahead agent is not in the ledger, so nothing seeds it
        assert!(!remainder.contains_key("ahead"));
        assert_eq!(remainder.len(), 2);
    }

    /// Builds a transcript line for one item, matching the live stamp shape
    ///
    /// # Arguments
    ///
    /// * `item` - The item to wrap
    fn line(item: TranscriptItem) -> TranscriptLine {
        TranscriptLine { ts: None, item }
    }

    /// The failure debris a resume superseded is hidden; live failures stay
    #[test]
    fn replay_hides_resume_superseded_debris() {
        // a session that failed terminally on a 401, was resumed, and
        // continued: the death's debris precedes the barrier
        let transcript = vec![
            line(TranscriptItem::User {
                text: "analyze this".to_owned(),
                scope: None,
            }),
            // a mid-run failure the model recovered from stays visible
            line(TranscriptItem::TurnFailed {
                agent: "main".to_owned(),
                error: "mid-run budget death".to_owned(),
                request_id: None,
            }),
            line(TranscriptItem::Assistant {
                text: "recovered and continued".to_owned(),
                scope: None,
            }),
            // the delegation whose process died mid-flight
            line(TranscriptItem::SubAgentStarted {
                name: "ReverseEngineer(Io)".to_owned(),
                role: Some("ReverseEngineer".to_owned()),
                goal: "triage".to_owned(),
                swarm: None,
                resumed: false,
            }),
            // the terminal death tail: the agent died, its delegation call
            // failed, and the main turn failed -- all after the last activity
            line(TranscriptItem::SubAgentTerminated {
                name: "ReverseEngineer(Io)".to_owned(),
                reason: "aborted".to_owned(),
                class: "other".to_owned(),
                salvage_post: None,
            }),
            line(TranscriptItem::ToolResult {
                agent: "main".to_owned(),
                call_id: "ic-1".to_owned(),
                tool: "reverse_engineer".to_owned(),
                result: "sub-agent terminated early".to_owned(),
                success: false,
            }),
            line(TranscriptItem::TurnFailed {
                agent: "main".to_owned(),
                error: "401 Unauthorized".to_owned(),
                request_id: None,
            }),
            // the resume barrier a later process appended
            line(TranscriptItem::SessionResumed { interrupted: true }),
            // the later process resumed the agent and re-answered the call
            line(TranscriptItem::SubAgentStarted {
                name: "ReverseEngineer(Io)".to_owned(),
                role: Some("ReverseEngineer".to_owned()),
                goal: "triage".to_owned(),
                swarm: None,
                resumed: true,
            }),
            line(TranscriptItem::ToolResult {
                agent: "main".to_owned(),
                call_id: "ic-1".to_owned(),
                tool: "reverse_engineer".to_owned(),
                result: "the real answer".to_owned(),
                success: true,
            }),
        ];
        let delegations = BTreeSet::from(["reverse_engineer".to_owned()]);
        let hidden = replay_suppressions(&transcript, &delegations);
        // the terminal tail is hidden: the terminated agent (resumed later),
        // the failed delegation result (re-answered later), the fatal error
        assert!(hidden.contains(&4), "the superseded termination must hide");
        assert!(
            hidden.contains(&5),
            "the re-answered failed result must hide"
        );
        assert!(hidden.contains(&6), "the terminal turn failure must hide");
        // the mid-run failure the model recovered from stays visible
        assert!(!hidden.contains(&1), "mid-run failures must stay visible");
        assert_eq!(hidden.len(), 3);

        // the rendered log shows no ⚠ 401 bubble and no ✖ termination
        let context = ChatContext::default();
        let _ = replay_transcript(
            transcript,
            &context,
            &SideRegistry::default(),
            &TraversalLog::default(),
            &AgentStatsLog::default(),
            &GanttLog::default(),
            &delegations,
        );
        let entries = context.access();
        let rendered: Vec<String> = entries.iter().map(|entry| format!("{entry:?}")).collect();
        drop(entries);
        let joined = rendered.join("\n");
        assert!(
            !joined.contains("401 Unauthorized"),
            "the fatal error must not render"
        );
        assert!(
            !joined.contains("terminated early"),
            "the superseded death must not render"
        );
        assert!(
            joined.contains("mid-run budget death"),
            "mid-run failures must render"
        );
    }

    /// A transcript with no barrier hides nothing
    #[test]
    fn replay_without_barrier_hides_nothing() {
        // a session that failed terminally and was never resumed keeps its
        // error visible -- there is nothing superseding it
        let transcript = vec![
            line(TranscriptItem::User {
                text: "analyze this".to_owned(),
                scope: None,
            }),
            line(TranscriptItem::TurnFailed {
                agent: "main".to_owned(),
                error: "401 Unauthorized".to_owned(),
                request_id: None,
            }),
        ];
        let hidden = replay_suppressions(&transcript, &BTreeSet::new());
        assert!(hidden.is_empty());
    }

    /// The replay hands its open calls to the live drain, which closes them
    #[test]
    fn replay_carry_closes_open_calls_through_live_map() {
        // a transcript whose final tool call never got its result
        let context = ChatContext::default();
        let carry = replay_transcript(
            as_lines(vec![TranscriptItem::ToolCall {
                agent: "main".to_owned(),
                call_id: "ic-7".to_owned(),
                tool: "reverse_engineer".to_owned(),
                args: r#"{"goal":"g","prompt":"p"}"#.to_owned(),
            }]),
            &context,
            &SideRegistry::default(),
            &TraversalLog::default(),
            &AgentStatsLog::default(),
            &GanttLog::default(),
            &BTreeSet::new(),
        );
        // the dead run's open call rides the carry into the live map
        assert_eq!(carry.open_calls.len(), 1);
        let mut live = LiveCalls::default();
        live.seed(carry);
        // the resumed process's result under the same id closes the bubble
        let (_, call_context) = live.finish("ic-7").expect("the seeded call must close");
        call_context.add_tool_result(
            live.get("ic-7")
                .map(|(id, _)| *id)
                .unwrap_or_else(Uuid::new_v4),
            "the real answer".to_owned(),
            Some(true),
        );
        // a second result for the same id finds nothing (already closed)
        assert!(live.finish("ic-7").is_none());
    }
}
