//! The agent stats view's events, state, and pure reducer
//!
//! [`StatsState::apply`] is the single fold both the live event drain and the
//! transcript replay run their [`StatsEvent`]s through, so a resumed session
//! rebuilds the same per-agent tallies the live run produced. Token usage
//! replays from the transcript's durable per-turn `usage` lines where they
//! exist; whatever the lines do not cover (sessions saved before usage lines
//! existed, turns the response hook never saw) is seeded from the snapshot
//! ledger's remainder instead, which carries no turn counts -- a purely
//! seeded agent's turns render as unknown rather than zero.
//!
//! Alongside the per-agent tallies the reducer keeps a session-wide
//! context-spend breakdown: every payload entering the conversation (tool
//! results, tool arguments, thinking, forum posts, LLM responses, and user
//! messages) is sized with a rough chars/4 token estimate and attributed to
//! its category and tool. The breakdown shows what the context is made of --
//! actual billed input tokens re-send that context every turn, so composition,
//! not billing, is what these estimates measure.

use std::collections::BTreeMap;

use thorpal::{ThorPalEvent, TranscriptItem, TranscriptLine};

use crate::handlers::ai::pricing::CostModel;

use super::super::status_bar::with_thousands;

/// One normalized stats event, derivable from live events and transcript
/// lines alike
#[derive(Debug, Clone, PartialEq)]
pub enum StatsEvent {
    /// An agent (sub-agent or swarm member) came into existence
    AgentStarted {
        /// The unique per-call display name of the agent
        agent: String,
        /// The human-facing role prefix of the agent, when known
        role: Option<String>,
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
        /// The name of the tool being called
        tool: String,
        /// The estimated tokens of the call's argument payload
        args_est: u64,
        /// Milliseconds since the Unix epoch when the call was captured
        ts: Option<u64>,
    },
    /// A tool call finished
    ToolResult {
        /// The display name of the agent that made the call
        agent: String,
        /// The name of the tool that ran
        tool: String,
        /// Whether the tool call succeeded
        success: bool,
        /// The estimated tokens of the model-visible result payload
        result_est: u64,
        /// Milliseconds since the Unix epoch when the result was captured
        ts: Option<u64>,
    },
    /// An agent produced assistant text (an LLM response entering the context)
    Message {
        /// The display name of the agent that produced the text
        agent: String,
        /// The estimated tokens of the response text
        est: u64,
        /// Milliseconds since the Unix epoch when the text was captured
        ts: Option<u64>,
    },
    /// The user sent a message
    UserMessage {
        /// The estimated tokens of the user's text
        est: u64,
        /// Milliseconds since the Unix epoch when the message was captured
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
    /// Carries no turn count: the snapshot stores only accumulated buckets, so
    /// a seeded agent's turn tally renders as unknown rather than zero.
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
    /// The session that wrote the transcript is gone, so no agent can still be
    /// busy -- the fold settles every tally rather than letting a torn call
    /// keep "time lived" growing toward now forever.
    ReplayEnded,
}

impl StatsEvent {
    /// Maps a live event onto a stats event, if it is one
    ///
    /// Live events carry no capture time of their own, so they are stamped at
    /// receive time -- the same convention the traversal log uses.
    ///
    /// # Arguments
    ///
    /// * `event` - The live event to map
    pub fn from_event(event: &ThorPalEvent) -> Option<StatsEvent> {
        // stamp live events as they arrive so lifetimes can be derived
        let ts = now_ms();
        match event {
            // a sub-agent coming alive gets a row, with its swarm attribution
            ThorPalEvent::SubAgentStarted {
                name, role, swarm, ..
            } => Some(StatsEvent::AgentStarted {
                agent: name.clone(),
                role: Some(role.clone()),
                swarm: swarm.clone(),
                ts,
            }),
            // a swarm's start and finish bound its wall-clock window
            ThorPalEvent::SwarmStarted { name, goal, .. } => Some(StatsEvent::SwarmStarted {
                swarm: name.clone(),
                goal: goal.clone(),
                ts,
            }),
            ThorPalEvent::SwarmFinished {
                name,
                succeeded,
                failed,
            } => Some(StatsEvent::SwarmFinished {
                swarm: name.clone(),
                succeeded: *succeeded,
                failed: *failed,
                ts,
            }),
            // a tool call and its result drive the per-agent call tallies and
            // the context-spend breakdown
            ThorPalEvent::ToolCallStarted {
                agent, tool, args, ..
            } => Some(StatsEvent::ToolCall {
                agent: agent.clone(),
                tool: tool.clone(),
                args_est: estimate_tokens(args),
                ts,
            }),
            ThorPalEvent::ToolCallFinished {
                agent,
                tool,
                result,
                success,
                ..
            } => Some(StatsEvent::ToolResult {
                agent: agent.clone(),
                tool: tool.clone(),
                success: *success,
                result_est: estimate_tokens(result),
                ts,
            }),
            // assistant text is an LLM response entering the context; the
            // final answer arrives as a per-turn message too, so the separate
            // run-finished event is skipped to avoid double counting
            ThorPalEvent::AgentMessage { agent, text } => Some(StatsEvent::Message {
                agent: agent.clone(),
                est: estimate_tokens(text),
                ts,
            }),
            // the user's own text enters the context as well, whichever
            // conversation (main or a side) it was typed into
            ThorPalEvent::UserMessage { text, .. } => Some(StatsEvent::UserMessage {
                est: estimate_tokens(text),
                ts,
            }),
            // a per-turn usage checkpoint feeds the token buckets and turn count
            ThorPalEvent::UsageUpdated {
                agent,
                input,
                output,
                cached,
                cache_creation,
                ..
            } => Some(StatsEvent::Usage {
                agent: agent.clone(),
                input: *input,
                output: *output,
                cached: *cached,
                cache_creation: *cache_creation,
                ts,
            }),
            // nothing else changes any per-agent tally
            _ => None,
        }
    }

