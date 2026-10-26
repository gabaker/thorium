//! Death salvage for sub-agents: turn a budget death into a partial handoff.
//!
//! A sub-agent that exhausts its turn budget (or dies on output truncation or
//! a fatal provider error) used to hand its parent nothing but a redacted
//! "the tool failed". This module salvages what the dead agent actually had:
//! the partial transcript (from the error itself when it carries one, else
//! the [`super::budget::TurnBudgetState`] mirror), the agent's own last
//! substantial message as the handoff body -- or, when it never wrote one, a
//! one-shot LLM post-mortem over the rendered transcript -- plus any unmet
//! extension request, all framed as a `SUB-AGENT TERMINATED EARLY` report the
//! parent model can act on.

use rig::Agent;
use rig::completion::{Prompt, PromptError, Usage};
use rig::message::{AssistantContent, Message, ToolResultContent, UserContent};
use tracing::instrument;

use super::budget::{ExtensionRequest, ReservedExtension, TurnBudgetState};

/// The fewest characters a final assistant message needs to count as a handoff.
///
/// Shorter text is almost always filler ("let me check the next function...")
/// rather than a wrap-up, so anything under this falls through to the
/// post-mortem summarizer.
const MIN_MEANINGFUL_HANDOFF_CHARS: usize = 160;

/// The max characters of rendered transcript handed to the post-mortem.
///
/// The tail is kept (the newest work matters most to a handoff) so a long run
/// cannot blow the summarizer's own context window.
const MAX_POST_MORTEM_TRANSCRIPT_CHARS: usize = 60_000;

/// The max characters a rendered tool-call argument line keeps.
const TOOL_CALL_ARG_CHARS: usize = 200;

/// The max characters a rendered tool-result block keeps.
const TOOL_RESULT_CHARS: usize = 2_000;

/// The output-token cap for the one-shot post-mortem completion.
const POST_MORTEM_MAX_TOKENS: u64 = 2_048;

/// Renders the human-facing reason a sub-agent died
///
/// # Arguments
///
/// * `error` - The error that stopped the sub-agent
pub(crate) fn death_reason(error: &PromptError) -> String {
    match error {
        // the turn cap is the common death; name it plainly
        PromptError::MaxTurnsError { max_turns, .. } => {
            format!("turn budget exhausted ({max_turns} model calls used)")
        }
        // a cancellation carries its own human-readable reason
        PromptError::PromptCancelled { reason, .. } => format!("run cancelled: {reason}"),
        // everything else (incl. rig's output-truncation error) self-describes
        other => other.to_string(),
    }
}

/// Classifies a sub-agent death for the mechanical recount
///
/// The classes mirror the audit's death taxonomy: `max_turns` for turn-cap
/// deaths, `context` for context-window exhaustion (classified off the error
/// text, same markers as the main loop's salvage), `cancelled` for
/// cancellations, and `other` for everything else. Carried on
/// [`crate::ThorPalEvent::SubAgentTerminated`] and its transcript marker so
/// an audit counts deaths by grep instead of by prose.
///
/// # Arguments
///
/// * `error` - The error that stopped the sub-agent
pub(crate) fn death_class(error: &PromptError) -> &'static str {
    match error {
        // the turn cap and cancellations classify structurally
        PromptError::MaxTurnsError { .. } => "max_turns",
        PromptError::PromptCancelled { .. } => "cancelled",
        // completion errors split on the shared context-length markers
        PromptError::CompletionError(completion)
            if crate::thorpal::retry::is_context_length_error(completion) =>
        {
            "context"
        }
        _ => "other",
    }
}

/// Borrows the canonical chat history a prompt error carries, when it has one
///
/// The turn-cap, unknown-tool, and cancellation errors all carry the full
/// history at failure; a completion (or memory) error carries none. Every
/// salvage path shares this one arm list so the carrying errors can never
/// drift apart between them.
///
/// # Arguments
///
/// * `error` - The error that stopped the agent
fn carried_history(error: &PromptError) -> Option<&[Message]> {
    match error {
        // these errors carry the full canonical history at failure
        PromptError::MaxTurnsError { chat_history, .. } => Some(chat_history),
        PromptError::UnknownToolCall { chat_history, .. } => Some(chat_history),
        PromptError::PromptCancelled { chat_history, .. } => Some(chat_history),
        // completion and memory errors carry nothing
        _ => None,
    }
}

