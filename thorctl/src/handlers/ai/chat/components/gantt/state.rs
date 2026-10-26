//! The gantt timeline view's events, state, and pure reducer
//!
//! [`GanttState::apply`] is the single fold both the live event drain and the
//! transcript replay run their [`GanttEvent`]s through, so a resumed session
//! rebuilds the same spans the live run produced. Live events carry no capture
//! time of their own and are stamped at receive time; persisted transcript
//! lines carry their original capture time, so a replayed timeline keeps its
//! real proportions. Token usage replays from the transcript's durable
//! per-turn `usage` lines where they exist; whatever the lines do not cover
//! (sessions saved before usage lines existed) is seeded from the snapshot
//! ledger's remainder instead.
//!
//! Unlike the traversal and stats folds, this one keeps everything a timeline
//! row needs in one place: swarm and agent spans with their goals, and every
//! tool call as an interval correlated on its `call_id` with its full argument
//! and result text for the detail panel. Per-call token figures are chars/4
//! context-size estimates (there is no per-call usage telemetry), so the view
//! renders them `~`-prefixed; per-agent token buckets are exact.
//!
//! The fold also keeps [`ActivityIntervals`]: the merged stretches of time
//! where work was actually happening, which the view's gap-compressed axis is
//! built from. A [`GanttEvent::ReplayEnded`] marker closes anything a torn
//! transcript left open, so a resumed session never reads as still running.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use thorpal::{ThorPalEvent, TranscriptItem, TranscriptLine};

use super::super::agent_stats::{estimate_tokens, now_ms};

/// How long a quiet stretch must be before it splits activity segments
///
/// The longest quiet-but-working stretch observed in a real session is ~32s
/// (model thinking between turns), so five minutes never splits live work but
/// always splits a stop/resume downtime, which is hours at minimum.
pub const GAP_THRESHOLD_MS: u64 = 300_000;

/// One normalized gantt event, derivable from live events and transcript
/// lines alike
#[derive(Debug, Clone, PartialEq)]
pub enum GanttEvent {
    /// An agent (sub-agent or swarm member) came into existence
    AgentStarted {
        /// The unique per-call display name of the agent
        agent: String,
        /// The human-facing role prefix of the agent, when known
        role: Option<String>,
        /// The overarching goal this agent's delegation serves, when known
        goal: Option<String>,
        /// The display name of the swarm this agent runs under, if any
        swarm: Option<String>,
        /// Milliseconds since the Unix epoch when the start was captured
        ts: Option<u64>,
    },
    /// A swarm of concurrent sub-agents was spawned
    SwarmStarted {
        /// The unique per-call display name of the swarm
        swarm: String,
        /// The overarching goal every task in the swarm serves
        goal: String,
        /// Milliseconds since the Unix epoch when the start was captured
        ts: Option<u64>,
    },
    /// Every member of a swarm finished, successfully or not
    SwarmFinished {
        /// The unique per-call display name of the swarm
        swarm: String,
        /// How many members finished their task successfully
        succeeded: usize,
        /// How many members failed with an error
        failed: usize,
        /// Milliseconds since the Unix epoch when the finish was captured
        ts: Option<u64>,
    },
    /// A tool call started
    ToolCall {
        /// The display name of the agent making the call
        agent: String,
        /// The call id correlating this call with its result
        call_id: String,
        /// The name of the tool being called
        tool: String,
        /// The raw JSON argument string the tool was called with
        args: String,
        /// Milliseconds since the Unix epoch when the call was captured
        ts: Option<u64>,
    },
    /// A tool call finished
    ToolResult {
        /// The display name of the agent that made the call
        agent: String,
        /// The call id correlating this result with its call
        call_id: String,
        /// The name of the tool that ran
        tool: String,
        /// The model-visible rendered output of the tool call
        result: String,
        /// Whether the tool call succeeded
        success: bool,
        /// Milliseconds since the Unix epoch when the result was captured
        ts: Option<u64>,
    },
    /// An agent produced assistant text (extends its lifetime window only)
    Message {
        /// The display name of the agent that produced the text
        agent: String,
        /// Milliseconds since the Unix epoch when the text was captured
        ts: Option<u64>,
    },
    /// One model turn's token usage, from a live checkpoint or a durable line
    Usage {
        /// The display name of the agent this usage belongs to
        agent: String,
        /// The input (prompt) tokens this turn consumed
        input: u64,
        /// The output (completion) tokens this turn produced
        output: u64,
        /// The input tokens this turn read from the provider's prompt cache
        cached: u64,
        /// The input tokens this turn wrote into the provider's prompt cache
        cache_creation: u64,
        /// Milliseconds since the Unix epoch when the checkpoint was captured
        ts: Option<u64>,
    },
    /// A resume-time token seed from the snapshot's per-agent usage ledger
    ///
    /// Carries no capture time: the snapshot stores only accumulated buckets,
    /// so a seed fills an agent's tokens without touching its lifetime window.
    SeedUsage {
        /// The display name of the agent this seed belongs to
        agent: String,
        /// The accumulated input (prompt) tokens
        input: u64,
        /// The accumulated output (completion) tokens
        output: u64,
        /// The accumulated cache-read input tokens
        cached: u64,
        /// The accumulated cache-write input tokens
        cache_creation: u64,
    },
    /// A resume-time marker pushed after the whole transcript replayed
    ///
    /// The session that wrote the transcript is gone, so anything still open
    /// when it ended (a torn tool call, a running swarm) was interrupted, not
    /// left running -- the fold settles it so no bar grows toward now forever.
    ReplayEnded,
}

