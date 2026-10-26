//! A conversation memory that mirrors the main agent's history to disk
//!
//! rig only appends to an agent's [`ConversationMemory`] once per settled run,
//! so a run that is quit mid-orchestration would persist nothing. This wrapper
//! delegates to the in-memory backend and rewrites the session store's
//! `chat.json` after every change -- both the canonical per-turn append and the
//! per-model-turn `pending` records the main agent's hook captures -- so the
//! root conversation survives a quit at any point.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use rig::agent::hook::{AgentHook, CompletionResponse, HookContext, ObservationAction};
use rig::completion::Message;
use rig::completion::message::{
    AssistantContent, ProviderCallId, ToolCallId, ToolResultContent, UserContent,
};
use rig::memory::{ConversationMemory, InMemoryConversationMemory, MemoryError};
use rig::wasm_compat::WasmBoxedFuture;

use super::resume::{CHAT_HISTORY_VERSION, ChatHistory};
use super::store::SessionStore;

/// One tool call an interrupted turn left dangling, with everything a resume
/// needs to answer it: the exact ids a result must echo, the tool it named,
/// and the arguments it was issued with (a delegation call's `goal`/`prompt`).
#[derive(Debug, Clone)]
pub(crate) struct DanglingCall {
    /// rig's correlation handle for the call, answered by the eventual result
    pub id: ToolCallId,
    /// The provider-issued call id, echoed back on the result when one exists
    pub provider: Option<ProviderCallId>,
    /// The called tool's name
    pub tool: String,
    /// The arguments the model issued the call with
    pub args: serde_json::Value,
}

/// Scans a message tail for the tool calls no result ever answered, in order
///
/// A result anywhere in the tail (or an earlier message of it) counts as an
/// answer, so a mid-turn tail whose completed calls were already answered
/// yields only the truly dangling trailing calls.
///
/// # Arguments
///
/// * `tail` - The messages to scan, in order
pub(crate) fn dangling_calls(tail: &[Message]) -> Vec<DanglingCall> {
    // scan the single slice through the shared iterator core
    dangling_calls_iter(tail.iter())
}

/// Scans a settled history joined with its pending tail for unanswered calls
///
/// The restore split consults the two halves together so a pending tail that
/// ANSWERS the settled half's trailing calls (a resume crashed between
/// settling its tail and recording a completed turn) is never re-detected as
/// dangling, while calls stranded in either half still surface.
///
/// # Arguments
///
/// * `settled` - The settled history half
/// * `pending` - The in-flight pending half
pub(crate) fn dangling_calls_joined(settled: &[Message], pending: &[Message]) -> Vec<DanglingCall> {
    // scan the two halves as one message stream
    dangling_calls_iter(settled.iter().chain(pending.iter()))
}

/// The shared scan core behind both dangling-call entry points
///
/// # Arguments
///
/// * `messages` - The message stream to scan, in order
fn dangling_calls_iter<'a>(
    messages: impl Iterator<Item = &'a Message> + Clone,
) -> Vec<DanglingCall> {
    // index every tool call the stream already answers
    let answered: HashSet<_> = messages
        .clone()
        .flat_map(|message| match message {
            Message::User { content } => content.as_slice(),
            _ => &[],
        })
        .filter_map(|content| match content {
            UserContent::ToolResult(result) => Some(result.call.clone()),
            _ => None,
        })
        .collect();
    // collect the calls no result ever answered, in order
    messages
        .flat_map(|message| match message {
            Message::Assistant { content, .. } => content.as_slice(),
            _ => &[],
        })
        .filter_map(|content| match content {
            AssistantContent::ToolCall(call) if !answered.contains(&call.id) => {
                Some(DanglingCall {
                    id: call.id.clone(),
                    provider: call.provider.clone(),
                    tool: call.function.name.clone(),
                    args: call.function.arguments.clone(),
                })
            }
            _ => None,
        })
        .collect()
}