    /// Maps a persisted transcript line onto a stats event, if it is one
    ///
    /// Durable per-turn `usage` lines fold token buckets and turn counts
    /// exactly like their live checkpoints; whatever they do not cover is
    /// seeded separately from the snapshot ledger's remainder.
    ///
    /// # Arguments
    ///
    /// * `line` - The persisted transcript line to map
    pub fn from_transcript(line: &TranscriptLine) -> Option<StatsEvent> {
        match &line.item {
            // a sub-agent breadcrumb, with its swarm attribution when the
            // transcript is new enough to carry one
            TranscriptItem::SubAgentStarted {
                name, role, swarm, ..
            } => Some(StatsEvent::AgentStarted {
                agent: name.clone(),
                role: role.clone(),
                swarm: swarm.clone(),
                ts: line.ts,
            }),
            // a swarm's start and finish breadcrumbs
            TranscriptItem::SwarmStarted { name, goal, .. } => Some(StatsEvent::SwarmStarted {
                swarm: name.clone(),
                goal: goal.clone(),
                ts: line.ts,
            }),
            TranscriptItem::SwarmFinished {
                name,
                succeeded,
                failed,
            } => Some(StatsEvent::SwarmFinished {
                swarm: name.clone(),
                succeeded: *succeeded,
                failed: *failed,
                ts: line.ts,
            }),
            // a tool call and its correlated result
            TranscriptItem::ToolCall {
                agent, tool, args, ..
            } => Some(StatsEvent::ToolCall {
                agent: agent.clone(),
                tool: tool.clone(),
                args_est: estimate_tokens(args),
                ts: line.ts,
            }),
            TranscriptItem::ToolResult {
                agent,
                tool,
                result,
                success,
                ..
            } => Some(StatsEvent::ToolResult {
                agent: agent.clone(),
                tool: tool.clone(),
                success: *success,
                result_est: estimate_tokens(result),
                ts: line.ts,
            }),
            // the persisted forms of assistant text: a sub-agent's messages
            // and the main agent's final answers (its per-turn text is never
            // persisted, so a resumed breakdown undercounts it slightly)
            TranscriptItem::SubAgentMessage { agent, text } => Some(StatsEvent::Message {
                agent: agent.clone(),
                est: estimate_tokens(text),
                ts: line.ts,
            }),
            TranscriptItem::Assistant { text, scope } => Some(StatsEvent::Message {
                // a side conversation's answers attribute to its own root row
                agent: match scope {
                    Some(scope) => format!("btw:{scope}"),
                    None => "main".to_owned(),
                },
                est: estimate_tokens(text),
                ts: line.ts,
            }),
            // the user's own text enters the context as well, whichever
            // conversation (main or a side) it was typed into
            TranscriptItem::User { text, .. } => Some(StatsEvent::UserMessage {
                est: estimate_tokens(text),
                ts: line.ts,
            }),
            // a durable per-turn usage checkpoint feeds the token buckets and
            // turn count exactly like its live counterpart
            TranscriptItem::Usage {
                agent,
                input,
                output,
                cached,
                cache_creation,
                ..
            } => Some(StatsEvent::Usage {
                agent: agent.clone(),
                input: *input,
                output: *output,
                cached: *cached,
                cache_creation: *cache_creation,
                ts: line.ts,
            }),
            // nothing else changes any per-agent tally
            _ => None,
        }
    }
}

/// The current wall clock as milliseconds since the Unix epoch
pub fn now_ms() -> Option<u64> {
    // a pre-epoch clock yields none rather than a bogus stamp
    u64::try_from(chrono::Utc::now().timestamp_millis()).ok()
}

/// Estimates the tokens a text payload costs, with the rough chars/4 heuristic
///
/// # Arguments
///
/// * `text` - The payload text to size
pub fn estimate_tokens(text: &str) -> u64 {
    // round up so a tiny non-empty payload never estimates to zero
    (text.chars().count() as u64).div_ceil(4)
}

/// The tool name rig's built-in thinking tool uses
const THINK_TOOL: &str = "think";

/// The forum tool names whose payloads are posts/comments, not routine calls
const FORUM_TOOLS: [&str; 2] = ["create_post", "create_comment"];

/// The accumulated stats for one agent (the main agent or a sub-agent)
#[derive(Debug, Clone, PartialEq)]
pub struct AgentStats {
    /// The unique per-call display name of this agent
    pub agent: String,
    /// The human-facing role prefix of this agent, when known
    pub role: Option<String>,
    /// The display name of the swarm this agent ran under, if any
    pub swarm: Option<String>,
    /// The stable palette slot assigned in first-seen order
    pub color_slot: usize,
    /// How many tool calls this agent made
    pub tool_calls: u64,
    /// How many of those calls finished successfully
    pub tool_ok: u64,
    /// How many of those calls finished in failure
    pub tool_err: u64,
    /// How many model turns this agent ran (count of usage checkpoints, live
    /// or replayed from durable transcript lines)
    pub turns: u64,
    /// The accumulated input (prompt) tokens
    pub input: u64,
    /// The accumulated output (completion) tokens
    pub output: u64,
    /// The accumulated cache-read input tokens
    pub cached: u64,
    /// The accumulated cache-write input tokens
    pub cache_creation: u64,
    /// This agent's tokens were seeded from a resumed snapshot, so its turn
    /// count is unknown rather than zero
    pub seeded: bool,
    /// Milliseconds since the epoch of this agent's first observed event
    pub first_ts: Option<u64>,
    /// Milliseconds since the epoch of this agent's last observed event
    pub last_ts: Option<u64>,
    /// How many of this agent's tool calls are currently in flight
    pub busy_calls: u32,
}

impl AgentStats {
    /// Creates a new, empty tally for an agent
    ///
    /// # Arguments
    ///
    /// * `agent` - The display name of the agent
    /// * `color_slot` - The stable palette slot assigned to the agent
    fn new(agent: &str, color_slot: usize) -> Self {
        AgentStats {
            agent: agent.to_owned(),
            role: None,
            swarm: None,
            color_slot,
            tool_calls: 0,
            tool_ok: 0,
            tool_err: 0,
            turns: 0,
            input: 0,
            output: 0,
            cached: 0,
            cache_creation: 0,
            seeded: false,
            first_ts: None,
            last_ts: None,
            busy_calls: 0,
        }
    }

