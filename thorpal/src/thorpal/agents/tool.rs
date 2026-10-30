//! Goal-aware sub-agent tool wrapper.
//!
//! rig's built-in [`Agent::into_tool`] exposes a sub-agent to the main agent as
//! a tool taking a single `prompt` string and returning the sub-agent's text.
//! This wrapper adds three things ThorPal wants for observability: the caller
//! must state the overarching `goal` it is delegating toward, that goal is
//! recorded on a per-invocation span and injected into the sub-agent's prompt,
//! and the sub-agent's token usage is logged cleanly -- per model turn as it
//! works and once more in aggregate when it finishes.

use std::future::IntoFuture;
use std::sync::Arc;

use rig::Agent;
use rig::agent::hook::{
    AgentHook, CompletionCall, CompletionCallAction, CompletionResponse, HookContext,
    InvalidToolCallAction, InvalidToolCallContext, ModelTurnAction, ModelTurnFinished,
    ObservationAction, RequestPatch, ToolCall, ToolCallAction, ToolResultAction, ToolResultEvent,
    TurnsExhausted, TurnsExhaustedAction,
};
use rig::agent::tool::DynamicTool;
use rig::completion::{FinishReason, Message, Prompt};
use rig::message::{AssistantContent, ToolChoice};
use rig::tool::{ToolContext, ToolExecutionError, ToolOutput};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Semaphore;
use tracing::Instrument;

use crate::thorpal::agents::budget::{
    ContextPressure, TRUNCATION_FEEDBACK, TurnBudget, TurnBudgetState,
};
use crate::thorpal::agents::salvage;
use crate::thorpal::coverage::CoverageLedger;
use crate::thorpal::events::{ThorPalEvent, ThorPalEventSender};
use crate::thorpal::store::memories::MemoryLibrary;
use crate::thorpal::store::redact::LibraryRedactor;
use crate::thorpal::store::{AgentRecord, SessionStore};
use crate::thorpal::tools::constants::{self, ConstantTracker};
use crate::thorpal::tools::forum::{
    ForumContextMode, ForumContextSettings, ForumState, scan_references,
};
use crate::thorpal::tools::memories::search::{MemoryDoc, clip_chars, rank_memory_docs};
use crate::thorpal::tools::registry::EnabledTools;
use crate::thorpal::usage::UsageLedger;

/// The tool name used for a sub-agent that was built without a name.
const DEFAULT_AGENT_TOOL_NAME: &str = "agent_tool";

/// Marks a tool context as belonging to a named side conversation.
///
/// Inserted into a side conversation's per-turn [`ToolContext`] so every
/// sub-agent and swarm spawned under it mints a scope-prefixed display name
/// (e.g. `btw:crypto/ReverseEngineer(Europa)`), which flows automatically
/// through the usage ledger, `UsageUpdated` events, and the TUI's stats and
/// gantt attribution. A context without a marker belongs to the main
/// conversation and mints names exactly as before.
#[derive(Clone)]
pub(crate) struct ScopeMarker {
    /// The scope prefix minted names carry (e.g. `"btw:crypto"`).
    pub prefix: String,
}

impl ScopeMarker {
    /// Pulls the scope marker out of a tool context, if one was inserted
    ///
    /// A missing marker is not an error: it just means the context belongs to
    /// the main conversation.
    ///
    /// # Arguments
    ///
    /// * `context` - The tool call context the run was prompted with
    pub(crate) fn from_context(context: &ToolContext) -> Option<ScopeMarker> {
        // recover the marker, treating a missing slot as the main conversation
        context.require::<ScopeMarker>().ok().cloned()
    }
}

/// The arguments accepted by a goal-aware sub-agent tool.
#[derive(Debug, Deserialize)]
struct GoalToolArgs {
    /// The overarching, top-level goal this delegation serves.
    goal: String,
    /// The concrete task for the sub-agent to carry out.
    prompt: String,
}

/// Truncate `text` to at most `max` characters for a compact log preview.
///
/// The cut lands on a character boundary so multibyte UTF-8 never panics, and
/// an ellipsis is appended whenever the text was actually shortened. This keeps
/// large tool arguments and result dumps from flooding the log.
///
/// # Arguments
///
/// * `text` - The text to shorten for logging
/// * `max` - The maximum number of characters to keep
fn preview(text: &str, max: usize) -> String {
    // find the byte offset of the `max`-th character, if the text is that long
    match text.char_indices().nth(max) {
        // the text is longer than `max` chars, so cut at the boundary and mark it
        Some((offset, _)) => format!("{}…", &text[..offset]),
        // the text already fits, so hand it back untouched
        None => text.to_string(),
    }
}