/// Salvages the dead sub-agent's partial transcript
///
/// The error's own history is canonical when it carries one (the turn-cap,
/// cancellation, and unknown-tool errors all do); a completion error carries
/// none, so the budget state's per-turn mirror stands in.
///
/// # Arguments
///
/// * `error` - The error that stopped the sub-agent
/// * `state` - The invocation's budget state, holding the mirrored history
pub(crate) fn salvaged_messages(error: &PromptError, state: &TurnBudgetState) -> Vec<Message> {
    match carried_history(error) {
        // the carried history is canonical when the error has one
        Some(history) => history.to_vec(),
        // completion errors carry nothing, so fall back to the hook's mirror
        None => state.transcript_snapshot(),
    }
}

/// Salvages the tail a failed turn added past an already-settled prefix
///
/// This is the side-conversation variant of [`salvaged_messages`]: the same
/// errors carry the canonical history, but a side turn runs no budget hook,
/// so an error carrying nothing salvages nothing (there is no mirror to fall
/// back on), and only the messages past the settled prefix are returned.
///
/// # Arguments
///
/// * `error` - The error that stopped the turn
/// * `settled` - How many leading messages were settled before the turn ran
pub(crate) fn salvaged_tail(error: &PromptError, settled: usize) -> Vec<Message> {
    match carried_history(error) {
        // keep only the messages the failed turn added past the settled prefix
        Some(history) => history.iter().skip(settled).cloned().collect(),
        // no carried history means nothing to salvage without a mirror
        None => Vec::new(),
    }
}

/// Finds the dead agent's own last substantial message, if it wrote one
///
/// Only the *newest* text-bearing assistant message may qualify (trailing
/// pure-tool-call turns are skipped): the wrap-up warning tells agents their
/// final message is their handoff, so when they complied this recovers it for
/// free. Older text is deliberately never promoted -- a 300-char turn-1 plan
/// ("I'll start by decompiling...") labeled as the agent's final report would
/// misinform the parent, so anything that is not a substantial last word falls
/// through to the post-mortem summarizer instead.
///
/// # Arguments
///
/// * `messages` - The salvaged partial transcript
pub(crate) fn last_meaningful_text(messages: &[Message]) -> Option<String> {
    // walk backwards to the newest assistant message that carries any text
    messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::Assistant { content, .. } => {
                // join the message's text parts, dropping tool calls and reasoning
                let text = content
                    .iter()
                    .filter_map(|part| match part {
                        AssistantContent::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                // keep scanning past assistant turns with no text at all
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    return None;
                }
                // the newest text either qualifies as the handoff or nothing does
                Some(
                    (trimmed.chars().count() >= MIN_MEANINGFUL_HANDOFF_CHARS)
                        .then(|| trimmed.to_string()),
                )
            }
            _ => None,
        })
        .flatten()
}

/// Whether a salvaged transcript contains any actual agent work to summarize
///
/// A transcript holding only the injected task prompt (an agent that died on
/// its very first model call) gives a post-mortem nothing to mine, and
/// prompting a summarizer for "concrete findings" over it invites fabricated
/// ones -- so the fallback is skipped unless some assistant turn or tool
/// result exists.
///
/// # Arguments
///
/// * `messages` - The salvaged partial transcript
pub(crate) fn has_agent_work(messages: &[Message]) -> bool {
    // any assistant turn or tool result means the agent actually did something
    messages.iter().any(|message| match message {
        Message::Assistant { .. } => true,
        Message::User { content } => content
            .iter()
            .any(|part| matches!(part, UserContent::ToolResult(_))),
        Message::System { .. } => false,
    })
}

/// Head-clips text to a character budget, marking any cut with an ellipsis
///
/// # Arguments
///
/// * `text` - The text to clip
/// * `max` - The max characters to keep
fn clip(text: &str, max: usize) -> String {
    // find the byte offset of the max-th character, if the text is that long
    match text.char_indices().nth(max) {
        // longer than the budget: cut on the boundary and mark it
        Some((offset, _)) => format!("{}…", &text[..offset]),
        // already fits: hand it back untouched
        None => text.to_string(),
    }
}