    /// How long this agent has lived, in milliseconds, if known
    ///
    /// A still-busy agent's lifetime extends to `now`, so its bar keeps growing
    /// while it works; a settled agent's lifetime is the span between its first
    /// and last observed events.
    ///
    /// # Arguments
    ///
    /// * `now` - The current wall clock in milliseconds since the epoch
    pub fn time_lived(&self, now: Option<u64>) -> Option<u64> {
        // an agent with no stamped events has no measurable lifetime
        let first = self.first_ts?;
        // a busy agent is still alive, so measure up to now when it is known
        let last = if self.busy_calls > 0 {
            now.or(self.last_ts)?
        } else {
            self.last_ts?
        };
        Some(last.saturating_sub(first))
    }
}

/// The accumulated stats for one swarm of concurrent sub-agents
#[derive(Debug, Clone, PartialEq)]
pub struct SwarmStats {
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
}

/// The session-wide context-spend breakdown, in estimated tokens per category
///
/// Sizes what the conversation's context is made of: each payload is counted
/// once as it enters the context, so these are composition estimates, not
/// billed token counts (billing re-sends the context every turn).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpendBreakdown {
    /// The estimated tokens of model-visible tool results
    pub tool_results: u64,
    /// The estimated tokens of tool call arguments
    pub tool_args: u64,
    /// The estimated tokens of the thinking tool's reasoning text
    pub thinking: u64,
    /// The estimated tokens of forum posts and comments (args and results)
    pub forum: u64,
    /// The estimated tokens of agents' assistant text (LLM responses)
    pub responses: u64,
    /// The estimated tokens of the user's own messages
    pub user: u64,
}

impl SpendBreakdown {
    /// The categories with their labels and values, sorted largest first
    pub fn ranked(&self) -> Vec<(&'static str, u64)> {
        // pair every category with its human-facing label
        let mut ranked = vec![
            ("tool results", self.tool_results),
            ("tool args", self.tool_args),
            ("thinking", self.thinking),
            ("forum posts", self.forum),
            ("LLM responses", self.responses),
            ("user messages", self.user),
        ];
        // the biggest consumer leads the panel
        ranked.sort_by(|(_, a), (_, b)| b.cmp(a));
        ranked
    }

    /// The estimated tokens across every category
    pub fn total(&self) -> u64 {
        self.tool_results
            .saturating_add(self.tool_args)
            .saturating_add(self.thinking)
            .saturating_add(self.forum)
            .saturating_add(self.responses)
            .saturating_add(self.user)
    }
}

/// The whole reduced per-agent stats state (a pure value, no locks)
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StatsState {
    /// Every agent's tally, keyed by display name
    pub agents: BTreeMap<String, AgentStats>,
    /// Every swarm's tally, keyed by display name
    pub swarms: BTreeMap<String, SwarmStats>,
    /// The session-wide context-spend breakdown by category
    pub spend: SpendBreakdown,
    /// The estimated context tokens per tool (arguments plus results)
    pub tool_spend: BTreeMap<String, u64>,
    /// The next palette slot to hand out in first-seen order
    next_slot: usize,
}

impl StatsState {
    /// Folds one event into the state (the reducer; pure and infallible)
    ///
    /// # Arguments
    ///
    /// * `event` - The stats event to fold in
    pub fn apply(&mut self, event: &StatsEvent) {
        match event {
            // an agent came alive: give it a row and record its attribution
            StatsEvent::AgentStarted {
                agent,
                role,
                swarm,
                ts,
            } => {
                let stats = self.tally(agent);
                // keep the first non-empty role and swarm we learn
                if stats.role.is_none() {
                    stats.role = role.clone();
                }
                if stats.swarm.is_none() {
                    stats.swarm = swarm.clone();
                }
                touch(stats, *ts);
                // register the member on its swarm so grouping knows it
                if let Some(swarm) = swarm {
                    let swarm = self.swarm(swarm);
                    if !swarm.members.iter().any(|member| member == agent) {
                        swarm.members.push(agent.clone());
                    }
                }
            }
            // a swarm came alive: record its goal and start time
            StatsEvent::SwarmStarted { swarm, goal, ts } => {
                let swarm = self.swarm(swarm);
                swarm.goal = goal.clone();
                swarm.first_ts = swarm.first_ts.or(*ts);
            }
            // a swarm finished: record its outcome and finish time
            StatsEvent::SwarmFinished {
                swarm,
                succeeded,
                failed,
                ts,
            } => {
                let swarm = self.swarm(swarm);
                swarm.outcome = Some((*succeeded, *failed));
                swarm.last_ts = *ts;
            }
            // a tool call started: count it, flag the agent busy, and charge
            // its argument payload to the context-spend breakdown
            StatsEvent::ToolCall {
                agent,
                tool,
                args_est,
                ts,
            } => {
                let stats = self.tally(agent);
                stats.tool_calls += 1;
                stats.busy_calls += 1;
                touch(stats, *ts);
                self.charge(tool, *args_est, SpendKind::Args);
            }
            // a tool call finished: settle the agent, tally the outcome, and
            // charge its result payload to the context-spend breakdown
            StatsEvent::ToolResult {
                agent,
                tool,
                success,
                result_est,
                ts,
            } => {
                let stats = self.tally(agent);
                stats.busy_calls = stats.busy_calls.saturating_sub(1);
                if *success {
                    stats.tool_ok += 1;
                } else {
                    stats.tool_err += 1;
                }
                touch(stats, *ts);
                self.charge(tool, *result_est, SpendKind::Results);
            }
            // assistant text: charge the LLM response to the breakdown and
            // extend its agent's event window
            StatsEvent::Message { agent, est, ts } => {
                let stats = self.tally(agent);
                touch(stats, *ts);
                self.spend.responses = self.spend.responses.saturating_add(*est);
            }
            // the user's own text only feeds the breakdown
            StatsEvent::UserMessage { est, .. } => {
                self.spend.user = self.spend.user.saturating_add(*est);
            }
            // a usage checkpoint, live or replayed: add the buckets and count
            // the turn
            StatsEvent::Usage {
                agent,
                input,
                output,
                cached,
                cache_creation,
                ts,
            } => {
                let stats = self.tally(agent);
                stats.input = stats.input.saturating_add(*input);
                stats.output = stats.output.saturating_add(*output);
                stats.cached = stats.cached.saturating_add(*cached);
                stats.cache_creation = stats.cache_creation.saturating_add(*cache_creation);
                stats.turns += 1;
                touch(stats, *ts);
            }
            // a resume seed: add the buckets without inventing a turn count
            StatsEvent::SeedUsage {
                agent,
                input,
                output,
                cached,
                cache_creation,
            } => {
                let stats = self.tally(agent);
                stats.input = stats.input.saturating_add(*input);
                stats.output = stats.output.saturating_add(*output);
                stats.cached = stats.cached.saturating_add(*cached);
                stats.cache_creation = stats.cache_creation.saturating_add(*cache_creation);
                stats.seeded = true;
            }
            // the replayed transcript ended: the session that wrote it is
            // gone, so no agent can still be busy
            StatsEvent::ReplayEnded => {
                for stats in self.agents.values_mut() {
                    stats.busy_calls = 0;
                }
            }
        }
    }