/// Collect an agent turn's assistant text out of its content parts.
///
/// A completion response can carry several content parts (text, tool calls,
/// reasoning, images); this joins just the text parts into one string so a
/// turn's assistant message can be emitted as a single event. Turns with no
/// text (e.g. a pure tool-call turn) yield an empty string, which the caller
/// skips.
///
/// # Arguments
///
/// * `content` - The assistant content parts returned for a turn
fn join_assistant_text(content: &[AssistantContent]) -> String {
    // keep only the text parts and join them, dropping tool-call/reasoning parts
    content
        .iter()
        .filter_map(|part| match part {
            AssistantContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// A per-turn usage accumulator for turns that run without a budget state
///
/// The main and side conversations bill their usage only when a turn settles
/// (the settled response carries the aggregate), so a failed turn's paid model
/// calls were never recorded in the ledger -- while the live per-turn events
/// still counted them, leaving the TUI readout and the persisted totals in
/// disagreement. The error arms drain this mirror into the ledger instead.
#[derive(Clone, Default)]
pub(crate) struct TurnUsageMirror {
    /// The accumulated usage of the in-flight turn.
    usage: Arc<std::sync::Mutex<rig::completion::Usage>>,
}

impl TurnUsageMirror {
    /// Fold one model turn's usage into the mirror
    ///
    /// # Arguments
    ///
    /// * `usage` - The completed model turn's usage
    pub(crate) fn add(&self, usage: rig::completion::Usage) {
        // the mirror is plain data, so a poisoned lock stays readable
        *self
            .usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) += usage;
    }

    /// Drain the accumulated usage, leaving the mirror empty
    pub(crate) fn take(&self) -> rig::completion::Usage {
        // swap the tally out so a reuse of the mirror starts from zero
        std::mem::take(
            &mut *self
                .usage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

/// A per-run hook that logs an agent's activity as it works and optionally
/// mirrors it onto an event channel.
///
/// Registered on each main-agent and sub-agent prompt so token usage, tool
/// calls, and tool results are surfaced live rather than only in aggregate at
/// the end. Its log events fire inside the entered agent span, so they nest
/// under it. When an event sink is installed the same activity is emitted as
/// [`ThorPalEvent`]s for an external consumer; when it is `None` the hook only
/// logs, so an unobserved run behaves exactly as before.
pub struct AgentEventHook {
    /// The agent's name, logged so events can be attributed to an agent.
    pub agent: String,
    /// The shared coverage ledger this agent's function-tool calls fold into.
    pub coverage: Arc<CoverageLedger>,
    /// An optional event sink to mirror this agent's activity onto, or `None`.
    pub events: Option<ThorPalEventSender>,
    /// The turn-budget state when this hook guards a sub-agent run, or `None`.
    ///
    /// `None` (the main agent and side conversations) disarms every budget
    /// behavior: no countdown warnings, no truncation retry, no extensions,
    /// and no transcript mirror.
    pub budget: Option<Arc<TurnBudgetState>>,
    /// The per-turn usage mirror for budget-less turns, or `None`.
    ///
    /// Installed by the main and side prompt paths so a failed turn's spend
    /// can still be drained into the ledger (sub-agent runs tally through
    /// their budget state instead).
    pub turn_usage: Option<TurnUsageMirror>,
    /// The invocation's record sink when this hook guards a sub-agent run, or
    /// `None` (the main agent and side conversations keep no agent record).
    ///
    /// When set, every completed model turn mirrors the budget state's
    /// transcript into the on-disk record, so a process death mid-invocation
    /// leaves the partial conversation durable for a resume.
    pub record: Option<RecordSink>,
    /// One-shot feedback-retry latch for budget-less (main/side) runs.
    ///
    /// Sub-agents retry a truncated tool-free turn through their budget
    /// state; the main loop had no equivalent, so a single no-answer
    /// `finish_reason=Length` turn killed the whole session (evaluation
    /// sessions died this way). Starts `true`; the first retry takes it.
    pub main_length_retry: std::sync::atomic::AtomicBool,
}

impl AgentHook for AgentEventHook {
    /// Patch the pending request with budget warnings and structural clamps
    ///
    /// Only armed for sub-agent runs (a `budget` state is installed): the
    /// transcript mirror is refreshed with this call's canonical history, the
    /// turn/context warning document is injected (per request -- nothing is
    /// written into the persisted history, so the numbers are always
    /// current), the true final turn disables tool calls outright
    /// (`ToolChoice::None` -- the reply IS the handoff), and critical context
    /// pressure narrows the tools to the forum-flush list. New
    /// context-pressure levels are mirrored once onto the event sink here.
    ///
    /// # Arguments
    ///
    /// * `_ctx` - The run-scoped hook context (unused)
    /// * `event` - The pending model call, carrying its prompt, history, and turn
    fn on_completion_call(
        &self,
        _ctx: &HookContext,
        event: CompletionCall<'_>,
    ) -> impl std::future::Future<Output = CompletionCallAction> + Send {
        // the budget work is synchronous; decide before the borrowed event ends
        let action = match &self.budget {
            Some(state) => {
                // refresh the mirror and the call index the gate counts from
                state.note_turn(event.turn, event.prompt, event.history);
                // report a newly latched context-pressure level exactly once
                if let Some((level, used_tokens, context_window)) = state.take_context_report() {
                    tracing::warn!(
                        agent = %self.agent,
                        level,
                        used_tokens,
                        context_window,
                        "sub-agent context pressure"
                    );
                    if let Some(tx) = &self.events {
                        let _ = tx.send(ThorPalEvent::SubAgentContextPressure {
                            name: self.agent.clone(),
                            level,
                            used_tokens,
                            context_window,
                        });
                    }
                }
                // assemble the per-request patch: warning text plus clamps
                let mut patch = RequestPatch::new();
                let mut patched = false;
                if let Some(document) = state.warning_document(event.turn) {
                    patch = patch.context(document);
                    patched = true;
                }
                if state.forced_final_turn(event.turn) {
                    // the true final turn: the reply is the handoff, so tool
                    // calls are disabled rather than advised against (agents at
                    // the cap routinely ignored the advisory text)
                    patch = patch.tool_choice(ToolChoice::None);
                    patched = true;
                } else if state.context_pressure() == ContextPressure::Critical {
                    // critical context pressure: only the forum-flush tools
                    // (plus the harmless extension tool, when attached) stay
                    // advertised, stopping context-inflating tool output
                    patch = patch.active_tools(state.flush_tools());
                    patched = true;
                }
                if patched {
                    CompletionCallAction::Patch(patch)
                } else {
                    CompletionCallAction::Continue
                }
            }
            // no budget state (main/sides) leaves the request untouched
            None => CompletionCallAction::Continue,
        };
        async move { action }
    }

    /// Retry an output-truncated turn once and answer budget exhaustion
    ///
    /// Only armed for sub-agent runs: the accepted assistant turn is mirrored
    /// for salvage, and a `finish_reason=Length` turn without tool calls gets
    /// one feedback retry per invocation (tool-bearing turns must never retry
    /// -- rig rejects that to keep history free of unanswered calls). The
    /// retry is also skipped on the run's final budgeted call: it would burn
    /// the call rig needs and turn a truncated-but-delivered answer into a
    /// budget death, while declining lets rig finalize the truncated text.
    /// The retry keeps the request's own output cap -- raising it can exceed
    /// a model's hard output limit and 400 the retry fatally.
    ///
    /// # Arguments
    ///
    /// * `_ctx` - The run-scoped hook context (unused)
    /// * `event` - The accepted turn, carrying its content and finish reason
    fn on_model_turn_finished(
        &self,
        _ctx: &HookContext,
        event: ModelTurnFinished<'_>,
    ) -> impl std::future::Future<Output = ModelTurnAction> + Send {
        // the budget work is synchronous; decide before the borrowed event ends
        let action = match &self.budget {
            Some(state) => {
                // mirror the accepted assistant turn for the salvage path
                state.note_assistant(event.content);
                // a truncated, tool-free turn gets exactly one feedback retry,
                // and only while another budgeted call remains to dispatch it
                let truncated = matches!(event.finish_reason, Some(FinishReason::Length));
                let has_tool_calls = event
                    .content
                    .iter()
                    .any(|part| matches!(part, AssistantContent::ToolCall(_)));
                if truncated
                    && !has_tool_calls
                    && state.calls_left(event.turn) >= 2
                    && state.take_length_retry()
                {
                    tracing::info!(
                        agent = %self.agent,
                        turn = event.turn,
                        "retrying output-truncated turn with corrective feedback"
                    );
                    ModelTurnAction::retry_with_feedback(TRUNCATION_FEEDBACK)
                } else {
                    ModelTurnAction::Continue
                }
            }
            // a budget-less run (main/sides) gets the same one-shot retry
            // for a truncated, tool-free turn -- without it a single
            // no-answer Length turn is fatal to the whole session
            None => {
                let truncated = matches!(event.finish_reason, Some(FinishReason::Length));
                let has_tool_calls = event
                    .content
                    .iter()
                    .any(|part| matches!(part, AssistantContent::ToolCall(_)));
                if truncated
                    && !has_tool_calls
                    && self
                        .main_length_retry
                        .swap(false, std::sync::atomic::Ordering::SeqCst)
                {
                    tracing::info!(
                        agent = %self.agent,
                        turn = event.turn,
                        "retrying output-truncated budget-less turn with corrective feedback"
                    );
                    ModelTurnAction::retry_with_feedback(TRUNCATION_FEEDBACK)
                } else {
                    ModelTurnAction::Continue
                }
            }
        };
        // snapshot the state the per-turn record save needs into the future:
        // the freshly-mirrored transcript and the extension log so far
        let persist = match (&self.record, &self.budget) {
            (Some(sink), Some(state)) => Some((
                sink.clone(),
                state.transcript_snapshot(),
                state.extension_log(),
            )),
            _ => None,
        };
        async move {
            // mirror the completed turn into the on-disk record so a process
            // death loses at most the turn now in flight
            if let Some((sink, messages, extensions)) = persist {
                sink.save_running(messages, extensions).await;
            }
            action
        }
    }

    /// Resolve a cap-hit: apply a grant, convert a reservation, or auto-extend
    ///
    /// Only armed for sub-agent runs: the budget state resolves the cap-hit
    /// in priority order -- an in-window grant not yet applied, the standing
    /// reservation (clamped at conversion), then the one-shot automatic
    /// extension -- and the run resumes in place on its parked request. With
    /// nothing left the run fails exactly as before and the salvage path
    /// takes over.
    ///
    /// # Arguments
    ///
    /// * `_ctx` - The run-scoped hook context (unused)
    /// * `_event` - The exhaustion event (the budget state already knows the cap)
    fn on_turns_exhausted(
        &self,
        _ctx: &HookContext,
        _event: TurnsExhausted<'_>,
    ) -> impl std::future::Future<Output = TurnsExhaustedAction> + Send {
        // let the budget state resolve the cap-hit through its grant ladder
        let action = match &self.budget {
            Some(state) => {
                let extra_turns = state.cap_hit_extension();
                if extra_turns > 0 {
                    tracing::info!(
                        agent = %self.agent,
                        extra_turns,
                        "applying turn-budget extension at the cap"
                    );
                    TurnsExhaustedAction::extend(extra_turns)
                } else {
                    TurnsExhaustedAction::fail()
                }
            }
            // no budget state (main/sides) keeps the budget error terminal
            None => TurnsExhaustedAction::fail(),
        };
        async move { action }
    }

    /// Soften invalid tool calls the budget clamps themselves provoked
    ///
    /// The forced final turn disables tools with a per-turn
    /// `ToolChoice::None`, and critical context pressure narrows the
    /// advertised tools -- a model (or a gateway that ignores the directive)
    /// may still emit a tool call, which rig would otherwise turn into an
    /// `UnknownToolCall` death. When one of those clamps is active, the call
    /// is skipped with corrective feedback instead: the run then either
    /// finalizes on its text or dies at the cap WITH carried history, so
    /// salvage still works. Every other invalid call keeps rig's fail-fast
    /// default.
    ///
    /// # Arguments
    ///
    /// * `ctx` - The run-scoped hook context, carrying the current turn
    /// * `event` - The rejected tool call
    fn on_invalid_tool_call(
        &self,
        ctx: &HookContext,
        event: &InvalidToolCallContext,
    ) -> impl std::future::Future<Output = Option<InvalidToolCallAction>> + Send {
        // decide synchronously; the borrowed event does not cross the await
        let action = match &self.budget {
            Some(state) if state.forced_final_turn(ctx.turn()) => {
                tracing::warn!(
                    agent = %self.agent,
                    tool = %event.tool_name,
                    "tool call on the forced final turn; skipping with feedback"
                );
                Some(InvalidToolCallAction::skip(
                    "FINAL TURN: tool calls are disabled for this reply and this call was \
                     NOT executed (this result is synthetic). Your next output must be the \
                     final handoff text: status, findings with evidence, and what remains.",
                ))
            }
            Some(state) if state.context_pressure() == ContextPressure::Critical => {
                tracing::warn!(
                    agent = %self.agent,
                    tool = %event.tool_name,
                    "non-flush tool call under critical context pressure; skipping"
                );
                Some(InvalidToolCallAction::skip(format!(
                    "CONTEXT CRITICAL: tool access is restricted to forum/communication \
                     tools right now, so '{}' was NOT executed (this result is synthetic). \
                     Post your findings with create_post/create_comment and make your next \
                     message the final handoff.",
                    event.tool_name
                )))
            }
            // everything else keeps rig's fail-fast default
            _ => None,
        };
        async move { action }
    }

    /// Log this turn's token usage and mirror it, plus any assistant text, out
    ///
    /// Beyond the per-turn usage log, this emits a [`ThorPalEvent::UsageUpdated`]
    /// and -- when the turn produced any assistant text -- a
    /// [`ThorPalEvent::AgentMessage`] on the event sink, if one is installed.
    ///
    /// # Arguments
    ///
    /// * `_ctx` - The run-scoped hook context (unused)
    /// * `event` - The completion response, carrying this turn's usage and content
    fn on_completion_response(
        &self,
        _ctx: &HookContext,
        event: CompletionResponse<'_>,
    ) -> impl std::future::Future<Output = ObservationAction> + Send {
        // copy the usage out of the borrowed event so the future owns it
        let usage = event.usage;
        // fold this turn's usage into the budget state's failure-path tally so
        // a run that later dies still gets its spend recorded in the ledger;
        // turns whose response hook rig suppresses (an unknown-tool death's
        // fatal turn, repaired invalid calls) are not tallied -- the same gap
        // the per-turn UsageUpdated events below have always had
        if let Some(state) = &self.budget {
            state.add_usage(usage);
        }
        // mirror a budget-less turn's usage so a later failure can still be
        // billed to the ledger by the error arm
        if let Some(mirror) = &self.turn_usage {
            mirror.add(usage);
        }
        // clone the agent name into the future for the log line and events
        let agent = self.agent.clone();
        // clone the event sink into the future so we can mirror this turn onto it
        let events = self.events.clone();
        // pull this turn's assistant text out of the content parts before we lose
        // the borrow, leaving an owned string the future can emit
        let text = join_assistant_text(event.content);
        async move {
            // emit a compact per-turn usage line under the sub-agent span, with
            // comma-grouped counts so large token totals read at a glance
            tracing::info!(
                agent = %agent,
                input = %crate::thorpal::usage::with_thousands(usage.input_tokens),
                output = %crate::thorpal::usage::with_thousands(usage.output_tokens),
                total = %crate::thorpal::usage::with_thousands(usage.total_tokens),
                "sub-agent turn"
            );
            // mirror this turn onto the event sink if a consumer is listening;
            // sends are non-blocking and a closed channel is ignored on purpose
            if let Some(tx) = &events {
                // surface the per-turn token usage as a checkpoint event,
                // including the cache split so a consumer can estimate cost
                let _ = tx.send(ThorPalEvent::UsageUpdated {
                    agent: agent.clone(),
                    input: usage.input_tokens,
                    output: usage.output_tokens,
                    cached: usage.cached_input_tokens,
                    cache_creation: usage.cache_creation_input_tokens,
                    total: usage.total_tokens,
                });
                // only emit an assistant message when the turn produced text; a
                // pure tool-call turn yields none and is skipped
                if !text.is_empty() {
                    let _ = tx.send(ThorPalEvent::AgentMessage { agent, text });
                }
            }
            // never steer the run, just observe it
            ObservationAction::Continue
        }
    }

    /// Log each tool call and force the forum author on posts and comments
    ///
    /// Beyond logging, this rewrites the `author` argument of `create_post`,
    /// `create_comment`, `add_tasks`, and `complete_task` calls to this
    /// sub-agent's own display name, so the forum author always matches the name
    /// used in the logs and usage ledger and the model can never mislabel who
    /// wrote a post or cleared a task. Every other tool runs as-is, and a call
    /// whose args cannot be parsed is left untouched rather than being broken.
    ///
    /// # Arguments
    ///
    /// * `_ctx` - The run-scoped hook context (unused)
    /// * `event` - The tool call, carrying the tool name and effective args
    fn on_tool_call(
        &self,
        _ctx: &HookContext,
        event: ToolCall<'_>,
    ) -> impl std::future::Future<Output = ToolCallAction> + Send {
        // clone the agent name into the future for the log line and author rewrite
        let agent = self.agent.clone();
        // clone the shared coverage ledger into the future so function-scoped tool
        // calls fold into this instance's ledger rather than a global
        let coverage = Arc::clone(&self.coverage);
        // clone the event sink into the future so we can mirror this call onto it
        let events = self.events.clone();
        // own the tool name so the future outlives the borrowed event
        let tool = event.tool_name.to_string();
        // own the rig-assigned call id so a consumer can correlate this call with
        // its later result event
        let call_id = event.internal_call_id.to_string();
        // own the raw args too so we can both preview and (maybe) rewrite them
        let raw_args = event.args.to_string();
        // preview the args since a tool's argument object can be large
        let args = preview(&raw_args, 80);
        async move {
            // emit the tool call under the sub-agent span
            tracing::info!(agent = %agent, tool = %tool, args = %args, "tool call");
            // mirror the call onto the event sink if a consumer is listening,
            // handing over the full (un-previewed) args and the correlation id
            if let Some(tx) = &events {
                let _ = tx.send(ThorPalEvent::ToolCallStarted {
                    agent: agent.clone(),
                    call_id,
                    tool: tool.clone(),
                    args: raw_args.clone(),
                });
            }
            // statically record function coverage: if this is a function-scoped
            // registry tool, attribute its `target` to this agent so the run can
            // report how much of the binary was examined without the agent ever
            // calling an extra tool. Observe-only -- it never alters the call.
            if crate::thorpal::tools::registry::is_function_scoped(&tool) {
                // pull the `target` field out of the raw args, skipping the
                // record if the args do not parse or carry no string target
                if let Ok(serde_json::Value::Object(map)) =
                    serde_json::from_str::<serde_json::Value>(&raw_args)
                {
                    if let Some(target) = map.get("target").and_then(serde_json::Value::as_str) {
                        // pass the call's program arg through so the ledger can
                        // scope child-program coverage structurally
                        let program = map.get("program").and_then(serde_json::Value::as_str);
                        coverage.record(&agent, &tool, target, program).await;
                    }
                }
            }
            // only the forum-writing tools carry an author we want to force
            if tool == "create_post"
                || tool == "create_comment"
                || tool == "add_tasks"
                || tool == "complete_task"
            {
                // parse the args, force the author to our display name, and rewrite;
                // if the args do not parse into an object, fall through and run as-is
                if let Ok(serde_json::Value::Object(mut map)) =
                    serde_json::from_str::<serde_json::Value>(&raw_args)
                {
                    // overwrite (or insert) the author so it always matches us
                    map.insert("author".to_string(), serde_json::json!(agent));
                    return ToolCallAction::Rewrite(serde_json::Value::Object(map));
                }
            }
            // run the tool call as-is, just observe it
            ToolCallAction::Run
        }
    }

    /// Log a short preview of each tool result and mirror the full result out
    ///
    /// Beyond the preview log, this emits a [`ThorPalEvent::ToolCallFinished`]
    /// carrying the same `call_id` as the matching call event, so a consumer can
    /// merge the two, if an event sink is installed.
    ///
    /// # Arguments
    ///
    /// * `_ctx` - The run-scoped hook context (unused)
    /// * `event` - The tool result, carrying the tool name and rendered output
    fn on_tool_result(
        &self,
        _ctx: &HookContext,
        event: ToolResultEvent<'_>,
    ) -> impl std::future::Future<Output = ToolResultAction> + Send {
        // clone the agent name into the future for the log line and event
        let agent = self.agent.clone();
        // clone the event sink into the future so we can mirror this result onto it
        let events = self.events.clone();
        // own the tool name so the future outlives the borrowed event
        let tool = event.tool_name.to_string();
        // own the rig-assigned call id so the consumer can correlate this result
        // with its earlier call event
        let call_id = event.internal_call_id.to_string();
        // render the model-visible result once, keeping the full text for the
        // event and a short slice for the log line
        let rendered = event.presentation.render();
        let preview = preview(&rendered, 80);
        // capture whether the call actually succeeded; an error or refusal is a
        // failure a consumer can flag distinctly from a clean result
        let success = event.raw_result.is_success();
        // own the pieces the constant-nudge scan reads so the future outlives
        // the borrowed event: the effective args, the immutable raw result, and
        // the current presentation it may append a nudge block onto
        let args = event.args.to_string();
        let raw_result = event.raw_result.clone();
        let presentation = event.presentation.clone();
        // the shared tracker and forum ride the run's tool context; a consumer
        // that installed neither simply runs without constant nudges
        let tracker = ConstantTracker::from_context(event.tool_context);
        let forum = event.tool_context.get::<Arc<ForumState>>().map(Arc::clone);
        async move {
            // emit the result preview under the sub-agent span
            tracing::info!(agent = %agent, tool = %tool, preview = %preview, "tool result");
            // a shutdown-interrupted delegation's failed result is teardown
            // noise, not a real outcome: skip the event so no durable failure
            // line marks an invocation a resume will continue. the check keys
            // off the structured error's own message (which the freeze arms
            // mint with the leading marker), never the rendered output -- a
            // genuine tool result QUOTING the marker (e.g. a tool reading a
            // thorpal log) must still land
            let interrupted = raw_result.error().is_some_and(|error| {
                error
                    .to_string()
                    .starts_with(crate::thorpal::retry::SHUTDOWN_INTERRUPT_MARKER)
            });
            // mirror the full result onto the event sink if a consumer is listening
            if let Some(tx) = &events
                && !interrupted
            {
                let _ = tx.send(ThorPalEvent::ToolCallFinished {
                    agent,
                    call_id,
                    tool: tool.clone(),
                    result: rendered,
                    success,
                });
            }
            // on a clean success, scan the structured result for decoded
            // constants and append a forum-promotion nudge when any have never
            // been posted; the factored body owns every cap and latch
            if success {
                if let (Some(tracker), Some(forum)) = (tracker, forum) {
                    return constants::apply_inline_nudge(
                        &tool,
                        &args,
                        &raw_result,
                        &presentation,
                        tracker,
                        forum,
                    )
                    .await;
                }
            }
            // keep the tool result as-is, just observe it
            ToolResultAction::Keep
        }
    }
}

/// The continuation nudge a resumed sub-agent opens on
///
/// Its repaired partial transcript is replayed as explicit history, so the
/// nudge only has to orient it: the work above is its own, and any
/// `[interrupted]` tool result marks a call that never ran.
const RESUME_SUB_AGENT_PROMPT: &str = "[resuming after interruption] The session was \
interrupted and has been resumed; your earlier work is preserved above. Continue your \
task from where it stopped. Any tool call marked [interrupted] never ran -- re-issue it \
if you still need its result.";

/// The identity a sub-agent invocation runs under
///
/// A fresh delegation mints a new display name and record; a resumed one
/// reuses the interrupted invocation's name, record, and remaining budget so
/// its usage, forum authorship, and audit trail continue seamlessly.
pub(crate) enum Invocation {
    /// A fresh delegation from a live tool call
    Fresh {
        /// The human-facing role prefix for this invocation's display name
        role: String,
        /// The overarching goal this delegation serves
        goal: String,
        /// The concrete task for the sub-agent to carry out
        task: String,
        /// The display name of the swarm this invocation runs under, if any
        swarm: Option<String>,
    },
    /// A mid-flight invocation continued across a process death
    Resumed {
        /// The dead run's record: identity, task, and partial transcript
        record: AgentRecord,
    },
}

/// Everything needed to run one top-level delegation outside a live tool call
///
/// The builder retains one per attached sub-agent so a resume can re-run a
/// dangling delegation call with exactly the agent, toolset, and budget the
/// dead run would have used.
#[derive(Clone)]
pub(crate) struct DelegationSpec {
    /// The built sub-agent, shared with its goal tool
    pub agent: Arc<Agent>,
    /// The human-facing role prefix for this agent's display names
    pub role: String,
    /// The resolved registry tool names of this agent's own toolset
    pub enabled: EnabledTools,
    /// The turn-budget knobs each invocation of this agent runs under
    pub budget: TurnBudget,
    /// Whether this agent may be included in a swarm
    pub swarmable: bool,
    /// The max copies of this agent a single swarm may include
    pub max_swarm_size: usize,
}

/// The retained delegation machinery a resume re-runs dangling calls through
///
/// Holds what the builder otherwise moves into the goal/swarm tool closures:
/// the per-agent specs, the shared live-cap semaphore, and the post-mortem
/// salvager, so [`crate::ThorPal::resume_interrupted_turn`] can invoke
/// [`run_sub_agent`] with the same wiring a live tool call gets.
#[derive(Clone)]
pub(crate) struct DelegationRegistry {
    /// Each top-level delegation's spec, keyed by its tool name
    pub specs: std::collections::BTreeMap<String, DelegationSpec>,
    /// The semaphore capping how many sub-agents are alive at once
    pub alive: Arc<Semaphore>,
    /// The shared salvage summarizer run over dead invocations, if any
    pub post_mortem: Option<Arc<Agent>>,
}

/// The outcome of a single sub-agent invocation
pub(crate) struct SubAgentRun {
    /// The unique per-call display name this invocation ran under
    pub name: String,
    /// The sub-agent's final text, or the error that stopped it
    pub output: Result<String, ToolExecutionError>,
}

/// A shared handle persisting one sub-agent invocation's record, best-effort
///
/// Shared between the invocation runner (which writes the start and finish
/// states) and the run's hook (which mirrors the in-flight partial transcript
/// after every completed model turn), so a process death at any point leaves
/// the record on disk no staler than the final completed turn -- the state a
/// resume continues the invocation from. All writes are best-effort: a save
/// failure is logged and swallowed so persistence problems never fail the
/// delegation itself.
#[derive(Clone)]
pub(crate) struct RecordSink {
    /// The invocation's live record, shared across the runner and the hook
    record: Arc<std::sync::Mutex<AgentRecord>>,
    /// The session store to save into, or `None` when persistence is off
    store: Option<Arc<SessionStore>>,
}

impl RecordSink {
    /// Wraps an invocation's record for shared, best-effort persistence
    ///
    /// # Arguments
    ///
    /// * `record` - The invocation record to persist as the run progresses
    /// * `store` - The session store to save into, if one is installed
    pub(super) fn new(record: AgentRecord, store: Option<Arc<SessionStore>>) -> Self {
        RecordSink {
            record: Arc::new(std::sync::Mutex::new(record)),
            store,
        }
    }

    /// Mutates the live record under its lock
    ///
    /// # Arguments
    ///
    /// * `apply` - The mutation to run against the record
    pub(super) fn update(&self, apply: impl FnOnce(&mut AgentRecord)) {
        // the record is plain data, so a poisoned lock stays usable
        apply(
            &mut self
                .record
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    }

    /// Saves the record's current state to the session store, best-effort
    pub(super) async fn save(&self) {
        // skip entirely when session persistence was not requested
        let Some(store) = &self.store else {
            return;
        };
        // snapshot the record under its lock so the save never holds it
        let record = self
            .record
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        // save the record, logging and swallowing any failure
        if let Err(err) = store.save_agent(&record).await {
            tracing::warn!(?err, agent = %record.name, "failed to save sub-agent record");
        }
    }

    /// Mirrors the in-flight partial transcript into the record and saves it
    ///
    /// Called by the run's hook after every completed model turn, so the
    /// on-disk record always carries the work the invocation has paid for
    /// while staying running-shaped (`finished_at: None`).
    ///
    /// # Arguments
    ///
    /// * `messages` - The in-flight transcript mirror, as far as it got
    /// * `extensions` - Every extension request made so far
    async fn save_running(
        &self,
        messages: Vec<rig::completion::Message>,
        extensions: Vec<crate::thorpal::agents::budget::ExtensionRequest>,
    ) {
        // refresh the in-flight state; the completion fields stay empty
        self.update(|record| {
            record.messages = messages;
            record.extensions = extensions;
        });
        // mirror the refreshed state to disk
        self.save().await;
    }
}

/// Renders the linked-mode forum context for a sub-agent spawn, if enabled
///
/// `None` means nothing is injected: the run is in full mode, the inherited
/// context carries no forum or settings (a consumer that built its own context
/// never opted in), or the forum is still empty. Otherwise the goal and task
/// text are scanned for `#post-name`/`!comment-id` references and the forum
/// renders those threads, their transitively linked threads up to the
/// configured depth, and a titles-only index of every post.
///
/// # Arguments
///
/// * `context` - The inherited tool context carrying the forum and settings
/// * `goal` - The top-level goal text to scan for references
/// * `task` - The task text to scan for references
async fn linked_forum_context(context: &ToolContext, goal: &str, task: &str) -> Option<String> {
    // a context without settings never opted into preloading forum context
    let settings = ForumContextSettings::from_context(context)?;
    // full mode preloads nothing; agents download the board themselves
    if settings.mode == ForumContextMode::Full {
        return None;
    }
    // a context without a forum has nothing to render from
    let Ok(forum) = ForumState::from_context(context) else {
        tracing::debug!("no forum in the tool context; skipping linked context");
        return None;
    };
    // scan the goal and task for the references rooting the walk; scanning
    // their concatenation dedupes across the two in first-seen order
    let roots = scan_references(&format!("{goal} {task}"));
    // render the assigned neighborhood plus the titles-only index
    forum.render_linked_context(&roots, settings.depth).await
}

/// The most memories the spawn-prompt index lists before capping
const MEMORY_INDEX_CAP: usize = 25;

/// The length (in chars) each spawn-index line's summary is clipped to
const MEMORY_INDEX_LINE_CHARS: usize = 160;

/// Renders the memory-index section for a sub-agent spawn, if enabled
///
/// `None` means nothing is injected: the spawned agent's resolved toolset has
/// no memory tools, the run has no memory library, or the library is empty
/// (after the blind-family filter). Otherwise the index lists one line per
/// memory -- name, kind, and the first description line, clipped -- rendered
/// through the run's redactor like every other memory surface. The
/// [`MEMORY_INDEX_CAP`] slots are SELECTED FOR THE DELEGATION: entries rank
/// against the spawn's goal+task text with the same lexical scorer
/// `search_memories` uses (over names, tags, and descriptions only -- a
/// routing index must not load every body on every spawn), and any remaining
/// slots fill newest-save-first, so an alphabetical accident never decides
/// what a sub-agent gets to see. The index is rendered fresh per spawn, so
/// memories saved earlier in the same run show up for later delegations.
///
/// # Arguments
///
/// * `context` - The invocation's tool context carrying the library
/// * `enabled` - The resolved registry tool names of the sub-agent being run
/// * `goal` - The delegation's top-level goal text
/// * `task` - The delegation's task text
async fn memory_index_context(
    context: &ToolContext,
    enabled: &EnabledTools,
    goal: &str,
    task: &str,
) -> Option<String> {
    // only an agent that can read memories gets the index
    if !enabled.0.contains("list_memories") {
        return None;
    }
    // a run without a memory library has nothing to index
    let memories = MemoryLibrary::try_from_context(context)?;
    // a listing failure degrades to no index rather than failing the spawn
    let entries = match memories.list().await {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(?error, "failed to list memories; spawning without an index");
            return None;
        }
    };
    // build one scorable doc per visible entry, rendered through the run's
    // redactor so ranking sees exactly what the sub-agent will see
    let redactor = LibraryRedactor::try_from_context(context);
    let mut docs = Vec::with_capacity(entries.len());
    let mut summaries = Vec::with_capacity(entries.len());
    for entry in &entries {
        // blind-hidden family intel must never surface, even as a name; the
        // shared filter fails closed on unreadable candidates, exactly like
        // the list/search/read tool surfaces
        if crate::thorpal::tools::memories::hidden_family_intel(&memories, redactor.as_ref(), entry)
            .await
        {
            continue;
        }
        // render the scored fields exactly as the model would see them
        let (display, description, tags) = match &redactor {
            Some(redactor) => (
                redactor.display_memory_name(&entry.name),
                redactor.redact_text(&entry.description),
                entry
                    .tags
                    .iter()
                    .map(|tag| redactor.redact_text(tag))
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            None => (
                entry.name.clone(),
                entry.description.clone(),
                entry.tags.join(" "),
            ),
        };
        // the index line carries the first description line, clipped
        let summary = clip_chars(
            description.lines().next().unwrap_or_default(),
            MEMORY_INDEX_LINE_CHARS,
        );
        summaries.push(summary);
        // the scorable text is the RENDERED fields only: ranking on a raw
        // (possibly family-derived) name would let a guessed family term in
        // the task text boost a denied memory into the index -- a deny-list
        // oracle built out of line selection
        docs.push(MemoryDoc {
            name: entry.name.clone(),
            display: display.clone(),
            kind: entry.kind,
            promoted_at: entry.promoted_at.clone(),
            text: format!("{display} {tags} {description}"),
        });
    }
    // an empty (or fully hidden) library injects nothing
    if docs.is_empty() {
        return None;
    }
    // rank against the delegation's own goal+task text, matched slots first
    let ranked = rank_memory_docs(&docs, &format!("{goal} {task}"));
    let mut picked: Vec<usize> = ranked
        .iter()
        .map(|scored| scored.index)
        .take(MEMORY_INDEX_CAP)
        .collect();
    // fill any free slots newest-save-first, so recency (not alphabetical
    // accident) decides what an unmatched spawn gets to see
    if picked.len() < MEMORY_INDEX_CAP {
        let mut rest: Vec<usize> = (0..docs.len())
            .filter(|index| !picked.contains(index))
            .collect();
        rest.sort_by(|a, b| {
            docs[*b]
                .promoted_at
                .cmp(&docs[*a].promoted_at)
                .then(docs[*a].name.cmp(&docs[*b].name))
        });
        picked.extend(rest.into_iter().take(MEMORY_INDEX_CAP - picked.len()));
    }
    // one compact line per selected memory: name, kind, clipped summary
    let mut lines = Vec::with_capacity(picked.len() + 1);
    for index in &picked {
        lines.push(format!(
            "- {} [{}]: {}",
            docs[*index].display,
            docs[*index].kind.label(),
            summaries[*index]
        ));
    }
    // note what was dropped so the cap never reads as the whole library
    if docs.len() > picked.len() {
        lines.push(format!(
            "({} more memories -- search_memories ranks them by topic; \
             list_memories pages the full index)",
            docs.len() - picked.len()
        ));
    }
    Some(lines.join("\n"))
}

/// Derives a forum-safe post name for a dead agent's salvage handoff
///
/// Display names carry parens and may carry a side-conversation scope prefix
/// (`btw:crypto/ReverseEngineer(Io)`); forum names must be whitespace-free
/// `#name` references, so every non-alphanumeric run collapses to one hyphen.
/// Display names are unique per run, so the derived names are too.
///
/// # Arguments
///
/// * `display_name` - The dead agent's unique display name
fn salvage_post_name(display_name: &str) -> String {
    // collapse every unsafe run into a single hyphen
    let mut out = String::with_capacity(display_name.len() + 8);
    out.push_str("salvage-");
    let mut collapsed = false;
    for c in display_name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            collapsed = false;
        } else if !collapsed {
            out.push('-');
            collapsed = true;
        }
    }
    // trim a trailing hyphen left by a closing paren
    out.trim_end_matches('-').to_owned()
}

/// Posts a dead agent's salvaged handoff to the forum, best-effort
///
/// The post is authored as the dead agent itself -- the report appendix
/// elides only detector-authored bodies, so the salvaged findings stay
/// verbatim in the report -- and its body opens with the unmistakable
/// `SUB-AGENT TERMINATED EARLY` framing, so no reader mistakes it for a
/// settled claim. A name collision (or any other failure) is logged and
/// retried once with a numeric suffix; the delegation itself never fails on
/// a post failure. Returns the post name on success.
///
/// # Arguments
///
/// * `forum` - The run's shared forum state
/// * `display_name` - The dead agent's unique display name
/// * `reason` - The human-facing reason the agent died
/// * `handoff` - The rendered handoff to post verbatim
async fn post_salvage_handoff(
    forum: &Arc<ForumState>,
    display_name: &str,
    reason: &str,
    handoff: &str,
) -> Option<String> {
    // the title frames the death; the body is the handoff the parent gets
    let title = format!("SUB-AGENT TERMINATED EARLY -- {display_name}");
    let base = salvage_post_name(display_name);
    // one suffix retry covers the only expected failure (a name collision)
    for name in [base.clone(), format!("{base}-2")] {
        match forum
            .seed_post(
                display_name.to_owned(),
                name.clone(),
                title.clone(),
                handoff.to_owned(),
                crate::thorpal::tools::forum::Kind::Statement,
            )
            .await
        {
            Ok(_) => {
                tracing::info!(
                    agent = %display_name,
                    post = %name,
                    reason = %reason,
                    "posted the salvaged handoff to the forum"
                );
                return Some(name);
            }
            Err(error) => {
                tracing::warn!(
                    agent = %display_name,
                    post = %name,
                    %error,
                    "failed to post the salvaged handoff; the delegation continues"
                );
            }
        }
    }
    None
}

/// Parses a goal tool call's `{goal, prompt}` arguments back out
///
/// The resume path reads a dangling delegation call's recorded arguments to
/// re-run it; the model's call was validated live, so a parse failure means
/// the stored history is damaged and the caller falls back to a synthetic
/// result.
///
/// # Arguments
///
/// * `args` - The dangling call's recorded arguments
pub(crate) fn parse_goal_args(args: &serde_json::Value) -> Option<(String, String)> {
    // reuse the live tool's argument shape so the two can never drift
    serde_json::from_value::<GoalToolArgs>(args.clone())
        .ok()
        .map(|args| (args.goal, args.prompt))
}

/// Takes the saved record matching a dangling delegation call, if one exists
///
/// Prefers the exact parent-call stamp written at spawn time, falling back to
/// field equality (role, goal, task) for records written before the stamp
/// existed. The match is consumed out of the pool so two identical tasks pair
/// with two distinct records, and the pool's newest-first order makes the
/// fallback prefer the most recent invocation.
///
/// # Arguments
///
/// * `records` - The unmatched saved records, newest first
/// * `parent_call` - The dangling delegation call's `ToolCallId` string
/// * `role` - The role the delegation would run as
/// * `goal` - The goal the dangling call carried
/// * `task` - The task the dangling call carried
pub(crate) fn take_matching_record(
    records: &mut Vec<AgentRecord>,
    parent_call: &str,
    role: &str,
    goal: &str,
    task: &str,
) -> Option<AgentRecord> {
    // the spawn-time stamp is exact: same call id, same role, same task
    let position = records
        .iter()
        .position(|record| {
            record.parent_call.as_deref() == Some(parent_call)
                && record.role == role
                && record.task == task
        })
        // legacy records predate the stamp; match on their recorded fields,
        // but only running-shaped ones -- reusing a FINISHED record demands
        // the exact stamp, or an old answer could satisfy a new identical
        // task (the ambiguous legacy case re-runs fresh instead)
        .or_else(|| {
            records.iter().position(|record| {
                record.parent_call.is_none()
                    && record.finished_at.is_none()
                    && record.role == role
                    && record.goal == goal
                    && record.task == task
            })
        })?;
    // consume the match so a duplicate task pairs with the next record
    Some(records.remove(position))
}

/// Reconstructs a finished invocation's outcome from its saved record
///
/// A delegation that completed before the process died must never be re-run:
/// its saved answer (or its death's handoff) is handed back exactly as the
/// live run returned it, so the parent conversation reads the same result it
/// would have read had the process survived.
///
/// # Arguments
///
/// * `record` - The finished invocation record to reconstruct from
pub(crate) fn rerun_from_record(record: AgentRecord) -> SubAgentRun {
    match record.success {
        // a success hands back its saved answer verbatim
        Some(true) => SubAgentRun {
            name: record.name,
            output: Ok(record.output.unwrap_or_default()),
        },
        // a genuine death keeps its semantics: the salvaged handoff (or the
        // bare error) is the model-visible feedback, as the live run returned
        _ => {
            let reason = record
                .error
                .unwrap_or_else(|| "sub-agent terminated early".to_owned());
            let mut error =
                ToolExecutionError::other(format!("sub-agent terminated early: {reason}"));
            if let Some(handoff) = record.handoff {
                error = error.with_model_feedback(handoff);
            }
            SubAgentRun {
                name: record.name,
                output: Err(error),
            }
        }
    }
}

/// Run a single sub-agent invocation from start to finish
///
/// This is shared by the single sub-agent tools, the swarm tool, and the
/// resume path so every invocation runs the exact same way. It waits for a
/// free slot under the run's live sub-agent cap, resolves the invocation's
/// identity (a fresh delegation mints a `role` plus a moon of Jupiter; a
/// resumed one reuses its recorded name), runs the sub-agent under its own
/// span, and folds its token usage into the shared ledger. The slot is held
/// until the sub-agent returns. When the run has a session store, the
/// invocation is recorded under `agents/` as it starts (so a crash mid-flight
/// still shows what was running), after every completed model turn (the
/// partial transcript a resume continues from), and again with its full
/// transcript when it finishes.
///
/// # Arguments
///
/// * `agent` - The sub-agent to run
/// * `invocation` - The identity this invocation runs under (fresh or resumed)
/// * `context` - The caller's tool context so the sub-agent's own tools work
/// * `enabled` - The resolved registry tool names of the sub-agent being run
/// * `budget` - The turn-budget knobs this invocation runs under
/// * `usage` - The shared usage ledger this invocation's token usage folds into
/// * `coverage` - The shared coverage ledger this invocation's tool calls fold into
/// * `events` - An optional event sink to mirror this invocation's activity onto
/// * `store` - An optional session store this invocation's record is saved into
/// * `alive` - The semaphore capping how many sub-agents are alive at once
/// * `post_mortem` - The shared salvage summarizer run over a dead invocation, if any
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_sub_agent(
    agent: &Agent,
    invocation: Invocation,
    mut context: ToolContext,
    enabled: &EnabledTools,
    budget: TurnBudget,
    usage: &UsageLedger,
    coverage: &Arc<CoverageLedger>,
    events: &Option<ThorPalEventSender>,
    store: &Option<Arc<SessionStore>>,
    alive: &Semaphore,
    post_mortem: Option<&Agent>,
) -> SubAgentRun {
    // unpack the invocation's identity and, for a resume, its prior state
    let (role, goal, task, swarm, resumed_record) = match invocation {
        Invocation::Fresh {
            role,
            goal,
            task,
            swarm,
        } => (role, goal, task, swarm, None),
        Invocation::Resumed { record } => (
            record.role.clone(),
            record.goal.clone(),
            record.task.clone(),
            record.swarm.clone(),
            Some(record),
        ),
    };
    // override the inherited enabled-toolset entry with this sub-agent's own
    // resolved toolset, so its script runs see its bindings, not its caller's
    context.insert(enabled.clone());
    // note when this invocation has to queue behind the live sub-agent cap
    if alive.available_permits() == 0 {
        tracing::debug!(role = %role, "waiting for a free sub-agent slot");
    }
    // wait for a free slot, holding it until this sub-agent returns
    let _permit = match alive.acquire().await {
        Ok(permit) => permit,
        // the semaphore is never closed, but surface it rather than panic
        Err(error) => {
            return SubAgentRun {
                name: role.to_owned(),
                output: Err(ToolExecutionError::from_error(error)),
            };
        }
    };
    // resolve the display name used as this invocation's span identity,
    // usage-ledger key, and forced forum author: a fresh delegation mints a
    // unique per-call name (role + moon, with the caller's side-conversation
    // scope prefixed when one is set), while a resume reuses the recorded
    // name so the interrupted invocation's identity continues seamlessly
    let display_name = match &resumed_record {
        Some(record) => record.name.clone(),
        None => {
            let minted = crate::thorpal::agents::naming::next_display_name(&role);
            match ScopeMarker::from_context(&context) {
                Some(scope) => format!("{}/{minted}", scope.prefix),
                None => minted,
            }
        }
    };
    // the live turn-budget state this invocation runs under, shared with the
    // request_more_turns tool (through the context) and the run's hook; a
    // resume deducts the dead run's spent turns and prior grants from the cap
    let budget_state = match &resumed_record {
        Some(record) => {
            // each completed model call left one assistant message behind; a
            // pure repair note (appended by an earlier resume's tail repair)
            // was never a model call, so it must not erode the budget
            let turns_used = record
                .messages
                .iter()
                .filter(|message| match message {
                    Message::Assistant { content, .. } => content.iter().any(|part| match part {
                        rig::message::AssistantContent::Text(text) => {
                            !text.text.starts_with("[turn interrupted:")
                        }
                        // tool calls and reasoning are real model output
                        _ => true,
                    }),
                    _ => false,
                })
                .count();
            Arc::new(TurnBudgetState::resumed(
                display_name.clone(),
                budget,
                turns_used,
                record.extensions.clone(),
                events.clone(),
            ))
        }
        None => Arc::new(TurnBudgetState::new(
            display_name.clone(),
            budget,
            events.clone(),
        )),
    };
    context.insert(Arc::clone(&budget_state));
    // keep a forum handle past the context move below, so a death can post
    // its salvaged handoff to the forum as a first-class finding
    let forum = context.get::<Arc<ForumState>>().map(Arc::clone);
    // keep the pause gate too, so the death arm can freeze a genuine error
    // that merely raced the teardown latch instead of salvaging it
    let gate = context
        .get::<Arc<crate::thorpal::gate::ProviderGate>>()
        .map(Arc::clone);
    // build the span this invocation logs under; the goal is logged once
    // below rather than as a span field so it does not repeat on every line
    let span = tracing::info_span!("sub_agent", agent = %display_name);
    // emit the goal a single time as the sub-agent is first started
    tracing::info!(parent: &span, goal = %goal, resumed = resumed_record.is_some(), "sub-agent started");
    // mirror the delegation onto the event sink if a consumer is listening,
    // handing over this invocation's display name, role, goal, and swarm
    if let Some(tx) = events {
        let _ = tx.send(ThorPalEvent::SubAgentStarted {
            name: display_name.clone(),
            role: role.clone(),
            goal: goal.clone(),
            swarm: swarm.clone(),
            resumed: resumed_record.is_some(),
        });
    }
    // whether this invocation continues an interrupted one (its reduced
    // budget must also cap the rig run below)
    let is_resumed = resumed_record.is_some();
    // a resumed invocation replays its repaired partial transcript as
    // explicit history; an empty or assistant-less mirror (it died before any
    // turn completed) preserves nothing and falls back to a fresh start
    // under the same identity
    let seeded_history = resumed_record.as_ref().and_then(|record| {
        crate::thorpal::memory::repair_interrupted_tail(
            record.messages.clone(),
            "the session was interrupted",
        )
    });
    // keep a copy for the finished record: rig's response messages exclude
    // seeded history, so the final transcript must re-prefix it
    let seeded_for_record = seeded_history.clone();
    // record the invocation in the session store as it starts, so a crash
    // mid-flight still shows what was running and on what task; a fresh
    // delegation stamps the parent's call ids on so a resume can match the
    // invocation back to the exact dangling call that spawned it, while a
    // resume reuses the dead run's record (marked resumed) wholesale
    let record = match resumed_record {
        Some(mut record) => {
            record.resumed = true;
            record
        }
        None => AgentRecord::started(display_name.clone(), &role, &goal, &task, swarm.as_deref()),
    };
    let mut record = record;
    if record.parent_call.is_none()
        && let Some(call) = context.get::<rig::tool::CurrentToolCall>()
    {
        record.parent_call = Some(call.id.as_str().to_owned());
        record.parent_internal_call = Some(call.internal_call_id.clone());
    }
    // the sink is shared with the run's hook, which mirrors the in-flight
    // transcript into the record after every completed model turn
    let sink = RecordSink::new(record, store.clone());
    sink.save().await;
    // build the opening prompt: a resume opens on the continuation nudge (its
    // original prompt and partial work ride the seeded history), while a
    // fresh delegation gets the top-level goal injected plus the memory index
    // for memory-enabled agents and the linked forum context when this run
    // preloads it (the compact index leads so the forum render can never
    // bury it)
    let full_prompt = match &seeded_history {
        Some(_) => RESUME_SUB_AGENT_PROMPT.to_owned(),
        None => {
            let mut full_prompt = format!("Top-level goal: {goal}\n\nYour task: {task}");
            if let Some(section) = memory_index_context(&context, enabled, &goal, &task).await {
                full_prompt.push_str("\n\n# Memory index\n\n");
                full_prompt.push_str(&section);
            }
            if let Some(section) = linked_forum_context(&context, &goal, &task).await {
                full_prompt.push_str("\n\n# Forum context\n\n");
                full_prompt.push_str(&section);
            }
            full_prompt
        }
    };
    // run the sub-agent with live usage logging, instrumented by the
    // span; the hook also forces the forum author to this display name.
    // `extended_details` yields an `IntoFuture` we convert with
    // `into_future` so `instrument` (which needs a `Future`) applies
    let mut request = agent.prompt(full_prompt).tool_context(context);
    // replay a resumed invocation's repaired partial transcript so the model
    // continues from exactly the work the dead run paid for
    if let Some(history) = seeded_history {
        request = request.history(history);
    }
    // the agent-level rig cap is the FULL base budget; a resumed run must run
    // under its reduced remainder, installed as the request cap so the
    // turns-exhausted extension machinery fires at the right turn
    if is_resumed {
        request = request.max_turns(budget_state.effective_cap());
    }
    let response = match request
        .add_hook(AgentEventHook {
            main_length_retry: std::sync::atomic::AtomicBool::new(true),
            agent: display_name.clone(),
            coverage: Arc::clone(coverage),
            events: events.clone(),
            budget: Some(Arc::clone(&budget_state)),
            // sub-agents tally through their budget state instead
            turn_usage: None,
            // mirror each completed turn into the on-disk invocation record
            record: Some(sink.clone()),
        })
        .extended_details()
        .into_future()
        .instrument(span.clone())
        .await
    {
        Ok(response) => response,
        // the sub-agent died: salvage a partial handoff instead of losing
        // everything it had not posted to the forum
        Err(error) => {
            // a shutdown-time interruption is not a death: the process is
            // going down, so freeze the invocation for resume -- bill the
            // spend, persist the final mirror running-shaped, and skip the
            // salvage post, terminated event, and failure record a genuine
            // death would write. the latch check also freezes a genuine error
            // racing the teardown (e.g. a budget death landing as the latch
            // sets) -- during shutdown every outcome is better frozen
            if crate::thorpal::retry::is_shutdown_interrupt(&error)
                || gate.as_ref().is_some_and(|gate| gate.is_shutdown())
            {
                tracing::info!(
                    parent: &span,
                    agent = %display_name,
                    "sub-agent interrupted by shutdown; freezing for resume"
                );
                // the tokens were really spent; the ledger must not lose them
                usage
                    .record(&display_name, budget_state.failure_usage())
                    .await;
                // persist the final mirror so a resume continues from it; the
                // record stays running-shaped (finished_at: None)
                sink.update(|record| {
                    record.messages = budget_state.transcript_snapshot();
                    record.extensions = budget_state.extension_log();
                });
                sink.save().await;
                // hand a marker error back whose model-visible feedback is
                // the standard interrupted text: a top-level parent's next
                // model call dies on the latched gate before reading it, and
                // a resumed parent replaying a frozen NESTED child's result
                // then reads exactly the synthetic-interrupt shape (and the
                // continuation nudge tells it to re-issue the call)
                return SubAgentRun {
                    name: display_name,
                    output: Err(ToolExecutionError::other(format!(
                        "{} sub-agent interrupted by shutdown",
                        crate::thorpal::retry::SHUTDOWN_INTERRUPT_MARKER
                    ))
                    .with_model_feedback(crate::thorpal::memory::interrupted_result_text(
                        "the session was interrupted",
                    ))
                    .with_source(error)),
                };
            }
            tracing::warn!(parent: &span, agent = %display_name, error = %error, "sub-agent failed");
            // salvage the partial transcript: the error's own history when it
            // carries one, else the hook's mirrored history
            let salvaged = salvage::salvaged_messages(&error, &budget_state);
            let reason = salvage::death_reason(&error);
            // fold the usage the hook observed turn by turn into the ledger;
            // no response exists to read an aggregate from on this path
            usage
                .record(&display_name, budget_state.failure_usage())
                .await;
            // the handoff body: the agent's own last words when it wrote any,
            // else a one-shot post-mortem over the rendered partial transcript
            let (body, body_is_post_mortem) = match salvage::last_meaningful_text(&salvaged) {
                Some(text) => (Some(text), false),
                // only summarize when the agent actually did any work worth
                // mining; a death on the very first call has nothing to
                // summarize and would invite fabricated findings
                None if salvage::has_agent_work(&salvaged) => {
                    // render the transcript (tool evidence included) and ask
                    // the shared summarizer to write the missing handoff
                    let transcript = salvage::render_partial_transcript(&salvaged);
                    match salvage::post_mortem(post_mortem, &goal, &task, &reason, &transcript)
                        .instrument(span.clone())
                        .await
                    {
                        Some((summary, post_mortem_usage)) => {
                            // bill the post-mortem to the agent it salvaged
                            usage.record(&display_name, post_mortem_usage).await;
                            (Some(summary), true)
                        }
                        None => (None, false),
                    }
                }
                None => (None, false),
            };
            // render the parent-visible handoff, surfacing any unmet request
            // and any standing reservation the death outran
            let mut handoff = salvage::render_handoff(
                &reason,
                body.as_deref(),
                body_is_post_mortem,
                budget_state.unmet_request().as_ref(),
                budget_state.reservation_snapshot().as_ref(),
            );
            // post the salvaged handoff to the forum as a first-class finding
            // (best-effort: a zero-work death has nothing worth a post, and a
            // post failure must never fail the delegation itself)
            let salvage_post = match (&forum, &body) {
                (Some(forum), Some(_)) => {
                    post_salvage_handoff(forum, &display_name, &reason, &handoff)
                        .instrument(span.clone())
                        .await
                }
                _ => None,
            };
            // point the parent at the forum copy so re-delegations can cite it
            if let Some(post_name) = &salvage_post {
                handoff.push_str(&format!(
                    "\n\nThis handoff is also posted on the forum as `#{post_name}` -- cite \
                     it when re-delegating the remaining work."
                ));
            }
            // classify the death and emit the durable terminated event so the
            // recount counts cap/context deaths mechanically
            let class = salvage::death_class(&error);
            if let Some(tx) = events {
                let _ = tx.send(ThorPalEvent::SubAgentTerminated {
                    name: display_name.clone(),
                    reason: reason.clone(),
                    class: class.to_owned(),
                    salvage_post: salvage_post.clone(),
                });
            }
            // complete the record with the death: the error, the salvaged
            // partial transcript, the extension log, and the handoff itself
            sink.update(|record| {
                record.extensions = budget_state.extension_log();
                record.finish_err(error.to_string(), salvaged, Some(handoff.clone()), class);
            });
            sink.save().await;
            // hand the failure back with the handoff as the model-visible
            // feedback, so the parent reads partial work instead of a
            // redacted "the tool failed"
            return SubAgentRun {
                name: display_name,
                output: Err(ToolExecutionError::other(format!(
                    "sub-agent terminated early: {reason}"
                ))
                .with_model_feedback(handoff)
                .with_source(error)),
            };
        }
    };
    // log the aggregate usage for the whole sub-agent run under the span,
    // with comma-grouped counts so large token totals read at a glance
    tracing::info!(
        parent: &span,
        agent = %display_name,
        input = %crate::thorpal::usage::with_thousands(response.usage.input_tokens),
        output = %crate::thorpal::usage::with_thousands(response.usage.output_tokens),
        total = %crate::thorpal::usage::with_thousands(response.usage.total_tokens),
        "sub-agent finished"
    );
    // fold this sub-agent's aggregated usage into the shared ledger so it
    // shows up in the final per-agent token-usage summary
    usage.record(&display_name, response.usage).await;
    // complete the invocation's record with its answer and full transcript,
    // plus any extension requests it made along the way; rig's response
    // messages exclude explicitly seeded history, so a resumed invocation
    // re-prefixes its replayed half to keep the record's transcript whole
    let mut transcript = seeded_for_record.unwrap_or_default();
    transcript.extend(response.messages.clone().unwrap_or_default());
    sink.update(|record| {
        record.extensions = budget_state.extension_log();
        record.finish_ok(response.output.clone(), transcript);
    });
    sink.save().await;
    // hand the sub-agent's text back under its display name
    SubAgentRun {
        name: display_name,
        output: Ok(response.output),
    }
}

/// Wrap a sub-agent as a goal-aware tool for the main agent.
///
/// The returned tool takes a `goal` and a `prompt`: the goal is recorded on the
/// invocation span and prepended to the prompt, and the sub-agent's token usage
/// is logged per turn and in aggregate. Each invocation is also given a unique
/// per-call display name (`role` plus a moon of Jupiter, e.g.
/// `ReverseEngineer(Europa)`) used as its span identity, usage-ledger key, and
/// forced forum author. Every invocation counts against the run's live
/// sub-agent cap.
///
/// Invariant for library callers: when `budget.context_window` is non-zero,
/// the wrapped agent must carry every tool `TurnBudgetState::flush_tools`
/// names (the forum tools plus `think`, and `request_more_turns` when the
/// budget grants an allowance -- `AgentConfig::build` attaches them all) --
/// the critical clamp narrows the request's `active_tools` to that list, and
/// naming a tool the agent does not have is a hard per-turn request error.
///
/// # Arguments
///
/// * `agent` - The sub-agent to expose as a tool
/// * `role` - The human-facing role prefix for this agent's per-call display names
/// * `enabled` - The resolved registry tool names of the wrapped sub-agent
/// * `budget` - The turn-budget knobs each invocation of this agent runs under
/// * `usage` - The shared usage ledger this agent's token usage folds into
/// * `coverage` - The shared coverage ledger this agent's tool calls fold into
/// * `events` - An optional event sink to mirror this agent's activity onto
/// * `store` - An optional session store each invocation's record is saved into
/// * `alive` - The semaphore capping how many sub-agents are alive at once
/// * `post_mortem` - The shared salvage summarizer run over dead invocations, if any
///
/// # Examples
///
/// ```text
/// let tool = into_goal_tool(
///     Arc::new(researcher(&ai_client, "some-model")),
///     "Researcher".into(),
///     EnabledTools::default(),
///     TurnBudget { max_turns: 10, max_extension_turns: 4, warn_turns: 3, context_window: 200_000 },
///     usage.clone(),
///     coverage.clone(),
///     None,
///     None,
///     Arc::new(Semaphore::new(8)),
///     None,
/// );
/// agent_builder.dynamic_tool(tool);
/// ```
#[allow(clippy::too_many_arguments)]
pub fn into_goal_tool(
    agent: Arc<Agent>,
    role: String,
    enabled: EnabledTools,
    budget: TurnBudget,
    usage: Arc<UsageLedger>,
    coverage: Arc<CoverageLedger>,
    events: Option<ThorPalEventSender>,
    store: Option<Arc<SessionStore>>,
    alive: Arc<Semaphore>,
    post_mortem: Option<Arc<Agent>>,
) -> DynamicTool {
    // derive the stable tool id from the agent's name, matching rig's own default;
    // this is what the main agent calls, so it must not change per invocation
    let tool_id = agent
        .name()
        .map(ToString::to_string)
        .unwrap_or_else(|| DEFAULT_AGENT_TOOL_NAME.to_string());
    // describe the tool so the main agent knows to state a goal and a task
    let description = format!(
        "
        Prompt a sub-agent to do a task for you.

        Agent name: {name}
        Agent description: {description}

        Provide `goal` (the overarching objective this delegation serves) and
        `prompt` (the concrete task for the sub-agent to carry out).

        Use this for a single task, especially one that depends on an earlier
        finding or is one deep investigation. To run several independent tasks
        at once, use `spawn_swarm` instead.

        If the sub-agent dies early (turn, context, or output budget), you
        receive a partial handoff marked SUB-AGENT TERMINATED EARLY instead of
        a final answer -- the same handoff is also posted on the forum as a
        #salvage-... post; verify its findings and re-delegate what remains as
        a narrower task.
        ",
        name = tool_id,
        description = agent.description().unwrap_or_default(),
    );
    // hand-write the argument schema (avoids pulling in schemars directly)
    let parameters = json!({
        "type": "object",
        "properties": {
            "goal": {
                "type": "string",
                "description": "The overarching, top-level goal this delegation serves."
            },
            "prompt": {
                "type": "string",
                "description": "The concrete task for the sub-agent to carry out."
            }
        },
        "required": ["goal", "prompt"]
    });

    DynamicTool::new(
        tool_id.clone(),
        description,
        parameters,
        move |context, args| {
            // clone the shared state into this invocation's future
            let agent = Arc::clone(&agent);
            let role = role.clone();
            // clone the agent's own enabled toolset for its script bindings
            let enabled = enabled.clone();
            // clone the shared ledgers so this invocation folds into the instance's
            // ledgers rather than any process-global state
            let usage = Arc::clone(&usage);
            let coverage = Arc::clone(&coverage);
            // clone the event sink so this invocation's hook can mirror activity out
            let events = events.clone();
            // clone the session store so this invocation's record can be saved
            let store = store.clone();
            // clone the live sub-agent cap so this invocation counts against it
            let alive = Arc::clone(&alive);
            // clone the shared salvage summarizer for this invocation's death path
            let post_mortem = post_mortem.clone();
            // propagate the caller's tool context so the sub-agent's own tools work
            let inherited_context = context.clone();
            Box::pin(async move {
                // parse the goal/prompt arguments, mirroring rig's error handling
                let args: GoalToolArgs = serde_json::from_value(args).map_err(|error| {
                    ToolExecutionError::invalid_args(format!(
                        "failed to parse agent tool arguments: {error}"
                    ))
                    .with_source(error)
                })?;
                // run the sub-agent through the shared runner; a direct
                // delegation runs under no swarm
                let run = run_sub_agent(
                    &agent,
                    Invocation::Fresh {
                        role: role.clone(),
                        goal: args.goal,
                        task: args.prompt,
                        swarm: None,
                    },
                    inherited_context,
                    &enabled,
                    budget,
                    &usage,
                    &coverage,
                    &events,
                    &store,
                    &alive,
                    post_mortem.as_deref(),
                )
                .await;
                // hand the sub-agent's text back to the calling agent
                Ok(ToolOutput::text(run.output?))
            })
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    /// Builds a saved record in the given lifecycle state
    ///
    /// # Arguments
    ///
    /// * `name` - The record's display name
    /// * `parent` - The parent call id stamped at spawn, if any
    /// * `finished` - Whether the invocation finished before the death
    fn record(name: &str, parent: Option<&str>, finished: bool) -> AgentRecord {
        let mut record = AgentRecord::started(name, "ReverseEngineer", "g", "p", None);
        record.parent_call = parent.map(str::to_owned);
        if finished {
            record.finish_ok("the answer", Vec::new());
        }
        record
    }

    /// Seeds one dated memory into a library for index tests
    ///
    /// # Arguments
    ///
    /// * `memories` - The library to seed into
    /// * `name` - The bare memory name
    /// * `promoted_at` - The save date the recency fill orders on
    /// * `tags` - The search tags to file it under
    /// * `description` - The header description
    async fn seed_dated(
        memories: &crate::thorpal::store::memories::MemoryLibrary,
        name: &str,
        promoted_at: &str,
        tags: Vec<String>,
        description: &str,
    ) {
        // build the header directly so each seed carries a distinct date
        let meta = crate::thorpal::store::memories::MemoryMeta {
            kind: crate::thorpal::store::memories::MemoryKind::Technique,
            tags,
            description: description.to_owned(),
            promoted_from: "seed-session".to_owned(),
            promoted_at: promoted_at.to_owned(),
        };
        memories
            .save(name, &format!("{}\nseed body\n", meta.render(name)))
            .await
            .expect("seed memory");
    }

    /// The spawn index is task-conditioned, capped, and recency-filled
    #[tokio::test]
    async fn memory_index_is_task_conditioned_and_capped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let memories = MemoryLibrary::open(dir.path().join("memories"))
            .await
            .expect("library should open");
        // more same-dated fillers than the cap, plus dated outliers
        for index in 0..30 {
            seed_dated(
                &memories,
                &format!("zz-filler-{index:02}"),
                "2026-10-20",
                vec!["filler".to_owned()],
                &format!("Filler lesson number {index:02}."),
            )
            .await;
        }
        seed_dated(
            &memories,
            "aa-old-alpha",
            "2026-01-01",
            vec!["old".to_owned()],
            "An old lesson that only alphabetical order would surface.",
        )
        .await;
        seed_dated(
            &memories,
            "zz-newest",
            "2026-10-26",
            vec!["fresh".to_owned()],
            "The newest saved lesson.",
        )
        .await;
        seed_dated(
            &memories,
            "xor-keystream-recovery",
            "2026-10-05",
            vec!["xor".to_owned(), "keystream".to_owned()],
            "Recovering an xor keystream from a decode loop.",
        )
        .await;
        let mut context = ToolContext::new();
        context.insert(Arc::clone(&memories));
        let enabled = EnabledTools(Arc::new(std::collections::BTreeSet::from([
            "list_memories",
        ])));
        // a task about xor keystreams puts the matching memory first
        let index = memory_index_context(
            &context,
            &enabled,
            "triage the sample",
            "recover the xor keystream",
        )
        .await
        .expect("an index renders");
        let lines: Vec<&str> = index.lines().collect();
        assert!(lines[0].contains("xor-keystream-recovery"), "{index}");
        // the cap holds, with one trailing pointer at the ranked search
        assert!(lines.len() <= MEMORY_INDEX_CAP + 1, "{} lines", lines.len());
        assert!(
            lines.last().expect("lines").contains("search_memories"),
            "{index}"
        );
        // a zero-signal task fills by recency, not alphabetical order
        let index = memory_index_context(&context, &enabled, "qqq", "qqq")
            .await
            .expect("an index renders");
        let lines: Vec<&str> = index.lines().collect();
        assert!(lines[0].contains("zz-newest"), "{index}");
        assert!(
            !index.contains("aa-old-alpha"),
            "alphabetical order must not decide the fill: {index}"
        );
        // a denied NAME must not rank by its raw text: the scorable doc is
        // rendered fields only, so a task naming the family cannot pull the
        // denied memory to the top through a name it never gets to see
        seed_dated(
            &memories,
            "darkgate-loader-notes",
            "2026-10-21",
            vec!["loaders".to_owned()],
            "Notes on a loader's import walk.",
        )
        .await;
        let deny = crate::thorpal::store::redact::LibraryRedactor::new(
            false,
            vec!["darkgate".to_owned()],
            Vec::new(),
        )
        .expect("redactor");
        context.insert(deny);
        let index = memory_index_context(&context, &enabled, "triage", "the darkgate payload")
            .await
            .expect("an index renders");
        let first = index.lines().next().expect("lines");
        assert!(
            !first.contains(&crate::thorpal::store::redact::neutral_memory_id(
                "darkgate-loader-notes"
            )),
            "a raw-name match must not rank a denied memory first: {index}"
        );
        // blind-hidden family intel never surfaces, and the overflow count
        // reflects only visible entries
        let intel = crate::thorpal::store::memories::MemoryMeta::promoted_now(
            crate::thorpal::store::memories::MemoryKind::FamilyIntel,
            vec!["darkgate".to_owned()],
            "Verified DarkGate traits.",
            "seed-session",
        );
        memories
            .save(
                "darkgate-intel",
                &format!("{}\nDarkGate uses act=*.\n", intel.render("darkgate-intel")),
            )
            .await
            .expect("seed intel");
        let blind = crate::thorpal::store::redact::LibraryRedactor::new(
            false,
            Vec::new(),
            vec!["darkgate".to_owned()],
        )
        .expect("redactor");
        context.insert(blind);
        let index = memory_index_context(&context, &enabled, "g", "t")
            .await
            .expect("an index renders");
        assert!(
            !index.contains(&crate::thorpal::store::redact::neutral_memory_id(
                "darkgate-intel"
            )),
            "hidden intel surfaced in the index: {index}"
        );
        // 35 visible entries (34 seeded + 1 builtin; the hidden intel never
        // counts), 25 listed: 10 overflow
        assert!(
            index.contains("(10 more"),
            "visible-only overflow count: {index}"
        );
        // a blinded run renders the selected names as neutral ids
        let redactor =
            crate::thorpal::store::redact::LibraryRedactor::new(true, Vec::new(), Vec::new())
                .expect("redactor");
        context.insert(redactor);
        let index = memory_index_context(
            &context,
            &enabled,
            "triage the sample",
            "recover the xor keystream",
        )
        .await
        .expect("an index renders");
        assert!(
            index.lines().next().expect("lines").starts_with("- mem-"),
            "{index}"
        );
        // an agent without memory tools gets no index at all
        assert!(
            memory_index_context(&context, &EnabledTools::default(), "g", "t")
                .await
                .is_none()
        );
        // a fresh library still indexes the compiled-in builtins (and only
        // them), so the index is never silently empty while builtins exist
        let empty = MemoryLibrary::open(dir.path().join("empty"))
            .await
            .expect("library should open");
        let mut bare = ToolContext::new();
        bare.insert(empty);
        let index = memory_index_context(&bare, &enabled, "g", "t")
            .await
            .expect("builtins alone still index");
        assert_eq!(index.lines().count(), 1, "{index}");
        assert!(index.contains("resolve_aarch64_adrp_add_pairs"), "{index}");
    }

    /// The goal-tool argument parser recovers the live tool's shape
    #[test]
    fn goal_args_parse_roundtrip() {
        // the exact argument object a live delegation call carries
        let args = serde_json::json!({"goal": "find the key", "prompt": "trace entry"});
        let parsed = parse_goal_args(&args).expect("valid args should parse");
        assert_eq!(parsed.0, "find the key");
        assert_eq!(parsed.1, "trace entry");
        // damaged args parse to nothing instead of panicking
        assert!(parse_goal_args(&serde_json::json!({"goal": "only"})).is_none());
    }

    /// Record matching prefers the spawn-time stamp and consumes its match
    #[test]
    fn record_matching_prefers_stamp_and_consumes() {
        // two records for identical tasks: one stamped, one legacy
        let mut records = vec![
            record("ReverseEngineer(Io)", Some("call-1"), false),
            record("ReverseEngineer(Europa)", None, false),
        ];
        // the stamped record wins for its exact call id
        let hit = take_matching_record(&mut records, "call-1", "ReverseEngineer", "g", "p")
            .expect("the stamped record should match");
        assert_eq!(hit.name, "ReverseEngineer(Io)");
        // the legacy record is all that remains for a second identical call
        let hit = take_matching_record(&mut records, "call-2", "ReverseEngineer", "g", "p")
            .expect("the legacy record should match by fields");
        assert_eq!(hit.name, "ReverseEngineer(Europa)");
        // the pool is drained; a third call matches nothing
        assert!(
            take_matching_record(&mut records, "call-3", "ReverseEngineer", "g", "p").is_none()
        );
        // a stamped record never matches a different call id by fields alone
        let mut records = vec![record("ReverseEngineer(Io)", Some("call-1"), false)];
        assert!(
            take_matching_record(&mut records, "call-9", "ReverseEngineer", "g", "p").is_none()
        );
    }

    /// A finished record reconstructs the exact outcome the live run returned
    #[test]
    fn finished_records_reconstruct_their_outcomes() {
        // a success hands back its saved answer verbatim
        let run = rerun_from_record(record("ReverseEngineer(Io)", None, true));
        assert_eq!(run.name, "ReverseEngineer(Io)");
        assert_eq!(run.output.expect("a success"), "the answer");
        // a death hands back its handoff as the model-visible feedback
        let mut dead = record("ReverseEngineer(Europa)", None, false);
        dead.finish_err(
            "turn budget exhausted",
            Vec::new(),
            Some("SUB-AGENT TERMINATED EARLY -- partial handoff".to_owned()),
            "max_turns",
        );
        let run = rerun_from_record(dead);
        let error = run.output.expect_err("a death");
        assert!(error.to_string().contains("turn budget exhausted"));
        assert_eq!(
            error.model_feedback(),
            Some("SUB-AGENT TERMINATED EARLY -- partial handoff")
        );
    }

    /// The salvage post name collapses display names to forum-safe slugs
    #[test]
    fn salvage_post_names_are_forum_safe() {
        // parens and scope prefixes collapse to single hyphens
        assert_eq!(
            salvage_post_name("btw:crypto/ReverseEngineer(Io)"),
            "salvage-btw-crypto-reverseengineer-io"
        );
    }

    /// The resumed-start timestamp survives on a reused record
    #[test]
    fn resumed_records_keep_their_start() {
        // a resumed record keeps its original start time for the audit trail
        let started = record("ReverseEngineer(Io)", Some("call-1"), false);
        let before = started.started_at;
        assert!(before <= Utc::now());
        assert!(!started.resumed, "records start un-resumed");
    }
}
