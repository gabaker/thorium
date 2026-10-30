//! The ordered, durable transcript of a session's displayable activity.
//!
//! Every displayable item of a run -- user messages, the main agent's answers,
//! sub-agent activity, and tool calls -- already flows through the single
//! ordered [`crate::ThorPalEvent`] sink in exactly the order a live consumer
//! renders it. The session store tees that stream into an append-only
//! `transcript.jsonl`, one JSON item per line, so a resume can replay the file
//! top-to-bottom and rebuild the chat log in its original interleaved order
//! without merging per-agent files or trusting timestamps.
//!
//! Per-turn token-usage checkpoints ride along as `usage` lines so an offline
//! consumer can rebuild exact token and cost timelines without the live event
//! stream; older readers skip the unknown kind via the store's lenient line
//! parser.

use serde::{Deserialize, Serialize};

use super::events::ThorPalEvent;
use super::gate::PauseKind;

/// One persisted transcript line: a displayable item plus an optional capture time
///
/// The timestamp is flattened alongside the item's own `kind`-tagged fields, so
/// a line written by an older build (no `ts` key) still parses with `ts: None`
/// and an older reader simply ignores the extra key. Consumers use the
/// timestamps, when present, to replay a session at proportional speed instead
/// of a fixed per-item rate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptLine {
    /// Milliseconds since the Unix epoch when this item was captured
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<u64>,
    /// The displayable item itself
    #[serde(flatten)]
    pub item: TranscriptItem,
}

impl TranscriptLine {
    /// Wraps an item with the current wall-clock capture time
    ///
    /// # Arguments
    ///
    /// * `item` - The transcript item to stamp
    pub fn stamped(item: TranscriptItem) -> Self {
        // capture the current wall clock as milliseconds since the epoch
        let ts = u64::try_from(chrono::Utc::now().timestamp_millis()).ok();
        TranscriptLine { ts, item }
    }
}