/// The synthetic result text answering a call an interruption orphaned
///
/// Shared by the tail repair (which answers every dangling call this way) and
/// the resume path (which answers only the non-delegation calls this way --
/// delegations get their real results instead).
///
/// # Arguments
///
/// * `reason` - A short human-readable description of why the turn died
pub(crate) fn interrupted_result_text(reason: &str) -> String {
    format!(
        "[interrupted] this tool call never completed -- the turn was \
         interrupted before a result was produced ({reason}). Do not \
         assume it ran; re-issue it if the result is still needed."
    )
}

/// Repairs an interrupted turn's partial messages into a settleable tail
///
/// A failed turn's captured work can end on an assistant message whose tool
/// calls were never answered -- a shape providers reject on replay -- so this
/// synthesizes an "[interrupted]" tool result for every dangling call and
/// closes the tail with a synthetic assistant note: appended as its own
/// message when the tail ends user-side, folded into the trailing assistant
/// message otherwise (two assistant messages back-to-back are a shape
/// providers also reject). The result ends assistant-side like any settled
/// turn with role alternation intact. Returns `None` when the tail carries
/// no assistant work worth keeping.
///
/// # Arguments
///
/// * `tail` - The interrupted turn's captured messages, in order
/// * `reason` - A short human-readable description of why the turn died
pub(crate) fn repair_interrupted_tail(
    mut tail: Vec<Message>,
    reason: &str,
) -> Option<Vec<Message>> {
    // a tail with no assistant content preserves nothing worth replaying
    if !tail
        .iter()
        .any(|message| matches!(message, Message::Assistant { .. }))
    {
        return None;
    }
    // collect the calls no result ever answered, in order
    let dangling = dangling_calls(&tail);
    // answer each dangling call so providers accept the replayed history
    if !dangling.is_empty() {
        let results = dangling
            .into_iter()
            .map(|call| {
                UserContent::tool_result_for(
                    call.id,
                    call.provider,
                    call.tool,
                    vec![ToolResultContent::text(interrupted_result_text(reason))],
                )
            })
            .collect();
        tail.push(Message::User { content: results });
    }
    // compose the closing note that marks where the turn died
    let note = format!(
        "[turn interrupted: {reason}. The partial work above is preserved \
         context; continue from here.]"
    );
    // close assistant-side so the next user turn alternates cleanly
    match tail.last_mut() {
        // a tail already ending assistant-side absorbs the note into that
        // message -- a second assistant message back-to-back is a shape
        // providers reject on every later turn
        Some(Message::Assistant { content, .. }) => {
            content.push(AssistantContent::text(note));
        }
        // the tail ends user-side (real or synthetic tool results), so a
        // fresh assistant note alternates cleanly
        _ => {
            tail.push(Message::Assistant {
                id: None,
                content: vec![AssistantContent::text(note)],
            });
        }
    }
    Some(tail)
}

/// A restored conversation split into its settled and interrupted halves
///
/// Produced by the session restore from `chat.json`: a conversation whose
/// in-flight tail left delegation calls dangling is held as `Interrupted` so
/// the resume can re-run those delegations for real and answer the exact
/// calls, while everything else settles as before (dangling non-delegation
/// calls get synthetic results through the tail repair).
pub(crate) enum RestoredConversation {
    /// The whole history is settled (any dead tail was repaired synthetically)
    Settled(Vec<Message>),
    /// The history ends in an interrupted turn whose dangling delegations the
    /// resume re-runs for real before the conversation continues
    Interrupted {
        /// The settled history preceding the interrupted turn
        settled: Vec<Message>,
        /// The interrupted turn's raw tail, held as pending without settling
        /// (empty when a prior resume settled it and died before continuing)
        tail: Vec<Message>,
        /// The calls the interruption left dangling, in order
        dangling: Vec<DanglingCall>,
    },
}