impl GanttEvent {
    /// Maps a live event onto a gantt event, if it is one
    ///
    /// Live events carry no capture time of their own, so they are stamped at
    /// receive time -- the same convention the traversal and stats logs use.
    ///
    /// # Arguments
    ///
    /// * `event` - The live event to map
    pub fn from_event(event: &ThorPalEvent) -> Option<GanttEvent> {
        // stamp live events as they arrive so spans can be derived
        let ts = now_ms();
        match event {
            // a sub-agent coming alive opens its span, with its goal and swarm
            ThorPalEvent::SubAgentStarted {
                name,
                role,
                goal,
                swarm,
                resumed: _,
            } => Some(GanttEvent::AgentStarted {
                agent: name.clone(),
                role: Some(role.clone()),
                goal: Some(goal.clone()),
                swarm: swarm.clone(),
                ts,
            }),
            // a swarm's start and finish bound its wall-clock window
            ThorPalEvent::SwarmStarted { name, goal, .. } => Some(GanttEvent::SwarmStarted {
                swarm: name.clone(),
                goal: goal.clone(),
                ts,
            }),
            ThorPalEvent::SwarmFinished {
                name,
                succeeded,
                failed,
            } => Some(GanttEvent::SwarmFinished {
                swarm: name.clone(),
                succeeded: *succeeded,
                failed: *failed,
                ts,
            }),
            // a tool call opens an interval; its result closes it
            ThorPalEvent::ToolCallStarted {
                agent,
                call_id,
                tool,
                args,
            } => Some(GanttEvent::ToolCall {
                agent: agent.clone(),
                call_id: call_id.clone(),
                tool: tool.clone(),
                args: args.clone(),
                ts,
            }),
            ThorPalEvent::ToolCallFinished {
                agent,
                call_id,
                tool,
                result,
                success,
            } => Some(GanttEvent::ToolResult {
                agent: agent.clone(),
                call_id: call_id.clone(),
                tool: tool.clone(),
                result: result.clone(),
                success: *success,
                ts,
            }),
            // assistant text only extends its agent's lifetime window
            ThorPalEvent::AgentMessage { agent, .. } => Some(GanttEvent::Message {
                agent: agent.clone(),
                ts,
            }),
            // a per-turn usage checkpoint feeds the token buckets
            ThorPalEvent::UsageUpdated {
                agent,
                input,
                output,
                cached,
                cache_creation,
                ..
            } => Some(GanttEvent::Usage {
                agent: agent.clone(),
                input: *input,
                output: *output,
                cached: *cached,
                cache_creation: *cache_creation,
                ts,
            }),
            // nothing else changes any span
            _ => None,
        }
    }

    /// Maps a persisted transcript line onto a gantt event, if it is one
    ///
    /// Durable per-turn `usage` lines fold token buckets exactly like their
    /// live checkpoints; whatever they do not cover is seeded separately from
    /// the snapshot ledger's remainder.
    ///
    /// # Arguments
    ///
    /// * `line` - The persisted transcript line to map
    pub fn from_transcript(line: &TranscriptLine) -> Option<GanttEvent> {
        match &line.item {
            // a sub-agent breadcrumb, with its goal and swarm attribution
            TranscriptItem::SubAgentStarted {
                name,
                role,
                goal,
                swarm,
                resumed: _,
            } => Some(GanttEvent::AgentStarted {
                agent: name.clone(),
                role: role.clone(),
                goal: Some(goal.clone()),
                swarm: swarm.clone(),
                ts: line.ts,
            }),
            // a swarm's start and finish breadcrumbs
            TranscriptItem::SwarmStarted { name, goal, .. } => Some(GanttEvent::SwarmStarted {
                swarm: name.clone(),
                goal: goal.clone(),
                ts: line.ts,
            }),
            TranscriptItem::SwarmFinished {
                name,
                succeeded,
                failed,
            } => Some(GanttEvent::SwarmFinished {
                swarm: name.clone(),
                succeeded: *succeeded,
                failed: *failed,
                ts: line.ts,
            }),
            // a tool call and its correlated result
            TranscriptItem::ToolCall {
                agent,
                call_id,
                tool,
                args,
            } => Some(GanttEvent::ToolCall {
                agent: agent.clone(),
                call_id: call_id.clone(),
                tool: tool.clone(),
                args: args.clone(),
                ts: line.ts,
            }),
            TranscriptItem::ToolResult {
                agent,
                call_id,
                tool,
                result,
                success,
            } => Some(GanttEvent::ToolResult {
                agent: agent.clone(),
                call_id: call_id.clone(),
                tool: tool.clone(),
                result: result.clone(),
                success: *success,
                ts: line.ts,
            }),
            // the persisted forms of assistant text extend lifetime windows
            TranscriptItem::SubAgentMessage { agent, .. } => Some(GanttEvent::Message {
                agent: agent.clone(),
                ts: line.ts,
            }),
            TranscriptItem::Assistant { scope, .. } => Some(GanttEvent::Message {
                // a side conversation's answers extend its own root's span
                agent: match scope {
                    Some(scope) => format!("btw:{scope}"),
                    None => "main".to_owned(),
                },
                ts: line.ts,
            }),
            // a durable per-turn usage checkpoint feeds the token buckets
            // exactly like its live counterpart
            TranscriptItem::Usage {
                agent,
                input,
                output,
                cached,
                cache_creation,
                ..
            } => Some(GanttEvent::Usage {
                agent: agent.clone(),
                input: *input,
                output: *output,
                cached: *cached,
                cache_creation: *cache_creation,
                ts: line.ts,
            }),
            // nothing else changes any span
            _ => None,
        }
    }
}

/// The merged stretches of time where work was actually happening
///
/// Every stamped event records a point and every closed tool call records its
/// full interval; anything closer than [`GAP_THRESHOLD_MS`] fuses, so the list
/// keeps one entry per burst of work and the space between entries is exactly
/// the idle downtime the view's gap-compressed axis collapses. Agent and swarm
/// envelope spans are deliberately not recorded: the main agent's envelope
/// covers a stop/restart gap, which is precisely the time to compress.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ActivityIntervals {
    /// The merged `(start, end)` intervals in ascending, disjoint order
    segments: Vec<(u64, u64)>,
}

