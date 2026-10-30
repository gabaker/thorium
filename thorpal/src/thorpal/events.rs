//! Optional, opt-in observability events an external consumer can subscribe to.
//!
//! ThorPal emits nothing unless a caller installs a sender via
//! [`crate::ThorPalBuilder::events`] (or [`crate::SessionConfig`]'s `events`
//! field); the standalone binary installs none, so its behavior is unchanged.
//! The sink is a plain `tokio` channel, so subscribing costs ThorPal no
//! dependency on any consumer -- a TUI such as `thorctl`'s owns the receiver end
//! and ThorPal only ever holds the sender.
//!
//! Events are emitted from the shared agent hook (see
//! [`crate::thorpal::agents::tool`]) for both the main agent and every
//! sub-agent, so a consumer sees the whole run's tool activity and progress.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

use super::tools::forum::{AskOption, ParentRef};

/// What caused a turn-budget extension to be granted or recorded.
///
/// Carried on [`ThorPalEvent::SubAgentExtended`] (and its transcript mirror)
/// so an audit recount can split in-window grants, standing reservations,
/// reservation conversions, and the one-shot automatic backstop mechanically
/// instead of from log prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionTrigger {
    /// The agent asked inside the wrap-up window and was granted directly.
    #[default]
    Requested,
    /// The agent asked too early; the request was recorded as a standing
    /// reservation that converts automatically at the cap.
    Reserved,
    /// A standing reservation converted into a real grant at the cap.
    ReservationApplied,
    /// The one-shot automatic extension fired at the cap with no request made.
    Auto,
}

impl std::fmt::Display for ExtensionTrigger {
    /// Renders the trigger as its snake_case wire spelling.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // match the serde spelling so logs and transcripts read identically
        let label = match self {
            ExtensionTrigger::Requested => "requested",
            ExtensionTrigger::Reserved => "reserved",
            ExtensionTrigger::ReservationApplied => "reservation_applied",
            ExtensionTrigger::Auto => "auto",
        };
        write!(f, "{label}")
    }
}

/// The sink an external consumer (e.g. `thorctl`'s TUI) installs to observe a run.
///
/// This is an unbounded sender so emitting an event never blocks the agent on a
/// slow or absent consumer: every emit site sends without awaiting and ignores a
/// closed channel, so a dropped receiver is a no-op rather than a stall.
pub type ThorPalEventSender = UnboundedSender<ThorPalEvent>;

