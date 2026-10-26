//! Sub-agent turn budgets: wrap-up warnings and self-service extensions.
//!
//! Every sub-agent invocation runs under rig's total model-call cap. Before
//! this module existed, hitting that cap killed the agent with no warning and
//! no final word, losing everything it had not posted to the forum. The
//! [`TurnBudgetState`] built here is shared three ways to fix that: the
//! per-run [`super::tool::AgentEventHook`] reads it to inject a countdown
//! warning into the request once few calls remain (and to answer rig's
//! turns-exhausted event with any granted extension), the `request_more_turns`
//! tool mutates it to grant bounded extensions with a recorded justification,
//! and the death-salvage path drains it for the agent record and the partial
//! handoff returned to the parent.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use chrono::{DateTime, Utc};
use rig::completion::{Document, Usage};
use rig::message::{AssistantContent, Message};
use rig::tool::{ToolContext, ToolExecutionError};
use serde::{Deserialize, Serialize};

use crate::thorpal::events::{ExtensionTrigger, ThorPalEvent, ThorPalEventSender};

/// The id the per-turn warning document is injected under.
const WARNING_DOCUMENT_ID: &str = "turn_budget_warning";

/// The id the context warning document uses when no turn warning is active.
const CONTEXT_DOCUMENT_ID: &str = "context_budget_warning";

/// The context-window percentage that arms the context wrap-up warning.
const CONTEXT_WARN_PERCENT: u64 = 80;

/// The context-window percentage that clamps tools to forum writes.
const CONTEXT_CRITICAL_PERCENT: u64 = 90;

/// The chars-per-token divisor the live history estimate uses.
///
/// Four chars per token is the usual English-text rule of thumb; the estimate
/// serializes messages to JSON first, whose structural overhead biases the
/// estimate high -- warnings fire early, never late.
const CHARS_PER_TOKEN: u64 = 4;

/// The justification recorded for the one-shot automatic cap extension.
const AUTO_EXTENSION_JUSTIFICATION: &str = "automatic extension at the turn cap (no request made)";

/// The tools left available when context pressure goes critical.
///
/// At the critical threshold the hook narrows the request's `active_tools` to
/// this list (plus `request_more_turns` when the agent carries it -- see
/// [`TurnBudgetState::flush_tools`]): the forum writes that persist findings,
/// the cheap forum reads needed to post coherently, and `think`. Every name
/// here is attached unconditionally by `AgentConfig::build` (workflow.rs) --
/// naming a missing tool in an `active_tools` allow-list is a hard request
/// error, so this list and that build step must stay in step.
pub(crate) const CONTEXT_FLUSH_TOOLS: &[&str] = &[
    "create_post",
    "create_comment",
    "set_status",
    "add_tasks",
    "complete_task",
    "list_posts",
    "list_questions",
    "search_forum",
    "follow_link",
    "think",
];

/// The feedback injected when a turn is retried after output truncation.
pub(crate) const TRUNCATION_FEEDBACK: &str = "Your last response ran out of output tokens before completing. Reply again with the \
     complete content, more concisely: lead with the most important findings and drop \
     anything nonessential.";

/// The turn-budget knobs one sub-agent invocation runs under
#[derive(Debug, Clone, Copy)]
pub struct TurnBudget {
    /// The base model-call cap (the workflow's `max_turns`)
    pub max_turns: usize,
    /// The total self-service extension allowance, in model calls
    pub max_extension_turns: usize,
    /// How many remaining calls arm the wrap-up warning
    pub warn_turns: usize,
    /// The model's context window in tokens; `0` disarms context warnings
    pub context_window: u64,
}

/// How full a sub-agent's context window is, in warning bands
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContextPressure {
    /// Below every threshold; no context warning applies
    None,
    /// At or past the warn threshold: order a forum flush now
    Warn,
    /// At or past the critical threshold: clamp tools to forum writes
    Critical,
}

/// A standing extension reservation made outside the warning window
///
/// Evaluation evidence showed agents ask for extensions early or never, and the
/// too-early refusal made the whole mechanism unreachable (exactly one call
/// all run, refused). An early request is now recorded here instead and
/// converts into a real grant automatically when the cap is actually hit.
#[derive(Debug, Clone)]
pub(crate) struct ReservedExtension {
    /// How many extra turns the agent asked to reserve
    pub requested: usize,
    /// The justification the agent gave
    pub justification: String,
    /// When the reservation was made
    pub at: DateTime<Utc>,
}

/// One extension request a sub-agent made during an invocation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionRequest {
    /// How many extra turns the agent asked for
    pub requested: usize,
    /// How many were actually granted (`0` for a refused, unmet request)
    pub granted: usize,
    /// The justification the agent gave
    pub justification: String,
    /// When the request was made
    pub at: DateTime<Utc>,
}

/// The live turn-budget state of one sub-agent invocation
///
/// Created per invocation in `run_sub_agent`, inserted into the run's
/// [`ToolContext`] for the `request_more_turns` tool, and handed to the run's
/// [`super::tool::AgentEventHook`] for warnings, extension application, and
/// the transcript mirror the salvage path falls back on when an error carries
/// no history of its own.
pub struct TurnBudgetState {
    /// The invocation's display name, for events and logs
    name: String,
    /// The static budget knobs this invocation runs under
    budget: TurnBudget,
    /// The last model-call index the hook observed (1-based)
    current_turn: AtomicUsize,
    /// The total extra turns granted by `request_more_turns` so far
    granted: AtomicUsize,
    /// The extra turns already applied to the running rig budget
    applied: AtomicUsize,
    /// The one-shot latch for the truncation feedback retry
    length_retry_used: AtomicBool,
    /// The one-shot latch for the automatic extension at the cap
    auto_extended: AtomicBool,
    /// A standing reservation made outside the window, converted at the cap
    reservation: Mutex<Option<ReservedExtension>>,
    /// Every extension request made, granted and refused alike
    requests: Mutex<Vec<ExtensionRequest>>,
    /// The per-turn usage folded in by the hook, for failure-path accounting
    usage: Mutex<Usage>,
    /// The last completed turn's context size in tokens (input + cache reads
    /// + cache writes -- the provider-reported identity), lagging one turn
    last_context_tokens: AtomicU64,
    /// A live chars/4 estimate over the pending request's history, which sees
    /// the giant tool result the lagging token count cannot
    estimated_context_tokens: AtomicU64,
    /// The highest context-pressure level already reported (event latch)
    context_reported: AtomicUsize,
    /// A mirror of the canonical history, for deaths whose error carries none
    transcript: Mutex<Vec<Message>>,
    /// An optional sink extension grants and context pressure are mirrored onto
    events: Option<ThorPalEventSender>,
}