/// A [`ConversationMemory`] that persists `chat.json` after every change
///
/// Clones share the same inner memory, pending buffer, and store handles, so
/// the builder can hand one clone to the agent and keep another for snapshots
/// and resume seeding. Persistence is best-effort: a serialization or write
/// failure is logged and swallowed rather than failing the turn, matching the
/// session autosave philosophy.
#[derive(Clone)]
pub(crate) struct PersistingConversationMemory {
    /// The in-memory backend holding the settled conversation history.
    inner: InMemoryConversationMemory,
    /// The session store `chat.json` is written to, or `None` to disable
    /// persistence (the wrapper then degrades to the plain in-memory backend).
    store: Option<Arc<SessionStore>>,
    /// The conversation id the persisted history is keyed by.
    conversation_id: String,
    /// The in-flight turn's messages captured so far by the main agent's hook.
    pending: Arc<Mutex<Vec<Message>>>,
    /// Serializes `chat.json` writers so concurrent saves can never interleave.
    write_lock: Arc<tokio::sync::Mutex<()>>,
}

impl PersistingConversationMemory {
    /// Builds a persisting memory around a fresh in-memory backend
    ///
    /// # Arguments
    ///
    /// * `conversation_id` - The conversation id the history is keyed by
    /// * `store` - The session store to persist into, if persistence is on
    pub(crate) fn new(conversation_id: &str, store: Option<Arc<SessionStore>>) -> Self {
        PersistingConversationMemory {
            inner: InMemoryConversationMemory::new(),
            store,
            conversation_id: conversation_id.to_owned(),
            pending: Arc::new(Mutex::new(Vec::new())),
            write_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Records messages from the in-flight turn and persists them
    ///
    /// Called by the main agent's per-model-turn hook so `chat.json` updates
    /// after every message, not just once the whole turn settles. The settled
    /// append clears this buffer, so pending never duplicates settled history.
    ///
    /// # Arguments
    ///
    /// * `messages` - The in-flight turn's newest messages
    pub(crate) async fn record_pending(&self, messages: Vec<Message>) {
        // extend the pending buffer under its own short-lived lock
        {
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pending.extend(messages);
        }
        // mirror the updated state to disk
        self.persist().await;
    }

    /// Seeds the settled and pending halves of a restored conversation
    ///
    /// Unlike the `append` path (which settles and clears pending), this
    /// installs an interrupted turn's tail back into the pending buffer
    /// without settling it, so chat.json keeps the exact two-halves shape the
    /// dead run left -- a crash during the resume loses nothing, and the
    /// resume path settles the tail only once its dangling delegations have
    /// produced their real results.
    ///
    /// # Arguments
    ///
    /// * `settled` - The settled conversation history to seed
    /// * `pending` - The interrupted turn's tail to hold as pending
    pub(crate) async fn seed(
        &self,
        settled: Vec<Message>,
        pending: Vec<Message>,
    ) -> Result<(), MemoryError> {
        // seed the settled half into the backend under our conversation id
        self.inner.append(&self.conversation_id, settled).await?;
        // install the held tail under the pending buffer's own short lock
        {
            let mut buffer = self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *buffer = pending;
        }
        // mirror the seeded state to disk once
        self.persist().await;
        Ok(())
    }

    /// Drains the in-flight turn's captured messages for salvage
    ///
    /// Called by a failed turn's error arm: the drained tail is repaired and
    /// appended as settled history, and that append clears pending and
    /// persists exactly once -- so on disk the tail moves from pending to
    /// settled in a single durable step. This is deliberately drain-only:
    /// persisting the emptied buffer here would open a crash window where
    /// the tail is on disk as neither pending nor settled history. A caller
    /// that drops the drained tail instead of appending it must call
    /// [`Self::persist`] itself so disk and memory agree.
    pub(crate) async fn take_pending_tail(&self) -> Vec<Message> {
        // drain the buffer under its own short-lived lock
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *pending)
    }

    /// Rewrites `chat.json` from the current settled and pending history
    ///
    /// A no-op when no session store is configured. Failures are logged and
    /// swallowed so persistence can never fail a turn. Crate-visible so a
    /// salvage path that drops a drained tail (instead of appending it as
    /// settled history) can bring disk back in step with memory.
    pub(crate) async fn persist(&self) {
        // skip entirely when session persistence was not requested
        let Some(store) = self.store.as_ref() else {
            return;
        };
        // serialize writers so two saves can never interleave their reads
        let _guard = self.write_lock.lock().await;
        // read the settled history back out of the inner backend
        let messages = self
            .inner
            .load(&self.conversation_id)
            .await
            .unwrap_or_default();
        // snapshot the pending buffer under its own short-lived lock
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        // assemble and write the history, logging and swallowing any failure
        let chat = ChatHistory {
            version: CHAT_HISTORY_VERSION,
            conversation_id: self.conversation_id.clone(),
            messages,
            pending,
        };
        if let Err(err) = store.save_chat(&chat).await {
            tracing::warn!(?err, "failed to persist chat history");
        }
    }
}

/// Records the main agent's in-flight turn into the persisting memory
///
/// rig only appends to conversation memory once per settled run, so this hook
/// captures each model turn as it completes -- the turn's prompt message (the
/// user's text on turn one, tool results after) and the assistant content it
/// produced -- and records it as pending history, which rewrites `chat.json`.
/// The settled append then replaces the pending tail with the canonical
/// history, so nothing is ever duplicated.
pub(crate) struct MainHistoryRecorder {
    /// The persisting memory the in-flight turn is recorded into.
    pub memory: PersistingConversationMemory,
}

impl AgentHook for MainHistoryRecorder {
    /// Record this model turn's prompt and assistant content as pending history
    ///
    /// # Arguments
    ///
    /// * `_ctx` - The run-scoped hook context (unused)
    /// * `event` - The completion response, carrying this turn's prompt and content
    fn on_completion_response(
        &self,
        _ctx: &HookContext,
        event: CompletionResponse<'_>,
    ) -> impl std::future::Future<Output = ObservationAction> + Send {
        // clone the turn's prompt and content before the borrowed event ends
        let prompt = event.prompt.clone();
        let content = event.content.clone();
        // clone the memory handle into the future so it can record the turn
        let memory = self.memory.clone();
        async move {
            // the prompt message always lands; it is the turn's user-side half
            let mut messages = vec![prompt];
            // only record an assistant message when the turn produced content
            if !content.is_empty() {
                messages.push(Message::Assistant { id: None, content });
            }
            // record the turn, which rewrites chat.json with the new tail
            memory.record_pending(messages).await;
            // never steer the run, just observe it
            ObservationAction::Continue
        }
    }
}

impl ConversationMemory for PersistingConversationMemory {
    /// Load the full conversation history for `conversation_id`
    ///
    /// # Arguments
    ///
    /// * `conversation_id` - The conversation to load
    fn load<'a>(
        &'a self,
        conversation_id: &'a str,
    ) -> WasmBoxedFuture<'a, Result<Vec<Message>, MemoryError>> {
        // reads need no persistence, so delegate straight to the backend
        self.inner.load(conversation_id)
    }

    /// Append `messages` and mirror the settled history to `chat.json`
    ///
    /// # Arguments
    ///
    /// * `conversation_id` - The conversation to append to
    /// * `messages` - The settled turn's messages
    fn append<'a>(
        &'a self,
        conversation_id: &'a str,
        messages: Vec<Message>,
    ) -> WasmBoxedFuture<'a, Result<(), MemoryError>> {
        Box::pin(async move {
            // append to the backend first so the persisted read sees it
            self.inner.append(conversation_id, messages).await?;
            // the turn settled, so its pending tail is now part of history
            self.pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
            // mirror the settled state to disk
            self.persist().await;
            Ok(())
        })
    }

    /// Clear the stored history and mirror the empty state to `chat.json`
    ///
    /// # Arguments
    ///
    /// * `conversation_id` - The conversation to clear
    fn clear<'a>(
        &'a self,
        conversation_id: &'a str,
    ) -> WasmBoxedFuture<'a, Result<(), MemoryError>> {
        Box::pin(async move {
            // clear the backend first so the persisted read sees the empty state
            self.inner.clear(conversation_id).await?;
            // a cleared conversation has no in-flight turn either
            self.pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
            // mirror the cleared state to disk
            self.persist().await;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig::completion::message::{Text, ToolCall, ToolCallId, ToolFunction, ToolResult};

    /// Builds an assistant message carrying one tool call with the given id
    ///
    /// # Arguments
    ///
    /// * `id` - The tool call id to mint the call under
    /// * `name` - The called tool's name
    fn assistant_call(id: &str, name: &str) -> (Message, ToolCallId) {
        let call_id = ToolCallId::new_or_mint(id);
        let message = Message::Assistant {
            id: None,
            content: vec![AssistantContent::ToolCall(ToolCall {
                id: call_id.clone(),
                provider: None,
                function: ToolFunction {
                    name: name.to_owned(),
                    arguments: serde_json::json!({}),
                },
                signature: None,
                additional_params: None,
            })],
        };
        (message, call_id)
    }

    /// A dangling tool call gets a synthetic result and a closing note
    #[test]
    fn repair_answers_dangling_calls_and_closes_assistant_side() {
        // a turn that died right after issuing a tool call
        let (call_message, call_id) = assistant_call("call-1", "decomp_function");
        let tail = vec![
            Message::User {
                content: vec![UserContent::text("look at entry")],
            },
            call_message,
        ];
        let repaired =
            repair_interrupted_tail(tail, "boom").expect("assistant work should be kept");
        // the synthetic result answers the dangling call by its exact id
        let result = repaired
            .iter()
            .find_map(|message| match message {
                Message::User { content } => content.iter().find_map(|part| match part {
                    UserContent::ToolResult(ToolResult { call, name, .. }) if *call == call_id => {
                        Some(name.clone())
                    }
                    _ => None,
                }),
                _ => None,
            })
            .expect("the dangling call must be answered");
        assert_eq!(result, "decomp_function");
        // the tail ends assistant-side with the interruption note
        match repaired.last().expect("a closing message") {
            Message::Assistant { content, .. } => {
                let text = content
                    .iter()
                    .filter_map(|part| match part {
                        AssistantContent::Text(Text { text, .. }) => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>();
                assert!(text.contains("turn interrupted"));
                assert!(text.contains("boom"));
            }
            other => panic!("expected a closing assistant note, got {other:?}"),
        }
    }

    /// An already-answered call is not answered again
    #[test]
    fn repair_skips_answered_calls() {
        // a turn whose call completed before the next model call died
        let (call_message, call_id) = assistant_call("call-1", "list_funcs");
        let tail = vec![
            call_message,
            Message::User {
                content: vec![UserContent::tool_result_for(
                    call_id.clone(),
                    None,
                    "list_funcs",
                    vec![ToolResultContent::text("ok")],
                )],
            },
        ];
        let repaired = repair_interrupted_tail(tail, "boom").expect("kept");
        // exactly one result answers the call: the real one
        let answers = repaired
            .iter()
            .flat_map(|message| match message {
                Message::User { content } => content.as_slice(),
                _ => &[],
            })
            .filter(|part| {
                matches!(part, UserContent::ToolResult(ToolResult { call, .. }) if *call == call_id)
            })
            .count();
        assert_eq!(answers, 1, "the answered call must not be re-answered");
    }

    /// An assistant-ending tail folds the note in with no same-role adjacency
    #[test]
    fn repair_merges_note_into_trailing_assistant() {
        // a turn that died right after a text-only model response
        let tail = vec![
            Message::User {
                content: vec![UserContent::text("look at entry")],
            },
            Message::Assistant {
                id: None,
                content: vec![AssistantContent::text("started tracing the loader")],
            },
        ];
        let repaired =
            repair_interrupted_tail(tail, "boom").expect("assistant work should be kept");
        // no extra message may be appended; the note merges into the last one
        assert_eq!(repaired.len(), 2, "the note must merge, not append");
        // role alternation holds everywhere in the repaired tail
        for pair in repaired.windows(2) {
            assert!(
                !matches!(
                    (&pair[0], &pair[1]),
                    (Message::Assistant { .. }, Message::Assistant { .. })
                        | (Message::User { .. }, Message::User { .. })
                ),
                "the repaired tail must never hold same-role adjacency"
            );
        }
        // the note landed inside the trailing assistant message's content
        match repaired.last().expect("a closing message") {
            Message::Assistant { content, .. } => {
                let text = content
                    .iter()
                    .filter_map(|part| match part {
                        AssistantContent::Text(Text { text, .. }) => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>();
                assert!(text.contains("started tracing the loader"));
                assert!(text.contains("turn interrupted"));
                assert!(text.contains("boom"));
            }
            other => panic!("expected a trailing assistant message, got {other:?}"),
        }
    }

    /// A tail with no assistant content preserves nothing
    #[test]
    fn repair_drops_assistant_less_tails() {
        // only the user's half of a turn that never got a model response
        let tail = vec![Message::User {
            content: vec![UserContent::text("hello?")],
        }];
        assert!(repair_interrupted_tail(tail, "boom").is_none());
    }

    /// Draining the pending tail empties the buffer for the next turn
    #[tokio::test]
    async fn take_pending_tail_drains_once() {
        // a store-less memory still buffers pending messages
        let memory = PersistingConversationMemory::new("main", None);
        memory
            .record_pending(vec![Message::User {
                content: vec![UserContent::text("hi")],
            }])
            .await;
        // the first drain hands the tail over
        let tail = memory.take_pending_tail().await;
        assert_eq!(tail.len(), 1);
        // the second drain finds nothing left
        assert!(memory.take_pending_tail().await.is_empty());
    }

    /// The dangling scan returns unanswered calls in order with their args
    #[test]
    fn dangling_calls_scan_orders_and_carries_args() {
        // two calls issued; only the first was ever answered
        let (answered_message, answered_id) = assistant_call("call-1", "list_funcs");
        let (dangling_message, _) = assistant_call("call-2", "reverse_engineer");
        let tail = vec![
            answered_message,
            Message::User {
                content: vec![UserContent::tool_result_for(
                    answered_id,
                    None,
                    "list_funcs",
                    vec![ToolResultContent::text("ok")],
                )],
            },
            dangling_message,
        ];
        let dangling = dangling_calls(&tail);
        // only the unanswered call is reported, args intact
        assert_eq!(dangling.len(), 1);
        assert_eq!(dangling[0].tool, "reverse_engineer");
        assert_eq!(dangling[0].args, serde_json::json!({}));
    }

    /// Seeding installs both halves without settling the pending tail
    #[tokio::test]
    async fn seed_installs_settled_and_pending_halves() {
        // a store-less memory still seeds both halves in RAM
        let memory = PersistingConversationMemory::new("main", None);
        memory
            .seed(
                vec![Message::user("hello"), Message::assistant("hi")],
                vec![Message::user("continue")],
            )
            .await
            .expect("seed should succeed");
        // the settled half loads through the normal read path
        let settled = memory.load("main").await.expect("load");
        assert_eq!(settled.len(), 2);
        // the pending half stays held, un-settled, until drained
        let pending = memory.take_pending_tail().await;
        assert_eq!(pending.len(), 1);
    }
}