/// A single observability event emitted during a ThorPal run.
///
/// Correlation: `call_id` is `rig`'s `internal_call_id`, which is unique per tool
/// invocation within a run, so a [`ThorPalEvent::ToolCallStarted`] and its
/// matching [`ThorPalEvent::ToolCallFinished`] carry the same `call_id`. A
/// consumer can therefore merge a call and its result into one display element.
#[derive(Debug, Clone)]
pub enum ThorPalEvent {
    /// The user sent a message, starting a new conversational turn.
    ///
    /// Emitted at the top of every prompt entry point so the user's side of the
    /// conversation enters the same ordered stream as the run's activity -- the
    /// session transcript is persisted from this stream, so order on disk is
    /// order on screen.
    UserMessage {
        /// The text the user sent.
        text: String,
        /// The named side conversation this turn belongs to, or `None` for the
        /// main conversation.
        scope: Option<String>,
    },
    /// An agent produced assistant text this turn (the main agent or a sub-agent).
    AgentMessage {
        /// The display name of the agent that produced the text (e.g. `"main"`).
        agent: String,
        /// The assistant text the agent produced this turn.
        text: String,
    },
    /// A sub-agent delegation started, with its per-call display name and goal.
    SubAgentStarted {
        /// The unique per-call display name (e.g. `"ReverseEngineer(Europa)"`).
        name: String,
        /// The human-facing role prefix for this agent (e.g. `"ReverseEngineer"`).
        role: String,
        /// The overarching goal this delegation serves.
        goal: String,
        /// The display name of the swarm this member runs under, if any.
        swarm: Option<String>,
        /// Whether this start continues an invocation a process death
        /// interrupted, rather than beginning a fresh delegation.
        resumed: bool,
    },
    /// A sub-agent's turn budget was extended (or an extension was reserved).
    ///
    /// Four triggers share this event: an in-window `request_more_turns`
    /// grant, a too-early request recorded as a standing reservation (the
    /// turns are not yet granted; `granted` carries the reserved count), that
    /// reservation converting at the cap, and the one-shot automatic
    /// extension at the cap. `trigger` names which one fired.
    SubAgentExtended {
        /// The display name of the sub-agent whose budget this concerns.
        name: String,
        /// The extra turns granted (or reserved, when `trigger` is `Reserved`).
        granted: usize,
        /// The extension turns left in the agent's allowance after this grant.
        remaining_allowance: usize,
        /// The justification the agent gave (or the automatic-grant note).
        justification: String,
        /// What caused this extension to be granted or recorded.
        trigger: ExtensionTrigger,
    },
    /// A sub-agent's conversation memory is nearing its context window.
    ///
    /// Emitted once per level per invocation (level-latched high-water): level
    /// 1 at the warn threshold, level 2 at the critical threshold where the
    /// agent's tools are clamped to forum writes. The matching transcript
    /// marker makes context pressure mechanically countable at audit time.
    SubAgentContextPressure {
        /// The display name of the sub-agent under context pressure.
        name: String,
        /// The pressure level reached: 1 = warn, 2 = critical.
        level: u8,
        /// The estimated context tokens in use when the level latched.
        used_tokens: u64,
        /// The configured context window the estimate is measured against.
        context_window: u64,
    },
    /// A sub-agent died before producing a final answer.
    ///
    /// Emitted from the death-salvage path with the death classified
    /// (`max_turns`, `context`, `cancelled`, or `other`) and, when the
    /// salvaged handoff was posted to the forum, the name of that post. The
    /// matching transcript marker is the audit recount surface for cap and
    /// context deaths.
    SubAgentTerminated {
        /// The display name of the sub-agent that died.
        name: String,
        /// The human-readable death reason (e.g. the turn-budget message).
        reason: String,
        /// The mechanical death class: `max_turns`, `context`, `cancelled`,
        /// or `other`.
        class: String,
        /// The forum post name the salvaged handoff landed under, if any.
        salvage_post: Option<String>,
    },
    /// A swarm of sub-agents was spawned to work on independent tasks at once.
    ///
    /// Each member also emits its own [`ThorPalEvent::SubAgentStarted`] once it
    /// gets a free slot under the run's live sub-agent cap.
    SwarmStarted {
        /// The unique per-call display name of this swarm (e.g. `"Swarm(Io)"`).
        name: String,
        /// The overarching goal every task in this swarm serves.
        goal: String,
        /// The role of each member in task order (e.g. `"ReverseEngineer"`).
        members: Vec<String>,
        /// Whether this start continues a swarm a process death interrupted,
        /// rather than beginning a fresh one.
        resumed: bool,
    },
    /// Every member of a swarm has finished, successfully or not.
    SwarmFinished {
        /// The unique per-call display name of this swarm (e.g. `"Swarm(Io)"`).
        name: String,
        /// How many members finished their task successfully.
        succeeded: usize,
        /// How many members failed with an error.
        failed: usize,
    },
    /// A tool call is about to run.
    ToolCallStarted {
        /// The display name of the agent making the call.
        agent: String,
        /// The `rig` `internal_call_id` correlating this call with its result.
        call_id: String,
        /// The name of the tool being called.
        tool: String,
        /// The raw JSON argument string the tool was called with.
        args: String,
    },
    /// A single captured log line produced while a streaming tool call runs.
    ///
    /// Emitted by a consumer-installed [`crate::ToolLogLayer`] rather than by
    /// the agent hooks, so it rides the caller's original sink and is never
    /// persisted to the session transcript. Correlated to its call by the same
    /// `call_id` as [`ThorPalEvent::ToolCallStarted`]; a line that arrives
    /// after its call finished is dropped by the consumer.
    ToolLog {
        /// The `call_id` of the running tool call this line belongs to.
        call_id: String,
        /// The formatted single log line.
        line: String,
    },
    /// A tool call finished.
    ToolCallFinished {
        /// The display name of the agent that made the call.
        agent: String,
        /// The `rig` `internal_call_id` correlating this result with its call.
        call_id: String,
        /// The name of the tool that ran.
        tool: String,
        /// The model-visible rendered output of the tool call.
        result: String,
        /// Whether the tool call succeeded; `false` on an error or refusal, so a
        /// consumer can flag a failed call (e.g. a red status marker).
        success: bool,
    },
    /// A per-turn token-usage checkpoint a consumer may show or ignore.
    UsageUpdated {
        /// The display name of the agent this usage belongs to.
        agent: String,
        /// The input (prompt) tokens this turn consumed.
        ///
        /// Cache semantics follow the provider: on the Anthropic/Bedrock
        /// providers this is the uncached remainder only, while on the OpenAI
        /// protocol it includes `cached`.
        input: u64,
        /// The output (completion) tokens this turn produced.
        output: u64,
        /// The input tokens this turn read from the provider's prompt cache.
        cached: u64,
        /// The input tokens this turn wrote into the provider's prompt cache
        /// (always 0 on the OpenAI protocol, which does not report cache writes).
        cache_creation: u64,
        /// The total tokens this turn accounted for.
        total: u64,
    },
    /// The current turn produced a final response for the caller.
    RunFinished {
        /// The assistant's final response for this turn.
        response: String,
        /// The named side conversation this turn belongs to, or `None` for the
        /// main conversation.
        scope: Option<String>,
    },
    /// A forum post or comment was created or had its status changed.
    ///
    /// Emitted after the change is stored, by an agent's tool call or by a human
    /// operator's comment/status change alike, so a consumer can refresh a live
    /// view of the discussion (e.g. via [`crate::ForumState::view`]).
    ForumUpdated {
        /// The post or comment that was just created or changed.
        target: ParentRef,
    },
    /// The main agent asked the analyst a question and is waiting on the answer.
    ///
    /// Live-only (never persisted): emitted by the analyst link only when a
    /// live analyst UI is attached, so a consumer (the chat TUI) can render a
    /// question modal and answer through [`crate::AnalystLink::answer`].
    /// Headless runs never see this event -- an ask auto-resolves to its
    /// recommended default instead.
    AnalystAskPending {
        /// The link-local ticket an answer is correlated back through.
        id: u64,
        /// The display name of the agent asking (always `"main"` in v1).
        agent: String,
        /// The question posed to the analyst.
        question: String,
        /// The 2-4 options offered, in display order.
        options: Vec<AskOption>,
        /// The label of the designated default/recommended option.
        recommended: String,
    },
    /// An analyst ask resolved: answered live, auto-defaulted, or declined.
    ///
    /// Emitted by the forum once the resolved ask is recorded and persisted,
    /// so a consumer can close a matching question modal, render the decision
    /// inline, and refresh its board view.
    AnalystAskResolved {
        /// The ticket from the matching [`ThorPalEvent::AnalystAskPending`]
        /// (0 when the ask auto-resolved with no analyst UI attached).
        id: u64,
        /// The persistent forum record id of the resolved ask.
        ask: usize,
        /// The display name of the agent that asked.
        agent: String,
        /// The question that was posed.
        question: String,
        /// The answer: a chosen option label, or the analyst's free text.
        chosen: String,
        /// The snake_case [`crate::AnswerSource`] spelling of who answered.
        source: String,
    },
    /// An agent flagged a finding or area as needing analyst review.
    ///
    /// Emitted by the forum once the flag is recorded and persisted, for both
    /// agent-raised flags and the "system" flags auto-filed when an ask
    /// resolves without an analyst answer.
    ReviewFlagRaised {
        /// The persistent forum record id of the flag.
        flag: usize,
        /// The display name of the agent that raised it ("system" when
        /// auto-filed for an unanswered ask).
        agent: String,
        /// The short title naming what needs review.
        title: String,
        /// The flagged forum item, when the flag anchored to one.
        reference: Option<ParentRef>,
    },
    /// A turn failed after exhausting its retry budget.
    ///
    /// Emitted when a main-agent prompt could not complete -- most often a
    /// transient completion fault (e.g. a provider response rig could not
    /// deserialize) that survived every retry. A consumer can render this inline
    /// so the run reports the failure instead of appearing to hang.
    TurnFailed {
        /// The display name of the agent whose turn failed (e.g. `"main"`).
        agent: String,
        /// The human-readable error describing why the turn failed.
        error: String,
        /// The provider's request id for the failed request, when the error
        /// preserved one (the id provider support asks for). The `error` text
        /// embeds the same id; this field spares consumers from parsing it out.
        request_id: Option<String>,
    },
    /// A completion request hit a transient provider fault (e.g. a 429
    /// throttle) and is backing off before its next attempt.
    ///
    /// Emitted once per failed attempt by the per-request retry wrapper (the
    /// `retry` module), so a consumer can render one live,
    /// in-place status line per retry episode -- an attempt counter, the time
    /// since the last attempt, and a countdown to the next -- instead of a
    /// scrolling log line per attempt. Each attempt is also persisted as an
    /// `llm_retry` transcript line so offline telemetry can count throttle
    /// storms and total backoff time (an episode is bounded by the retry
    /// budget, so the line volume is too).
    LlmRetrying {
        /// The display name of the agent whose request is retrying (e.g. `"main"`).
        agent: String,
        /// A key unique per retry episode, correlating every attempt of one
        /// failing request with its [`ThorPalEvent::LlmRetryResolved`].
        retry_id: String,
        /// The attempt that just failed (1-based).
        attempt: u32,
        /// The attempt budget before the error is surfaced as fatal.
        max_attempts: u32,
        /// A short human-readable summary of the fault (e.g. `"429 Too Many Requests"`).
        error: String,
        /// The provider's request id for the attempt that just failed, when
        /// the error preserved one (the id provider support asks for).
        request_id: Option<String>,
        /// When the attempt that just failed was observed.
        last_attempt_at: DateTime<Utc>,
        /// When the next attempt will fire.
        next_attempt_at: DateTime<Utc>,
    },
    /// A retry episode ended: the request finally succeeded, or its budget ran
    /// out and the error was surfaced to the caller.
    ///
    /// Only emitted when the episode's first attempt failed (a request that
    /// succeeds immediately emits nothing). A consumer clears the episode's
    /// live status line on this event; a final failure then surfaces through
    /// the run's normal error paths (e.g. [`ThorPalEvent::TurnFailed`] or a
    /// failed tool result).
    LlmRetryResolved {
        /// The display name of the agent whose request was retrying.
        agent: String,
        /// The episode key from the matching [`ThorPalEvent::LlmRetrying`] events.
        retry_id: String,
        /// How many attempts the episode consumed in total.
        attempts: u32,
        /// Whether the request finally succeeded (`false` means the budget ran
        /// out or a fatal error ended the episode).
        succeeded: bool,
    },
    /// A content-filtering proxy killed a request and a scrubbed retry fired.
    ///
    /// Emitted once per scrub escalation: the request's text content was
    /// rewritten (level 1 defangs IOCs, level 2 additionally neutralizes bulk
    /// payload blobs and bounds oversized tool results) and the request was
    /// re-sent. Persisted history and the session store keep the pristine
    /// text; only the one attempt's wire copy is scrubbed. The matching
    /// transcript marker makes this class mechanically countable at audit
    /// time instead of from log banners.
    LlmScrubbed {
        /// The display name of the agent whose request was scrubbed.
        agent: String,
        /// The episode key correlating this scrub with the request's
        /// [`ThorPalEvent::LlmRetrying`]/[`ThorPalEvent::LlmRetryResolved`] record.
        retry_id: String,
        /// The scrub level the retry escalated to (1 = defang, 2 = deep).
        level: u8,
        /// A short human-readable summary of the kill (e.g. `"403 Forbidden"`).
        error: String,
        /// The provider's request id for the killed request, when the error
        /// preserved one (what the proxy team asks for when chasing a kill).
        request_id: Option<String>,
    },
    /// Report generation stepped down its content-filter ladder.
    ///
    /// Emitted once per degradation: the pristine one-shot was blocked and
    /// retried defanged (rung 1), the defanged one-shot was blocked and the
    /// report fell back to per-section generation (rung 2), or an individual
    /// section was replaced with a placeholder (also rung 2). The matching
    /// transcript marker keeps the loss auditable next to the stored report.
    ReportDegraded {
        /// The ladder rung the generation escalated to.
        rung: u8,
        /// What degraded and why (operator-facing prose).
        detail: String,
    },
    /// The whole session paused, waiting for the provider to become usable.
    ///
    /// Emitted by the provider gate when a request hits an auth/credential
    /// fault or spends its whole transient-retry budget: every agent's next
    /// request parks on the same gate, so one event describes the whole
    /// session's state. Re-emitted with updated fields after each failed
    /// probe so a consumer refreshes its banner in place. Only the initial
    /// publish (`probe_attempts == 0`) is persisted, as a `provider_paused`
    /// transcript line; probe refreshes are live banner updates whose tally
    /// lands on the matching [`ThorPalEvent::SessionResumed`] instead.
    SessionPaused {
        /// Why the session paused.
        kind: super::gate::PauseKind,
        /// The display name of the agent whose request first hit the fault.
        agent: String,
        /// A short human-readable summary of the fault (e.g. `"401 Unauthorized"`).
        error: String,
        /// The provider's request id for the most recent fault, when the error
        /// preserved one. Mirrors `error`: overwritten by each failed probe.
        request_id: Option<String>,
        /// When the pause began.
        since: DateTime<Utc>,
        /// How many automatic probes have failed since the pause began.
        probe_attempts: u32,
        /// When the next automatic probe fires.
        next_probe_at: DateTime<Utc>,
    },
    /// A paused session resumed: a probe succeeded, the operator forced a
    /// retry that succeeded, or the operator aborted the pause.
    SessionResumed {
        /// Why the session had paused.
        kind: super::gate::PauseKind,
        /// Whether the provider actually recovered (`false` means the operator
        /// aborted the pause and the parked requests surfaced their errors).
        recovered: bool,
        /// How long the session was paused, in milliseconds.
        paused_ms: u64,
        /// How many automatic probes failed over the whole pause.
        probe_attempts: u32,
    },
}