    /// Gets or creates the tally for an agent, assigning its palette slot
    ///
    /// Any event upserts its agent, so `"main"` (which is never announced via
    /// an agent-started breadcrumb) still gets a row on its first activity.
    ///
    /// # Arguments
    ///
    /// * `agent` - The display name of the agent
    fn tally(&mut self, agent: &str) -> &mut AgentStats {
        // palette slots are handed out in first-seen order and never change
        let slot = self.next_slot;
        let stats = self
            .agents
            .entry(agent.to_owned())
            .or_insert_with(|| AgentStats::new(agent, slot));
        // only a freshly inserted row consumed the slot
        if stats.color_slot == slot {
            self.next_slot += 1;
        }
        stats
    }

    /// Charges a tool payload to the context-spend breakdown and tool ranking
    ///
    /// The thinking tool's payloads count as thinking and the forum tools'
    /// as forum posts, so those categories answer "what fills the context"
    /// directly; everything else lands in the generic tool buckets.
    ///
    /// # Arguments
    ///
    /// * `tool` - The name of the tool the payload belongs to
    /// * `est` - The estimated tokens of the payload
    /// * `kind` - Whether the payload was the call's arguments or its result
    fn charge(&mut self, tool: &str, est: u64, kind: SpendKind) {
        // every payload counts toward its tool's ranking entry
        let entry = self.tool_spend.entry(tool.to_owned()).or_default();
        *entry = entry.saturating_add(est);
        // pick the category the payload belongs to
        let bucket = if tool == THINK_TOOL {
            &mut self.spend.thinking
        } else if FORUM_TOOLS.contains(&tool) {
            &mut self.spend.forum
        } else {
            match kind {
                SpendKind::Args => &mut self.spend.tool_args,
                SpendKind::Results => &mut self.spend.tool_results,
            }
        };
        *bucket = bucket.saturating_add(est);
    }

    /// Gets or creates the tally for a swarm
    ///
    /// # Arguments
    ///
    /// * `name` - The display name of the swarm
    fn swarm(&mut self, name: &str) -> &mut SwarmStats {
        self.swarms
            .entry(name.to_owned())
            .or_insert_with(|| SwarmStats {
                name: name.to_owned(),
                goal: String::new(),
                members: Vec::new(),
                outcome: None,
                first_ts: None,
                last_ts: None,
            })
    }
}

/// Whether a tool payload was the call's arguments or its result
#[derive(Debug, Clone, Copy)]
enum SpendKind {
    /// The payload was the call's argument string
    Args,
    /// The payload was the call's model-visible result
    Results,
}

/// Folds an event's capture time into an agent's first/last window
///
/// # Arguments
///
/// * `stats` - The agent tally to update
/// * `ts` - The event's capture time, if it carried one
fn touch(stats: &mut AgentStats, ts: Option<u64>) {
    if let Some(ts) = ts {
        // the first stamp wins the start; every stamp can extend the end
        stats.first_ts = Some(stats.first_ts.map_or(ts, |first| first.min(ts)));
        stats.last_ts = Some(stats.last_ts.map_or(ts, |last| last.max(ts)));
    }
}

/// The metric the stats view plots one bar per agent of
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    /// The estimated dollar cost of the agent's accumulated usage
    Cost,
    /// How many tool calls the agent made
    ToolCalls,
    /// The agent's accumulated input (prompt) tokens
    InputTokens,
    /// The agent's accumulated output (completion) tokens
    OutputTokens,
    /// How long the agent lived, first event to last (or now while busy)
    TimeLived,
    /// How many model turns the agent ran
    Turns,
}

impl Metric {
    /// The metrics in their cycling order
    const ALL: [Metric; 6] = [
        Metric::Cost,
        Metric::ToolCalls,
        Metric::InputTokens,
        Metric::OutputTokens,
        Metric::TimeLived,
        Metric::Turns,
    ];

