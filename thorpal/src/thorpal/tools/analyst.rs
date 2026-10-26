//! Tools for the agent to interact with the human analyst.
//!
//! Two tools live here. `ask_analyst` poses one structured multiple-choice
//! question: with a live analyst UI attached (thorctl's chat TUI) the tool
//! future parks on the [`AnalystLink`] until the analyst answers; with none
//! attached (headless, batch, and blinded evaluation runs) it never waits --
//! the recommended default is taken, the question is recorded on the forum,
//! and a "system" review flag is auto-filed so the fork stays on the analyst's
//! queue. `flag_for_review` is the non-blocking half: it marks a finding,
//! conclusion, or area as needing human review without ever pausing the run.
//! Neither recorded asks nor review flags gate the `finish` tool.

use std::collections::HashSet;

use rig::tool::{ToolContext, ToolExecutionError};
use serde::Serialize;

use crate::thorpal::analyst::{AnalystAnswer, AnalystLink, PendingAsk};
use crate::thorpal::tools::forum::{
    AnalystAsk, AnswerSource, AskOption, AskRequest, ForumState, ParentRef, ReviewFlag,
};

/// The author attributed to review flags auto-filed for unanswered asks
const SYSTEM_AUTHOR: &str = "system";

/// The fewest options one ask may offer: a single option is not a question
const MIN_ASK_OPTIONS: usize = 2;

/// The most options one ask may offer, so the modal and record stay scannable
const MAX_ASK_OPTIONS: usize = 4;

/// The length an auto-filed flag's question excerpt is truncated to
const FLAG_TITLE_QUESTION_CHARS: usize = 80;

/// The model-visible result of an `ask_analyst` call
#[derive(Debug, Clone, Serialize)]
pub struct AskAnalystResult {
    /// The answer: a chosen option label, or the analyst's free text
    pub chosen: String,
    /// Who answered: "analyst", "analyst_other", "auto_default", or "declined"
    pub source: String,
    /// A follow-up instruction the model can act on directly
    pub message: String,
}

/// Truncates text to at most `max` characters, appending an ellipsis when cut
///
/// # Arguments
///
/// * `text` - The text to truncate
/// * `max` - The most characters to keep
fn truncate(text: &str, max: usize) -> String {
    // count chars so a cut never lands inside a UTF-8 sequence
    if text.chars().count() <= max {
        return text.to_owned();
    }
    // keep the leading excerpt and mark the cut
    let cut: String = text.chars().take(max).collect();
    format!("{cut}…")
}

/// Validates and canonicalizes one `ask_analyst` call's arguments
///
/// Trims every field, bounds the option count, rejects empty or duplicate
/// labels, and canonicalizes `recommended` to the matched option's exact
/// spelling so records and renders stay consistent.
///
/// # Arguments
///
/// * `author` - The display name of the asking agent
/// * `question` - The question to pose to the analyst
/// * `options` - The options offered, in display order
/// * `recommended` - The label of the designated default option
fn validate_ask(
    author: String,
    question: String,
    options: Vec<AskOption>,
    recommended: String,
) -> Result<AskRequest, ToolExecutionError> {
    // an empty question cannot be answered meaningfully, reject it loudly
    let question = question.trim().to_owned();
    if question.is_empty() {
        return Err(ToolExecutionError::invalid_args(
            "`question` must not be empty".to_owned(),
        ));
    }
    // bound the option count so the modal and the record stay scannable
    if options.len() < MIN_ASK_OPTIONS || options.len() > MAX_ASK_OPTIONS {
        return Err(ToolExecutionError::invalid_args(format!(
            "`options` must offer between {MIN_ASK_OPTIONS} and {MAX_ASK_OPTIONS} choices; \
             got {}",
            options.len()
        )));
    }
    // trim each option and reject empty or (case-insensitively) duplicate labels
    let mut seen = HashSet::new();
    let mut cleaned = Vec::with_capacity(options.len());
    for option in options {
        let label = option.label.trim().to_owned();
        if label.is_empty() {
            return Err(ToolExecutionError::invalid_args(
                "every option needs a non-empty `label`".to_owned(),
            ));
        }
        if !seen.insert(label.to_ascii_lowercase()) {
            return Err(ToolExecutionError::invalid_args(format!(
                "option labels must be unique; {label:?} repeats"
            )));
        }
        cleaned.push(AskOption {
            label,
            description: option.description.trim().to_owned(),
        });
    }
    // the recommended default must name one of the offered options; keep the
    // option's exact spelling so the record and every render agree
    let wanted = recommended.trim();
    let Some(matched) = cleaned
        .iter()
        .find(|option| option.label.eq_ignore_ascii_case(wanted))
    else {
        return Err(ToolExecutionError::invalid_args(format!(
            "`recommended` ({wanted:?}) must match one of the option labels"
        )));
    };
    let recommended = matched.label.clone();
    // bundle the validated payload for resolution
    Ok(AskRequest {
        asked_by: author,
        question,
        options: cleaned,
        recommended,
    })
}

