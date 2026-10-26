//! Named side conversations forked from the main agent.
//!
//! A side conversation (the TUI's `/btw <name>`) starts as a copy of the main
//! agent's settled history and then grows its own turns, driven through the
//! *same built main agent* -- identical preamble, tools, and shared state --
//! using rig's per-run history override, so the main conversation's memory
//! (and its `chat.json`) is never touched. Each side records its token usage
//! under a `btw:<name>` ledger key, and sub-agents spawned during a side turn
//! mint scope-prefixed display names (see
//! [`crate::thorpal::agents::tool::ScopeMarker`]), so a side's whole token
//! spend attributes to it. When a side is done, [`summarize`] condenses its
//! transcript so the findings can be handed back to the main conversation.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use rig::completion::{Message, Prompt};
use tracing::instrument;

use super::events::ThorPalEvent;
use super::resume::render_messages;
use super::{DEFAULT_MAX_TOKENS, ThorPal};

/// The state of one named side conversation forked from the main agent.
pub(crate) struct SideState {
    /// The settled side history: the fork-time copy of the main conversation
    /// plus every settled side turn's new messages.
    pub messages: Vec<Message>,
    /// When this side conversation was forked from the main agent.
    pub created_at: DateTime<Utc>,
}

/// The map of live side conversations, keyed by their names.
///
/// Each side sits behind its own lock so turns within one side serialize while
/// different sides (and the main conversation) run concurrently.
pub(crate) type SideMap = tokio::sync::Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<SideState>>>>;

/// Builds the one-shot summary prompt over a side conversation's transcript
///
/// When a prior summary and analyst feedback are supplied, the prompt asks for
/// a revision of the prior summary instead of a fresh one.
///
/// # Arguments
///
/// * `transcript` - The side conversation rendered as a plain transcript
/// * `prior` - The previously generated summary being revised, if any
/// * `feedback` - The analyst's feedback steering the revision, if any
fn build_summary_prompt(transcript: &str, prior: Option<&str>, feedback: Option<&str>) -> String {
    // the base prompt asks for a concise, findings-first summary
    let mut prompt = format!(
        "You are summarizing a side conversation that was forked from a \
         running binary-analysis session. Write a concise summary of the \
         findings, decisions, and open questions from the side conversation \
         below, suitable to hand back to the lead analyst in the main \
         session. Lead with the concrete findings. Output only the summary \
         with no preamble or commentary.\n\n\
         <side_conversation>\n{transcript}\n</side_conversation>"
    );
    // a revision carries the prior summary and the analyst's feedback
    if let (Some(prior), Some(feedback)) = (prior, feedback) {
        prompt.push_str(&format!(
            "\n\n<previous_summary>\n{prior}\n</previous_summary>\n\n\
             <analyst_feedback>\n{feedback}\n</analyst_feedback>\n\n\
             Revise the previous summary according to the analyst's feedback. \
             Output only the revised summary."
        ));
    }
    prompt
}

/// Generates (or revises) a short summary of a side conversation
///
/// Runs a bare one-shot agent (no tools, no memory) over the side's rendered
/// transcript, recording its token usage under the side's `btw:<name>` ledger
/// key and mirroring a `UsageUpdated` event so the TUI's readouts attribute the
/// summarizer's spend to the side it summarized.
///
/// # Arguments
///
/// * `thorpal` - The ThorPal instance the side conversation belongs to
/// * `name` - The side conversation's name
/// * `messages` - The side conversation's settled history to summarize
/// * `prior` - The previously generated summary being revised, if any
/// * `feedback` - The analyst's feedback steering the revision, if any
#[instrument(name = "sides::summarize", skip_all, err(Debug))]
pub(crate) async fn summarize(
    thorpal: &ThorPal,
    name: &str,
    messages: &[Message],
    prior: Option<&str>,
    feedback: Option<&str>,
) -> Result<String, anyhow::Error> {
    // render the side's history as a plain transcript for the prompt
    let transcript = super::report::render_transcript(&render_messages(messages));
    // assemble the one-shot prompt, revising the prior summary when asked
    let prompt = build_summary_prompt(&transcript, prior, feedback);
    // the side's scoped ledger key its spend attributes under
    let agent_key = format!("btw:{name}");
    // a builder with no tools added yields an agent that can only reply with
    // text; build it through the retrying wrapper so a stray throttle paces
    // the summary instead of failing /done, and so it rides the pause gate
    let agent = thorpal
        .gate
        .retrying_agent(thorpal.provider.model(), &agent_key)
        .max_tokens(DEFAULT_MAX_TOKENS)
        .build();
    // one bare completion round trip writes the summary; extended details
    // carry the usage so the side can be billed for its own summarization
    let response = agent.prompt(prompt).extended_details().await?;
    // fold the summarizer's usage into the shared ledger under the side's key
    thorpal.usage.record(&agent_key, response.usage).await;
    // mirror the usage onto the event sink so live readouts attribute it too
    if let Some(tx) = &thorpal.events {
        let _ = tx.send(ThorPalEvent::UsageUpdated {
            agent: agent_key,
            input: response.usage.input_tokens,
            output: response.usage.output_tokens,
            cached: response.usage.cached_input_tokens,
            cache_creation: response.usage.cache_creation_input_tokens,
            total: response.usage.total_tokens,
        });
    }
    Ok(response.output)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh summary prompt carries the transcript but no revision blocks
    #[test]
    fn summary_prompt_without_feedback_is_fresh() {
        // build a prompt with no prior summary or feedback
        let prompt = build_summary_prompt("## User:\nhello", None, None);
        // the transcript is embedded and no revision blocks appear
        assert!(prompt.contains("<side_conversation>"));
        assert!(prompt.contains("hello"));
        assert!(!prompt.contains("<previous_summary>"));
        assert!(!prompt.contains("<analyst_feedback>"));
    }

    /// A revision prompt carries the prior summary and the analyst's feedback
    #[test]
    fn summary_prompt_with_feedback_revises() {
        // build a prompt revising a prior summary with feedback
        let prompt = build_summary_prompt(
            "## User:\nhello",
            Some("old summary"),
            Some("shorter, focus on the crypto"),
        );
        // both revision blocks are embedded alongside the transcript
        assert!(prompt.contains("<previous_summary>\nold summary"));
        assert!(prompt.contains("<analyst_feedback>\nshorter, focus on the crypto"));
    }
}