impl TurnBudgetState {
    /// Creates the turn-budget state for one sub-agent invocation
    ///
    /// # Arguments
    ///
    /// * `name` - The invocation's unique display name
    /// * `budget` - The budget knobs this invocation runs under
    /// * `events` - An optional sink extension grants are mirrored onto
    pub fn new(
        name: impl Into<String>,
        budget: TurnBudget,
        events: Option<ThorPalEventSender>,
    ) -> Self {
        TurnBudgetState {
            name: name.into(),
            budget,
            current_turn: AtomicUsize::new(0),
            granted: AtomicUsize::new(0),
            applied: AtomicUsize::new(0),
            length_retry_used: AtomicBool::new(false),
            auto_extended: AtomicBool::new(false),
            reservation: Mutex::new(None),
            requests: Mutex::new(Vec::new()),
            usage: Mutex::new(Usage::new()),
            last_context_tokens: AtomicU64::new(0),
            estimated_context_tokens: AtomicU64::new(0),
            context_reported: AtomicUsize::new(0),
            transcript: Mutex::new(Vec::new()),
            events,
        }
    }

    /// Creates the budget state for a resumed invocation, deducting prior spend
    ///
    /// The resumed run re-enters rig with its turn counter back at 1, so the
    /// prior run's spend is folded into the knobs instead: the base cap grows
    /// by what was already granted and shrinks by the calls already made
    /// (floored at 1 so the forced-final-turn handoff clamp still fires), and
    /// the extension allowance shrinks by the grants already made. The prior
    /// requests seed the log so the finished record carries the invocation's
    /// whole extension history.
    ///
    /// # Arguments
    ///
    /// * `name` - The invocation's unique display name
    /// * `budget` - The budget knobs the original invocation ran under
    /// * `turns_used` - The model calls the invocation made before the interruption
    /// * `prior_requests` - The extension requests made before the interruption
    /// * `events` - An optional sink extension grants are mirrored onto
    pub fn resumed(
        name: impl Into<String>,
        budget: TurnBudget,
        turns_used: usize,
        prior_requests: Vec<ExtensionRequest>,
        events: Option<ThorPalEventSender>,
    ) -> Self {
        // the grants already made raise the cap exactly as they did live
        let prior_granted: usize = prior_requests.iter().map(|request| request.granted).sum();
        // fold the prior spend into the knobs the fresh counter runs under
        let budget = TurnBudget {
            max_turns: budget
                .max_turns
                .saturating_add(prior_granted)
                .saturating_sub(turns_used)
                .max(1),
            max_extension_turns: budget.max_extension_turns.saturating_sub(prior_granted),
            warn_turns: budget.warn_turns,
            context_window: budget.context_window,
        };
        // a prior automatic extension already consumed the one-shot latch;
        // re-arming it would grant one more free horizon per crash cycle
        let auto_used = prior_requests
            .iter()
            .any(|request| request.justification == AUTO_EXTENSION_JUSTIFICATION);
        // start from a fresh state and seed the request log with the history
        let state = Self::new(name, budget, events);
        state.auto_extended.store(auto_used, Ordering::SeqCst);
        *state
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = prior_requests;
        state
    }

    /// Pulls this run's shared turn-budget state out of a tool context
    ///
    /// Every sub-agent runner inserts the state before prompting, so a missing
    /// value is a wiring bug (or a non-sub-agent context), and it surfaces as
    /// an invalid-args tool error the model can read.
    ///
    /// # Arguments
    ///
    /// * `context` - The tool call context the run was prompted with
    pub(crate) fn from_context(
        context: &ToolContext,
    ) -> Result<Arc<TurnBudgetState>, ToolExecutionError> {
        // recover the shared state, mapping a missing slot to a tool error
        context
            .require::<Arc<TurnBudgetState>>()
            .map(Arc::clone)
            .map_err(|err| ToolExecutionError::invalid_args(err.to_string()))
    }

    /// Notes a model call being prepared, refreshing the transcript mirror
    ///
    /// The mirror is replaced (not appended to) with the canonical history
    /// plus the pending prompt, so it self-heals across retries and recovered
    /// turns; the per-call clone is the same cost the request builder already
    /// pays to assemble the request.
    ///
    /// # Arguments
    ///
    /// * `turn` - The 1-based model-call index being prepared
    /// * `prompt` - The prompt message this call carries
    /// * `history` - The canonical history preceding the prompt
    pub(crate) fn note_turn(&self, turn: usize, prompt: &Message, history: &[Message]) {
        // remember where the run is so the gate and warnings count from it
        self.current_turn.store(turn, Ordering::SeqCst);
        // rebuild the mirror from the canonical history plus the pending prompt
        let mut mirror = Vec::with_capacity(history.len() + 1);
        mirror.extend_from_slice(history);
        mirror.push(prompt.clone());
        // estimate the pending request's context size off the live history --
        // this is the only signal that sees a giant tool result BEFORE the
        // call that would die on it (the token count lags one turn)
        if self.budget.context_window > 0 {
            let chars: u64 = mirror.iter().map(estimate_message_chars).sum();
            self.estimated_context_tokens
                .store(chars / CHARS_PER_TOKEN, Ordering::SeqCst);
        }
        *self
            .transcript
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = mirror;
    }

    /// Appends an accepted assistant turn to the transcript mirror
    ///
    /// # Arguments
    ///
    /// * `content` - The assistant content the model produced this turn
    pub(crate) fn note_assistant(&self, content: &[AssistantContent]) {
        // skip empty turns; rig itself never records an empty assistant message
        if !content.is_empty() {
            self.transcript
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(Message::Assistant {
                    id: None,
                    content: content.to_vec(),
                });
        }
    }