/// Builds the model-visible result for a recorded ask
///
/// # Arguments
///
/// * `record` - The recorded ask the result describes
/// * `flagged` - Whether the default paths' auto-flag was actually filed
///   (ignored for the analyst-answered sources, which never auto-flag)
fn result_for(record: &AnalystAsk, flagged: bool) -> AskAnalystResult {
    // the default paths' follow-up names the review flag only when it was
    // really filed, so the model and the review queue can never disagree
    let recorded = if flagged {
        "The question was recorded and flagged for analyst review"
    } else {
        "The question was recorded (the review-flag budget was already spent)"
    };
    // phrase the follow-up instruction for how the answer was produced
    let message = match record.source {
        AnswerSource::Analyst => format!(
            "The analyst chose {:?}. Proceed down that path and do not re-ask this question.",
            record.chosen
        ),
        AnswerSource::AnalystOther => format!(
            "The analyst answered in their own words: {:?}. Proceed accordingly and do not \
             re-ask this question.",
            record.chosen
        ),
        AnswerSource::AutoDefault => format!(
            "No analyst is attached, so the recommended default {:?} was taken. {recorded}; \
             proceed without waiting and do not re-ask.",
            record.chosen
        ),
        AnswerSource::Declined => format!(
            "The analyst declined to answer, so the recommended default {:?} was taken. \
             {recorded}; proceed and do not re-ask.",
            record.chosen
        ),
    };
    AskAnalystResult {
        chosen: record.chosen.clone(),
        source: record.source.wire().to_owned(),
        message,
    }
}

/// Resolves an ask on its default path: record the recommended option and flag
///
/// Shared by the headless path (no analyst attached) and the declined path
/// (Esc or TUI teardown): the recommended default is recorded as the answer and
/// a "system" review flag is auto-filed so the unanswered fork stays on the
/// analyst's queue.
///
/// # Arguments
///
/// * `forum` - The shared forum to record the ask and flag on
/// * `ask` - The validated ask payload being resolved
/// * `source` - Why the default was taken (auto-default or declined)
/// * `ticket` - The live ask ticket, or 0 when no analyst UI was attached
async fn take_default(
    forum: &ForumState,
    ask: AskRequest,
    source: AnswerSource,
    ticket: u64,
) -> Result<AskAnalystResult, ToolExecutionError> {
    // the recommended option is the default path
    let chosen = ask.recommended.clone();
    let record = forum.record_ask(ask, chosen, source, ticket).await?;
    // auto-file the review flag so the unanswered fork stays on the queue
    let why = match source {
        AnswerSource::Declined => "the analyst declined to answer",
        _ => "no analyst was attached",
    };
    let title = format!(
        "Unanswered analyst question: {}",
        truncate(&record.question, FLAG_TITLE_QUESTION_CHARS)
    );
    let reason = format!(
        "ask_analyst took the recommended default {:?} because {why}; revisit this decision.",
        record.chosen
    );
    // a failed auto-flag (e.g. the per-run flag cap) must not fail the ask,
    // but the result must not claim a flag that was never filed either
    let flagged = match forum
        .add_review_flag(
            SYSTEM_AUTHOR.to_owned(),
            title,
            reason,
            None,
            Some(record.id),
        )
        .await
    {
        Ok(_) => true,
        Err(err) => {
            tracing::warn!(
                ?err,
                "failed to auto-file the review flag for an unanswered ask"
            );
            false
        }
    };
    Ok(result_for(&record, flagged))
}