/// One durable, displayable item of a session's transcript.
///
/// Mirrors the subset of [`ThorPalEvent`]s a consumer renders as chat-log
/// entries, plus checkpoints that exist for offline telemetry rather than
/// display: per-turn usage lines so token timelines survive a restart, and
/// retry/pause lines so provider stalls stay countable after the live stream
/// is gone. Purely presentational events (forum refreshes, live tool logs,
/// pending analyst asks) are not transcript items. Items are serialized
/// tagged by `kind` so the JSONL file stays self-describing and future
/// variants can be skipped by older readers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptItem {
    /// A message the user sent.
    User {
        /// The text the user sent.
        text: String,
        /// The named side conversation this message belongs to, or `None` for
        /// the main conversation (and every pre-scope transcript).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
    },
    /// A conversation's final answer for a turn (main or a side conversation).
    Assistant {
        /// The answer text the conversation's agent produced.
        text: String,
        /// The named side conversation this answer belongs to, or `None` for
        /// the main conversation (and every pre-scope transcript).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
    },
    /// Assistant text a sub-agent produced mid-run.
    SubAgentMessage {
        /// The display name of the sub-agent (e.g. `"ReverseEngineer(Europa)"`).
        agent: String,
        /// The text the sub-agent produced.
        text: String,
    },
    /// A sub-agent delegation started.
    SubAgentStarted {
        /// The unique per-call display name of the sub-agent.
        name: String,
        /// The human-facing role prefix for this agent (e.g. `"ReverseEngineer"`).
        ///
        /// Older transcripts predate this field, so it defaults to `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<String>,
        /// The overarching goal this delegation serves.
        goal: String,
        /// The display name of the swarm this member runs under, if any.
        ///
        /// Older transcripts predate this field, so it defaults to `None` and
        /// those sessions simply resume without swarm grouping.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        swarm: Option<String>,
        /// Whether this start continued an interrupted invocation across a
        /// resume rather than beginning a fresh delegation.
        ///
        /// Older transcripts predate this field, so it defaults to `false`.
        #[serde(default)]
        resumed: bool,
    },
    /// A sub-agent's turn budget was extended (or an extension was reserved).
    SubAgentExtended {
        /// The display name of the sub-agent whose budget this concerns.
        name: String,
        /// The extra turns granted (or reserved, when `trigger` is `reserved`).
        granted: usize,
        /// The extension turns left in the agent's allowance after this grant.
        remaining_allowance: usize,
        /// The justification the agent gave (or the automatic-grant note).
        justification: String,
        /// What caused this extension to be granted or recorded.
        ///
        /// Older transcripts predate this field, so it defaults to
        /// `requested` -- the only trigger that existed when they were written.
        #[serde(default)]
        trigger: crate::thorpal::events::ExtensionTrigger,
    },
    /// A sub-agent's conversation memory crossed a context-pressure level.
    ///
    /// Durable so a run audit can count context pressure mechanically
    /// (`grep '"kind":"context_pressure"'`); emitted once per level per
    /// invocation.
    ContextPressure {
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
    /// Durable so a run audit can count cap/context deaths mechanically
    /// (`grep '"kind":"sub_agent_terminated"'`) and see whether the salvaged
    /// handoff reached the forum.
    SubAgentTerminated {
        /// The display name of the sub-agent that died.
        name: String,
        /// The human-readable death reason.
        reason: String,
        /// The mechanical death class: `max_turns`, `context`, `cancelled`,
        /// or `other`.
        class: String,
        /// The forum post name the salvaged handoff landed under, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        salvage_post: Option<String>,
    },
    /// A swarm of concurrent sub-agents was spawned.
    SwarmStarted {
        /// The unique per-call display name of this swarm (e.g. `"Swarm(Io)"`).
        name: String,
        /// The overarching goal every task in this swarm serves.
        goal: String,
        /// The role of each member in task order.
        members: Vec<String>,
        /// Whether this start continued an interrupted swarm across a resume
        /// rather than beginning a fresh one.
        ///
        /// Older transcripts predate this field, so it defaults to `false`.
        #[serde(default)]
        resumed: bool,
    },
    /// Every member of a swarm finished, successfully or not.
    SwarmFinished {
        /// The unique per-call display name of this swarm.
        name: String,
        /// How many members finished their task successfully.
        succeeded: usize,
        /// How many members failed with an error.
        failed: usize,
    },
    /// A tool call started.
    ToolCall {
        /// The display name of the agent making the call.
        agent: String,
        /// The call id correlating this call with its result.
        call_id: String,
        /// The name of the tool being called.
        tool: String,
        /// The raw JSON argument string the tool was called with.
        args: String,
    },
    /// A tool call finished.
    ToolResult {
        /// The display name of the agent that made the call.
        agent: String,
        /// The call id correlating this result with its call.
        call_id: String,
        /// The name of the tool that ran.
        tool: String,
        /// The model-visible rendered output of the tool call.
        result: String,
        /// Whether the tool call succeeded.
        success: bool,
    },
    /// One model turn's token-usage checkpoint.
    ///
    /// Durable (unlike most telemetry) so offline consumers can rebuild exact
    /// per-turn token and cost timelines; sessions written before this variant
    /// existed simply have no `usage` lines and fall back to their snapshot's
    /// cumulative per-agent ledger.
    Usage {
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
        #[serde(default)]
        cached: u64,
        /// The input tokens this turn wrote into the provider's prompt cache.
        #[serde(default)]
        cache_creation: u64,
        /// The total tokens this turn accounted for, as the provider reported.
        #[serde(default)]
        total: u64,
    },
    /// A turn failed after exhausting its retry budget.
    TurnFailed {
        /// The display name of the agent whose turn failed.
        agent: String,
        /// The human-readable error describing why the turn failed.
        error: String,
        /// The provider's request id for the failed request, when the error
        /// preserved one.
        ///
        /// Older transcripts predate this field, so it defaults to `None`
        /// (the `error` text embeds the same id when one was captured).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },
    /// A resume barrier: a later process reopened this session here.
    ///
    /// Appended by the session restore itself, never mapped from an event --
    /// the live [`ThorPalEvent::SessionResumed`] is a provider-pause ending
    /// and persists as [`TranscriptItem::ProviderResumed`] instead, so this
    /// kind stays exclusively the process-restore barrier a replay keys on to
    /// tell debris a dead run left behind (the terminal failure tail before a
    /// barrier) apart from this run's live records and hide it from the chat
    /// log.
    SessionResumed {
        /// Whether the resumed session carried an interrupted turn whose
        /// delegations the resume continues.
        #[serde(default)]
        interrupted: bool,
    },
    /// A content-filter kill answered with a scrubbed retry.
    ///
    /// Durable so a run audit can count the content-filter class mechanically
    /// (`grep '"kind":"llm_scrubbed"'`) instead of reconstructing it from
    /// termination banners.
    LlmScrubbed {
        /// The display name of the agent whose request was scrubbed.
        agent: String,
        /// The scrub level the retry escalated to (1 = defang, 2 = deep).
        level: u8,
        /// The retry episode this scrub belongs to, correlating it with the
        /// episode's `llm_retry`/`llm_retry_resolved` lines so offline
        /// telemetry can report what scrubbing rescued and what it cost.
        ///
        /// Older transcripts predate this field, so it defaults to `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_id: Option<String>,
        /// The provider's request id for the killed request, when the error
        /// preserved one (what the proxy team asks for when chasing a kill).
        ///
        /// Older transcripts predate this field, so it defaults to `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },
    /// Report generation stepped down its content-filter ladder.
    ///
    /// Durable so the loss is auditable next to the stored report: which rung
    /// the generation ended on and what each degradation cost.
    ReportDegraded {
        /// The ladder rung the generation escalated to.
        rung: u8,
        /// What degraded and why (operator-facing prose).
        detail: String,
    },
    /// An analyst ask resolved (answered live, auto-defaulted, or declined).
    ///
    /// Durable so a resumed session still shows which fork was taken and who
    /// chose it, and so audits can count asks mechanically
    /// (`grep '"kind":"analyst_ask"'`).
    AnalystAsk {
        /// The display name of the agent that asked.
        agent: String,
        /// The question that was posed.
        question: String,
        /// The answer: a chosen option label, or the analyst's free text.
        chosen: String,
        /// The snake_case answer source: `analyst`, `analyst_other`,
        /// `auto_default`, or `declined`.
        source: String,
    },
    /// An agent flagged a finding or area as needing analyst review.
    ///
    /// Durable so a resumed session still shows the review-queue entry and so
    /// audits can count flags mechanically (`grep '"kind":"review_flag"'`).
    ReviewFlag {
        /// The display name of the agent that raised the flag ("system" when
        /// auto-filed for an unanswered ask).
        agent: String,
        /// The short title naming what needs review.
        title: String,
    },
    /// A completion attempt hit a transient provider fault and backed off.
    ///
    /// Durable (one line per failed attempt, bounded by the episode's retry
    /// budget) so offline telemetry can count throttle storms and sum the
    /// backoff time a run spent waiting on the provider instead of analyzing.
    LlmRetry {
        /// The display name of the agent whose request is retrying.
        agent: String,
        /// The episode key correlating this attempt with its siblings and the
        /// episode's closing `llm_retry_resolved` line.
        retry_id: String,
        /// The attempt that just failed (1-based).
        attempt: u32,
        /// The attempt budget before the error is surfaced as fatal.
        max_attempts: u32,
        /// A short human-readable summary of the fault (e.g. `"429 Too Many Requests"`).
        error: String,
        /// The provider's request id for the attempt that just failed, when
        /// the error preserved one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        /// How long this attempt backs off before the next one fires, in
        /// milliseconds.
        backoff_ms: u64,
    },
    /// A retry episode ended: the request succeeded or its budget ran out.
    ///
    /// Durable so offline telemetry can count episodes (distinct from
    /// attempts) and their outcomes without pairing `llm_retry` lines by hand.
    LlmRetryResolved {
        /// The display name of the agent whose request was retrying.
        agent: String,
        /// The episode key from the episode's `llm_retry` lines.
        retry_id: String,
        /// How many attempts the episode consumed in total.
        attempts: u32,
        /// Whether the request finally succeeded (`false` means the budget
        /// ran out or a fatal error ended the episode).
        succeeded: bool,
    },
    /// The whole session paused, waiting for the provider to become usable.
    ///
    /// Durable once per pause episode (the initial publish only -- probe
    /// refreshes are live banner updates), so offline telemetry can count
    /// outages and auth stalls; the pause's duration and probe tally land on
    /// the matching [`TranscriptItem::ProviderResumed`] line.
    ProviderPaused {
        /// Why the session paused.
        pause: PauseKind,
        /// The display name of the agent whose request first hit the fault.
        agent: String,
        /// A short human-readable summary of the fault (e.g. `"401 Unauthorized"`).
        error: String,
        /// The provider's request id for the fault, when the error preserved
        /// one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },
    /// A provider pause ended: a probe succeeded or the operator aborted.
    ///
    /// Durable so offline telemetry can measure how long a session stalled on
    /// the provider (rather than the analysis) and how many probes it took.
    /// Deliberately a distinct kind from the `session_resumed` restore
    /// barrier, which replay consumers key on to hide dead-run debris.
    ProviderResumed {
        /// Why the session had paused.
        pause: PauseKind,
        /// Whether the provider actually recovered (`false` means the
        /// operator aborted the pause and the parked requests surfaced their
        /// errors).
        recovered: bool,
        /// How long the session was paused, in milliseconds.
        paused_ms: u64,
        /// How many automatic probes failed over the whole pause.
        probe_attempts: u32,
    },
    /// The resolved tool roster a role's agents were offered this process run.
    ///
    /// Appended once per built role (plus `"main"`) at build time, never
    /// mapped from a live event: group names expand against the registry
    /// compiled into the running binary and MCP servers advertise their tools
    /// only at connect time, so without this line the offered roster cannot
    /// be reconstructed offline and "offered but never called" is
    /// unanswerable. Every instance of a role shares the one built agent, so
    /// a per-role line is the per-agent truth; a resumed session appends a
    /// fresh set, keeping roster drift across binary upgrades visible.
    ToolsOffered {
        /// The role whose agents are offered these tools (e.g.
        /// `"ReverseEngineer"`, or `"main"` for the main agent).
        role: String,
        /// The sorted model-visible names of every tool offered, always-on
        /// orchestration tools and MCP-prefixed names included.
        tools: Vec<String>,
        /// The thorpal crate version that resolved this roster.
        version: String,
    },
    /// The native detector roster one seeding pass ran.
    ///
    /// Appended once per seeding pass (once per sample in a multi-sample
    /// session), never mapped from a live event. The registry compiled into
    /// the recording binary is what ran -- a later reader's registry may
    /// differ -- so this line is what makes "detectors that never fired"
    /// answerable offline, zero-hit detections included.
    DetectorsRun {
        /// The scoped sibling program this pass seeded, or `None` for the
        /// primary sample's pass.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sample: Option<String>,
        /// Every registry detection's outcome for this pass.
        detectors: Vec<DetectorRunLine>,
        /// The thorpal crate version whose registry ran.
        version: String,
    },
}

/// One registry detection's durable outcome inside a `detectors_run` line
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectorRunLine {
    /// The detection's stable registry name (e.g. `"debugger_evasion"`).
    pub name: String,
    /// The ATT&CK technique id the detection evidences.
    pub technique: String,
    /// How many hits the detection produced.
    #[serde(default)]
    pub hits: u32,
    /// Whether the detection failed instead of completing.
    #[serde(default)]
    pub failed: bool,
}

/// True for the agents that own a whole conversation, whose per-turn text
/// would duplicate the conversation's final answer
///
/// The main agent (`"main"`) and every side conversation's root agent
/// (`btw:<name>` without a sub-agent suffix) deliver their final answer as a
/// scoped [`ThorPalEvent::RunFinished`], so their per-turn `AgentMessage` text
/// is skipped from the transcript to avoid duplication.
///
/// # Arguments
///
/// * `agent` - The display name of the agent that produced the text
fn is_conversation_agent(agent: &str) -> bool {
    // the main agent and unprefixed side roots own whole conversations
    agent == "main" || (agent.starts_with("btw:") && !agent.contains('/'))
}

impl TranscriptItem {
    /// Maps an event onto its durable transcript item, if one is persisted
    ///
    /// Mirrors the live display rules a consumer applies: a conversation-root
    /// agent's per-turn `AgentMessage` text is skipped (the final answer
    /// arrives as a scoped `RunFinished` and would otherwise duplicate), and
    /// purely presentational events (`ForumUpdated`, live tool logs, pending
    /// analyst asks) are not transcript items at all. Telemetry checkpoints
    /// are persisted beyond what a consumer displays: per-turn `UsageUpdated`
    /// lines so token timelines survive a restart, and retry/pause lines so
    /// provider stalls stay countable offline (a pause's probe refreshes stay
    /// live-only -- the initial publish and the resume carry the whole story).
    ///
    /// # Arguments
    ///
    /// * `event` - The event to map onto a transcript item
    pub fn from_event(event: &ThorPalEvent) -> Option<TranscriptItem> {
        // map each displayable event variant onto its durable mirror
        match event {
            // the user's message enters the stream from the prompt entry point
            ThorPalEvent::UserMessage { text, scope } => Some(TranscriptItem::User {
                text: text.clone(),
                scope: scope.clone(),
            }),
            // a conversation's final answer for the turn
            ThorPalEvent::RunFinished { response, scope } => Some(TranscriptItem::Assistant {
                text: response.clone(),
                scope: scope.clone(),
            }),
            // a sub-agent's text is displayable; a conversation root's per-turn
            // text is skipped so the final answer is never duplicated
            ThorPalEvent::AgentMessage { agent, text } => {
                (!is_conversation_agent(agent)).then(|| TranscriptItem::SubAgentMessage {
                    agent: agent.clone(),
                    text: text.clone(),
                })
            }
            // a sub-agent delegation breadcrumb
            ThorPalEvent::SubAgentStarted {
                name,
                role,
                goal,
                swarm,
                resumed,
            } => Some(TranscriptItem::SubAgentStarted {
                name: name.clone(),
                role: Some(role.clone()),
                goal: goal.clone(),
                swarm: swarm.clone(),
                resumed: *resumed,
            }),
            // a sub-agent's turn-budget extension is a durable breadcrumb, so
            // a resumed session still shows who asked for more time and why
            ThorPalEvent::SubAgentExtended {
                name,
                granted,
                remaining_allowance,
                justification,
                trigger,
            } => Some(TranscriptItem::SubAgentExtended {
                name: name.clone(),
                granted: *granted,
                remaining_allowance: *remaining_allowance,
                justification: justification.clone(),
                trigger: *trigger,
            }),
            // a context-pressure level is durable so audits count it by grep
            ThorPalEvent::SubAgentContextPressure {
                name,
                level,
                used_tokens,
                context_window,
            } => Some(TranscriptItem::ContextPressure {
                name: name.clone(),
                level: *level,
                used_tokens: *used_tokens,
                context_window: *context_window,
            }),
            // a sub-agent death is durable with its class and salvage post
            ThorPalEvent::SubAgentTerminated {
                name,
                reason,
                class,
                salvage_post,
            } => Some(TranscriptItem::SubAgentTerminated {
                name: name.clone(),
                reason: reason.clone(),
                class: class.clone(),
                salvage_post: salvage_post.clone(),
            }),
            // a swarm's start and finish breadcrumbs
            ThorPalEvent::SwarmStarted {
                name,
                goal,
                members,
                resumed,
            } => Some(TranscriptItem::SwarmStarted {
                name: name.clone(),
                goal: goal.clone(),
                members: members.clone(),
                resumed: *resumed,
            }),
            ThorPalEvent::SwarmFinished {
                name,
                succeeded,
                failed,
            } => Some(TranscriptItem::SwarmFinished {
                name: name.clone(),
                succeeded: *succeeded,
                failed: *failed,
            }),
            // a tool call and its correlated result
            ThorPalEvent::ToolCallStarted {
                agent,
                call_id,
                tool,
                args,
            } => Some(TranscriptItem::ToolCall {
                agent: agent.clone(),
                call_id: call_id.clone(),
                tool: tool.clone(),
                args: args.clone(),
            }),
            ThorPalEvent::ToolCallFinished {
                agent,
                call_id,
                tool,
                result,
                success,
            } => Some(TranscriptItem::ToolResult {
                agent: agent.clone(),
                call_id: call_id.clone(),
                tool: tool.clone(),
                result: result.clone(),
                success: *success,
            }),
            // a failed turn renders as an inline error, so it is durable too,
            // structured request id included so offline consumers are spared
            // parsing it back out of the error text
            ThorPalEvent::TurnFailed {
                agent,
                error,
                request_id,
            } => Some(TranscriptItem::TurnFailed {
                agent: agent.clone(),
                error: error.clone(),
                request_id: request_id.clone(),
            }),
            // a scrubbed retry is durable so audits can count the
            // content-filter class without banner archaeology; the episode key
            // joins it to the retry lines so rescues stay attributable
            ThorPalEvent::LlmScrubbed {
                agent,
                retry_id,
                level,
                request_id,
                ..
            } => Some(TranscriptItem::LlmScrubbed {
                agent: agent.clone(),
                level: *level,
                retry_id: Some(retry_id.clone()),
                request_id: request_id.clone(),
            }),
            // a report degradation is durable next to the stored report
            ThorPalEvent::ReportDegraded { rung, detail } => Some(TranscriptItem::ReportDegraded {
                rung: *rung,
                detail: detail.clone(),
            }),
            // a per-turn usage checkpoint is durable so offline consumers can
            // rebuild exact token timelines without the live event stream
            ThorPalEvent::UsageUpdated {
                agent,
                input,
                output,
                cached,
                cache_creation,
                total,
            } => Some(TranscriptItem::Usage {
                agent: agent.clone(),
                input: *input,
                output: *output,
                cached: *cached,
                cache_creation: *cache_creation,
                total: *total,
            }),
            // a resolved analyst ask is durable so the taken fork survives a
            // resume (the live ticket and record id are display-only, dropped)
            ThorPalEvent::AnalystAskResolved {
                agent,
                question,
                chosen,
                source,
                ..
            } => Some(TranscriptItem::AnalystAsk {
                agent: agent.clone(),
                question: question.clone(),
                chosen: chosen.clone(),
                source: source.clone(),
            }),
            // a raised review flag is durable so the review queue survives a
            // resume (the record id and anchor live on in forum.json)
            ThorPalEvent::ReviewFlagRaised { agent, title, .. } => {
                Some(TranscriptItem::ReviewFlag {
                    agent: agent.clone(),
                    title: title.clone(),
                })
            }
            // each failed transient attempt is durable so offline telemetry
            // can count throttle storms and sum backoff time; the wall-clock
            // pair collapses to the one delta the fold needs
            ThorPalEvent::LlmRetrying {
                agent,
                retry_id,
                attempt,
                max_attempts,
                error,
                request_id,
                last_attempt_at,
                next_attempt_at,
            } => Some(TranscriptItem::LlmRetry {
                agent: agent.clone(),
                retry_id: retry_id.clone(),
                attempt: *attempt,
                max_attempts: *max_attempts,
                error: error.clone(),
                request_id: request_id.clone(),
                // clamp a backwards schedule to a zero backoff
                backoff_ms: u64::try_from((*next_attempt_at - *last_attempt_at).num_milliseconds())
                    .unwrap_or(0),
            }),
            // the episode's close is durable so episodes and their outcomes
            // count without pairing attempt lines by hand
            ThorPalEvent::LlmRetryResolved {
                agent,
                retry_id,
                attempts,
                succeeded,
            } => Some(TranscriptItem::LlmRetryResolved {
                agent: agent.clone(),
                retry_id: retry_id.clone(),
                attempts: *attempts,
                succeeded: *succeeded,
            }),
            // only a pause's initial publish is durable; the re-emissions
            // after each failed probe are live banner refreshes whose tally
            // lands on the resume line instead
            ThorPalEvent::SessionPaused {
                kind,
                agent,
                error,
                request_id,
                probe_attempts,
                ..
            } => (*probe_attempts == 0).then(|| TranscriptItem::ProviderPaused {
                pause: *kind,
                agent: agent.clone(),
                error: error.clone(),
                request_id: request_id.clone(),
            }),
            // a pause's end is durable with the whole episode's cost; this is
            // deliberately NOT the `session_resumed` restore barrier, which
            // replay consumers key on to hide dead-run debris
            ThorPalEvent::SessionResumed {
                kind,
                recovered,
                paused_ms,
                probe_attempts,
            } => Some(TranscriptItem::ProviderResumed {
                pause: *kind,
                recovered: *recovered,
                paused_ms: *paused_ms,
                probe_attempts: *probe_attempts,
            }),
            // forum refreshes, live tool log lines, and pending analyst asks
            // are not transcript items (they are live telemetry, not
            // conversation; a pending ask lands durably once it resolves)
            ThorPalEvent::ForumUpdated { .. }
            | ThorPalEvent::AnalystAskPending { .. }
            | ThorPalEvent::ToolLog { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every item variant survives a JSONL round-trip
    #[test]
    fn transcript_items_round_trip_as_jsonl() {
        // build one of every variant in a plausible order
        let items = vec![
            TranscriptItem::User {
                text: "analyze this".to_owned(),
                scope: None,
            },
            TranscriptItem::SubAgentStarted {
                name: "ReverseEngineer(Europa)".to_owned(),
                role: Some("ReverseEngineer".to_owned()),
                goal: "triage".to_owned(),
                swarm: Some("Swarm(Io)".to_owned()),
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
            TranscriptItem::Usage {
                agent: "ReverseEngineer(Europa)".to_owned(),
                input: 1_200,
                output: 300,
                cached: 8_000,
                cache_creation: 400,
                total: 9_900,
            },
            TranscriptItem::SubAgentMessage {
                agent: "ReverseEngineer(Europa)".to_owned(),
                text: "found it".to_owned(),
            },
            TranscriptItem::SubAgentExtended {
                name: "ReverseEngineer(Europa)".to_owned(),
                granted: 4,
                remaining_allowance: 2,
                justification: "two functions left".to_owned(),
                trigger: crate::thorpal::events::ExtensionTrigger::ReservationApplied,
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
            TranscriptItem::SwarmStarted {
                name: "Swarm(Io)".to_owned(),
                goal: "answer questions".to_owned(),
                members: vec!["ReverseEngineer".to_owned()],
                resumed: false,
            },
            TranscriptItem::SessionResumed { interrupted: true },
            TranscriptItem::SwarmFinished {
                name: "Swarm(Io)".to_owned(),
                succeeded: 1,
                failed: 0,
            },
            TranscriptItem::TurnFailed {
                agent: "main".to_owned(),
                error: "boom".to_owned(),
                request_id: Some("req-77aa".to_owned()),
            },
            TranscriptItem::LlmScrubbed {
                agent: "ReverseEngineer(Europa)".to_owned(),
                level: 1,
                retry_id: Some("ReverseEngineer(Europa)#3".to_owned()),
                request_id: Some("req-42cc".to_owned()),
            },
            TranscriptItem::LlmRetry {
                agent: "main".to_owned(),
                retry_id: "main#1".to_owned(),
                attempt: 2,
                max_attempts: 12,
                error: "429 Too Many Requests".to_owned(),
                request_id: Some("req-13bb".to_owned()),
                backoff_ms: 2_000,
            },
            TranscriptItem::LlmRetryResolved {
                agent: "main".to_owned(),
                retry_id: "main#1".to_owned(),
                attempts: 3,
                succeeded: true,
            },
            TranscriptItem::ProviderPaused {
                pause: PauseKind::Auth,
                agent: "main".to_owned(),
                error: "401 Unauthorized".to_owned(),
                request_id: Some("req-90dd".to_owned()),
            },
            TranscriptItem::ProviderResumed {
                pause: PauseKind::RetryBudgetExhausted,
                recovered: true,
                paused_ms: 184_000,
                probe_attempts: 2,
            },
            TranscriptItem::ToolsOffered {
                role: "ReverseEngineer".to_owned(),
                tools: vec!["decomp_function".to_owned(), "think".to_owned()],
                version: "0.1.0".to_owned(),
            },
            TranscriptItem::Assistant {
                text: "done".to_owned(),
                scope: None,
            },
        ];
        // serialize each item to one JSON line, then parse the lines back
        let encoded = items
            .iter()
            .map(|item| serde_json::to_string(item).expect("item should serialize"))
            .collect::<Vec<_>>()
            .join("\n");
        let decoded: Vec<TranscriptItem> = encoded
            .lines()
            .map(|line| serde_json::from_str(line).expect("line should parse"))
            .collect();
        // the order and count survive; spot-check the first and last items
        assert_eq!(decoded.len(), items.len());
        assert!(matches!(&decoded[0], TranscriptItem::User { text, .. } if text == "analyze this"));
        let last = decoded.last().expect("at least one item");
        assert!(matches!(last, TranscriptItem::Assistant { text, .. } if text == "done"));
    }

    /// A timestamp-less line from an older build still parses, and a stamped
    /// line round-trips with its timestamp intact
    #[test]
    fn transcript_lines_are_backward_compatible() {
        // an old line has only the item's own kind-tagged fields
        let old_line = r#"{"kind":"user","text":"analyze this"}"#;
        let parsed: TranscriptLine = serde_json::from_str(old_line).expect("old line should parse");
        // the missing timestamp defaults to none and the item survives
        assert!(parsed.ts.is_none());
        assert!(
            matches!(&parsed.item, TranscriptItem::User { text, scope: None } if text == "analyze this")
        );
        // a stamped line round-trips with both the timestamp and the item
        let stamped = TranscriptLine::stamped(TranscriptItem::Assistant {
            text: "done".to_owned(),
            scope: None,
        });
        let encoded = serde_json::to_string(&stamped).expect("line should serialize");
        let decoded: TranscriptLine = serde_json::from_str(&encoded).expect("line should parse");
        assert_eq!(decoded.ts, stamped.ts);
        assert!(decoded.ts.is_some());
        assert!(matches!(&decoded.item, TranscriptItem::Assistant { text, .. } if text == "done"));
        // the flattened encoding keeps the kind tag at the top level
        assert!(encoded.contains(r#""kind":"assistant""#));
    }

    /// A usage checkpoint maps from its live event, round-trips as a `usage`
    /// line, and a line missing the defaulted cache buckets still parses
    #[test]
    fn usage_checkpoints_are_durable() {
        // the live per-turn event maps onto a durable usage line
        let event = ThorPalEvent::UsageUpdated {
            agent: "ReverseEngineer(Europa)".to_owned(),
            input: 1_000,
            output: 250,
            cached: 9_000,
            cache_creation: 500,
            total: 10_750,
        };
        let item = TranscriptItem::from_event(&event).expect("usage should be durable");
        // the mapped item carries every bucket across under the usage kind
        let encoded = serde_json::to_string(&item).expect("item should serialize");
        assert!(encoded.contains(r#""kind":"usage""#));
        let decoded: TranscriptItem = serde_json::from_str(&encoded).expect("item should parse");
        assert!(matches!(
            &decoded,
            TranscriptItem::Usage {
                agent,
                input: 1_000,
                output: 250,
                cached: 9_000,
                cache_creation: 500,
                total: 10_750,
            } if agent == "ReverseEngineer(Europa)"
        ));
        // a hand-trimmed line missing the defaulted buckets parses as zeros
        let trimmed = r#"{"kind":"usage","agent":"main","input":10,"output":5}"#;
        let parsed: TranscriptLine =
            serde_json::from_str(trimmed).expect("trimmed line should parse");
        assert!(matches!(
            &parsed.item,
            TranscriptItem::Usage {
                cached: 0,
                cache_creation: 0,
                total: 0,
                ..
            }
        ));
    }

    /// An extension breadcrumb from an older build (no trigger key) parses as
    /// a plain in-window request, the only trigger that existed then
    #[test]
    fn sub_agent_extended_is_backward_compatible() {
        // an old line carries no trigger field at all
        let old_line = r#"{"kind":"sub_agent_extended","name":"ReverseEngineer(Europa)","granted":4,"remaining_allowance":2,"justification":"two functions left"}"#;
        let parsed: TranscriptLine = serde_json::from_str(old_line).expect("old line should parse");
        // the missing trigger defaults to the historical requested semantics
        assert!(matches!(
            &parsed.item,
            TranscriptItem::SubAgentExtended {
                trigger: crate::thorpal::events::ExtensionTrigger::Requested,
                ..
            }
        ));
        // the new triggers serialize snake_case under the same kind tag
        let item = TranscriptItem::SubAgentExtended {
            name: "ReverseEngineer(Europa)".to_owned(),
            granted: 3,
            remaining_allowance: 1,
            justification: "automatic extension at the turn cap (no request made)".to_owned(),
            trigger: crate::thorpal::events::ExtensionTrigger::Auto,
        };
        let encoded = serde_json::to_string(&item).expect("item should serialize");
        assert!(encoded.contains(r#""trigger":"auto""#));
    }

    /// A sub-agent breadcrumb from an older build (no role or swarm keys) still
    /// parses, and a swarm-attributed breadcrumb round-trips intact
    #[test]
    fn sub_agent_started_is_backward_compatible() {
        // an old line carries only the name and goal
        let old_line =
            r#"{"kind":"sub_agent_started","name":"ReverseEngineer(Europa)","goal":"triage"}"#;
        let parsed: TranscriptLine = serde_json::from_str(old_line).expect("old line should parse");
        // the missing role and swarm default to none
        assert!(matches!(
            &parsed.item,
            TranscriptItem::SubAgentStarted {
                role: None,
                swarm: None,
                ..
            }
        ));
        // a swarm-attributed breadcrumb keeps its membership through a round-trip
        let item = TranscriptItem::SubAgentStarted {
            name: "ReverseEngineer(Europa)".to_owned(),
            role: Some("ReverseEngineer".to_owned()),
            goal: "triage".to_owned(),
            swarm: Some("Swarm(Io)".to_owned()),
            resumed: false,
        };
        let encoded = serde_json::to_string(&item).expect("item should serialize");
        let decoded: TranscriptItem = serde_json::from_str(&encoded).expect("item should parse");
        assert!(matches!(
            &decoded,
            TranscriptItem::SubAgentStarted { swarm: Some(swarm), .. } if swarm == "Swarm(Io)"
        ));
    }

    /// A scope-less user line from an older build still parses, and a scoped
    /// line round-trips with its side-conversation name intact
    #[test]
    fn scoped_lines_are_backward_compatible() {
        // an old line carries no scope key at all
        let old_line = r#"{"kind":"assistant","text":"done"}"#;
        let parsed: TranscriptLine = serde_json::from_str(old_line).expect("old line should parse");
        // the missing scope defaults to the main conversation
        assert!(matches!(
            &parsed.item,
            TranscriptItem::Assistant { scope: None, .. }
        ));
        // a scoped line keeps its side-conversation name through a round-trip
        let item = TranscriptItem::User {
            text: "check the crypto".to_owned(),
            scope: Some("crypto".to_owned()),
        };
        let encoded = serde_json::to_string(&item).expect("item should serialize");
        let decoded: TranscriptItem = serde_json::from_str(&encoded).expect("item should parse");
        assert!(matches!(
            &decoded,
            TranscriptItem::User { scope: Some(scope), .. } if scope == "crypto"
        ));
        // a main-conversation line omits the scope key entirely
        let main_item = TranscriptItem::User {
            text: "hello".to_owned(),
            scope: None,
        };
        let encoded = serde_json::to_string(&main_item).expect("item should serialize");
        assert!(!encoded.contains("scope"));
    }

    /// The event mapping keeps displayable events and drops the rest
    #[test]
    fn from_event_mirrors_display_rules() {
        // the main agent's per-turn text is skipped to avoid duplication
        let main_msg = ThorPalEvent::AgentMessage {
            agent: "main".to_owned(),
            text: "thinking".to_owned(),
        };
        assert!(TranscriptItem::from_event(&main_msg).is_none());
        // a side conversation root's per-turn text is skipped the same way
        let side_msg = ThorPalEvent::AgentMessage {
            agent: "btw:crypto".to_owned(),
            text: "thinking".to_owned(),
        };
        assert!(TranscriptItem::from_event(&side_msg).is_none());
        // a sub-agent spawned from a side conversation is kept and attributed
        let side_sub_msg = ThorPalEvent::AgentMessage {
            agent: "btw:crypto/ReverseEngineer(Europa)".to_owned(),
            text: "found it".to_owned(),
        };
        assert!(matches!(
            TranscriptItem::from_event(&side_sub_msg),
            Some(TranscriptItem::SubAgentMessage { .. })
        ));
        // a sub-agent's text is kept and attributed
        let sub_msg = ThorPalEvent::AgentMessage {
            agent: "ReverseEngineer(Europa)".to_owned(),
            text: "found it".to_owned(),
        };
        assert!(matches!(
            TranscriptItem::from_event(&sub_msg),
            Some(TranscriptItem::SubAgentMessage { .. })
        ));
        // usage checkpoints are durable so offline consumers can rebuild
        // exact token timelines
        let usage = ThorPalEvent::UsageUpdated {
            agent: "main".to_owned(),
            input: 1,
            output: 2,
            cached: 0,
            cache_creation: 0,
            total: 3,
        };
        assert!(matches!(
            TranscriptItem::from_event(&usage),
            Some(TranscriptItem::Usage {
                input: 1,
                output: 2,
                total: 3,
                ..
            })
        ));
        // live tool log lines are telemetry, never transcript items
        let log = ThorPalEvent::ToolLog {
            call_id: "call-1".to_owned(),
            line: "[Ghidra stdout] analyzing".to_owned(),
        };
        assert!(TranscriptItem::from_event(&log).is_none());
    }

    /// Retry attempts and pause boundaries map onto durable lines: every
    /// attempt and episode close persists, a pause persists only on its
    /// initial publish, and a pause ending persists with the episode's cost
    #[test]
    fn retry_and_pause_lines_are_durable() {
        // a failed attempt maps with its backoff collapsed to one delta
        let last_attempt_at = chrono::Utc::now();
        let retrying = ThorPalEvent::LlmRetrying {
            agent: "main".to_owned(),
            retry_id: "main#1".to_owned(),
            attempt: 2,
            max_attempts: 12,
            error: "429 Too Many Requests".to_owned(),
            request_id: Some("req-13bb".to_owned()),
            last_attempt_at,
            next_attempt_at: last_attempt_at + chrono::Duration::milliseconds(2_000),
        };
        let item = TranscriptItem::from_event(&retrying).expect("attempt should persist");
        assert!(matches!(
            &item,
            TranscriptItem::LlmRetry { attempt: 2, backoff_ms: 2_000, request_id: Some(id), .. }
                if id == "req-13bb"
        ));
        // the serialized line carries the new kind tag
        let encoded = serde_json::to_string(&item).expect("item should serialize");
        assert!(encoded.contains(r#""kind":"llm_retry""#));
        // a backwards schedule clamps to a zero backoff instead of panicking
        let backwards = ThorPalEvent::LlmRetrying {
            agent: "main".to_owned(),
            retry_id: "main#1".to_owned(),
            attempt: 3,
            max_attempts: 12,
            error: "429 Too Many Requests".to_owned(),
            request_id: None,
            last_attempt_at,
            next_attempt_at: last_attempt_at - chrono::Duration::milliseconds(5),
        };
        assert!(matches!(
            TranscriptItem::from_event(&backwards),
            Some(TranscriptItem::LlmRetry { backoff_ms: 0, .. })
        ));
        // an episode close maps with its outcome
        let resolved = ThorPalEvent::LlmRetryResolved {
            agent: "main".to_owned(),
            retry_id: "main#1".to_owned(),
            attempts: 3,
            succeeded: true,
        };
        assert!(matches!(
            TranscriptItem::from_event(&resolved),
            Some(TranscriptItem::LlmRetryResolved {
                attempts: 3,
                succeeded: true,
                ..
            })
        ));
        // a pause's initial publish persists with its snake_case kind
        let paused = ThorPalEvent::SessionPaused {
            kind: PauseKind::Auth,
            agent: "main".to_owned(),
            error: "401 Unauthorized".to_owned(),
            request_id: Some("req-90dd".to_owned()),
            since: last_attempt_at,
            probe_attempts: 0,
            next_probe_at: last_attempt_at + chrono::Duration::seconds(60),
        };
        let item = TranscriptItem::from_event(&paused).expect("initial pause should persist");
        let encoded = serde_json::to_string(&item).expect("item should serialize");
        assert!(encoded.contains(r#""kind":"provider_paused""#));
        assert!(encoded.contains(r#""pause":"auth""#));
        // a probe refresh of the same pause is a live banner update only
        let refreshed = ThorPalEvent::SessionPaused {
            kind: PauseKind::Auth,
            agent: "main".to_owned(),
            error: "401 Unauthorized".to_owned(),
            request_id: None,
            since: last_attempt_at,
            probe_attempts: 1,
            next_probe_at: last_attempt_at + chrono::Duration::seconds(120),
        };
        assert!(TranscriptItem::from_event(&refreshed).is_none());
        // a pause ending persists the episode's whole cost under its own kind
        // (never the `session_resumed` restore barrier replay keys on)
        let resumed = ThorPalEvent::SessionResumed {
            kind: PauseKind::RetryBudgetExhausted,
            recovered: false,
            paused_ms: 184_000,
            probe_attempts: 2,
        };
        let item = TranscriptItem::from_event(&resumed).expect("pause end should persist");
        assert!(matches!(
            &item,
            TranscriptItem::ProviderResumed {
                recovered: false,
                paused_ms: 184_000,
                probe_attempts: 2,
                ..
            }
        ));
        let encoded = serde_json::to_string(&item).expect("item should serialize");
        assert!(encoded.contains(r#""kind":"provider_resumed""#));
        assert!(encoded.contains(r#""pause":"retry_budget_exhausted""#));
        assert!(!encoded.contains(r#""kind":"session_resumed""#));
    }

    /// Scrub and turn-failure lines carry their retry episode and request id,
    /// older lines without those keys still parse, and absent ids are omitted
    /// from the encoding entirely
    #[test]
    fn scrub_and_turn_failure_lines_link_their_episode() {
        // a live scrub maps with its episode key and request id attached
        let scrubbed = ThorPalEvent::LlmScrubbed {
            agent: "ReverseEngineer(Europa)".to_owned(),
            retry_id: "ReverseEngineer(Europa)#3".to_owned(),
            level: 2,
            error: "403 Forbidden".to_owned(),
            request_id: Some("req-42cc".to_owned()),
        };
        let item = TranscriptItem::from_event(&scrubbed).expect("scrub should persist");
        assert!(matches!(
            &item,
            TranscriptItem::LlmScrubbed { level: 2, retry_id: Some(retry), request_id: Some(req), .. }
                if retry == "ReverseEngineer(Europa)#3" && req == "req-42cc"
        ));
        // a live turn failure maps with its structured request id attached
        let failed = ThorPalEvent::TurnFailed {
            agent: "main".to_owned(),
            error: "boom (request id: req-77aa)".to_owned(),
            request_id: Some("req-77aa".to_owned()),
        };
        assert!(matches!(
            TranscriptItem::from_event(&failed),
            Some(TranscriptItem::TurnFailed { request_id: Some(id), .. }) if id == "req-77aa"
        ));
        // old lines predating the linkage keys still parse with none
        let old_scrub = r#"{"kind":"llm_scrubbed","agent":"main","level":1}"#;
        let parsed: TranscriptLine =
            serde_json::from_str(old_scrub).expect("old line should parse");
        assert!(matches!(
            &parsed.item,
            TranscriptItem::LlmScrubbed {
                retry_id: None,
                request_id: None,
                ..
            }
        ));
        let old_failure = r#"{"kind":"turn_failed","agent":"main","error":"boom"}"#;
        let parsed: TranscriptLine =
            serde_json::from_str(old_failure).expect("old line should parse");
        assert!(matches!(
            &parsed.item,
            TranscriptItem::TurnFailed {
                request_id: None,
                ..
            }
        ));
        // an absent id is omitted from the encoding, not written as null
        let bare = TranscriptItem::TurnFailed {
            agent: "main".to_owned(),
            error: "boom".to_owned(),
            request_id: None,
        };
        let encoded = serde_json::to_string(&bare).expect("item should serialize");
        assert!(!encoded.contains("request_id"));
    }

    /// A detector roster round-trips under its own kind and is never mapped
    /// from a live event (it is appended directly after each seeding pass)
    #[test]
    fn detectors_run_lines_are_seeding_appends() {
        // a roster line round-trips with zero-hit and failed entries intact
        let item = TranscriptItem::DetectorsRun {
            sample: Some("payload.dll".to_owned()),
            detectors: vec![
                DetectorRunLine {
                    name: "debugger_evasion".to_owned(),
                    technique: "T1622".to_owned(),
                    hits: 3,
                    failed: false,
                },
                DetectorRunLine {
                    name: "packing".to_owned(),
                    technique: "T1027.002".to_owned(),
                    hits: 0,
                    failed: true,
                },
            ],
            version: "9.9.9".to_owned(),
        };
        let encoded = serde_json::to_string(&item).expect("item should serialize");
        assert!(encoded.contains(r#""kind":"detectors_run""#));
        let decoded: TranscriptItem = serde_json::from_str(&encoded).expect("item should parse");
        let TranscriptItem::DetectorsRun {
            sample, detectors, ..
        } = decoded
        else {
            panic!("expected a detectors_run item");
        };
        assert_eq!(sample.as_deref(), Some("payload.dll"));
        assert_eq!(detectors.len(), 2);
        assert_eq!(detectors[1].hits, 0);
        assert!(detectors[1].failed);
        // the primary pass's line omits the sample key entirely
        let primary = TranscriptItem::DetectorsRun {
            sample: None,
            detectors: Vec::new(),
            version: "9.9.9".to_owned(),
        };
        let encoded = serde_json::to_string(&primary).expect("item should serialize");
        assert!(!encoded.contains("sample"));
    }

    /// An offered-tool manifest round-trips under its own kind and is never
    /// mapped from a live event (it is appended directly at build time)
    #[test]
    fn tools_offered_lines_are_build_time_appends() {
        // the manifest round-trips with its roster order intact
        let item = TranscriptItem::ToolsOffered {
            role: "main".to_owned(),
            tools: vec!["create_post".to_owned(), "think".to_owned()],
            version: "9.9.9".to_owned(),
        };
        let encoded = serde_json::to_string(&item).expect("item should serialize");
        assert!(encoded.contains(r#""kind":"tools_offered""#));
        let decoded: TranscriptItem = serde_json::from_str(&encoded).expect("item should parse");
        assert!(matches!(
            &decoded,
            TranscriptItem::ToolsOffered { role, tools, version }
                if role == "main"
                    && tools == &["create_post".to_owned(), "think".to_owned()]
                    && version == "9.9.9"
        ));
    }
}