impl ActivityIntervals {
    /// Records one activity interval, fusing it with its bridgeable neighbors
    ///
    /// # Arguments
    ///
    /// * `start` - Milliseconds since the epoch when the activity started
    /// * `end` - Milliseconds since the epoch when the activity ended
    pub fn record(&mut self, start: u64, end: u64) {
        // normalize an inverted interval rather than trusting the caller
        let (mut start, mut end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        // find the first segment close enough to bridge with the new interval
        let from = self
            .segments
            .partition_point(|(_, seg_end)| seg_end.saturating_add(GAP_THRESHOLD_MS) < start);
        // fuse every segment the new interval overlaps or bridges into it (a
        // closed call recorded late can swallow several point segments)
        let mut to = from;
        while let Some((seg_start, seg_end)) = self.segments.get(to) {
            if *seg_start > end.saturating_add(GAP_THRESHOLD_MS) {
                break;
            }
            start = start.min(*seg_start);
            end = end.max(*seg_end);
            to += 1;
        }
        // replace the fused range with the single merged segment
        self.segments.splice(from..to, [(start, end)]);
    }

    /// The merged intervals in ascending, disjoint order
    pub fn segments(&self) -> &[(u64, u64)] {
        &self.segments
    }

    /// The end of the merged segment containing a stamp, if one contains it
    ///
    /// # Arguments
    ///
    /// * `ts` - The stamp to look up
    pub fn segment_end_at(&self, ts: u64) -> Option<u64> {
        // find the last segment starting at or before the stamp
        let index = self.segments.partition_point(|(start, _)| *start <= ts);
        // the stamp must land inside that segment, not past its end
        self.segments[..index]
            .last()
            .filter(|(_, end)| ts <= *end)
            .map(|(_, end)| *end)
    }
}

/// One tool call's interval on the timeline, with its full payloads
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpan {
    /// The call id correlating this call with its result
    pub call_id: String,
    /// The display name of the agent that made the call
    pub agent: String,
    /// The name of the tool that was called
    pub tool: String,
    /// The raw JSON argument string the tool was called with
    pub args: String,
    /// The model-visible rendered output, once the call finished
    pub result: Option<String>,
    /// Whether the call succeeded, once it finished
    pub success: Option<bool>,
    /// Milliseconds since the epoch when the call started, if stamped
    pub start_ts: Option<u64>,
    /// Milliseconds since the epoch when the call finished, if stamped
    pub end_ts: Option<u64>,
    /// The estimated tokens of the call's argument payload (chars/4)
    pub args_est: u64,
    /// The estimated tokens of the call's result payload (chars/4)
    pub result_est: u64,
    /// Whether a replayed transcript ended before this call's result arrived
    pub interrupted: bool,
}

impl ToolSpan {
    /// Whether this call is still in flight (no result has arrived)
    pub fn in_flight(&self) -> bool {
        // a call without a recorded success flag has not finished yet, unless
        // the session that made it died first
        self.success.is_none() && !self.interrupted
    }
}

/// One agent's span on the timeline, with its goal and token buckets
#[derive(Debug, Clone, PartialEq)]
pub struct AgentSpan {
    /// The unique per-call display name of this agent
    pub agent: String,
    /// The human-facing role prefix of this agent, when known
    pub role: Option<String>,
    /// The overarching goal this agent's delegation serves, when known
    pub goal: Option<String>,
    /// The display name of the swarm this agent ran under, if any
    pub swarm: Option<String>,
    /// The stable palette slot assigned in first-seen order
    pub color_slot: usize,
    /// Milliseconds since the epoch of this agent's first observed event
    pub first_ts: Option<u64>,
    /// Milliseconds since the epoch of this agent's last observed event
    pub last_ts: Option<u64>,
    /// How many of this agent's tool calls are currently in flight
    pub busy_calls: u32,
    /// The accumulated input (prompt) tokens
    pub input: u64,
    /// The accumulated output (completion) tokens
    pub output: u64,
    /// The accumulated cache-read input tokens
    pub cached: u64,
    /// The accumulated cache-write input tokens
    pub cache_creation: u64,
    /// This agent's tokens were seeded from a resumed snapshot
    pub seeded: bool,
    /// The indices into [`GanttState::calls`] of this agent's tool calls
    pub calls: Vec<usize>,
}

impl AgentSpan {
    /// Creates a new, empty span for an agent
    ///
    /// # Arguments
    ///
    /// * `agent` - The display name of the agent
    /// * `color_slot` - The stable palette slot assigned to the agent
    fn new(agent: &str, color_slot: usize) -> Self {
        AgentSpan {
            agent: agent.to_owned(),
            role: None,
            goal: None,
            swarm: None,
            color_slot,
            first_ts: None,
            last_ts: None,
            busy_calls: 0,
            input: 0,
            output: 0,
            cached: 0,
            cache_creation: 0,
            seeded: false,
            calls: Vec::new(),
        }
    }

    /// This agent's span end, extended to `now` while any call is in flight
    ///
    /// # Arguments
    ///
    /// * `now` - The current wall clock in milliseconds since the epoch
    pub fn end(&self, now: Option<u64>) -> Option<u64> {
        // a busy agent is still alive, so its span runs up to now when known
        if self.busy_calls > 0 {
            now.or(self.last_ts)
        } else {
            self.last_ts
        }
    }

    /// The accumulated tokens across every bucket
    pub fn total_tokens(&self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cached)
            .saturating_add(self.cache_creation)
    }
}

/// One swarm's span on the timeline, with its goal and members
#[derive(Debug, Clone, PartialEq)]
pub struct SwarmSpan {
    /// The unique per-call display name of this swarm
    pub name: String,
    /// The overarching goal every task in this swarm serves
    pub goal: String,
    /// The member agents' display names, in the order they came alive
    pub members: Vec<String>,
    /// The `(succeeded, failed)` outcome once every member finished
    pub outcome: Option<(usize, usize)>,
    /// Milliseconds since the epoch when this swarm started, if stamped
    pub first_ts: Option<u64>,
    /// Milliseconds since the epoch when this swarm finished, if stamped
    pub last_ts: Option<u64>,
    /// Whether a replayed transcript ended before this swarm finished
    pub interrupted: bool,
}

/// The whole reduced timeline state (a pure value, no locks)
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GanttState {
    /// Every agent's span, keyed by display name
    pub agents: BTreeMap<String, AgentSpan>,
    /// Every swarm's span, keyed by display name
    pub swarms: BTreeMap<String, SwarmSpan>,
    /// Every tool call's interval, in arrival order
    pub calls: Vec<ToolSpan>,
    /// The in-flight call ids mapped to their [`Self::calls`] indices
    pending: HashMap<String, usize>,
    /// Milliseconds since the epoch of the earliest stamped event
    pub min_ts: Option<u64>,
    /// Milliseconds since the epoch of the latest stamped event
    pub max_ts: Option<u64>,
    /// The merged stretches of time where work was actually happening
    pub activity: ActivityIntervals,
    /// The next palette slot to hand out in first-seen order
    next_slot: usize,
}