/// Resolves one validated ask through the analyst link
///
/// With a live analyst attached the ask is announced and the tool future parks
/// on its answer channel; with none attached (or once the analyst declines) the
/// recommended default is taken and recorded. Either way exactly one
/// [`AnalystAsk`] is recorded, and only unanswered asks auto-file a flag.
///
/// # Arguments
///
/// * `forum` - The shared forum to record the resolved ask on
/// * `link` - The analyst link to announce and wait through
/// * `ask` - The validated ask payload to resolve
pub(crate) async fn resolve_ask(
    forum: &ForumState,
    link: &AnalystLink,
    ask: AskRequest,
) -> Result<AskAnalystResult, ToolExecutionError> {
    // refuse before announcing so a live analyst is never prompted for an ask
    // that could not be recorded anyway
    forum.ensure_ask_capacity().await?;
    // park an answer channel atomically with any racing detach: None means no
    // analyst is attached (or a quit just detached), so the default path runs
    // immediately instead of ever waiting
    let Some((ticket, rx)) = link.register() else {
        return take_default(forum, ask, AnswerSource::AutoDefault, 0).await;
    };
    // announce the ask to the attached UI
    link.announce(PendingAsk {
        id: ticket,
        agent: ask.asked_by.clone(),
        question: ask.question.clone(),
        options: ask.options.clone(),
        recommended: ask.recommended.clone(),
    });
    // wait for the analyst; a closed channel (teardown race) counts as declined
    let answer = rx.await.unwrap_or(AnalystAnswer::Declined);
    match answer {
        // an offered option was picked: record it as the analyst's decision
        AnalystAnswer::Picked { label } => {
            // canonicalize to the option's exact spelling when it matches one
            let chosen = ask
                .options
                .iter()
                .find(|option| option.label.eq_ignore_ascii_case(label.trim()))
                .map(|option| option.label.clone())
                .unwrap_or(label);
            let record = forum
                .record_ask(ask, chosen, AnswerSource::Analyst, ticket)
                .await?;
            Ok(result_for(&record, false))
        }
        // a free-text answer is recorded verbatim
        AnalystAnswer::Other { text } => {
            let record = forum
                .record_ask(ask, text, AnswerSource::AnalystOther, ticket)
                .await?;
            Ok(result_for(&record, false))
        }
        // declined (Esc or teardown): same path as headless, marked declined
        AnalystAnswer::Declined => take_default(forum, ask, AnswerSource::Declined, ticket).await,
    }
}

#[rig::tool_macro(
    description = "Asks the human analyst ONE structured multiple-choice question and waits for their answer. Use this rarely: only for a genuine fork a human must decide -- scope or priority choices, environment-specific judgment (e.g. whether an artifact is expected in their environment), or whether to pursue an expensive avenue -- never for anything evidence or your own tools can settle. Provide the question, 2-4 options (each with a short unique label and a description of what choosing it means), and recommended: the label of the option to take by default. When no analyst is attached (headless and batch runs) this does NOT wait: it immediately returns the recommended default, records the question for later review, and files a needs-review flag -- so never call it expecting to pause an unattended run. The result names the chosen answer and its source (analyst, analyst_other, auto_default, or declined); proceed down the chosen path and never re-ask an answered question. At most 10 asks per run."
)]
pub async fn ask_analyst(
    #[rig(context)] context: &mut ToolContext,
    author: String,
    question: String,
    options: Vec<AskOption>,
    recommended: String,
) -> Result<AskAnalystResult, ToolExecutionError> {
    // recover this run's shared forum and analyst link
    let forum = ForumState::from_context(context)?;
    let link = AnalystLink::from_context(context)?;
    // validate and canonicalize the ask before touching anything shared
    let ask = validate_ask(author, question, options, recommended)?;
    // resolve it: wait on a live analyst, or default-and-record unattended
    resolve_ask(&forum, &link, ask).await
}