    /// The next metric in the cycle, wrapping at the end
    pub fn next(self) -> Metric {
        // find this metric's position and step forward with wraparound
        let pos = Metric::ALL.iter().position(|m| *m == self).unwrap_or(0);
        Metric::ALL[(pos + 1) % Metric::ALL.len()]
    }

    /// The previous metric in the cycle, wrapping at the start
    pub fn prev(self) -> Metric {
        // find this metric's position and step backward with wraparound
        let pos = Metric::ALL.iter().position(|m| *m == self).unwrap_or(0);
        Metric::ALL[(pos + Metric::ALL.len() - 1) % Metric::ALL.len()]
    }

    /// The human-facing label for this metric
    pub fn label(self) -> &'static str {
        match self {
            Metric::Cost => "cost ($)",
            Metric::ToolCalls => "tool calls",
            Metric::InputTokens => "input tokens",
            Metric::OutputTokens => "output tokens",
            Metric::TimeLived => "time lived",
            Metric::Turns => "turns",
        }
    }

    /// This metric's value for one agent, or `None` when it is unknowable
    ///
    /// Unknown values (cost without an estimator, turns for an agent whose
    /// usage was seeded from a snapshot, lifetimes with no stamped events)
    /// render as a dash rather than a zero-length bar.
    ///
    /// # Arguments
    ///
    /// * `stats` - The agent tally to read
    /// * `cost` - The session's cost estimator, when its model is priceable
    /// * `now` - The current wall clock in milliseconds since the epoch
    pub fn value(
        self,
        stats: &AgentStats,
        cost: Option<&CostModel>,
        now: Option<u64>,
    ) -> Option<f64> {
        match self {
            // cost needs an estimator and any usage at all to be meaningful
            Metric::Cost => {
                let cost = cost?;
                Some(cost.cost(
                    stats.input,
                    stats.output,
                    stats.cached,
                    stats.cache_creation,
                ))
            }
            Metric::ToolCalls => Some(stats.tool_calls as f64),
            Metric::InputTokens => Some(stats.input as f64),
            Metric::OutputTokens => Some(stats.output as f64),
            Metric::TimeLived => stats.time_lived(now).map(|ms| ms as f64),
            // a seeded agent with no live turns has an unknown turn count,
            // not a zero one
            Metric::Turns => {
                if stats.seeded && stats.turns == 0 {
                    None
                } else {
                    Some(stats.turns as f64)
                }
            }
        }
    }

    /// Formats one of this metric's values for the bar list's value column
    ///
    /// # Arguments
    ///
    /// * `value` - The value to format
    pub fn format(self, value: f64) -> String {
        match self {
            Metric::Cost => fmt_cost(value),
            Metric::TimeLived => fmt_duration(value as u64),
            // the count metrics all render as grouped integers
            Metric::ToolCalls | Metric::InputTokens | Metric::OutputTokens | Metric::Turns => {
                with_thousands(value as u64)
            }
        }
    }
}

/// Formats a dollar cost, with sub-dollar runs shown at tenth-cent precision
///
/// # Arguments
///
/// * `dollars` - The estimated dollar cost to format
pub fn fmt_cost(dollars: f64) -> String {
    // small runs need the extra digits to not read as $0.00
    if dollars < 1.0 {
        format!("${dollars:.4}")
    } else {
        format!("${dollars:.2}")
    }
}

/// Formats a duration in milliseconds into a compact human-readable form
///
/// # Arguments
///
/// * `ms` - The duration in milliseconds to format
pub fn fmt_duration(ms: u64) -> String {
    // sub-second durations show raw milliseconds
    if ms < 1_000 {
        return format!("{ms}ms");
    }
    // sub-minute durations show tenth-of-a-second precision
    let secs = ms / 1_000;
    if secs < 60 {
        return format!("{:.1}s", ms as f64 / 1_000.0);
    }
    // sub-hour durations show minutes and seconds
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m {:02}s", secs % 60);
    }
    // anything longer shows hours and minutes
    format!("{}h {:02}m", mins / 60, mins % 60)
}

/// One identity in the bar list: an agent's bar or a swarm's section header
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowId {
    /// An agent's bar, keyed by its display name
    Agent(String),
    /// A swarm's section header, keyed by its display name
    Swarm(String),
}

/// One agent's tally paired with its already-computed metric value
type ValuedAgent<'s> = (&'s AgentStats, Option<f64>);

/// One laid-out row of the bar list
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// The identity this row plots or heads
    pub id: RowId,
    /// Whether the selection can land on this row
    ///
    /// Agent bars and real swarm headers are selectable; the synthetic
    /// "(no swarm)" section header is a divider only.
    pub selectable: bool,
    /// The metric value this row plots, or `None` when it is unknown
    pub value: Option<f64>,
}