impl GanttState {
    /// Folds one event into the state (the reducer; pure and infallible)
    ///
    /// # Arguments
    ///
    /// * `event` - The gantt event to fold in
    pub fn apply(&mut self, event: &GanttEvent) {
        match event {
            // an agent came alive: open its span and record its attribution
            GanttEvent::AgentStarted {
                agent,
                role,
                goal,
                swarm,
                ts,
            } => {
                let span = self.span(agent);
                // keep the first non-empty role, goal, and swarm we learn
                if span.role.is_none() {
                    span.role = role.clone();
                }
                if span.goal.is_none() {
                    span.goal = goal.clone();
                }
                if span.swarm.is_none() {
                    span.swarm = swarm.clone();
                }
                touch(span, *ts);
                self.window(*ts);
                // register the member on its swarm so grouping knows it
                if let Some(swarm) = swarm {
                    let swarm = self.swarm(swarm);
                    if !swarm.members.iter().any(|member| member == agent) {
                        swarm.members.push(agent.clone());
                    }
                }
            }
            // a swarm came alive: record its goal and start time
            GanttEvent::SwarmStarted { swarm, goal, ts } => {
                let swarm = self.swarm(swarm);
                swarm.goal = goal.clone();
                swarm.first_ts = swarm.first_ts.or(*ts);
                // a start is unambiguous evidence the swarm is running, so
                // clear any latches a replayed name collision left behind
                // (display names restart every process, so a resumed session
                // can mint a name the dead session already used)
                swarm.interrupted = false;
                swarm.outcome = None;
                swarm.last_ts = None;
                self.window(*ts);
            }
            // a swarm finished: record its outcome and finish time
            GanttEvent::SwarmFinished {
                swarm,
                succeeded,
                failed,
                ts,
            } => {
                let swarm = self.swarm(swarm);
                swarm.outcome = Some((*succeeded, *failed));
                swarm.last_ts = *ts;
                self.window(*ts);
            }
            // a tool call started: open its interval and flag the agent busy
            GanttEvent::ToolCall {
                agent,
                call_id,
                tool,
                args,
                ts,
            } => {
                // record the interval and remember it by call id until the
                // result settles it
                let index = self.calls.len();
                self.calls.push(ToolSpan {
                    call_id: call_id.clone(),
                    agent: agent.clone(),
                    tool: tool.clone(),
                    args: args.clone(),
                    result: None,
                    success: None,
                    start_ts: *ts,
                    end_ts: None,
                    args_est: estimate_tokens(args),
                    result_est: 0,
                    interrupted: false,
                });
                self.pending.insert(call_id.clone(), index);
                // the owning agent gains a busy call and the interval's index
                let span = self.span(agent);
                span.busy_calls += 1;
                span.calls.push(index);
                touch(span, *ts);
                self.window(*ts);
            }
            // a tool call finished: close its interval and settle the agent
            GanttEvent::ToolResult {
                agent,
                call_id,
                result,
                success,
                ts,
                ..
            } => {
                // settle the matching in-flight interval; an orphan result
                // (torn transcript) still touches its agent's window below
                let mut closed = None;
                if let Some(index) = self.pending.remove(call_id)
                    && let Some(call) = self.calls.get_mut(index)
                {
                    call.result = Some(result.clone());
                    call.success = Some(*success);
                    call.end_ts = *ts;
                    call.result_est = estimate_tokens(result);
                    // remember the settled interval for the activity record
                    if let (Some(start), Some(end)) = (call.start_ts, call.end_ts) {
                        closed = Some((start, end));
                    }
                }
                // a closed call's whole interval is activity, so a long call's
                // quiet interior never reads as an idle gap
                if let Some((start, end)) = closed {
                    self.activity.record(start, end);
                }
                let span = self.span(agent);
                span.busy_calls = span.busy_calls.saturating_sub(1);
                touch(span, *ts);
                self.window(*ts);
            }
            // assistant text only extends its agent's lifetime window
            GanttEvent::Message { agent, ts } => {
                let span = self.span(agent);
                touch(span, *ts);
                self.window(*ts);
            }
            // a usage checkpoint, live or replayed: add the token buckets
            GanttEvent::Usage {
                agent,
                input,
                output,
                cached,
                cache_creation,
                ts,
            } => {
                let span = self.span(agent);
                span.input = span.input.saturating_add(*input);
                span.output = span.output.saturating_add(*output);
                span.cached = span.cached.saturating_add(*cached);
                span.cache_creation = span.cache_creation.saturating_add(*cache_creation);
                touch(span, *ts);
                self.window(*ts);
            }
            // a resume seed: add the buckets without touching any window
            GanttEvent::SeedUsage {
                agent,
                input,
                output,
                cached,
                cache_creation,
            } => {
                let span = self.span(agent);
                span.input = span.input.saturating_add(*input);
                span.output = span.output.saturating_add(*output);
                span.cached = span.cached.saturating_add(*cached);
                span.cache_creation = span.cache_creation.saturating_add(*cache_creation);
                span.seeded = true;
            }
            // the replayed transcript ended: the session that wrote it is
            // gone, so settle everything it left open as interrupted
            GanttEvent::ReplayEnded => {
                // settle every dangling call at its agent's last sign of life
                let dangling: Vec<usize> = self.pending.drain().map(|(_, index)| index).collect();
                for index in dangling {
                    // read the owner's last stamp before mutating the call
                    let last = self
                        .calls
                        .get(index)
                        .and_then(|call| self.agents.get(&call.agent))
                        .and_then(|span| span.last_ts);
                    // cap the evidence at the end of the burst of work the
                    // call started in: an appended later session's stamps for
                    // the same agent must not inflate a torn call across the
                    // downtime between them
                    let burst_end = self
                        .calls
                        .get(index)
                        .and_then(|call| call.start_ts)
                        .and_then(|start| self.activity.segment_end_at(start));
                    let last = match (last, burst_end) {
                        (Some(last), Some(burst_end)) => Some(last.min(burst_end)),
                        (last, burst_end) => last.or(burst_end),
                    };
                    if let Some(call) = self.calls.get_mut(index) {
                        call.interrupted = true;
                        // close at the latest evidence: the call's own start
                        // or the owner's capped last stamp, whichever is later
                        call.end_ts = match (call.start_ts, last) {
                            (Some(start), Some(last)) => Some(start.max(last)),
                            (start, last) => start.or(last),
                        };
                    }
                }
                // no agent can still be busy once its session is gone
                for span in self.agents.values_mut() {
                    span.busy_calls = 0;
                }
                // an outcome-less swarm was interrupted, not left running
                for swarm in self.swarms.values_mut() {
                    if swarm.outcome.is_none() {
                        swarm.interrupted = true;
                    }
                }
            }
        }
    }

    /// Whether any work is genuinely still in flight
    ///
    /// The timeline's right edge only tracks the wall clock while this holds,
    /// so a resumed session with nothing running stays frozen at its last
    /// stamp instead of compressing the whole run into the left margin.
    pub fn has_open_work(&self) -> bool {
        // any unsettled call, busy agent, or still-running swarm counts
        !self.pending.is_empty()
            || self.agents.values().any(|span| span.busy_calls > 0)
            || self
                .swarms
                .values()
                .any(|swarm| swarm.outcome.is_none() && !swarm.interrupted)
    }

    /// Gets or creates the span for an agent, assigning its palette slot
    ///
    /// Any event upserts its agent, so `"main"` (which is never announced via
    /// an agent-started breadcrumb) still gets a span on its first activity.
    ///
    /// # Arguments
    ///
    /// * `agent` - The display name of the agent
    fn span(&mut self, agent: &str) -> &mut AgentSpan {
        // palette slots are handed out in first-seen order and never change
        let slot = self.next_slot;
        let span = self
            .agents
            .entry(agent.to_owned())
            .or_insert_with(|| AgentSpan::new(agent, slot));
        // only a freshly inserted span consumed the slot
        if span.color_slot == slot {
            self.next_slot += 1;
        }
        span
    }