#[rig::tool_macro(
    description = "Flags a finding, conclusion, or area as needing review by the human analyst. Non-blocking: it never pauses the run and never prevents finish. Use it for low-confidence conclusions worth human eyes, judgment calls you made alone, and anomalies you could not fully explain. Provide a short title, the reason human review is needed, and optionally reference: the forum item it concerns (\"#post-name\" or \"!123\"). Flags land in the session's review queue, the forum dump, and the report appendix's \"Needs analyst review\" section. At most 40 flags per run -- fold related concerns into one flag."
)]
pub async fn flag_for_review(
    #[rig(context)] context: &mut ToolContext,
    author: String,
    title: String,
    reason: String,
    reference: Option<String>,
) -> Result<ReviewFlag, ToolExecutionError> {
    // recover this run's shared forum
    let forum = ForumState::from_context(context)?;
    // parse the optional forum reference, treating blank strings as absent
    let reference = reference
        .as_deref()
        .map(str::trim)
        .filter(|reference| !reference.is_empty())
        .map(ParentRef::parse)
        .transpose()?;
    // raise the flag on the forum (validates the reference exists and the cap)
    forum
        .add_review_flag(author, title, reason, reference, None)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thorpal::events::ThorPalEvent;

    /// Builds the 2-option payload the tests ask with
    fn sample_options() -> Vec<AskOption> {
        vec![
            AskOption {
                label: "static".to_owned(),
                description: "prioritize static indicators".to_owned(),
            },
            AskOption {
                label: "dynamic".to_owned(),
                description: "prioritize dynamic indicators".to_owned(),
            },
        ]
    }

    /// Invalid payloads are rejected and valid ones are canonicalized
    #[test]
    fn validate_ask_rejects_bad_payloads_and_canonicalizes_good_ones() {
        // an empty question is rejected
        assert!(
            validate_ask(
                "main".to_owned(),
                "  ".to_owned(),
                sample_options(),
                "static".to_owned()
            )
            .is_err()
        );
        // too few and too many options are rejected
        assert!(
            validate_ask(
                "main".to_owned(),
                "which path?".to_owned(),
                sample_options()[..1].to_vec(),
                "static".to_owned()
            )
            .is_err()
        );
        let mut too_many = sample_options();
        too_many.extend((0..3).map(|index| AskOption {
            label: format!("extra-{index}"),
            description: String::new(),
        }));
        assert!(
            validate_ask(
                "main".to_owned(),
                "which path?".to_owned(),
                too_many,
                "static".to_owned()
            )
            .is_err()
        );
        // duplicate labels (case-insensitively) are rejected
        let mut duped = sample_options();
        duped[1].label = "Static".to_owned();
        assert!(
            validate_ask(
                "main".to_owned(),
                "which path?".to_owned(),
                duped,
                "static".to_owned()
            )
            .is_err()
        );
        // a recommended label that matches no option is rejected
        assert!(
            validate_ask(
                "main".to_owned(),
                "which path?".to_owned(),
                sample_options(),
                "neither".to_owned()
            )
            .is_err()
        );
        // a good payload trims fields and canonicalizes the recommended spelling
        let ask = validate_ask(
            "main".to_owned(),
            " which path? ".to_owned(),
            sample_options(),
            " STATIC ".to_owned(),
        )
        .expect("a valid ask was rejected");
        assert_eq!(ask.question, "which path?");
        assert_eq!(ask.recommended, "static");
    }

    /// With no analyst attached the ask auto-defaults, records, and auto-flags
    #[tokio::test]
    async fn headless_asks_default_record_and_flag() {
        let forum = ForumState::new();
        let link = AnalystLink::new();
        // resolve an ask with no analyst attached
        let ask = validate_ask(
            "main".to_owned(),
            "which path?".to_owned(),
            sample_options(),
            "static".to_owned(),
        )
        .expect("a valid ask was rejected");
        let result = resolve_ask(&forum, &link, ask)
            .await
            .expect("the headless ask failed");
        // the recommended default was taken without waiting
        assert_eq!(result.chosen, "static");
        assert_eq!(result.source, "auto_default");
        // exactly one ask was recorded with the auto-default source
        let asks = forum.asks_snapshot().await;
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].chosen, "static");
        assert_eq!(asks[0].source, AnswerSource::AutoDefault);
        // and a system review flag was auto-filed pointing back at the ask
        let flags = forum.flags_snapshot().await;
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].author, SYSTEM_AUTHOR);
        assert_eq!(flags[0].ask, Some(asks[0].id));
    }

    /// A live analyst's picked option is recorded without an auto-flag
    #[tokio::test]
    async fn interactive_asks_record_the_picked_option() {
        let forum = ForumState::new();
        let link = AnalystLink::new();
        link.attach();
        // watch the event stream for the pending-ask announcement
        let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
        link.set_events(tx);
        let ask = validate_ask(
            "main".to_owned(),
            "which path?".to_owned(),
            sample_options(),
            "static".to_owned(),
        )
        .expect("a valid ask was rejected");
        // answer from "the TUI" once the pending announcement arrives
        let resolve = resolve_ask(&forum, &link, ask);
        let answer = async {
            match events.recv().await {
                Some(ThorPalEvent::AnalystAskPending { id, .. }) => {
                    // pick the non-default option, in a different casing
                    assert!(link.answer(
                        id,
                        AnalystAnswer::Picked {
                            label: "DYNAMIC".to_owned()
                        }
                    ));
                }
                other => panic!("expected a pending-ask event, got {other:?}"),
            }
        };
        let (result, ()) = tokio::join!(resolve, answer);
        let result = result.expect("the interactive ask failed");
        // the pick was canonicalized to the option's exact spelling
        assert_eq!(result.chosen, "dynamic");
        assert_eq!(result.source, "analyst");
        // the record carries the analyst source and no flag was auto-filed
        let asks = forum.asks_snapshot().await;
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].source, AnswerSource::Analyst);
        assert!(forum.flags_snapshot().await.is_empty());
    }

    /// A declined ask takes the default and auto-files a review flag
    #[tokio::test]
    async fn declined_asks_default_and_flag() {
        let forum = ForumState::new();
        let link = AnalystLink::new();
        link.attach();
        let (tx, mut events) = tokio::sync::mpsc::unbounded_channel();
        link.set_events(tx);
        let ask = validate_ask(
            "main".to_owned(),
            "which path?".to_owned(),
            sample_options(),
            "static".to_owned(),
        )
        .expect("a valid ask was rejected");
        // decline from "the TUI" once the pending announcement arrives
        let resolve = resolve_ask(&forum, &link, ask);
        let answer = async {
            match events.recv().await {
                Some(ThorPalEvent::AnalystAskPending { id, .. }) => {
                    assert!(link.answer(id, AnalystAnswer::Declined));
                }
                other => panic!("expected a pending-ask event, got {other:?}"),
            }
        };
        let (result, ()) = tokio::join!(resolve, answer);
        let result = result.expect("the declined ask failed");
        // the default was taken and marked declined, with the auto-flag filed
        assert_eq!(result.chosen, "static");
        assert_eq!(result.source, "declined");
        assert_eq!(forum.asks_snapshot().await.len(), 1);
        assert_eq!(forum.flags_snapshot().await.len(), 1);
    }

    /// The per-run ask cap refuses further asks with a usable fallback hint
    #[tokio::test]
    async fn ask_cap_refuses_past_the_limit() {
        let forum = ForumState::new();
        let link = AnalystLink::new();
        // spend the whole ask budget on the headless path
        for index in 0..10 {
            let ask = validate_ask(
                "main".to_owned(),
                format!("question {index}?"),
                sample_options(),
                "static".to_owned(),
            )
            .expect("a valid ask was rejected");
            resolve_ask(&forum, &link, ask)
                .await
                .expect("an in-budget ask failed");
        }
        // the next ask is refused and points at flag_for_review instead
        let ask = validate_ask(
            "main".to_owned(),
            "one too many?".to_owned(),
            sample_options(),
            "static".to_owned(),
        )
        .expect("a valid ask was rejected");
        let err = resolve_ask(&forum, &link, ask)
            .await
            .expect_err("the over-cap ask was not refused");
        assert!(err.to_string().contains("flag_for_review"));
    }
}