/// Lays out the bar list's rows for one metric and grouping mode
///
/// Flat mode yields one agent bar per row, sorted descending by value (then by
/// name for stability). Grouped mode yields one section per swarm -- a header
/// row carrying the summed member value followed by its member bars -- with the
/// sections themselves ordered by aggregate descending and a trailing
/// "(no swarm)" section holding the main agent and solo delegations.
///
/// # Arguments
///
/// * `state` - The reduced stats state to lay out
/// * `metric` - The metric the bars plot
/// * `grouped` - Whether to group the bars into swarm sections
/// * `cost` - The session's cost estimator, when its model is priceable
/// * `now` - The current wall clock in milliseconds since the epoch
pub fn build_rows(
    state: &StatsState,
    metric: Metric,
    grouped: bool,
    cost: Option<&CostModel>,
    now: Option<u64>,
) -> Vec<Row> {
    // compute every agent's value once; unknown values sort as zero
    let mut values: Vec<ValuedAgent> = state
        .agents
        .values()
        .map(|stats| (stats, metric.value(stats, cost, now)))
        .collect();
    // sort descending by value, then by name so ties are stable
    values.sort_by(|(a, av), (b, bv)| {
        let av = av.unwrap_or(0.0);
        let bv = bv.unwrap_or(0.0);
        bv.partial_cmp(&av)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.agent.cmp(&b.agent))
    });
    // flat mode is just the sorted bars
    if !grouped {
        return values
            .into_iter()
            .map(|(stats, value)| Row {
                id: RowId::Agent(stats.agent.clone()),
                selectable: true,
                value,
            })
            .collect();
    }
    // grouped mode: bucket the sorted bars by swarm, keeping their order
    let mut sections: Vec<(Option<&str>, Vec<ValuedAgent>)> = Vec::new();
    for (stats, value) in values {
        let swarm = stats.swarm.as_deref();
        // append to the swarm's existing section or start a new one
        match sections.iter_mut().find(|(name, _)| *name == swarm) {
            Some((_, members)) => members.push((stats, value)),
            None => sections.push((swarm, vec![(stats, value)])),
        }
    }
    // order the swarm sections by their aggregate value descending, with the
    // ungrouped section always trailing
    sections.sort_by(|(a_name, a_members), (b_name, b_members)| {
        // the ungrouped bucket sorts after every real swarm
        match (a_name, b_name) {
            (None, Some(_)) => return std::cmp::Ordering::Greater,
            (Some(_), None) => return std::cmp::Ordering::Less,
            _ => (),
        }
        let a_sum: f64 = a_members.iter().filter_map(|(_, v)| *v).sum();
        let b_sum: f64 = b_members.iter().filter_map(|(_, v)| *v).sum();
        b_sum
            .partial_cmp(&a_sum)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    // flatten the sections into header + member rows
    let mut rows = Vec::new();
    for (swarm, members) in sections {
        // sum the members' known values for the header's aggregate; a section
        // of entirely unknown values gets an unknown aggregate
        let aggregate = members
            .iter()
            .filter_map(|(_, value)| *value)
            .fold(None, |sum: Option<f64>, value| {
                Some(sum.unwrap_or(0.0) + value)
            });
        // the time-lived aggregate is wall time, not a sum of member lifetimes
        let aggregate = if metric == Metric::TimeLived {
            wall_time(&members, now)
        } else {
            aggregate
        };
        rows.push(Row {
            // a real swarm's header is selectable (its chat interleaves its
            // members); the synthetic ungrouped divider is not
            id: match swarm {
                Some(name) => RowId::Swarm(name.to_owned()),
                None => RowId::Swarm(String::new()),
            },
            selectable: swarm.is_some(),
            value: aggregate,
        });
        // the member bars follow their header in sorted order
        rows.extend(members.into_iter().map(|(stats, value)| Row {
            id: RowId::Agent(stats.agent.clone()),
            selectable: true,
            value,
        }));
    }
    rows
}

/// The wall-clock span covered by a set of member agents, in milliseconds
///
/// Spans the earliest first event to the latest last event (extended to now for
/// a still-busy member), so a swarm's "time lived" reads as how long the swarm
/// was on screen rather than a sum of overlapping member lifetimes.
///
/// # Arguments
///
/// * `members` - The member tallies with their already-computed values
/// * `now` - The current wall clock in milliseconds since the epoch
fn wall_time(members: &[ValuedAgent], now: Option<u64>) -> Option<f64> {
    // the window opens at the earliest stamped first event
    let first = members
        .iter()
        .filter_map(|(stats, _)| stats.first_ts)
        .min()?;
    // the window closes at the latest stamped last event, or now while busy
    let last = members
        .iter()
        .filter_map(|(stats, _)| {
            if stats.busy_calls > 0 {
                now.or(stats.last_ts)
            } else {
                stats.last_ts
            }
        })
        .max()?;
    Some(last.saturating_sub(first) as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A started tool call event for an agent
    fn call(agent: &str, ts: Option<u64>) -> StatsEvent {
        StatsEvent::ToolCall {
            agent: agent.to_owned(),
            tool: "memory_map".to_owned(),
            args_est: 1,
            ts,
        }
    }

    /// A finished tool call event for an agent
    fn result(agent: &str, success: bool, ts: Option<u64>) -> StatsEvent {
        StatsEvent::ToolResult {
            agent: agent.to_owned(),
            tool: "memory_map".to_owned(),
            success,
            result_est: 25,
            ts,
        }
    }

    /// A usage checkpoint event for an agent
    fn usage(agent: &str, input: u64, output: u64) -> StatsEvent {
        StatsEvent::Usage {
            agent: agent.to_owned(),
            input,
            output,
            cached: 0,
            cache_creation: 0,
            ts: None,
        }
    }

    /// A replay-ended marker settles every busy agent
    #[test]
    fn replay_ended_settles_busy_agents() {
        let mut state = StatsState::default();
        // a torn transcript leaves the agent busy with no result in sight
        state.apply(&call("main", Some(10)));
        let main = state.agents.get("main").expect("main row");
        assert_eq!(main.busy_calls, 1);
        // while busy, the agent's lifetime keeps growing toward now
        assert_eq!(main.time_lived(Some(1_000)), Some(990));
        state.apply(&StatsEvent::ReplayEnded);
        // settled: the lifetime freezes at the agent's last stamp
        let main = state.agents.get("main").expect("main row");
        assert_eq!(main.busy_calls, 0);
        assert_eq!(main.time_lived(Some(1_000)), Some(0));
    }

    /// Tool calls, results, and usage checkpoints tally per agent
    #[test]
    fn events_tally_per_agent() {
        let mut state = StatsState::default();
        // two agents interleave calls, results, and usage checkpoints
        state.apply(&call("main", Some(10)));
        state.apply(&result("main", true, Some(20)));
        state.apply(&call("ReverseEngineer(Europa)", Some(30)));
        state.apply(&result("ReverseEngineer(Europa)", false, Some(40)));
        state.apply(&usage("main", 100, 10));
        state.apply(&usage("main", 200, 20));
        // the main agent tallied one ok call, two turns, and summed buckets
        let main = state.agents.get("main").expect("main row");
        assert_eq!(main.tool_calls, 1);
        assert_eq!(main.tool_ok, 1);
        assert_eq!(main.turns, 2);
        assert_eq!(main.input, 300);
        assert_eq!(main.output, 30);
        assert_eq!(main.busy_calls, 0);
        // the sub-agent tallied one failed call and its event window
        let sub = state
            .agents
            .get("ReverseEngineer(Europa)")
            .expect("sub row");
        assert_eq!(sub.tool_err, 1);
        assert_eq!(sub.first_ts, Some(30));
        assert_eq!(sub.last_ts, Some(40));
        // palette slots were handed out in first-seen order
        assert_eq!(main.color_slot, 0);
        assert_eq!(sub.color_slot, 1);
    }

    /// A busy agent's lifetime extends to now; a settled one's does not
    #[test]
    fn time_lived_extends_while_busy() {
        let mut state = StatsState::default();
        // the agent's only call is still in flight
        state.apply(&call("main", Some(100)));
        let main = state.agents.get("main").expect("main row");
        assert_eq!(main.busy_calls, 1);
        // the lifetime stretches to the supplied now
        assert_eq!(main.time_lived(Some(600)), Some(500));
        // once the call settles the lifetime is the event window
        state.apply(&result("main", true, Some(300)));
        let main = state.agents.get("main").expect("main row");
        assert_eq!(main.time_lived(Some(600)), Some(200));
    }

    /// Swarm membership fills from the agent-started attribution
    #[test]
    fn swarm_membership_fills_from_agent_started() {
        let mut state = StatsState::default();
        // the swarm starts, then two members come alive under it
        state.apply(&StatsEvent::SwarmStarted {
            swarm: "Swarm(Io)".to_owned(),
            goal: "answer questions".to_owned(),
            ts: Some(10),
        });
        for name in ["ReverseEngineer(Europa)", "Triage(Callisto)"] {
            state.apply(&StatsEvent::AgentStarted {
                agent: name.to_owned(),
                role: None,
                swarm: Some("Swarm(Io)".to_owned()),
                ts: Some(20),
            });
        }
        state.apply(&StatsEvent::SwarmFinished {
            swarm: "Swarm(Io)".to_owned(),
            succeeded: 2,
            failed: 0,
            ts: Some(90),
        });
        // the swarm knows its members, goal, window, and outcome
        let swarm = state.swarms.get("Swarm(Io)").expect("swarm row");
        assert_eq!(swarm.members.len(), 2);
        assert_eq!(swarm.goal, "answer questions");
        assert_eq!(swarm.outcome, Some((2, 0)));
        // each member row carries its swarm attribution
        let member = state
            .agents
            .get("ReverseEngineer(Europa)")
            .expect("member row");
        assert_eq!(member.swarm.as_deref(), Some("Swarm(Io)"));
    }

    /// A resume seed adds token buckets without inventing a turn count
    #[test]
    fn seeded_usage_leaves_turns_unknown() {
        let mut state = StatsState::default();
        state.apply(&StatsEvent::SeedUsage {
            agent: "main".to_owned(),
            input: 1_000,
            output: 100,
            cached: 50,
            cache_creation: 25,
        });
        let main = state.agents.get("main").expect("main row");
        // the buckets landed but the agent is flagged seeded with no turns
        assert_eq!(main.input, 1_000);
        assert!(main.seeded);
        assert_eq!(main.turns, 0);
        // the turns metric reads as unknown, not zero
        assert_eq!(Metric::Turns.value(main, None, None), None);
        // a live turn after the seed makes the count known again
        state.apply(&usage("main", 10, 1));
        let main = state.agents.get("main").expect("main row");
        assert_eq!(Metric::Turns.value(main, None, None), Some(1.0));
    }

    /// Live events and transcript lines map onto equivalent stats events
    #[test]
    fn from_event_and_from_transcript_agree() {
        // the live form of a sub-agent start with swarm attribution
        let live = ThorPalEvent::SubAgentStarted {
            name: "ReverseEngineer(Europa)".to_owned(),
            role: "ReverseEngineer".to_owned(),
            goal: "triage".to_owned(),
            swarm: Some("Swarm(Io)".to_owned()),
            resumed: false,
        };
        let live_event = StatsEvent::from_event(&live).expect("live event maps");
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
        let replayed = StatsEvent::from_transcript(&line).expect("transcript line maps");
        // both agree once the live receive-time stamp is normalized away
        match (live_event, replayed) {
            (
                StatsEvent::AgentStarted {
                    agent: a,
                    role: ar,
                    swarm: asw,
                    ..
                },
                StatsEvent::AgentStarted {
                    agent: b,
                    role: br,
                    swarm: bsw,
                    ..
                },
            ) => {
                assert_eq!(a, b);
                assert_eq!(ar, br);
                assert_eq!(asw, bsw);
            }
            other => panic!("expected matching agent starts, got {other:?}"),
        }
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
            StatsEvent::from_event(&usage),
            Some(StatsEvent::Usage { .. })
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
            StatsEvent::from_transcript(&usage_line),
            Some(StatsEvent::Usage {
                ts: Some(42_000),
                ..
            })
        ));
    }

    /// Flat rows sort descending by value with names breaking ties
    #[test]
    fn flat_rows_sort_by_value_then_name() {
        let mut state = StatsState::default();
        // three agents with distinct and tied call counts
        for _ in 0..3 {
            state.apply(&call("busy", None));
        }
        state.apply(&call("alpha", None));
        state.apply(&call("beta", None));
        let rows = build_rows(&state, Metric::ToolCalls, false, None, None);
        // the busiest agent leads and the tie breaks alphabetically
        let order: Vec<&RowId> = rows.iter().map(|row| &row.id).collect();
        assert_eq!(
            order,
            vec![
                &RowId::Agent("busy".to_owned()),
                &RowId::Agent("alpha".to_owned()),
                &RowId::Agent("beta".to_owned()),
            ]
        );
        assert_eq!(rows[0].value, Some(3.0));
    }

    /// Grouped rows section by swarm with aggregates and a trailing ungrouped
    #[test]
    fn grouped_rows_section_by_swarm() {
        let mut state = StatsState::default();
        // one swarm member, one solo agent, and the main agent
        state.apply(&StatsEvent::AgentStarted {
            agent: "ReverseEngineer(Europa)".to_owned(),
            role: None,
            swarm: Some("Swarm(Io)".to_owned()),
            ts: None,
        });
        state.apply(&call("ReverseEngineer(Europa)", None));
        state.apply(&call("ReverseEngineer(Europa)", None));
        state.apply(&call("main", None));
        let rows = build_rows(&state, Metric::ToolCalls, true, None, None);
        // the swarm section leads: a selectable header then its member
        assert_eq!(rows[0].id, RowId::Swarm("Swarm(Io)".to_owned()));
        assert!(rows[0].selectable);
        assert_eq!(rows[0].value, Some(2.0));
        assert_eq!(
            rows[1].id,
            RowId::Agent("ReverseEngineer(Europa)".to_owned())
        );
        // the ungrouped section trails with an unselectable divider header
        assert_eq!(rows[2].id, RowId::Swarm(String::new()));
        assert!(!rows[2].selectable);
        assert_eq!(rows[3].id, RowId::Agent("main".to_owned()));
    }

    /// Payloads charge the context-spend breakdown by category and tool
    #[test]
    fn context_spend_buckets_by_category() {
        let mut state = StatsState::default();
        // a routine tool call: args and result land in the generic buckets
        state.apply(&call("main", None));
        state.apply(&result("main", true, None));
        // a thinking payload counts as thinking, not tool args
        state.apply(&StatsEvent::ToolCall {
            agent: "main".to_owned(),
            tool: "think".to_owned(),
            args_est: 40,
            ts: None,
        });
        // a forum post counts as forum, args and result alike
        state.apply(&StatsEvent::ToolCall {
            agent: "main".to_owned(),
            tool: "create_post".to_owned(),
            args_est: 30,
            ts: None,
        });
        state.apply(&StatsEvent::ToolResult {
            agent: "main".to_owned(),
            tool: "create_post".to_owned(),
            success: true,
            result_est: 5,
            ts: None,
        });
        // responses and user messages fill their own categories
        state.apply(&StatsEvent::Message {
            agent: "main".to_owned(),
            est: 60,
            ts: None,
        });
        state.apply(&StatsEvent::UserMessage { est: 10, ts: None });
        // every payload landed in exactly one category
        assert_eq!(state.spend.tool_args, 1);
        assert_eq!(state.spend.tool_results, 25);
        assert_eq!(state.spend.thinking, 40);
        assert_eq!(state.spend.forum, 35);
        assert_eq!(state.spend.responses, 60);
        assert_eq!(state.spend.user, 10);
        assert_eq!(state.spend.total(), 171);
        // the biggest consumer leads the ranked panel
        assert_eq!(state.spend.ranked()[0], ("LLM responses", 60));
        // the per-tool ranking sums args and results per tool
        assert_eq!(state.tool_spend.get("memory_map"), Some(&26));
        assert_eq!(state.tool_spend.get("create_post"), Some(&35));
        // the chars/4 estimate rounds up so tiny payloads never vanish
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("ok"), 1);
        assert_eq!(estimate_tokens("12345678"), 2);
    }

    /// A durable usage line folds exactly like its live checkpoint: buckets
    /// fill, the turn counts, and the stamp bounds the agent's lifetime
    #[test]
    fn transcript_usage_lines_fold_like_live_checkpoints() {
        // a persisted usage checkpoint line for a sub-agent
        let line = TranscriptLine {
            ts: Some(42_000),
            item: TranscriptItem::Usage {
                agent: "ReverseEngineer(Europa)".to_owned(),
                input: 1_000,
                output: 100,
                cached: 9_000,
                cache_creation: 500,
                total: 10_600,
            },
        };
        // the line maps onto a usage event carrying its capture stamp
        let event = StatsEvent::from_transcript(&line).expect("usage lines fold");
        let mut state = StatsState::default();
        state.apply(&event);
        // the buckets filled, the turn counted, and the stamp was kept
        let stats = state
            .agents
            .get("ReverseEngineer(Europa)")
            .expect("a usage row");
        assert_eq!(stats.input, 1_000);
        assert_eq!(stats.output, 100);
        assert_eq!(stats.cached, 9_000);
        assert_eq!(stats.cache_creation, 500);
        assert_eq!(stats.turns, 1);
        assert!(!stats.seeded);
        assert_eq!(stats.first_ts, Some(42_000));
        // a replayed turn count renders as a real value, not an unknown dash
        assert_eq!(Metric::Turns.value(stats, None, None), Some(1.0));
    }

    /// The formatting helpers cover their precision buckets
    #[test]
    fn formatting_helpers_cover_their_buckets() {
        // costs switch precision at a dollar
        assert_eq!(fmt_cost(0.0132), "$0.0132");
        assert_eq!(fmt_cost(12.3456), "$12.35");
        // durations climb through their units
        assert_eq!(fmt_duration(850), "850ms");
        assert_eq!(fmt_duration(12_340), "12.3s");
        assert_eq!(fmt_duration(272_000), "4m 32s");
        assert_eq!(fmt_duration(3_720_000), "1h 02m");
        // counts group their thousands through the metric formatter
        assert_eq!(Metric::InputTokens.format(48_120.0), "48,120");
    }
}