/// Keeps only the newest `max` characters of text, cutting on a char boundary
///
/// # Arguments
///
/// * `text` - The text to trim from the front
/// * `max` - The max characters to keep from the end
fn keep_tail(text: String, max: usize) -> String {
    // count once; most transcripts fit and return untouched
    let total = text.chars().count();
    if total <= max {
        return text;
    }
    // find the byte offset where the kept tail starts
    let skip = total - max;
    let offset = text
        .char_indices()
        .nth(skip)
        .map(|(offset, _)| offset)
        .unwrap_or(0);
    format!(
        "…(transcript trimmed to its last {max} characters)\n{}",
        &text[offset..]
    )
}

/// Renders a partial transcript for the post-mortem summarizer
///
/// Deliberately richer than the chat-bubble renderers: a tool-heavy sub-agent
/// keeps its evidence in tool calls and results, so those are rendered too
/// (clipped per item), and only the newest part of a long run is kept.
///
/// # Arguments
///
/// * `messages` - The salvaged partial transcript
pub(crate) fn render_partial_transcript(messages: &[Message]) -> String {
    // accumulate the rendered sections in conversation order
    let mut rendered = String::new();
    for message in messages {
        match message {
            // the system preamble is boilerplate the summarizer does not need
            Message::System { .. } => {}
            // user messages carry the prompt text and every tool result
            Message::User { content } => {
                for part in content {
                    match part {
                        UserContent::Text(text) => {
                            rendered.push_str(&format!("## User:\n{}\n\n", text.text));
                        }
                        UserContent::ToolResult(result) => {
                            // join the result's text/json items into one body
                            let body = result
                                .content
                                .iter()
                                .filter_map(|item| match item {
                                    ToolResultContent::Text(text) => Some(text.text.clone()),
                                    ToolResultContent::Json { value, .. } => {
                                        Some(value.to_string())
                                    }
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                            rendered.push_str(&format!(
                                "## Tool result ({}):\n{}\n\n",
                                result.name,
                                clip(&body, TOOL_RESULT_CHARS)
                            ));
                        }
                        // images, audio, and documents have no text to salvage
                        _ => {}
                    }
                }
            }
            // assistant messages carry the agent's text and its tool calls
            Message::Assistant { content, .. } => {
                for part in content {
                    match part {
                        AssistantContent::Text(text) => {
                            rendered.push_str(&format!("## Assistant:\n{}\n\n", text.text));
                        }
                        AssistantContent::ToolCall(call) => {
                            rendered.push_str(&format!(
                                "[tool call] {} {}\n\n",
                                call.function.name,
                                clip(&call.function.arguments.to_string(), TOOL_CALL_ARG_CHARS)
                            ));
                        }
                        // reasoning and images are not worth the budget
                        _ => {}
                    }
                }
            }
        }
    }
    // keep only the newest work when the rendering runs long
    keep_tail(rendered, MAX_POST_MORTEM_TRANSCRIPT_CHARS)
}

/// Builds the one-shot post-mortem prompt over a dead agent's transcript
///
/// # Arguments
///
/// * `goal` - The top-level goal the dead agent's delegation served
/// * `task` - The concrete task the dead agent was given
/// * `reason` - The human-facing reason the agent died
/// * `transcript` - The rendered partial transcript
fn build_post_mortem_prompt(goal: &str, task: &str, reason: &str, transcript: &str) -> String {
    format!(
        "A sub-agent analyzing a binary was terminated before it could report ({reason}). \
         Below is its partial transcript. Write the handoff it failed to write, for the \
         orchestrator that delegated the task: lead with the concrete findings (verbatim \
         constants, addresses, function names, IOCs), then what it was attempting when it \
         was terminated, then what remains to be done. Output only the handoff, with no \
         preamble or commentary.\n\n\
         <task>\nTop-level goal: {goal}\nTask: {task}\n</task>\n\n\
         <partial_transcript>\n{transcript}\n</partial_transcript>"
    )
}

/// Generates a post-mortem handoff over a dead agent's partial transcript
///
/// Runs the shared bare one-shot summarizer (no tools, a single model call,
/// a bounded output cap) and returns the handoff body plus the usage to bill
/// under the dead agent's name. `None` -- no summarizer installed, nothing to
/// summarize, or the summarizer itself failed -- degrades the handoff to its
/// static framing rather than ever failing the delegation.
///
/// # Arguments
///
/// * `agent` - The shared post-mortem summarizer agent, if one was built
/// * `goal` - The top-level goal the dead agent's delegation served
/// * `task` - The concrete task the dead agent was given
/// * `reason` - The human-facing reason the agent died
/// * `transcript` - The rendered partial transcript
#[instrument(name = "salvage::post_mortem", skip_all)]
pub(crate) async fn post_mortem(
    agent: Option<&Agent>,
    goal: &str,
    task: &str,
    reason: &str,
    transcript: &str,
) -> Option<(String, Usage)> {
    // no summarizer agent means the fallback is skipped entirely
    let agent = agent?;
    // an empty transcript has nothing worth a model call
    if transcript.trim().is_empty() {
        return None;
    }
    // assemble the one-shot prompt over the rendered transcript
    let prompt = build_post_mortem_prompt(goal, task, reason, transcript);
    // one bare completion round trip writes the handoff; a single turn and a
    // bounded output cap keep the salvage itself from dying the same way
    match agent
        .prompt(prompt)
        .max_turns(1)
        .max_tokens(POST_MORTEM_MAX_TOKENS)
        .extended_details()
        .await
    {
        // hand back the body plus the usage to bill to the dead agent
        Ok(response) => Some((response.output, response.usage)),
        // a failed post-mortem degrades the handoff, never the delegation
        Err(err) => {
            tracing::warn!(?err, "post-mortem summarizer failed; salvaging without it");
            None
        }
    }
}

/// Renders the parent-visible handoff for a dead sub-agent
///
/// This is the model-visible text the parent receives as the failed tool
/// result, so it frames the death, carries the salvaged body, surfaces any
/// unmet extension request, and tells the parent what to do next.
///
/// # Arguments
///
/// * `reason` - The human-facing reason the agent died
/// * `body` - The salvaged handoff body, if any was recovered
/// * `body_is_post_mortem` - Whether the body came from the summarizer
/// * `unmet` - The agent's last refused extension request, if any
/// * `reservation` - The agent's unapplied standing reservation, if any
pub(crate) fn render_handoff(
    reason: &str,
    body: Option<&str>,
    body_is_post_mortem: bool,
    unmet: Option<&ExtensionRequest>,
    reservation: Option<&ReservedExtension>,
) -> String {
    // open with the death framing so the parent never mistakes this for an answer
    let mut handoff = format!(
        "SUB-AGENT TERMINATED EARLY -- {reason}.\n\
         No final answer was produced; below is a salvaged partial handoff.\n\n"
    );
    // the body is the agent's own last report, a post-mortem, or an admission
    match body {
        Some(body) if body_is_post_mortem => {
            handoff.push_str(&format!(
                "## Post-mortem summary of its partial transcript\n{body}\n"
            ));
        }
        Some(body) => {
            handoff.push_str(&format!("## Last report from the agent\n{body}\n"));
        }
        None => {
            handoff.push_str("The agent died before producing any usable output.\n");
        }
    }
    // surface the unmet extension request so the orchestrator knows it asked
    if let Some(unmet) = unmet {
        handoff.push_str(&format!(
            "\n## Unmet extension request\n\
             The agent asked for {requested} more turn(s) beyond its allowance and was \
             refused: \"{justification}\"\n",
            requested = unmet.requested,
            justification = unmet.justification,
        ));
    }
    // surface a reservation the death outran (a non-cap death never converts
    // it), so the orchestrator still sees the agent predicted needing more
    if let Some(reservation) = reservation {
        handoff.push_str(&format!(
            "\n## Standing extension reservation (never applied)\n\
             The agent had reserved {requested} extra turn(s) for the cap, but died \
             before reaching it: \"{justification}\"\n",
            requested = reservation.requested,
            justification = reservation.justification,
        ));
    }
    // close with what the parent should actually do with partial work
    handoff.push_str(
        "\nTreat this as partial work: findings it posted to the forum before dying are \
         already there -- verify them as usual. If the remaining work matters, re-delegate \
         it as a narrower, better-scoped task (and consider a larger turn budget for it).",
    );
    handoff
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rig::message::{Text, ToolCall, ToolCallId, ToolFunction, ToolResult};

    /// Build an assistant message carrying just text
    ///
    /// # Arguments
    ///
    /// * `text` - The text the message carries
    fn assistant(text: &str) -> Message {
        Message::Assistant {
            id: None,
            content: vec![AssistantContent::Text(Text::from(text.to_string()))],
        }
    }

    /// A long filler string comfortably over the meaningful-handoff floor
    fn long_text() -> String {
        "The decryption routine at 0x401200 uses a rolling XOR with key 0x5A seeded from \
         the PE timestamp; the C2 list decodes to three domains and the mutex is \
         Global\\mx-7f3a. Remaining: confirm the persistence writer."
            .to_string()
    }

    /// The last substantial assistant message wins; filler never counts.
    #[test]
    fn last_meaningful_text_picks_the_newest_long_message() {
        // an empty transcript has nothing to salvage
        assert!(last_meaningful_text(&[]).is_none());
        // short filler and tool-only turns never count as a handoff
        let short = vec![assistant("checking the next function")];
        assert!(last_meaningful_text(&short).is_none());
        // the newest long message wins over an older one
        let messages = vec![
            assistant(&format!("OLD {}", long_text())),
            Message::user("tool results"),
            assistant(&format!("NEW {}", long_text())),
        ];
        let found = last_meaningful_text(&messages).expect("the long text should count");
        assert!(found.starts_with("NEW"));
        // stale long narration is never promoted over a short newest message
        let stale = vec![
            assistant(&format!("PLAN {}", long_text())),
            Message::user("tool results"),
            assistant("checking one more function"),
        ];
        assert!(last_meaningful_text(&stale).is_none());
        // a trailing pure-tool-call turn does not hide the real handoff
        let trailing_tool_call = vec![
            assistant(&format!("HANDOFF {}", long_text())),
            Message::Assistant {
                id: None,
                content: vec![AssistantContent::ToolCall(ToolCall {
                    id: ToolCallId::new_or_mint("call_9"),
                    provider: None,
                    function: ToolFunction {
                        name: "create_post".to_string(),
                        arguments: serde_json::json!({}),
                    },
                    signature: None,
                    additional_params: None,
                })],
            },
        ];
        let found = last_meaningful_text(&trailing_tool_call).expect("the handoff should be found");
        assert!(found.starts_with("HANDOFF"));
    }

    /// Only transcripts with assistant turns or tool results count as work.
    #[test]
    fn has_agent_work_requires_actual_work() {
        // the injected task prompt alone is not work worth a post-mortem
        let prompt_only = vec![Message::user("Top-level goal: understand the sample")];
        assert!(!has_agent_work(&prompt_only));
        // an empty transcript is certainly not work
        assert!(!has_agent_work(&[]));
        // any assistant turn counts as work
        let with_turn = vec![Message::user("task"), assistant("looking")];
        assert!(has_agent_work(&with_turn));
        // a tool result counts as work even with no assistant text mirrored
        let with_result = vec![Message::User {
            content: vec![UserContent::ToolResult(ToolResult {
                call: ToolCallId::new_or_mint("call_1"),
                provider: None,
                name: "list_funcs".to_string(),
                content: vec![ToolResultContent::Text(Text::from("fn list".to_string()))],
            })],
        }];
        assert!(has_agent_work(&with_result));
    }

    /// The transcript rendering keeps tool evidence and respects the tail cap.
    #[test]
    fn render_partial_transcript_keeps_tool_evidence() {
        let messages = vec![
            Message::user("Top-level goal: understand the sample"),
            Message::Assistant {
                id: None,
                content: vec![AssistantContent::ToolCall(ToolCall {
                    id: ToolCallId::new_or_mint("call_1"),
                    provider: None,
                    function: ToolFunction {
                        name: "decomp_function".to_string(),
                        arguments: serde_json::json!({"target": "0x401200"}),
                    },
                    signature: None,
                    additional_params: None,
                })],
            },
            Message::User {
                content: vec![UserContent::ToolResult(ToolResult {
                    call: ToolCallId::new_or_mint("call_1"),
                    provider: None,
                    name: "decomp_function".to_string(),
                    content: vec![ToolResultContent::Text(Text::from(
                        "void decrypt(char *buf) { /* xor 0x5A */ }".to_string(),
                    ))],
                })],
            },
        ];
        let rendered = render_partial_transcript(&messages);
        // the call, its args, and its result all survive into the rendering
        assert!(rendered.contains("[tool call] decomp_function"));
        assert!(rendered.contains("0x401200"));
        assert!(rendered.contains("## Tool result (decomp_function):"));
        assert!(rendered.contains("xor 0x5A"));
        // a huge rendering keeps only its newest characters
        let huge = vec![assistant(&"x".repeat(MAX_POST_MORTEM_TRANSCRIPT_CHARS * 2))];
        let rendered = render_partial_transcript(&huge);
        assert!(rendered.starts_with("…(transcript trimmed"));
        assert!(rendered.chars().count() <= MAX_POST_MORTEM_TRANSCRIPT_CHARS + 100);
    }

    /// The handoff frames the death and carries the body and unmet request.
    #[test]
    fn render_handoff_frames_the_death() {
        // a body straight from the agent is labeled as its own report
        let handoff = render_handoff(
            "turn budget exhausted (10 model calls used)",
            Some("findings..."),
            false,
            None,
            None,
        );
        assert!(handoff.starts_with("SUB-AGENT TERMINATED EARLY"));
        assert!(handoff.contains("## Last report from the agent"));
        assert!(handoff.contains("findings..."));
        assert!(!handoff.contains("Unmet extension request"));
        // a post-mortem body is labeled as a summary instead
        let handoff = render_handoff("reason", Some("summary..."), true, None, None);
        assert!(handoff.contains("## Post-mortem summary"));
        // no body at all is admitted plainly
        let handoff = render_handoff("reason", None, false, None, None);
        assert!(handoff.contains("died before producing any usable output"));
        // an unmet request surfaces with its justification
        let unmet = ExtensionRequest {
            requested: 5,
            granted: 0,
            justification: "one constant left to verify".to_string(),
            at: Utc::now(),
        };
        let handoff = render_handoff("reason", None, false, Some(&unmet), None);
        assert!(handoff.contains("## Unmet extension request"));
        assert!(handoff.contains("5 more turn(s)"));
        assert!(handoff.contains("one constant left to verify"));
    }

    /// Death reasons name the cap plainly and pass other errors through.
    #[test]
    fn death_reasons_name_the_cap() {
        // the turn-cap death is named with its budget
        let err = PromptError::MaxTurnsError {
            max_turns: 10,
            chat_history: Box::new(vec![Message::user("task")]),
            prompt: Box::new(Message::user("pending")),
        };
        assert_eq!(
            death_reason(&err),
            "turn budget exhausted (10 model calls used)"
        );
        // the carried history is what salvage prefers over the mirror
        let state = TurnBudgetState::new(
            "ReverseEngineer(Io)",
            super::super::budget::TurnBudget {
                max_turns: 10,
                max_extension_turns: 2,
                warn_turns: 2,
                context_window: 0,
            },
            None,
        );
        assert_eq!(salvaged_messages(&err, &state).len(), 1);
    }

    /// The side-salvage tail skips the settled prefix and ignores bare errors
    #[test]
    fn salvaged_tail_skips_settled_and_ignores_bare_errors() {
        // a carried history yields only the messages past the settled prefix
        let err = PromptError::MaxTurnsError {
            max_turns: 10,
            chat_history: Box::new(vec![
                Message::user("settled"),
                Message::user("new prompt"),
                assistant("partial work"),
            ]),
            prompt: Box::new(Message::user("pending")),
        };
        assert_eq!(salvaged_tail(&err, 1).len(), 2);
        // a prefix covering the whole history leaves nothing to salvage
        assert!(salvaged_tail(&err, 3).is_empty());
        // an error carrying no history salvages nothing, even with no prefix
        let bare = PromptError::CompletionError(rig::completion::CompletionError::ProviderError(
            "provider exploded".to_owned(),
        ));
        assert!(salvaged_tail(&bare, 0).is_empty());
    }
}