    /// Gets or creates the span for a swarm
    ///
    /// # Arguments
    ///
    /// * `name` - The display name of the swarm
    fn swarm(&mut self, name: &str) -> &mut SwarmSpan {
        self.swarms
            .entry(name.to_owned())
            .or_insert_with(|| SwarmSpan {
                name: name.to_owned(),
                goal: String::new(),
                members: Vec::new(),
                outcome: None,
                first_ts: None,
                last_ts: None,
                interrupted: false,
            })
    }

    /// Folds an event's capture time into the session-wide window
    ///
    /// # Arguments
    ///
    /// * `ts` - The event's capture time, if it carried one
    fn window(&mut self, ts: Option<u64>) {
        if let Some(ts) = ts {
            // the first stamp wins the start; every stamp can extend the end
            self.min_ts = Some(self.min_ts.map_or(ts, |min| min.min(ts)));
            self.max_ts = Some(self.max_ts.map_or(ts, |max| max.max(ts)));
            // every stamp is evidence of activity for the gap-compressed axis
            self.activity.record(ts, ts);
        }
    }

    /// A swarm's effective span window, falling back to its members
    ///
    /// A swarm's own stamps bound it when present; a still-running swarm (or a
    /// legacy transcript without swarm stamps) falls back to the earliest
    /// member start and the latest member end (extended to `now` while any
    /// member is busy).
    ///
    /// # Arguments
    ///
    /// * `swarm` - The swarm span to window
    /// * `now` - The current wall clock in milliseconds since the epoch
    pub fn swarm_window(&self, swarm: &SwarmSpan, now: Option<u64>) -> (Option<u64>, Option<u64>) {
        // gather the member spans the fallback reads
        let members: Vec<&AgentSpan> = swarm
            .members
            .iter()
            .filter_map(|member| self.agents.get(member))
            .collect();
        // the swarm's own start stamp wins; else the earliest member start
        let start = swarm
            .first_ts
            .or_else(|| members.iter().filter_map(|span| span.first_ts).min());
        // the swarm's own finish stamp wins; else the latest member end
        let end = swarm
            .last_ts
            .or_else(|| members.iter().filter_map(|span| span.end(now)).max());
        (start, end)
    }
}

/// Folds an event's capture time into an agent span's first/last window
///
/// # Arguments
///
/// * `span` - The agent span to update
/// * `ts` - The event's capture time, if it carried one
fn touch(span: &mut AgentSpan, ts: Option<u64>) {
    if let Some(ts) = ts {
        // the first stamp wins the start; every stamp can extend the end
        span.first_ts = Some(span.first_ts.map_or(ts, |first| first.min(ts)));
        span.last_ts = Some(span.last_ts.map_or(ts, |last| last.max(ts)));
    }
}

/// One identity in the timeline's row tree
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum RowKey {
    /// A swarm's row, keyed by its display name
    Swarm(String),
    /// An agent's row, keyed by its display name
    Agent(String),
    /// A tool call's row, keyed by its call id
    Call(String),
}

/// One laid-out row of the timeline
#[derive(Debug, Clone, PartialEq)]
pub struct GanttRow {
    /// The identity this row plots
    pub key: RowKey,
    /// How deep in the tree this row sits (roots are zero)
    pub depth: u8,
    /// Whether this row can expand to show children
    pub expandable: bool,
    /// Whether this row is currently expanded
    pub expanded: bool,
    /// Milliseconds since the epoch when this row's span starts, if stamped
    pub start: Option<u64>,
    /// Milliseconds since the epoch when this row's span ends; `None` while
    /// the span has a start but is still in flight (the bar extends to now)
    pub end: Option<u64>,
}

/// Lays out the timeline's visible rows for the current expansion state
///
/// Roots are the swarms and the non-swarm agents (including `"main"`), merged
/// and sorted chronologically by effective start (ties by name). An expanded
/// swarm is followed by its member agent rows in spawn order; an expanded
/// agent is followed by one row per tool call in call order.
///
/// # Arguments
///
/// * `state` - The reduced timeline state to lay out
/// * `expanded` - The keys of the rows that are currently expanded
/// * `now` - The current wall clock in milliseconds since the epoch
pub fn build_rows(
    state: &GanttState,
    expanded: &BTreeSet<RowKey>,
    now: Option<u64>,
) -> Vec<GanttRow> {
    // collect the roots with their effective start times for sorting
    let mut roots: Vec<(Option<u64>, RowKey)> = Vec::new();
    for swarm in state.swarms.values() {
        let (start, _) = state.swarm_window(swarm, now);
        roots.push((start, RowKey::Swarm(swarm.name.clone())));
    }
    for span in state.agents.values() {
        // swarm members render under their swarm, not at the root
        if span.swarm.is_none() {
            roots.push((span.first_ts, RowKey::Agent(span.agent.clone())));
        }
    }
    // order the roots chronologically, with unstamped rows and ties by name
    roots.sort_by(|(a_ts, a_key), (b_ts, b_key)| {
        let a_ts = a_ts.unwrap_or(u64::MAX);
        let b_ts = b_ts.unwrap_or(u64::MAX);
        a_ts.cmp(&b_ts).then_with(|| a_key.cmp(b_key))
    });
    // walk the tree, descending into expanded rows
    let mut rows = Vec::new();
    for (_, key) in roots {
        match key {
            RowKey::Swarm(name) => push_swarm(state, expanded, now, &name, &mut rows),
            RowKey::Agent(name) => push_agent(state, expanded, &name, 0, &mut rows),
            // calls are never roots
            RowKey::Call(_) => (),
        }
    }
    rows
}

/// Pushes one swarm's row and, when expanded, its member agent rows
///
/// # Arguments
///
/// * `state` - The reduced timeline state being laid out
/// * `expanded` - The keys of the rows that are currently expanded
/// * `now` - The current wall clock in milliseconds since the epoch
/// * `name` - The display name of the swarm to push
/// * `rows` - The row list being built
fn push_swarm(
    state: &GanttState,
    expanded: &BTreeSet<RowKey>,
    now: Option<u64>,
    name: &str,
    rows: &mut Vec<GanttRow>,
) {
    let Some(swarm) = state.swarms.get(name) else {
        return;
    };
    let key = RowKey::Swarm(name.to_owned());
    let is_expanded = expanded.contains(&key);
    // a swarm with no recorded outcome is still running, so its bar grows --
    // unless a replayed transcript ended first and the swarm was interrupted
    let (start, mut end) = state.swarm_window(swarm, now);
    if swarm.outcome.is_none() && !swarm.interrupted {
        end = None;
    }
    rows.push(GanttRow {
        key,
        depth: 0,
        expandable: !swarm.members.is_empty(),
        expanded: is_expanded,
        start,
        end,
    });
    // an expanded swarm shows its members in the order they came alive
    if is_expanded {
        for member in &swarm.members {
            push_agent(state, expanded, member, 1, rows);
        }
    }
}