    /// Folds one model turn's token usage into the failure-path tally
    ///
    /// # Arguments
    ///
    /// * `usage` - The usage one completed model call reported
    pub(crate) fn add_usage(&self, usage: Usage) {
        // the completed turn's context size is the provider's own count:
        // on Anthropic/Bedrock `input` excludes cache reads, so the identity
        // is input + cached + cache_creation (the OpenAI protocol folds
        // `cached` into `input`, overcounting here -- warnings fire early,
        // never late)
        let context = usage
            .input_tokens
            .saturating_add(usage.cached_input_tokens)
            .saturating_add(usage.cache_creation_input_tokens);
        self.last_context_tokens.store(context, Ordering::SeqCst);
        // accumulate under the lock; `Usage` sums component-wise
        *self
            .usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) += usage;
    }

    /// Takes a copy of the usage observed so far, for a failed run's ledger entry
    pub(crate) fn failure_usage(&self) -> Usage {
        // copy the tally out so the caller can record it after the run died
        *self
            .usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The effective model-call cap: the base budget plus every granted extension
    pub(crate) fn effective_cap(&self) -> usize {
        // the cap only ever grows, so a plain load is enough
        self.budget
            .max_turns
            .saturating_add(self.granted.load(Ordering::SeqCst))
    }

    /// How many model calls remain at a given call index, counting that call
    ///
    /// At 1-based call `turn` the run has spent `turn - 1` earlier calls, so
    /// with a cap of 10 the answer is 3 at turn 8, 2 at turn 9, and 1 at turn
    /// 10 (the final call).
    ///
    /// # Arguments
    ///
    /// * `turn` - The 1-based model-call index to count from
    pub(crate) fn calls_left(&self, turn: usize) -> usize {
        // subtract the calls already spent before this one, saturating so a
        // stale or zero turn index can never underflow
        self.effective_cap().saturating_sub(turn.saturating_sub(1))
    }

    /// The extension turns still available under this invocation's allowance
    fn extension_left(&self) -> usize {
        self.budget
            .max_extension_turns
            .saturating_sub(self.granted.load(Ordering::SeqCst))
    }

    /// The granted turns not yet applied to the running rig budget
    pub(crate) fn pending_extension(&self) -> usize {
        // the non-consuming sibling of `drain_pending_extension`
        self.granted
            .load(Ordering::SeqCst)
            .saturating_sub(self.applied.load(Ordering::SeqCst))
    }

    /// Takes a copy of the standing reservation, if the agent made one early
    pub(crate) fn reservation_snapshot(&self) -> Option<ReservedExtension> {
        // clone the reservation out so the salvage path owns its copy
        self.reservation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The context tokens currently in use, by the stronger of the two signals
    fn context_used(&self) -> u64 {
        // the token count is exact but lags a turn; the char estimate is
        // rough but live -- the max of both is the safe reading
        self.last_context_tokens
            .load(Ordering::SeqCst)
            .max(self.estimated_context_tokens.load(Ordering::SeqCst))
    }

    /// How full this invocation's context window is, in warning bands
    pub(crate) fn context_pressure(&self) -> ContextPressure {
        // a zero window disarms context tracking entirely
        let window = self.budget.context_window;
        if window == 0 {
            return ContextPressure::None;
        }
        // integer-only comparison: used/window >= percent/100
        let used = self.context_used();
        if used.saturating_mul(100) >= window.saturating_mul(CONTEXT_CRITICAL_PERCENT) {
            ContextPressure::Critical
        } else if used.saturating_mul(100) >= window.saturating_mul(CONTEXT_WARN_PERCENT) {
            ContextPressure::Warn
        } else {
            ContextPressure::None
        }
    }

    /// Takes the once-per-level context-pressure report, if a new level latched
    ///
    /// The warning *document* re-injects every call at or above a threshold
    /// (the number must stay current), but the event and transcript marker
    /// fire once per level per invocation -- this high-water latch is what
    /// makes an audit recount count sessions, not turns.
    pub(crate) fn take_context_report(&self) -> Option<(u8, u64, u64)> {
        // map the live pressure onto its report level
        let level = match self.context_pressure() {
            ContextPressure::None => return None,
            ContextPressure::Warn => 1usize,
            ContextPressure::Critical => 2usize,
        };
        // only a level above the recorded high-water mark reports
        let seen = self.context_reported.fetch_max(level, Ordering::SeqCst);
        (level > seen).then(|| (level as u8, self.context_used(), self.budget.context_window))
    }

    /// Whether a cap-hit right now would extend the run instead of ending it
    ///
    /// True when an unapplied grant is pending, a standing reservation can
    /// still convert, or the one-shot automatic extension is unspent with
    /// allowance remaining -- the exact predicate `cap_hit_extension` acts on.
    fn cap_hit_would_extend(&self) -> bool {
        // an in-window grant waiting to apply always extends
        if self.pending_extension() > 0 {
            return true;
        }
        // with the allowance spent, nothing can convert or auto-grant
        let allowance_left = self.extension_left();
        if allowance_left == 0 {
            return false;
        }
        // a standing reservation converts (clamped, but non-zero here)
        if self
            .reservation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
        {
            return true;
        }
        // the automatic backstop fires once, for min(warn_turns, allowance)
        !self.auto_extended.load(Ordering::SeqCst) && self.budget.warn_turns > 0
    }

    /// Whether the given call is the run's true final turn, with no way out
    ///
    /// On a true final turn the hook disables tool calls entirely so the
    /// reply IS the final handoff (agents at the cap routinely ignored the
    /// advisory FINAL TURN text and died tool-calling with no final answer).
    ///
    /// # Arguments
    ///
    /// * `turn` - The 1-based model-call index being prepared
    pub(crate) fn forced_final_turn(&self, turn: usize) -> bool {
        // final means one call left AND no mechanism left to extend past it
        self.calls_left(turn) == 1 && !self.cap_hit_would_extend()
    }

    /// The tool allow-list the critical context clamp narrows requests to
    ///
    /// The forum/communication set, plus `request_more_turns` when this agent
    /// actually carries it (a zero allowance means the tool was never
    /// attached, and naming a missing tool in `active_tools` is a hard
    /// request error). The extension tool stays ALLOWED even though the
    /// warnings stop advertising it under pressure: its result is tiny (it
    /// cannot inflate context), and a model calling it from an earlier
    /// warning's instinct must not void its whole flush batch through rig's
    /// skip-suppresses-the-turn invalid-call semantics.
    pub(crate) fn flush_tools(&self) -> Vec<String> {
        // start from the unconditionally-attached communication set
        let mut tools: Vec<String> = CONTEXT_FLUSH_TOOLS
            .iter()
            .map(|name| (*name).to_string())
            .collect();
        // keep the extension tool reachable when the agent carries it
        if self.budget.max_extension_turns > 0 {
            tools.push("request_more_turns".to_owned());
        }
        tools
    }

    /// Resolves a cap-hit: pending grant, reservation conversion, or auto
    ///
    /// Called by the hook's turns-exhausted answer. In priority order: (1) an
    /// in-window grant not yet applied, (2) the standing reservation, clamped
    /// to the remaining allowance at conversion time (so later grants debit
    /// first and the total can never exceed `max_turns + max_extension_turns`),
    /// (3) the one-shot automatic extension of `min(warn_turns,
    /// allowance_left)` -- exactly one wrap-up horizon, so the re-armed
    /// warnings fire through it. Returns the turns to extend by, `0` to let
    /// the run end.
    pub(crate) fn cap_hit_extension(&self) -> usize {
        // (1) an in-window grant waiting to be applied wins outright
        let pending = self.drain_pending_extension();
        if pending > 0 {
            return pending;
        }
        // (2) a standing reservation converts now, clamped to the allowance
        let reserved = self
            .reservation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(reserved) = reserved {
            let granted_now = reserved.requested.min(self.extension_left());
            if granted_now > 0 {
                self.granted.fetch_add(granted_now, Ordering::SeqCst);
                self.push_request(
                    reserved.requested,
                    granted_now,
                    reserved.justification.clone(),
                );
                self.notify_extension(
                    granted_now,
                    reserved.justification,
                    ExtensionTrigger::ReservationApplied,
                );
                return self.drain_pending_extension();
            }
            // a zero-convertible reservation is recorded as the unmet request
            self.push_request(reserved.requested, 0, reserved.justification);
        }
        // (3) the automatic backstop fires at most once per invocation
        if !self.auto_extended.swap(true, Ordering::SeqCst) {
            let granted_now = self.budget.warn_turns.min(self.extension_left());
            if granted_now > 0 {
                self.granted.fetch_add(granted_now, Ordering::SeqCst);
                self.push_request(
                    granted_now,
                    granted_now,
                    AUTO_EXTENSION_JUSTIFICATION.to_string(),
                );
                self.notify_extension(
                    granted_now,
                    AUTO_EXTENSION_JUSTIFICATION.to_string(),
                    ExtensionTrigger::Auto,
                );
                return self.drain_pending_extension();
            }
        }
        // nothing left: the cap stands and the run ends
        0
    }

    /// Logs and mirrors one extension grant (or reservation) onto the sink
    ///
    /// # Arguments
    ///
    /// * `granted` - The turns granted (or reserved, for the `Reserved` trigger)
    /// * `justification` - The justification carried on the record
    /// * `trigger` - What caused this extension
    fn notify_extension(&self, granted: usize, justification: String, trigger: ExtensionTrigger) {
        // the remaining allowance after this change, for the event
        let remaining_allowance = self.extension_left();
        // log under the sub-agent span driving this change
        tracing::info!(
            agent = %self.name,
            granted,
            remaining_allowance,
            %trigger,
            justification = %justification,
            "sub-agent turn budget extended"
        );
        // mirror onto the event sink if a consumer is listening
        if let Some(tx) = &self.events {
            let _ = tx.send(ThorPalEvent::SubAgentExtended {
                name: self.name.clone(),
                granted,
                remaining_allowance,
                justification,
                trigger,
            });
        }
    }

    /// Builds the warning document for a model call, when one is warranted
    ///
    /// `None` outside both the turn window and the context thresholds. The
    /// turn warning carries the exact remaining-call count and wrap-up
    /// directions; the context warning orders an immediate forum flush. When
    /// both are active they merge into ONE document (turn text first) whose
    /// halves are rendered against the SAME per-request state -- the forced
    /// final turn and the critical tool clamp both reshape the turn half, so
    /// the merged text can never order a tool the same patch just removed.
    /// Injected per request and never into persisted history, so the numbers
    /// are always current.
    ///
    /// # Arguments
    ///
    /// * `turn` - The 1-based model-call index being prepared
    pub(crate) fn warning_document(&self, turn: usize) -> Option<Document> {
        // count how many calls remain, including the one being prepared
        let calls_left = self.calls_left(turn);
        // resolve the request's structural state once so both halves agree
        let pressure = self.context_pressure();
        let forced = self.forced_final_turn(turn);
        // the clamp narrows the advertised tools on this very request, so the
        // turn half must not order the extension tool it cannot reach
        let clamped = pressure == ContextPressure::Critical && !forced;
        // render the turn warning only inside its window
        let turn_text = (calls_left <= self.budget.warn_turns).then(|| {
            warning_text(
                calls_left,
                self.effective_cap(),
                self.extension_left(),
                self.reservation_snapshot()
                    .map(|reserved| reserved.requested),
                forced,
                clamped,
            )
        });
        // render the context warning at either threshold, every call -- the
        // percentage must stay current (the event latch is separate); on the
        // forced final turn the flush orders would name disabled tools, so a
        // put-it-in-this-reply variant renders instead
        let context_text = match pressure {
            ContextPressure::None => None,
            pressure => Some(context_warning_text(
                pressure,
                self.context_used(),
                self.budget.context_window,
                forced,
            )),
        };
        // merge into one document so the two warnings never compete
        let (id, text) = match (turn_text, context_text) {
            (None, None) => return None,
            (Some(turn_text), None) => (WARNING_DOCUMENT_ID, turn_text),
            (None, Some(context_text)) => (CONTEXT_DOCUMENT_ID, context_text),
            (Some(turn_text), Some(context_text)) => (
                WARNING_DOCUMENT_ID,
                format!("{turn_text}\n\n{context_text}"),
            ),
        };
        Some(Document {
            id: id.to_string(),
            text,
            additional_props: HashMap::new(),
        })
    }

    /// Requests extra turns for this invocation, enforcing the whole policy
    ///
    /// Refused only for zero turns or a spent allowance (the latter still
    /// recorded as the unmet request the parent-visible handoff will carry).
    /// Inside the warning window the grant is clamped to the remaining
    /// allowance and applied to the running rig budget the next time the cap
    /// is actually hit. OUTSIDE the window the request becomes a standing
    /// reservation (latest wins) that converts automatically at the cap --
    /// agents tend to ask early or never, so an early ask must count.
    /// Every outcome is recorded and mirrored as a
    /// [`ThorPalEvent::SubAgentExtended`] with its trigger.
    ///
    /// # Arguments
    ///
    /// * `requested` - How many extra turns the agent asked for
    /// * `justification` - The agent's one-sentence justification
    pub(crate) fn request_extension(
        &self,
        requested: usize,
        justification: String,
    ) -> Result<String, ToolExecutionError> {
        // zero turns is a malformed request, not a policy decision
        if requested == 0 {
            return Err(ToolExecutionError::invalid_args("turns must be at least 1"));
        }
        // a spent allowance refuses regardless of the window -- a reservation
        // against nothing could never convert -- and records the unmet request
        let allowance_left = self.extension_left();
        if allowance_left == 0 {
            self.push_request(requested, 0, justification);
            return Err(ToolExecutionError::invalid_args(format!(
                "Extension allowance exhausted ({allowance} extra turns already granted this \
                 task). Your request and justification were recorded for the orchestrator. \
                 Post every unposted finding to the forum NOW and make your next message the \
                 final handoff: status, concrete findings with evidence, and what remains.",
                allowance = self.budget.max_extension_turns,
            )));
        }
        // inside the warning window the grant lands immediately
        let calls_left = self.calls_left(self.current_turn.load(Ordering::SeqCst));
        if calls_left <= self.budget.warn_turns {
            // clamp the grant to the allowance and take it
            let granted_now = requested.min(allowance_left);
            self.granted.fetch_add(granted_now, Ordering::SeqCst);
            // record the request, granted amount, and justification durably
            self.push_request(requested, granted_now, justification.clone());
            self.notify_extension(granted_now, justification, ExtensionTrigger::Requested);
            // confirm the grant and keep the pressure on wrapping up
            return Ok(format!(
                "Granted {granted_now} extra turn(s) (requested {requested}); your budget is \
                 now {cap} model calls total and {remaining} extension turn(s) remain. The \
                 justification was recorded for the orchestrator. Post findings and wrap up -- \
                 further extensions are not guaranteed.",
                cap = self.effective_cap(),
                remaining = self.extension_left(),
            ));
        }
        // outside the window the request becomes a standing reservation; a
        // later request replaces an earlier one (the latest justification is
        // the one that matters at the cap)
        let reservation = ReservedExtension {
            requested,
            justification: justification.clone(),
            at: Utc::now(),
        };
        *self
            .reservation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reservation);
        self.notify_extension(requested, justification, ExtensionTrigger::Reserved);
        // confirm the reservation without inviting further budget chatter
        Ok(format!(
            "Too early to grant now ({calls_left} model calls of a {cap}-call budget remain), \
             so this is recorded as a STANDING RESERVATION of {requested} extra turn(s): it \
             applies automatically if you actually hit your cap (clamped to the \
             {allowance_left}-turn allowance then remaining). Keep working -- do not call \
             request_more_turns again unless your needs change.",
            cap = self.effective_cap(),
        ))
    }

    /// Records one extension request, granted and refused alike
    ///
    /// # Arguments
    ///
    /// * `requested` - How many extra turns the agent asked for
    /// * `granted` - How many were actually granted
    /// * `justification` - The justification the agent gave
    fn push_request(&self, requested: usize, granted: usize, justification: String) {
        // append the request under the lock so the log stays ordered
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(ExtensionRequest {
                requested,
                granted,
                justification,
                at: Utc::now(),
            });
    }

    /// Drains the granted turns not yet applied to the running rig budget
    ///
    /// Consumed by the hook's turns-exhausted answer: whatever was granted
    /// since the last drain is applied exactly once. A grant that lands during
    /// the drain stays pending and is picked up at the next exhaustion.
    pub(crate) fn drain_pending_extension(&self) -> usize {
        // snapshot the grant total, then mark everything up to it applied
        let granted = self.granted.load(Ordering::SeqCst);
        let applied = self.applied.swap(granted, Ordering::SeqCst);
        granted.saturating_sub(applied)
    }

    /// Takes the one-shot truncation retry, returning whether it was still free
    pub(crate) fn take_length_retry(&self) -> bool {
        // the latch flips exactly once per invocation
        !self.length_retry_used.swap(true, Ordering::SeqCst)
    }

    /// Takes a copy of every extension request made, for the agent record
    pub(crate) fn extension_log(&self) -> Vec<ExtensionRequest> {
        // clone the log out so the record owns its copy
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The last refused extension request, if the agent asked and was denied
    ///
    /// This is what the parent-visible handoff surfaces so the orchestrator
    /// knows the agent wanted more budget and why.
    pub(crate) fn unmet_request(&self) -> Option<ExtensionRequest> {
        // the latest zero-grant entry is the request that went unmet
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .rev()
            .find(|request| request.granted == 0)
            .cloned()
    }

    /// Takes a copy of the mirrored transcript, for salvage without an error history
    pub(crate) fn transcript_snapshot(&self) -> Vec<Message> {
        // clone the mirror out so salvage owns its copy
        self.transcript
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// Renders the wrap-up warning text for a remaining-call count
///
/// # Arguments
///
/// * `calls_left` - How many model calls remain, including the current one
/// * `cap` - The effective model-call cap, including granted extensions
/// * `extension_left` - The extension turns still available to request
/// * `reserved` - The agent's standing reservation size, if one exists
/// * `forced` - Whether this is the true final turn with tools disabled
/// * `clamped` - Whether the critical context clamp restricts this request's
///   tools, in which case the extension tool must not be ordered (extra turns
///   cannot buy context space anyway)
fn warning_text(
    calls_left: usize,
    cap: usize,
    extension_left: usize,
    reserved: Option<usize>,
    forced: bool,
    clamped: bool,
) -> String {
    // the very last call gets a blunter, reply-now variant
    if calls_left <= 1 {
        // the true final turn has its tools disabled: say so instead of
        // offering escapes that no longer exist
        if forced {
            return format!(
                "FINAL TURN: this is your LAST model call of a {cap}-call budget and TOOL \
                 CALLS ARE DISABLED for this reply. Reply now with your final handoff: \
                 status, concrete findings with evidence, and exactly what remains to be \
                 done. This reply is the only thing the orchestrator will receive."
            );
        }
        // not actually final: a reservation or the automatic backstop will
        // extend the run if the agent keeps working -- be honest about it
        // (but never order the extension tool while the context clamp holds)
        let continuation = match reserved {
            Some(reserved) => format!(
                " Your standing reservation of {reserved} extra turn(s) applies \
                 automatically if you keep working past this call -- spend it on posting \
                 findings and the final handoff, not on new investigations."
            ),
            None if extension_left > 0 && !clamped => format!(
                " Your one remaining alternative is a single request_more_turns call \
                 ({extension_left} extension turn(s) available)."
            ),
            None => String::new(),
        };
        return format!(
            "FINAL TURN: this is your LAST model call of a {cap}-call budget. Reply now with \
             your final handoff: status, concrete findings with evidence, and exactly what \
             remains to be done. If you call tools instead, their results will never reach \
             you -- only forum posts those calls make survive.{continuation}"
        );
    }
    // inside the window but not final: ordered wrap-up directions, with the
    // extension line matching the agent's actual options
    let extension = match reserved {
        Some(reserved) => format!(
            "2. Your reserved {reserved} extra turn(s) apply automatically at the cap -- do \
             not call request_more_turns again unless your needs change.\n"
        ),
        None if extension_left > 0 && !clamped => format!(
            "2. If finishing properly truly needs more calls, call request_more_turns with \
             the count and a one-sentence justification ({extension_left} extension turn(s) \
             available this task).\n"
        ),
        // under the clamp the priority is the flush, not budget chatter
        None if extension_left > 0 => {
            "2. Do NOT call request_more_turns right now -- extra turns cannot add context \
             memory; post findings instead.\n"
                .to_string()
        }
        None => "2. Your extension allowance is spent; no more turns can be granted.\n".to_string(),
    };
    format!(
        "TURN BUDGET WARNING: {calls_left} model calls left (including this one) of a \
         {cap}-call budget. Wrap up now, in this order:\n\
         1. Post every unposted finding to the forum NOW (create_post / create_comment) -- \
         anything that lives only in tool output or your head is lost when the budget runs \
         out.\n\
         {extension}\
         3. Make your final message a handoff: status, concrete findings with evidence, and \
         exactly what remains to be done (and where to pick up).\n\
         Do not start new deep investigations."
    )
}

/// Renders the context-pressure warning text for a usage reading
///
/// Deliberately never mentions `request_more_turns`: turns cannot buy context
/// space, so the only remedies are flushing findings to the forum and keeping
/// further tool output small.
///
/// # Arguments
///
/// * `pressure` - The warning band the context usage landed in
/// * `used` - The estimated context tokens in use
/// * `window` - The configured context window the estimate is measured against
/// * `forced` - Whether this is the true final turn (tools disabled), where
///   flush orders would name tools the request cannot reach
fn context_warning_text(pressure: ContextPressure, used: u64, window: u64, forced: bool) -> String {
    // render the live percentage so the number tracks the real pressure
    let percent = used.saturating_mul(100) / window.max(1);
    // on the forced final turn the only remaining surface is the reply
    // itself: ordering tool-based flushes or promising a next message would
    // contradict the tools-disabled turn half
    if forced {
        return format!(
            "CONTEXT NOTE: your conversation memory is also ~{percent}% full \
             (~{used} of {window} tokens); everything not in this final reply is lost, so \
             include every unposted finding in it."
        );
    }
    match pressure {
        // the critical band pairs with the forum-write tool clamp
        ContextPressure::Critical => format!(
            "CONTEXT CRITICAL: your conversation memory is ~{percent}% full \
             (~{used} of {window} tokens) and the next large tool result can kill this run \
             with NO further warning -- everything not on the forum would be lost. Tool \
             access is restricted to forum/communication tools for this reply. Post every \
             unposted finding NOW as several small posts (create_post / create_comment), \
             then make your next message the final handoff: status, findings with evidence, \
             and exactly what remains."
        ),
        // the warn band orders the flush while there is still room to work
        _ => format!(
            "CONTEXT BUDGET WARNING: your conversation memory is ~{percent}% full \
             (~{used} of {window} tokens). Running out kills this run mid-flight, losing \
             everything not posted to the forum -- and extra turns CANNOT help (turns do \
             not add memory). Do this now:\n\
             1. Post every unposted finding to the forum NOW as several small posts \
             (create_post / create_comment).\n\
             2. Keep further tool output minimal: small read windows, filters, no bulk \
             dumps.\n\
             3. If the work is essentially done, make your next message the final handoff."
        ),
    }
}

/// Estimates one message's rendered size in characters
///
/// Serializes the message to JSON and counts bytes: that sees every content
/// kind (text, tool calls, tool results, encoded blobs) without pattern
/// matching rig's message shape, and the structural overhead biases the
/// estimate high -- context warnings fire early, never late.
///
/// # Arguments
///
/// * `message` - The history message to estimate
fn estimate_message_chars(message: &Message) -> u64 {
    // a message that fails to serialize contributes nothing to the estimate
    serde_json::to_string(message)
        .map(|rendered| rendered.len() as u64)
        .unwrap_or(0)
}

/// Requests extra model-call turns for the running sub-agent, with a justification
///
/// # Arguments
///
/// * `context` - The tool call context carrying this run's turn-budget state
/// * `turns` - How many extra model calls the agent is asking for
/// * `justification` - The agent's one-sentence justification for the grant
#[rig::tool_macro(
    description = "Requests extra model-call turns for yourself when you genuinely need \
more to finish the task properly. Provide `turns` (how many extra calls you need) and a \
one-sentence `justification`; grants are auto-approved but clamped to your per-task \
extension allowance, and the justification is recorded for the orchestrator. Inside the \
TURN BUDGET WARNING window the grant applies immediately; called earlier, it is recorded \
as a standing reservation that applies automatically if you actually hit your cap -- \
either way, one call is enough. Extra turns do NOT add context memory: if a CONTEXT \
BUDGET WARNING appears, post findings to the forum instead."
)]
pub async fn request_more_turns(
    #[rig(context)] context: &mut ToolContext,
    turns: usize,
    justification: String,
) -> Result<String, ToolExecutionError> {
    // recover this run's shared budget state and apply the whole grant policy
    let state = TurnBudgetState::from_context(context)?;
    state.request_extension(turns, justification)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a budget state with a 10-call cap, 4-turn allowance, 3-turn window
    fn state() -> TurnBudgetState {
        TurnBudgetState::new(
            "ReverseEngineer(Io)",
            TurnBudget {
                max_turns: 10,
                max_extension_turns: 4,
                warn_turns: 3,
                // context tracking stays disarmed for the turn-only tests
                context_window: 0,
            },
            None,
        )
    }

    /// Build a budget state with context tracking armed over a given window
    ///
    /// # Arguments
    ///
    /// * `window` - The context window in tokens
    fn context_state(window: u64) -> TurnBudgetState {
        TurnBudgetState::new(
            "ReverseEngineer(Io)",
            TurnBudget {
                max_turns: 10,
                max_extension_turns: 4,
                warn_turns: 3,
                context_window: window,
            },
            None,
        )
    }

    /// The remaining-call math counts the current call and never underflows.
    #[test]
    fn calls_left_counts_the_current_call() {
        let state = state();
        // the first call of a 10-call budget has all 10 left
        assert_eq!(state.calls_left(1), 10);
        // the window arms at turns 8, 9, and 10
        assert_eq!(state.calls_left(8), 3);
        assert_eq!(state.calls_left(9), 2);
        assert_eq!(state.calls_left(10), 1);
        // a stale zero turn index saturates instead of underflowing
        assert_eq!(state.calls_left(0), 10);
    }

    /// Warnings only render inside the window, and the final call is blunt.
    #[test]
    fn warnings_arm_inside_the_window() {
        let state = state();
        // before the window no document is injected at all
        assert!(state.warning_document(7).is_none());
        // inside the window the exact count and the tool are both named
        let doc = state.warning_document(8).expect("turn 8 should warn");
        assert!(doc.text.contains("3 model calls left"));
        assert!(doc.text.contains("request_more_turns"));
        assert!(!doc.text.contains("FINAL TURN"));
        // the last call switches to the reply-now variant
        let doc = state.warning_document(10).expect("turn 10 should warn");
        assert!(doc.text.contains("FINAL TURN"));
    }

    /// Grants clamp to the allowance and raise the effective cap.
    #[test]
    fn grants_clamp_to_the_allowance() {
        let state = state();
        // move inside the window so the gate opens
        state.current_turn.store(9, Ordering::SeqCst);
        // an oversized request is clamped to the 4-turn allowance
        let reply = state
            .request_extension(50, "two functions left to decompile".to_string())
            .expect("a request inside the window should grant");
        assert!(reply.contains("Granted 4 extra turn(s)"));
        assert_eq!(state.effective_cap(), 14);
        // the whole grant is pending until the cap is actually hit
        assert_eq!(state.drain_pending_extension(), 4);
        // draining twice applies nothing new
        assert_eq!(state.drain_pending_extension(), 0);
        // the grant is recorded with what was asked and what was given
        let log = state.extension_log();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].requested, 50);
        assert_eq!(log[0].granted, 4);
        // a fully granted request is not an unmet one
        assert!(state.unmet_request().is_none());
    }

    /// Requests outside the warning window become standing reservations.
    #[test]
    fn early_requests_become_reservations() {
        let state = state();
        // turn 2 of 10 is well before the window: the ask is reserved, not lost
        state.current_turn.store(2, Ordering::SeqCst);
        let reply = state
            .request_extension(2, "deep unpacking ahead".to_string())
            .expect("an early request should be reserved");
        assert!(reply.contains("STANDING RESERVATION"));
        assert!(reply.contains("2 extra turn(s)"));
        // nothing is granted yet: the cap and the request log are untouched
        assert_eq!(state.effective_cap(), 10);
        assert!(state.extension_log().is_empty());
        assert!(state.unmet_request().is_none());
        // the reservation is visible to the warnings and the salvage path
        let reserved = state.reservation_snapshot().expect("reserved");
        assert_eq!(reserved.requested, 2);
        assert_eq!(reserved.justification, "deep unpacking ahead");
        // a later early request replaces the earlier one -- latest wins
        state
            .request_extension(3, "more layers than expected".to_string())
            .expect("a replacement reservation should be accepted");
        let reserved = state.reservation_snapshot().expect("replaced");
        assert_eq!(reserved.requested, 3);
        // zero turns is malformed regardless of the window
        state.current_turn.store(9, Ordering::SeqCst);
        assert!(state.request_extension(0, "none".to_string()).is_err());
    }

    /// A spent allowance refuses (anywhere) and records the unmet request.
    #[test]
    fn exhausted_allowance_records_the_unmet_request() {
        let state = state();
        // spend the whole allowance with one clamped grant
        state.current_turn.store(9, Ordering::SeqCst);
        state
            .request_extension(10, "finishing decryption".to_string())
            .expect("the first request should grant");
        // with nothing left to grant OR reserve, every further ask refuses --
        // window position no longer matters (a reservation could never convert)
        let err = state
            .request_extension(5, "one more constant to verify".to_string())
            .expect_err("a spent allowance should refuse");
        assert!(err.to_string().contains("allowance exhausted"));
        let unmet = state
            .unmet_request()
            .expect("the refusal should be recorded");
        assert_eq!(unmet.requested, 5);
        assert_eq!(unmet.granted, 0);
        assert_eq!(unmet.justification, "one more constant to verify");
    }

    /// The truncation retry latch fires exactly once per invocation.
    #[test]
    fn length_retry_latch_is_one_shot() {
        let state = state();
        // the first take wins the retry, every later take loses it
        assert!(state.take_length_retry());
        assert!(!state.take_length_retry());
    }

    /// The transcript mirror replaces per call and appends accepted turns.
    #[test]
    fn transcript_mirror_tracks_the_run() {
        let state = state();
        // the first call mirrors its history plus the pending prompt
        state.note_turn(1, &Message::user("do the task"), &[]);
        assert_eq!(state.transcript_snapshot().len(), 1);
        // an accepted assistant turn appends to the mirror
        state.note_assistant(&[AssistantContent::text("working on it")]);
        assert_eq!(state.transcript_snapshot().len(), 2);
        // the next call replaces the mirror with the canonical history
        let history = vec![
            Message::user("do the task"),
            Message::assistant("working on it"),
        ];
        state.note_turn(2, &Message::user("tool results"), &history);
        assert_eq!(state.transcript_snapshot().len(), 3);
        // an empty assistant turn is skipped rather than recorded
        state.note_assistant(&[]);
        assert_eq!(state.transcript_snapshot().len(), 3);
    }

    /// A cap-hit converts the standing reservation, clamped to the allowance.
    #[test]
    fn cap_hit_converts_the_reservation() {
        let state = state();
        // reserve more than the 4-turn allowance, early in the run
        state.current_turn.store(2, Ordering::SeqCst);
        state
            .request_extension(50, "many layers to unpack".to_string())
            .expect("the early request should be reserved");
        // the cap-hit converts the reservation, clamped to the allowance
        assert_eq!(state.cap_hit_extension(), 4);
        assert_eq!(state.effective_cap(), 14);
        // the conversion is recorded with what was asked and what was given
        let log = state.extension_log();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].requested, 50);
        assert_eq!(log[0].granted, 4);
        assert_eq!(log[0].justification, "many layers to unpack");
        // the reservation is consumed and the grant is already applied
        assert!(state.reservation_snapshot().is_none());
        assert_eq!(state.pending_extension(), 0);
        // the next cap-hit finds the allowance spent AND the auto latch burns
        // on nothing, so the run ends there
        assert_eq!(state.cap_hit_extension(), 0);
    }

    /// With no request made, the cap-hit auto-extends exactly once.
    #[test]
    fn cap_hit_auto_extends_exactly_once() {
        let state = state();
        // the automatic backstop grants one wrap-up horizon: min(warn, left)
        assert_eq!(state.cap_hit_extension(), 3);
        assert_eq!(state.effective_cap(), 13);
        // the grant is recorded with the automatic justification
        let log = state.extension_log();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].granted, 3);
        assert!(log[0].justification.contains("automatic"));
        // the latch is one-shot: the next cap-hit ends the run even though
        // one allowance turn remains
        assert_eq!(state.cap_hit_extension(), 0);
    }

    /// An unapplied in-window grant wins over reservation and auto backstop.
    #[test]
    fn pending_grant_wins_over_reservation_and_auto() {
        let state = state();
        // reserve early, then also win an in-window grant
        state.current_turn.store(2, Ordering::SeqCst);
        state
            .request_extension(3, "reserved early".to_string())
            .expect("reserved");
        state.current_turn.store(9, Ordering::SeqCst);
        state
            .request_extension(2, "in-window grant".to_string())
            .expect("granted");
        // the cap-hit applies the pending grant only; the reservation stays
        assert_eq!(state.cap_hit_extension(), 2);
        assert!(state.reservation_snapshot().is_some());
        // the next cap-hit converts the reservation, clamped to what is left
        // of the allowance (4 - 2 = 2)
        assert_eq!(state.cap_hit_extension(), 2);
        assert!(state.reservation_snapshot().is_none());
        // total spend stays bounded by max_turns + max_extension_turns
        assert_eq!(state.effective_cap(), 14);
    }

    /// The forced final turn fires only when nothing can extend the run.
    #[test]
    fn forced_final_turn_table() {
        // a fresh state at its last call still has the auto backstop
        let fresh = state();
        assert!(!fresh.forced_final_turn(10));
        // with the auto latch spent and no reservation, the last call is final
        let auto_spent = state();
        assert_eq!(auto_spent.cap_hit_extension(), 3);
        assert_eq!(auto_spent.cap_hit_extension(), 0);
        // the cap is now 13; its last call is turn 13 and nothing can extend
        assert!(auto_spent.forced_final_turn(13));
        assert!(!auto_spent.forced_final_turn(12));
        // a standing reservation keeps the last call non-final
        let reserved = state();
        reserved.current_turn.store(2, Ordering::SeqCst);
        reserved
            .request_extension(2, "reserved".to_string())
            .expect("reserved");
        assert!(!reserved.forced_final_turn(10));
        // the cap-hit converts the reservation first, then the auto backstop
        // fires once with what remains of the allowance, then nothing is left
        assert_eq!(reserved.cap_hit_extension(), 2);
        assert_eq!(reserved.cap_hit_extension(), 2);
        assert_eq!(reserved.cap_hit_extension(), 0);
        // the cap is now 14 and nothing can extend: its last call is final
        assert!(reserved.forced_final_turn(14));
        // a pending in-window grant keeps it non-final too
        let granted = state();
        granted.current_turn.store(9, Ordering::SeqCst);
        granted
            .request_extension(1, "granted".to_string())
            .expect("granted");
        assert!(!granted.forced_final_turn(10));
        // and a spent allowance with the auto latch burned is final
        let spent = state();
        spent.current_turn.store(9, Ordering::SeqCst);
        spent
            .request_extension(10, "spend it all".to_string())
            .expect("granted");
        assert_eq!(spent.cap_hit_extension(), 4);
        assert_eq!(spent.cap_hit_extension(), 0);
        assert!(spent.forced_final_turn(14));
        // the forced variant says tools are disabled; the open one does not
        let forced = warning_text(1, 13, 0, None, true, false);
        assert!(forced.contains("TOOL CALLS ARE DISABLED"));
        let open = warning_text(1, 10, 4, None, false, false);
        assert!(!open.contains("TOOL CALLS ARE DISABLED"));
        assert!(open.contains("request_more_turns"));
    }

    /// A standing reservation suppresses the re-request nudge in warnings.
    #[test]
    fn reservation_suppresses_the_request_nudge() {
        let state = state();
        // reserve early, then enter the warning window
        state.current_turn.store(2, Ordering::SeqCst);
        state
            .request_extension(2, "reserved".to_string())
            .expect("reserved");
        let doc = state.warning_document(8).expect("turn 8 should warn");
        // the warning names the reservation instead of inviting another call
        assert!(doc.text.contains("reserved 2 extra turn(s)"));
        assert!(doc.text.contains("not call request_more_turns again"));
        // the final-turn variant names the reservation too
        let doc = state.warning_document(10).expect("turn 10 should warn");
        assert!(doc.text.contains("standing reservation of 2 extra turn(s)"));
    }

    /// Context warnings arm at the thresholds off either usage signal.
    #[test]
    fn context_warnings_arm_at_the_thresholds() {
        let tokens = context_state(1000);
        // below the warn threshold no document is injected at all
        tokens.last_context_tokens.store(500, Ordering::SeqCst);
        assert_eq!(tokens.context_pressure(), ContextPressure::None);
        assert!(tokens.warning_document(2).is_none());
        // the token signal arms the warn band at 80%
        tokens.last_context_tokens.store(800, Ordering::SeqCst);
        assert_eq!(tokens.context_pressure(), ContextPressure::Warn);
        let doc = tokens.warning_document(2).expect("warn should inject");
        assert_eq!(doc.id, "context_budget_warning");
        assert!(doc.text.contains("CONTEXT BUDGET WARNING"));
        assert!(doc.text.contains("~80%"));
        // turns cannot buy context space, so the tool is never advertised
        assert!(!doc.text.contains("request_more_turns"));
        // the live char estimate arms the band on its own (max of both)
        let estimated = context_state(1000);
        let giant = Message::user("x".repeat(3600 * CHARS_PER_TOKEN as usize));
        estimated.note_turn(1, &giant, &[]);
        assert_eq!(estimated.context_pressure(), ContextPressure::Critical);
        let doc = estimated
            .warning_document(1)
            .expect("critical should inject");
        assert!(doc.text.contains("CONTEXT CRITICAL"));
        // inside the turn window both warnings merge into ONE document, and
        // the merged text never ORDERS the extension tool the critical clamp
        // just removed from the request -- it forbids it instead
        let merged = context_state(1000);
        merged.last_context_tokens.store(950, Ordering::SeqCst);
        let doc = merged.warning_document(9).expect("both should inject");
        assert_eq!(doc.id, "turn_budget_warning");
        assert!(doc.text.contains("TURN BUDGET WARNING"));
        assert!(doc.text.contains("CONTEXT CRITICAL"));
        assert!(!doc.text.contains("call request_more_turns with"));
        assert!(!doc.text.contains("one remaining alternative"));
        assert!(doc.text.contains("Do NOT call request_more_turns"));
        // the clamped last-pre-extension call drops the alternative line too
        let doc = merged.warning_document(10).expect("final should inject");
        assert!(!doc.text.contains("one remaining alternative"));
        // a zero window keeps context tracking fully disarmed
        let disarmed = state();
        disarmed
            .last_context_tokens
            .store(u64::MAX, Ordering::SeqCst);
        assert_eq!(disarmed.context_pressure(), ContextPressure::None);
    }

    /// The clamp allow-list keeps the extension tool exactly when attached.
    #[test]
    fn flush_tools_track_the_extension_allowance() {
        // an agent with an allowance keeps request_more_turns reachable, so a
        // stale request cannot void a flush batch via skip semantics
        let with_allowance = state();
        let tools = with_allowance.flush_tools();
        assert!(tools.iter().any(|tool| tool == "create_post"));
        assert!(tools.iter().any(|tool| tool == "request_more_turns"));
        // a zero-allowance agent never had the tool attached: naming it in
        // active_tools would be a hard request error, so it stays out
        let without = TurnBudgetState::new(
            "Researcher(Io)",
            TurnBudget {
                max_turns: 10,
                max_extension_turns: 0,
                warn_turns: 3,
                context_window: 1000,
            },
            None,
        );
        assert!(
            !without
                .flush_tools()
                .iter()
                .any(|tool| tool == "request_more_turns")
        );
    }

    /// The forced final turn's context half fits a tools-disabled reply.
    #[test]
    fn forced_final_context_text_names_no_tools() {
        // exhaust the extension mechanisms so the final call is forced: the
        // auto backstop fires once (3 = min(warn, allowance)), then nothing
        let state = context_state(1000);
        assert_eq!(state.cap_hit_extension(), 3);
        assert_eq!(state.cap_hit_extension(), 0);
        state.last_context_tokens.store(950, Ordering::SeqCst);
        // the cap is now 13; its last call renders the merged document
        let turn = state.effective_cap();
        assert!(state.forced_final_turn(turn));
        let doc = state.warning_document(turn).expect("should inject");
        // the turn half says tools are disabled; the context half must not
        // order tool-based flushes or promise a next message
        assert!(doc.text.contains("TOOL CALLS ARE DISABLED"));
        assert!(doc.text.contains("CONTEXT NOTE"));
        assert!(!doc.text.contains("create_post"));
        assert!(!doc.text.contains("next message"));
    }

    /// The context report latches once per level, in rising order.
    #[test]
    fn take_context_report_latches_per_level() {
        let state = context_state(1000);
        // below the thresholds there is nothing to report
        assert!(state.take_context_report().is_none());
        // crossing warn reports level 1 exactly once
        state.last_context_tokens.store(810, Ordering::SeqCst);
        let (level, used, window) = state.take_context_report().expect("warn reports");
        assert_eq!((level, used, window), (1, 810, 1000));
        assert!(state.take_context_report().is_none());
        // crossing critical reports level 2 exactly once
        state.last_context_tokens.store(920, Ordering::SeqCst);
        let (level, ..) = state.take_context_report().expect("critical reports");
        assert_eq!(level, 2);
        assert!(state.take_context_report().is_none());
    }

    /// A resumed state deducts prior spend and grants from its knobs
    #[test]
    fn resumed_state_deducts_prior_spend() {
        // the original invocation ran under a 10-call cap with a 4-turn
        // allowance, spent 6 calls, and was granted 2 extra turns
        let budget = TurnBudget {
            max_turns: 10,
            max_extension_turns: 4,
            warn_turns: 3,
            context_window: 0,
        };
        let prior = vec![ExtensionRequest {
            requested: 2,
            granted: 2,
            justification: "more to verify".to_owned(),
            at: Utc::now(),
        }];
        let state = TurnBudgetState::resumed("ReverseEngineer(Io)", budget, 6, prior, None);
        // cap = 10 base + 2 granted - 6 used = 6; nothing is pre-granted live
        assert_eq!(state.effective_cap(), 6);
        // the allowance shrank by the grants already made
        assert_eq!(state.extension_left(), 2);
        // the prior requests survive into the record's extension log
        assert_eq!(state.extension_log().len(), 1);
        // a run that overspent its cap still gets the one handoff turn
        let starved = TurnBudgetState::resumed("ReverseEngineer(Io)", budget, 20, Vec::new(), None);
        assert_eq!(starved.effective_cap(), 1);
    }
}