/// Pushes one agent's row and, when expanded, its tool call rows
///
/// # Arguments
///
/// * `state` - The reduced timeline state being laid out
/// * `expanded` - The keys of the rows that are currently expanded
/// * `name` - The display name of the agent to push
/// * `depth` - How deep in the tree this agent sits
/// * `rows` - The row list being built
fn push_agent(
    state: &GanttState,
    expanded: &BTreeSet<RowKey>,
    name: &str,
    depth: u8,
    rows: &mut Vec<GanttRow>,
) {
    let Some(span) = state.agents.get(name) else {
        return;
    };
    let key = RowKey::Agent(name.to_owned());
    let is_expanded = expanded.contains(&key);
    // a busy agent's bar is open-ended so it keeps growing to now
    let end = if span.busy_calls > 0 {
        None
    } else {
        span.last_ts
    };
    rows.push(GanttRow {
        key,
        depth,
        expandable: !span.calls.is_empty(),
        expanded: is_expanded,
        start: span.first_ts,
        end,
    });
    // an expanded agent shows its tool calls in call order
    if is_expanded {
        for index in &span.calls {
            let Some(call) = state.calls.get(*index) else {
                continue;
            };
            rows.push(GanttRow {
                key: RowKey::Call(call.call_id.clone()),
                depth: depth + 1,
                expandable: false,
                expanded: false,
                start: call.start_ts,
                end: if call.in_flight() { None } else { call.end_ts },
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A started tool call event for an agent
    fn call(agent: &str, call_id: &str, ts: Option<u64>) -> GanttEvent {
        GanttEvent::ToolCall {
            agent: agent.to_owned(),
            call_id: call_id.to_owned(),
            tool: "memory_map".to_owned(),
            args: "{\"addr\": 4096}".to_owned(),
            ts,
        }
    }

    /// A finished tool call event for an agent
    fn result(agent: &str, call_id: &str, success: bool, ts: Option<u64>) -> GanttEvent {
        GanttEvent::ToolResult {
            agent: agent.to_owned(),
            call_id: call_id.to_owned(),
            tool: "memory_map".to_owned(),
            result: "a map of memory".to_owned(),
            success,
            ts,
        }
    }

    /// Tool calls pair with their results by call id into closed intervals
    #[test]
    fn calls_pair_with_results_by_call_id() {
        let mut state = GanttState::default();
        // two calls interleave; the second finishes first
        state.apply(&call("main", "call-1", Some(10)));
        state.apply(&call("main", "call-2", Some(20)));
        state.apply(&result("main", "call-2", true, Some(30)));
        // the first call is still in flight with no end stamp
        let first = &state.calls[0];
        assert!(first.in_flight());
        assert_eq!(first.start_ts, Some(10));
        assert_eq!(first.end_ts, None);
        // the second call settled with its result, outcome, and end stamp
        let second = &state.calls[1];
        assert!(!second.in_flight());
        assert_eq!(second.end_ts, Some(30));
        assert_eq!(second.result.as_deref(), Some("a map of memory"));
        assert_eq!(second.success, Some(true));
        assert!(second.result_est > 0);
        // the agent still has one busy call and owns both intervals
        let main = state.agents.get("main").expect("main span");
        assert_eq!(main.busy_calls, 1);
        assert_eq!(main.calls, vec![0, 1]);
        // the session window spans every stamp seen so far
        assert_eq!(state.min_ts, Some(10));
        assert_eq!(state.max_ts, Some(30));
    }

    /// Swarm spans fill from their breadcrumbs and member attributions
    #[test]
    fn swarm_spans_fill_from_breadcrumbs() {
        let mut state = GanttState::default();
        // the swarm starts, two members come alive under it, and it finishes
        state.apply(&GanttEvent::SwarmStarted {
            swarm: "Swarm(Io)".to_owned(),
            goal: "answer questions".to_owned(),
            ts: Some(10),
        });
        for name in ["ReverseEngineer(Europa)", "Triage(Callisto)"] {
            state.apply(&GanttEvent::AgentStarted {
                agent: name.to_owned(),
                role: Some("ReverseEngineer".to_owned()),
                goal: Some("find the key".to_owned()),
                swarm: Some("Swarm(Io)".to_owned()),
                ts: Some(20),
            });
        }
        state.apply(&GanttEvent::SwarmFinished {
            swarm: "Swarm(Io)".to_owned(),
            succeeded: 2,
            failed: 0,
            ts: Some(90),
        });
        // the swarm knows its goal, members, window, and outcome
        let swarm = state.swarms.get("Swarm(Io)").expect("swarm span");
        assert_eq!(swarm.goal, "answer questions");
        assert_eq!(swarm.members.len(), 2);
        assert_eq!(swarm.outcome, Some((2, 0)));
        assert_eq!(state.swarm_window(swarm, None), (Some(10), Some(90)));
        // each member span carries its goal and swarm attribution
        let member = state
            .agents
            .get("ReverseEngineer(Europa)")
            .expect("member span");
        assert_eq!(member.goal.as_deref(), Some("find the key"));
        assert_eq!(member.swarm.as_deref(), Some("Swarm(Io)"));
    }

    /// A resume seed fills token buckets without touching any window
    #[test]
    fn seed_fills_buckets_without_a_window() {
        let mut state = GanttState::default();
        state.apply(&GanttEvent::SeedUsage {
            agent: "main".to_owned(),
            input: 1_000,
            output: 100,
            cached: 50,
            cache_creation: 25,
        });
        // the buckets landed but no window opened anywhere
        let main = state.agents.get("main").expect("main span");
        assert_eq!(main.input, 1_000);
        assert_eq!(main.total_tokens(), 1_175);
        assert!(main.seeded);
        assert_eq!(main.first_ts, None);
        assert_eq!(state.min_ts, None);
    }

    /// Live events and transcript lines map onto equivalent gantt events
    #[test]
    fn from_event_and_from_transcript_agree() {
        use thorpal::{ThorPalEvent, TranscriptItem, TranscriptLine};
        // the live form of a sub-agent start with its goal and swarm
        let live = ThorPalEvent::SubAgentStarted {
            name: "ReverseEngineer(Europa)".to_owned(),
            role: "ReverseEngineer".to_owned(),
            goal: "triage".to_owned(),
            swarm: Some("Swarm(Io)".to_owned()),
            resumed: false,
        };
        let live_event = GanttEvent::from_event(&live).expect("live event maps");
        // the persisted form of the same breadcrumb
        let line = TranscriptLine {
            ts: None,
            item: TranscriptItem::SubAgentStarted {
                name: "ReverseEngineer(Europa)".to_owned(),
                role: Some("ReverseEngineer".to_owned()),
                goal: "triage".to_owned(),
                swarm: Some("Swarm(Io)".to_owned()),
                resumed: false,
            },
        };
        let replayed = GanttEvent::from_transcript(&line).expect("transcript line maps");
        // both agree once the live receive-time stamp is normalized away
        match (live_event, replayed) {
            (
                GanttEvent::AgentStarted {
                    agent: a,
                    role: ar,
                    goal: ag,
                    swarm: asw,
                    ..
                },
                GanttEvent::AgentStarted {
                    agent: b,
                    role: br,
                    goal: bg,
                    swarm: bsw,
                    ..
                },
            ) => {
                assert_eq!(a, b);
                assert_eq!(ar, br);
                assert_eq!(ag, bg);
                assert_eq!(asw, bsw);
            }
            other => panic!("expected matching agent starts, got {other:?}"),
        }
        // tool calls keep their call id and full args from both sources
        let live_call = ThorPalEvent::ToolCallStarted {
            agent: "main".to_owned(),
            call_id: "call-1".to_owned(),
            tool: "memory_map".to_owned(),
            args: "{}".to_owned(),
        };
        assert!(matches!(
            GanttEvent::from_event(&live_call),
            Some(GanttEvent::ToolCall { call_id, .. }) if call_id == "call-1"
        ));
        // usage checkpoints map from the live stream and the transcript alike
        let usage = ThorPalEvent::UsageUpdated {
            agent: "main".to_owned(),
            input: 1,
            output: 2,
            cached: 0,
            cache_creation: 0,
            total: 3,
        };
        assert!(matches!(
            GanttEvent::from_event(&usage),
            Some(GanttEvent::Usage { .. })
        ));
        let usage_line = TranscriptLine {
            ts: Some(42_000),
            item: TranscriptItem::Usage {
                agent: "main".to_owned(),
                input: 1,
                output: 2,
                cached: 0,
                cache_creation: 0,
                total: 3,
            },
        };
        assert!(matches!(
            GanttEvent::from_transcript(&usage_line),
            Some(GanttEvent::Usage {
                ts: Some(42_000),
                ..
            })
        ));
    }

    /// A durable usage line fills an agent's token buckets without seeding it
    #[test]
    fn transcript_usage_lines_fill_token_buckets() {
        let mut state = GanttState::default();
        // fold a replayed usage checkpoint for a sub-agent
        state.apply(&GanttEvent::Usage {
            agent: "ReverseEngineer(Europa)".to_owned(),
            input: 1_000,
            output: 100,
            cached: 9_000,
            cache_creation: 500,
            ts: Some(42_000),
        });
        // the buckets filled and the stamp bounds the agent's span
        let span = state
            .agents
            .get("ReverseEngineer(Europa)")
            .expect("a usage span");
        assert_eq!(span.total_tokens(), 10_600);
        assert!(!span.seeded);
        assert_eq!(span.first_ts, Some(42_000));
    }

    /// Rows order chronologically and descend into expanded entries
    #[test]
    fn rows_order_and_expand() {
        let mut state = GanttState::default();
        // the main agent works first, then a swarm with one member spawns
        state.apply(&call("main", "call-1", Some(10)));
        state.apply(&result("main", "call-1", true, Some(20)));
        state.apply(&GanttEvent::SwarmStarted {
            swarm: "Swarm(Io)".to_owned(),
            goal: "answer questions".to_owned(),
            ts: Some(30),
        });
        state.apply(&GanttEvent::AgentStarted {
            agent: "ReverseEngineer(Europa)".to_owned(),
            role: None,
            goal: Some("find the key".to_owned()),
            swarm: Some("Swarm(Io)".to_owned()),
            ts: Some(40),
        });
        state.apply(&call("ReverseEngineer(Europa)", "call-2", Some(50)));
        // collapsed: the main agent's root leads, the swarm follows
        let collapsed = build_rows(&state, &BTreeSet::new(), Some(100));
        assert_eq!(collapsed.len(), 2);
        assert_eq!(collapsed[0].key, RowKey::Agent("main".to_owned()));
        assert_eq!(collapsed[1].key, RowKey::Swarm("Swarm(Io)".to_owned()));
        // the unfinished swarm's bar is open-ended; the settled agent's is not
        assert!(collapsed[1].expandable);
        assert_eq!(collapsed[1].end, None);
        assert_eq!(collapsed[0].end, Some(20));
        // expanding the swarm and its member reveals the whole subtree
        let expanded: BTreeSet<RowKey> = BTreeSet::from([
            RowKey::Swarm("Swarm(Io)".to_owned()),
            RowKey::Agent("ReverseEngineer(Europa)".to_owned()),
        ]);
        let rows = build_rows(&state, &expanded, Some(100));
        let keys: Vec<&RowKey> = rows.iter().map(|row| &row.key).collect();
        assert_eq!(
            keys,
            vec![
                &RowKey::Agent("main".to_owned()),
                &RowKey::Swarm("Swarm(Io)".to_owned()),
                &RowKey::Agent("ReverseEngineer(Europa)".to_owned()),
                &RowKey::Call("call-2".to_owned()),
            ]
        );
        // depths step down the tree and the in-flight call is open-ended
        assert_eq!(rows[2].depth, 1);
        assert_eq!(rows[3].depth, 2);
        assert_eq!(rows[3].end, None);
        // the busy member's own bar is open-ended too
        assert_eq!(rows[2].end, None);
    }

    /// Activity intervals bridge nearby stamps and split across real downtime
    #[test]
    fn activity_intervals_merge_with_bridge() {
        let mut activity = ActivityIntervals::default();
        // points within the bridge fuse into one segment
        activity.record(1_000, 1_000);
        activity.record(4_000, 4_000);
        activity.record(35_000, 35_000);
        assert_eq!(activity.segments(), &[(1_000, 35_000)]);
        // a stop/restart-sized gap splits a new segment off
        let restart = 35_000 + 86_400_000;
        activity.record(restart, restart);
        assert_eq!(activity.segments(), &[(1_000, 35_000), (restart, restart)]);
        // a late interval spanning both segments fuses everything back
        activity.record(2_000, restart + 5_000);
        assert_eq!(activity.segments(), &[(1_000, restart + 5_000)]);
    }

    /// Every stamped event and closed call feeds the state's activity record
    #[test]
    fn state_records_activity_from_stamps_and_closed_calls() {
        let mut state = GanttState::default();
        // a call whose quiet interior is far longer than the gap threshold
        state.apply(&call("main", "call-1", Some(1_000)));
        state.apply(&result("main", "call-1", true, Some(2_000_000)));
        // the closed interval bridged the whole call into one segment
        assert_eq!(state.activity.segments(), &[(1_000, 2_000_000)]);
    }

    /// A replay-ended marker settles everything a torn transcript left open
    #[test]
    fn replay_ended_closes_dangling_work() {
        let mut state = GanttState::default();
        // a swarm starts and its member tears mid-call; a later message is the
        // member's last sign of life
        state.apply(&GanttEvent::SwarmStarted {
            swarm: "Swarm(Io)".to_owned(),
            goal: "answer questions".to_owned(),
            ts: Some(10),
        });
        state.apply(&GanttEvent::AgentStarted {
            agent: "ReverseEngineer(Europa)".to_owned(),
            role: Some("ReverseEngineer".to_owned()),
            goal: Some("find the key".to_owned()),
            swarm: Some("Swarm(Io)".to_owned()),
            ts: Some(20),
        });
        state.apply(&call("ReverseEngineer(Europa)", "call-1", Some(30)));
        state.apply(&GanttEvent::Message {
            agent: "ReverseEngineer(Europa)".to_owned(),
            ts: Some(40),
        });
        // the torn run reads as open work until the replay-ended marker lands
        assert!(state.has_open_work());
        state.apply(&GanttEvent::ReplayEnded);
        assert!(!state.has_open_work());
        // the dangling call settled as interrupted at its agent's last stamp
        let torn = &state.calls[0];
        assert!(torn.interrupted);
        assert!(!torn.in_flight());
        assert_eq!(torn.end_ts, Some(40));
        assert_eq!(torn.success, None);
        // the member settled and the outcome-less swarm reads as interrupted
        let member = state
            .agents
            .get("ReverseEngineer(Europa)")
            .expect("member span");
        assert_eq!(member.busy_calls, 0);
        let swarm = state.swarms.get("Swarm(Io)").expect("swarm span");
        assert!(swarm.interrupted);
        // every laid-out row is closed, so no bar can grow toward now
        let expanded: BTreeSet<RowKey> = BTreeSet::from([
            RowKey::Swarm("Swarm(Io)".to_owned()),
            RowKey::Agent("ReverseEngineer(Europa)".to_owned()),
        ]);
        let rows = build_rows(&state, &expanded, Some(1_000_000));
        assert!(rows.iter().all(|row| row.end.is_some()));
    }

    /// A torn call settles inside its own burst, not at a later session's stamps
    #[test]
    fn replay_ended_caps_torn_call_inside_its_burst() {
        let mut state = GanttState::default();
        // day one: a call tears mid-flight; a message is the burst's last stamp
        state.apply(&call("main", "call-1", Some(1_000)));
        state.apply(&GanttEvent::Message {
            agent: "main".to_owned(),
            ts: Some(2_000),
        });
        // day two, appended by a continued session: the same agent works again
        state.apply(&call("main", "call-2", Some(86_400_000)));
        state.apply(&result("main", "call-2", true, Some(86_460_000)));
        state.apply(&GanttEvent::ReplayEnded);
        // the torn call closed at its own burst's end, not across the downtime
        let torn = &state.calls[0];
        assert!(torn.interrupted);
        assert_eq!(torn.end_ts, Some(2_000));
    }

    /// A same-name swarm restart clears the latches a dead run left behind
    #[test]
    fn swarm_restart_clears_replayed_latches() {
        let mut state = GanttState::default();
        // a replayed swarm is torn (no finish) and the replay marker latches it
        state.apply(&GanttEvent::SwarmStarted {
            swarm: "Swarm(Io)".to_owned(),
            goal: "answer questions".to_owned(),
            ts: Some(10),
        });
        state.apply(&GanttEvent::ReplayEnded);
        assert!(
            state
                .swarms
                .get("Swarm(Io)")
                .expect("swarm span")
                .interrupted
        );
        assert!(!state.has_open_work());
        // display names restart every process, so a resumed session's first
        // swarm mints the dead one's name -- the start clears the latches
        state.apply(&GanttEvent::SwarmStarted {
            swarm: "Swarm(Io)".to_owned(),
            goal: "another task".to_owned(),
            ts: Some(86_400_000),
        });
        let swarm = state.swarms.get("Swarm(Io)").expect("swarm span");
        assert!(!swarm.interrupted);
        assert_eq!(swarm.outcome, None);
        assert_eq!(swarm.last_ts, None);
        assert!(state.has_open_work());
        // a finished swarm's stale outcome clears on a restart the same way
        state.apply(&GanttEvent::SwarmFinished {
            swarm: "Swarm(Io)".to_owned(),
            succeeded: 1,
            failed: 0,
            ts: Some(86_401_000),
        });
        assert!(!state.has_open_work());
        state.apply(&GanttEvent::SwarmStarted {
            swarm: "Swarm(Io)".to_owned(),
            goal: "a third task".to_owned(),
            ts: Some(86_402_000),
        });
        assert_eq!(
            state.swarms.get("Swarm(Io)").expect("swarm span").outcome,
            None
        );
        assert!(state.has_open_work());
    }

    /// Open work tracks pending calls, busy agents, and running swarms
    #[test]
    fn has_open_work_tracks_pending_busy_and_swarms() {
        let mut state = GanttState::default();
        assert!(!state.has_open_work());
        // an in-flight call opens work; its result settles it
        state.apply(&call("main", "call-1", Some(10)));
        assert!(state.has_open_work());
        state.apply(&result("main", "call-1", true, Some(20)));
        assert!(!state.has_open_work());
        // a running swarm opens work; its finish settles it
        state.apply(&GanttEvent::SwarmStarted {
            swarm: "Swarm(Io)".to_owned(),
            goal: "answer questions".to_owned(),
            ts: Some(30),
        });
        assert!(state.has_open_work());
        state.apply(&GanttEvent::SwarmFinished {
            swarm: "Swarm(Io)".to_owned(),
            succeeded: 1,
            failed: 0,
            ts: Some(40),
        });
        assert!(!state.has_open_work());
    }
}
