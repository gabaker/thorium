//! A Reddit-like shared forum for agents to post findings, discuss them, and
//! attach evidence via threaded comments.
//!
//! Posts are identified by a unique, human-readable `name` (no spaces) and
//! comments by a numeric id. Both are referenced with GitLab-style syntax:
//! `#post-name` for a post, `!123` for a comment. This syntax is used both in
//! free text and as the literal string format for the `parent`/`reference`
//! tool arguments below, so agents only need to learn one convention.
//!
//! The forum's mutable data lives in a single [`ForumState`], shared as an
//! `Arc` across all of one ThorPal instance's agents through the run's
//! [`ToolContext`]. Each `#[rig::tool_macro]` tool below is a thin wrapper that
//! pulls that `Arc<ForumState>` out of the context and delegates to the matching
//! [`ForumState`] method, so two ThorPal instances never share forum data.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use rig::tool::{ToolContext, ToolExecutionError};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};
use tracing::instrument;

use crate::thorpal::events::{ThorPalEvent, ThorPalEventSender};
use crate::thorpal::resume::ForumSnapshot;
use crate::thorpal::seed::SEED_AUTHOR;
use crate::thorpal::store::SessionStore;
use crate::thorpal::store::library::ScriptLibrary;
use crate::thorpal::store::memories::MemoryLibrary;
use crate::thorpal::tools::constants::{ConstantTracker, MAX_FINISH_CONSTANTS};

/// BM25 lexical search over the forum (backs `search_forum` and the TUI)
mod search;

pub(crate) use search::ForumTokenizer;
pub use search::{SearchHit, match_ranges, search_terms};

/// How a comment's evidence relates to whatever it is replying to
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stance {
    /// This comment's evidence supports the claim it is replying to
    Supports,
    /// This comment's evidence disproves the claim it is replying to
    Disproves,
    /// This comment neither supports nor disproves the claim it is replying to
    Neutral,
}

impl Stance {
    /// Parses a stance from a case-insensitive string
    ///
    /// # Arguments
    ///
    /// * `raw` - The raw stance string to parse (e.g. "supports")
    fn parse(raw: &str) -> Result<Self, ToolExecutionError> {
        // match on the lowercased stance so agents can use any casing
        match raw.trim().to_ascii_lowercase().as_str() {
            "supports" => Ok(Stance::Supports),
            "disproves" => Ok(Stance::Disproves),
            "neutral" => Ok(Stance::Neutral),
            _ => Err(ToolExecutionError::invalid_args(format!(
                "Invalid stance {raw:?}; expected one of \"supports\", \"disproves\", \"neutral\""
            ))),
        }
    }
}

/// The resolution status of a question post or comment
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionStatus {
    /// This question has no answer yet
    Unsolved,
    /// This question has an answer with enough supporting evidence to accept
    Solved,
    /// This question has an answer, but not enough evidence to trust it yet
    NeedsEvidence,
    /// This question cannot be answered by ThorPal due to missing tools or information
    Blocked,
}

impl QuestionStatus {
    /// Parses a question status from a case-insensitive string
    ///
    /// An empty string defaults to [`QuestionStatus::Unsolved`] so a freshly
    /// raised question is open until an agent explicitly resolves it.
    ///
    /// # Arguments
    ///
    /// * `raw` - The raw status string to parse (e.g. "needs_evidence")
    fn parse(raw: &str) -> Result<Self, ToolExecutionError> {
        // match on the lowercased status so agents can use any casing, and
        // tolerate the several spellings a model might emit for NeedsEvidence
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "unsolved" => Ok(QuestionStatus::Unsolved),
            "solved" => Ok(QuestionStatus::Solved),
            "needs_evidence" | "needsevidence" | "needs-evidence" => {
                Ok(QuestionStatus::NeedsEvidence)
            }
            "blocked" => Ok(QuestionStatus::Blocked),
            _ => Err(ToolExecutionError::invalid_args(format!(
                "Invalid status {raw:?}; expected one of \"unsolved\", \"solved\", \
                 \"needs_evidence\", \"blocked\""
            ))),
        }
    }
}

/// Whether a post or comment states something or asks something
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A statement of fact, claim, or finding
    Statement,
    /// A question, tracked by its current resolution status
    Question(QuestionStatus),
}

impl Kind {
    /// Parses a kind from a case-insensitive kind string plus a status string
    ///
    /// The `status` string is only consulted for questions; for statements it is
    /// ignored, so a statement can never carry a (meaningless) status.
    ///
    /// # Arguments
    ///
    /// * `kind` - The raw kind string to parse (e.g. "question")
    /// * `status` - The raw status string, only used when `kind` is a question
    fn parse(kind: &str, status: &str) -> Result<Self, ToolExecutionError> {
        // match on the lowercased kind so agents can use any casing
        match kind.trim().to_ascii_lowercase().as_str() {
            "statement" | "fact" | "claim" => Ok(Kind::Statement),
            "question" => Ok(Kind::Question(QuestionStatus::parse(status)?)),
            _ => Err(ToolExecutionError::invalid_args(format!(
                "Invalid kind {kind:?}; expected \"statement\" or \"question\""
            ))),
        }
    }

    /// Returns a mutable handle to the question status, if this is a question
    ///
    /// Used by `set_status` to transition a question without caring whether the
    /// target was a post or a comment.
    fn status_mut(&mut self) -> Option<&mut QuestionStatus> {
        // only questions carry a status to mutate
        match self {
            Kind::Question(status) => Some(status),
            Kind::Statement => None,
        }
    }

    /// Returns true when this is a question still awaiting a trusted answer
    ///
    /// Open means [`QuestionStatus::Unsolved`] or [`QuestionStatus::NeedsEvidence`];
    /// Solved and Blocked questions are considered closed for termination.
    fn is_open(&self) -> bool {
        // a question is open while it has no trusted answer yet
        matches!(
            self,
            Kind::Question(QuestionStatus::Unsolved | QuestionStatus::NeedsEvidence)
        )
    }

    /// Renders this kind as a short human-readable label for the forum dumps
    ///
    /// Statements render as `Statement`; questions render as
    /// `Question · <Status>` so a reader sees the resolution state inline.
    fn label(&self) -> String {
        // questions carry their status alongside the kind so the dump is scannable
        match self {
            Kind::Statement => "Statement".to_string(),
            Kind::Question(status) => format!("Question · {status:?}"),
        }
    }
}

/// A reference to whatever a comment is replying to
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ParentRef {
    /// A post, identified by its unique name
    Post(String),
    /// A comment, identified by its numeric id
    Comment(usize),
}

impl ParentRef {
    /// Parses a `#post-name` or `!comment-id` reference string
    ///
    /// # Arguments
    ///
    /// * `raw` - The raw reference string to parse (e.g. "#my-post" or "!42")
    pub(crate) fn parse(raw: &str) -> Result<Self, ToolExecutionError> {
        // a leading '#' means this reference points at a post by name
        if let Some(name) = raw.strip_prefix('#') {
            Ok(ParentRef::Post(name.to_owned()))
        } else if let Some(id) = raw.strip_prefix('!') {
            // a leading '!' means this reference points at a comment by id
            id.parse::<usize>().map(ParentRef::Comment).map_err(|_| {
                ToolExecutionError::invalid_args(format!(
                    "Invalid comment reference {raw:?}; expected !<number>"
                ))
            })
        } else {
            // anything else is not a valid reference
            Err(ToolExecutionError::invalid_args(format!(
                "Invalid reference {raw:?}; expected #<post-name> or !<comment-id>"
            )))
        }
    }
}

/// Extracts every `#post-name` / `!comment-id` reference from free text
///
/// The scan is deterministic and dependency-free: the text is split on
/// whitespace (post names can never contain spaces), common wrapping
/// punctuation is trimmed from each token, and whatever still parses as a
/// reference is kept, deduped in first-seen order. False positives that merely
/// look like references (e.g. `#include` or `!1` in disassembly) are harmless:
/// the linked-context walk silently skips references that resolve to nothing
/// on the forum.
///
/// # Arguments
///
/// * `text` - The free text to scan (e.g. a task prompt or a forum body)
pub(crate) fn scan_references(text: &str) -> Vec<ParentRef> {
    // track seen references so repeats keep only their first occurrence
    let mut seen = HashSet::new();
    let mut refs = Vec::new();
    // walk every whitespace-separated token in the text
    for token in text.split_whitespace() {
        // trim wrapping punctuation so refs inside prose or parens still parse;
        // '!' is only trimmed from the end, so a bare `!123` reference survives
        let token = token.trim_start_matches(['(', '[', '{', '"', '\'', '`', '<']);
        let token = token.trim_end_matches([
            '.', ',', ';', ':', '!', '?', ')', ']', '}', '"', '\'', '`', '>',
        ]);
        // keep whatever still parses as a #post-name or !comment-id reference
        if let Ok(parsed) = ParentRef::parse(token) {
            // drop a bare "#" that would parse as an empty post name
            if matches!(&parsed, ParentRef::Post(name) if name.is_empty()) {
                continue;
            }
            // dedupe while preserving the first-seen order
            if seen.insert(parsed.clone()) {
                refs.push(parsed);
            }
        }
    }
    refs
}

/// How much of the forum a sub-agent is handed at spawn time
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForumContextMode {
    /// No forum content is preloaded; agents are told to call `list_posts`
    /// first and so download every post with its full body themselves
    Full,
    /// The assigned question's thread plus everything it transitively
    /// references is rendered into the sub-agent's starting prompt, together
    /// with a titles-only index of every post
    #[default]
    Linked,
}

/// The default number of link hops [`ForumContextMode::Linked`] follows when
/// expanding the assigned thread's references into further threads
pub const DEFAULT_FORUM_LINK_DEPTH: usize = 2;

/// The forum-context settings seeded into every run's [`ToolContext`]
///
/// Sub-agent spawns pull these back out of their inherited context to decide
/// whether (and how deep) to preload linked forum context into the prompt.
#[derive(Debug, Clone, Copy)]
pub struct ForumContextSettings {
    /// Which forum-context mode sub-agent spawns use
    pub mode: ForumContextMode,
    /// How many link hops the linked-mode graph walk follows
    pub depth: usize,
}

impl Default for ForumContextSettings {
    /// The default settings: linked mode at the default link depth
    fn default() -> Self {
        ForumContextSettings {
            mode: ForumContextMode::default(),
            depth: DEFAULT_FORUM_LINK_DEPTH,
        }
    }
}

impl ForumContextSettings {
    /// Pulls the run's forum-context settings out of a [`ToolContext`], if set
    ///
    /// Unlike [`ForumState::from_context`], a missing value is not an error: a
    /// consumer that built its own context never opted into context preloading,
    /// so the caller treats `None` as "inject nothing".
    ///
    /// # Arguments
    ///
    /// * `context` - The tool call context the run was prompted with
    pub(crate) fn from_context(context: &ToolContext) -> Option<ForumContextSettings> {
        // recover the settings, treating a missing slot as opting out
        context.require::<ForumContextSettings>().ok().copied()
    }
}

/// A top-level post raising a question, claim, or finding for discussion
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Post {
    /// The unique, space-free name this post is referenced by (as `#name`)
    pub name: String,
    /// The agent that created this post
    pub author: String,
    /// A short title for this post
    pub title: String,
    /// The body of this post
    pub body: String,
    /// Whether this post states something or asks a (tracked) question
    pub kind: Kind,
    /// The forum revision this post was last created or changed at; higher is
    /// more recent, so a viewer can find the latest activity (0 for snapshots
    /// written before revisions were tracked)
    #[serde(default)]
    pub updated: u64,
}

/// The result of creating a post, plus an optional advisory notice
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatePostResult {
    /// The post that was created
    pub post: Post,
    /// A non-fatal advisory (e.g. a nudge to split a long post), if any
    pub notice: Option<String>,
}

/// A comment replying to a post or to another comment
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    /// The numeric id this comment is referenced by (as `!id`)
    pub id: usize,
    /// The post or comment this comment is replying to
    pub parent: ParentRef,
    /// The agent that created this comment
    pub author: String,
    /// The body of this comment
    pub body: String,
    /// How this comment's evidence relates to what it is replying to
    pub stance: Stance,
    /// Whether this comment states something or asks a (tracked) question
    pub kind: Kind,
    /// The forum revision this comment was last created or changed at; higher is
    /// more recent, so a viewer can find the latest activity (0 for snapshots
    /// written before revisions were tracked)
    #[serde(default)]
    pub updated: u64,
}

/// A completion-gate checklist item attached to a question post or comment
///
/// A question cannot be set to [`QuestionStatus::Solved`] while any of its
/// tasks is incomplete, and completing a task records which comment holds the
/// information that satisfied it, so every cleared gate carries an audit trail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    /// The numeric id this task is referenced by (shown as `task <id>`)
    pub id: usize,
    /// The question post or comment this task gates
    pub parent: ParentRef,
    /// Who created this task (an agent display name, "user", or "detector")
    pub author: String,
    /// What must be checked or done for this task to count as complete
    pub description: String,
    /// How this task was completed, or `None` while it is still open
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed: Option<TaskCompletion>,
    /// The forum revision this task was last created or changed at; higher is
    /// more recent (0 for snapshots written before tasks were tracked)
    #[serde(default)]
    pub updated: u64,
}

/// The audit record of how a completion-gate task was completed
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskCompletion {
    /// The id of the comment holding the information that completed the task
    pub comment: usize,
    /// Who marked the task complete
    pub author: String,
}

/// Structured detector provenance for one seeded forum node
///
/// Seeding flattens each finding's technique, confidence, suspicion, and
/// source into template text the model reads; this record keeps the same
/// facts structured so offline telemetry never has to parse them back out.
/// It is persisted in `forum.json` beside the posts and deliberately kept out
/// of every model-visible forum view -- it costs the model zero tokens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeProvenance {
    /// The seeded node this provenance describes
    pub node: ParentRef,
    /// The seeding shape: `bucket`, `finding`, `rollup`, `question`, or
    /// `annotation`
    #[serde(default)]
    pub shape: String,
    /// What derived a seeded question: `derived`, `script`, `format`, or
    /// `prior_toolkit` (absent for non-question shapes)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question_origin: Option<String>,
    /// One record per finding the node carries, possibly empty
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<FindingProvenance>,
}

/// One detector finding's structured facts, as seeded
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindingProvenance {
    /// The ATT&CK technique id the finding evidences (e.g. `"T1497.003"`)
    #[serde(default)]
    pub technique: String,
    /// The confidence label: `fact`, `likely`, `unsure`, or `untrusted`
    #[serde(default)]
    pub confidence: String,
    /// The numeric suspicion score (higher is more suspicious)
    #[serde(default)]
    pub suspicion: i64,
    /// The producing analysis: `native`, `capa`, `llm`, `script`, `yara`, or
    /// `emulation`
    #[serde(default)]
    pub source: String,
    /// The producing detection's stable name: a native registry name, or the
    /// library script name for script hits (exporters copy this only when
    /// `source == "native"`, so script names stay local)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detection: Option<String>,
    /// How many backing sites the finding carries beyond its primary address
    #[serde(default)]
    pub sites: u64,
    /// The recovered artifact's class key (e.g. `network_endpoint`), when the
    /// finding carried one -- never the artifact's value
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_class: Option<String>,
    /// Whether the finding was suppressed into the runtime-noise rollup
    #[serde(default)]
    pub suppressed: bool,
}

/// One selectable option offered to the analyst by the `ask_analyst` tool
///
/// Derives a JSON schema through rig's re-exported `schemars` (the same path
/// the tool macro resolves for its generated params structs), so the nested
/// option list is schema-described without thorpal depending on schemars.
#[derive(Debug, Clone, Serialize, Deserialize, rig::schemars::JsonSchema)]
#[schemars(crate = "rig::schemars")]
pub struct AskOption {
    /// The short label the analyst picks this option by (unique within one ask)
    pub label: String,
    /// What choosing this option means for the analysis
    pub description: String,
}

/// How a recorded analyst ask got its answer
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerSource {
    /// A live analyst picked one of the offered options
    Analyst,
    /// A live analyst typed a free-text answer instead of an option
    AnalystOther,
    /// No analyst was attached, so the recommended default was taken
    AutoDefault,
    /// An analyst was attached but declined (Esc / TUI quit); default taken
    Declined,
}

impl AnswerSource {
    /// Returns the snake_case wire spelling events and tool results carry
    pub fn wire(&self) -> &'static str {
        // match the serde spelling so events, results, and json read identically
        match self {
            AnswerSource::Analyst => "analyst",
            AnswerSource::AnalystOther => "analyst_other",
            AnswerSource::AutoDefault => "auto_default",
            AnswerSource::Declined => "declined",
        }
    }

    /// Renders this source as a short human-readable label for dumps and reports
    pub fn label(&self) -> &'static str {
        // spell out what each source means for the reader of a report
        match self {
            AnswerSource::Analyst => "answered by the analyst",
            AnswerSource::AnalystOther => "answered by the analyst in their own words",
            AnswerSource::AutoDefault => "auto-default (no analyst attached)",
            AnswerSource::Declined => "default taken (analyst declined)",
        }
    }
}

/// The validated payload of one `ask_analyst` call, before resolution
///
/// Built by the ask tool after validating the question, options, and
/// recommended label, and consumed by [`ForumState::record_ask`] once the ask
/// resolves (an analyst answered, the default was taken, or the ask was
/// declined).
#[derive(Debug, Clone)]
pub struct AskRequest {
    /// The display name of the agent asking (always `"main"` in v1)
    pub asked_by: String,
    /// The question posed to the analyst
    pub question: String,
    /// The 2-4 options offered, in display order
    pub options: Vec<AskOption>,
    /// The exact label of the designated default/recommended option
    pub recommended: String,
}

/// A resolved `ask_analyst` question, recorded for review and the report
///
/// Asks are recorded only once resolved, so there is never a persisted pending
/// state, and recorded asks never gate the `finish` tool (which only scans
/// question posts and comments).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalystAsk {
    /// The numeric id this ask is referenced by (shown as `ask <id>`)
    pub id: usize,
    /// The display name of the agent that asked (always `"main"` in v1)
    pub asked_by: String,
    /// The question posed to the analyst
    pub question: String,
    /// The 2-4 options offered, in display order
    pub options: Vec<AskOption>,
    /// The exact label of the designated default/recommended option
    pub recommended: String,
    /// The answer: a chosen option label, or the analyst's free text
    pub chosen: String,
    /// Who or what produced the answer
    pub source: AnswerSource,
    /// The forum revision this ask was recorded at (0 for snapshots written
    /// before asks were tracked)
    #[serde(default)]
    pub updated: u64,
}

/// A non-blocking "needs analyst review" flag an agent raised
///
/// Review flags never gate the `finish` tool: they are the agent's durable
/// hand-off of low-confidence conclusions and lone judgment calls to the human
/// analyst, surfaced on the TUI board and in the report appendix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewFlag {
    /// The numeric id this flag is referenced by (shown as `flag <id>`)
    pub id: usize,
    /// Who raised it: an agent display name, or `"system"` when auto-filed for
    /// an ask that resolved without an analyst answer
    pub author: String,
    /// A short title naming what needs review
    pub title: String,
    /// Why human eyes are needed
    pub reason: String,
    /// The post or comment this flag concerns, if it anchored to one
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<ParentRef>,
    /// The recorded ask this flag was auto-filed for, if any
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask: Option<usize>,
    /// The forum revision this flag was raised at (0 for snapshots written
    /// before flags were tracked)
    #[serde(default)]
    pub updated: u64,
}

/// A comment together with its full nested reply thread
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommentNode {
    /// This node's comment
    pub comment: Comment,
    /// The completion-gate tasks on this comment, if it is a question with any
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<Task>,
    /// The replies to this comment, each with their own nested replies
    pub replies: Vec<CommentNode>,
}

/// A post together with its full nested comment thread
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostWithThread {
    /// This thread's post
    pub post: Post,
    /// The completion-gate tasks on this post, if it is a question with any
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<Task>,
    /// The top-level comments on this post, each with their own nested replies
    pub comments: Vec<CommentNode>,
}

/// Whatever `follow_link` resolved a reference to, with its thread attached
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ForumNode {
    /// A resolved post and its comment thread
    Post(PostWithThread),
    /// A resolved comment and its reply thread
    Comment(CommentNode),
}

/// A structured, point-in-time view of the whole forum for an external viewer
///
/// Built by [`ForumState::view`] so a consumer such as a TUI can render every
/// thread itself and jump straight to the most recent activity.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ForumView {
    /// Every post with its full nested comment thread, sorted by post name
    pub threads: Vec<PostWithThread>,
    /// The post or comment that was most recently created or changed, if any
    pub last_updated: Option<ParentRef>,
    /// Every recorded analyst ask, sorted by id (empty on pre-feature views)
    #[serde(default)]
    pub asks: Vec<AnalystAsk>,
    /// Every review flag raised, sorted by id (empty on pre-feature views)
    #[serde(default)]
    pub flags: Vec<ReviewFlag>,
}

// the body length (in characters) above which create_post returns a non-fatal
// nudge to split a post into several smaller, single-topic posts next time
const POST_BODY_SPLIT_HINT_CHARS: usize = 600;

// the most tasks one call (add_tasks, or create_post's tasks list) may attach,
// so a single tool call cannot flood a question's completion checklist
const MAX_TASKS_PER_ADD: usize = 20;

/// The number of hits `search_forum` returns when no limit is given
const DEFAULT_SEARCH_LIMIT: usize = 8;

/// The most hits one `search_forum` call may return, so a single tool call
/// cannot flood the model's context with low-relevance tail matches
const MAX_SEARCH_LIMIT: usize = 25;

// the number of characters of content that fit inside a box's borders, i.e. the
// width text is wrapped to when rendering the boxed on-disk forum
const BOX_INNER_WIDTH: usize = 86;

/// The author every user-planted question and task is attributed to.
///
/// Exported so offline telemetry can classify forum entries by author the
/// same way it classifies detector-seeded ones via [`crate::DETECTOR_AUTHOR`].
pub const USER_AUTHOR: &str = "user";

/// The most `ask_analyst` questions one run may record, so a looping model can
/// neither stall an interactive session nor flood the review queue
pub(crate) const MAX_ASKS_PER_RUN: usize = 10;

/// The most review flags one run may raise, so the analyst's review queue
/// stays scannable
const MAX_FLAGS_PER_RUN: usize = 40;

/// The size bound (in bytes) on the rendered report appendix
///
/// The measured worst case across all 24 evaluation sessions rendered to
/// roughly 205 KB, so 256 KB keeps every observed forum intact while still
/// bounding a pathological one.
pub(crate) const REPORT_APPENDIX_MAX_BYTES: usize = 256 * 1024;

/// The marker a detector body's reasoning prose is replaced with in the appendix
const DETECTOR_BODY_ELIDED: &str = "[reasoning elided — see forum.json]";

/// Reduce a detector body to its evidence lines for the appendix
///
/// Keeps the finding/content lines and every `Site:`/`Sites (`/`Artifact (`/
/// `Context:` line — the values and addresses the report must preserve
/// verbatim — and drops the `Reasoning:` block (replaced by one elision
/// marker): the persuasion prose is what the appendix budget cannot afford.
/// Bodies without a `Reasoning:` anchor pass through unchanged.
///
/// # Arguments
///
/// * `body` - The detector-authored body to filter
fn elide_detector_reasoning(body: &str) -> String {
    let mut out = String::new();
    let mut skipping = false;
    for line in body.lines() {
        // the reasoning block starts at its label and ends at an evidence line
        if line.starts_with("Reasoning:") {
            if !skipping {
                out.push_str(DETECTOR_BODY_ELIDED);
                out.push('\n');
            }
            skipping = true;
            continue;
        }
        if skipping
            && !(line.starts_with("Site:")
                || line.starts_with("Sites (")
                || line.starts_with("Artifact (")
                || line.starts_with("Context:"))
        {
            continue;
        }
        skipping = false;
        out.push_str(line);
        out.push('\n');
    }
    // drop the trailing newline so callers can line-iterate like before
    out.truncate(out.trim_end_matches('\n').len());
    out
}

/// The fixed banner the report appendix opens with
const REPORT_APPENDIX_HEADER: &str = "---\n\n\
## Appendix — Forum Evidence (machine-generated)\n\n\
Generated deterministically from the session's shared forum after the report\n\
above was written, so that every decoded string, IOC, key, path, and constant\n\
posted during analysis is preserved verbatim even where the narrative\n\
summarizes. Detector reasoning prose is elided; detector evidence lines\n\
(sites, artifacts) and agent findings are complete.\n";

/// The instance-scoped state backing a single ThorPal forum.
///
/// Every agent in one ThorPal instance -- the main agent and each sub-agent --
/// shares one `Arc<ForumState>`, threaded to the forum tools through the run's
/// [`ToolContext`] (see [`ForumState::from_context`]). Two ThorPal instances get
/// two independent forums, so they never collide the way the old process-global
/// statics did.
pub struct ForumState {
    /// All posts, keyed by their unique name so create_post can check-and-insert
    /// uniqueness under a single write lock.
    posts: RwLock<HashMap<String, Post>>,
    /// All comments, keyed by their numeric id.
    comments: RwLock<HashMap<usize, Comment>>,
    /// All completion-gate tasks, keyed by their numeric id.
    tasks: RwLock<HashMap<usize, Task>>,
    /// All resolved analyst asks, keyed by their numeric id.
    asks: RwLock<HashMap<usize, AnalystAsk>>,
    /// All "needs analyst review" flags, keyed by their numeric id.
    flags: RwLock<HashMap<usize, ReviewFlag>>,
    /// The next id to hand out to a newly created comment.
    next_comment_id: AtomicUsize,
    /// The next id to hand out to a newly created task.
    next_task_id: AtomicUsize,
    /// The next id to hand out to a newly recorded analyst ask.
    next_ask_id: AtomicUsize,
    /// The next id to hand out to a newly raised review flag.
    next_flag_id: AtomicUsize,
    /// The next revision to stamp onto a created or changed post/comment.
    revision: AtomicU64,
    /// The BM25 engine last fitted over the forum, keyed by the revision it
    /// was fitted at and refitted lazily on the first search after a mutation
    /// (see [`search::engine_at`]). The mutex serializes fits so concurrent
    /// searches reuse one fit instead of each paying for their own.
    search_cache: Mutex<Option<search::CachedEngine>>,
    /// Set by the `finish` tool when the main agent declares it has no open
    /// questions left and nothing more to ask; the control loop reads-and-clears
    /// it each iteration.
    finish_requested: AtomicBool,
    /// Structured provenance for detector-seeded nodes, keyed by the node
    /// each record describes. Written only by the seeding wrappers, carried
    /// in snapshots for offline telemetry, and never part of any
    /// model-visible view.
    provenance: RwLock<HashMap<ParentRef, NodeProvenance>>,
    /// The session store the forum is persisted into (as `forum.json`) after
    /// every mutation, installed once at build time when the run has a store.
    /// Left unset for a consumer that wants no on-disk persistence.
    store: OnceLock<Arc<SessionStore>>,
    /// The sink a [`ThorPalEvent::ForumUpdated`] is emitted on after every
    /// mutation, installed once at build time when the run has an event sink.
    events: OnceLock<ThorPalEventSender>,
}

impl ForumState {
    /// Creates a fresh, empty forum state
    pub fn new() -> Self {
        ForumState {
            posts: RwLock::new(HashMap::new()),
            comments: RwLock::new(HashMap::new()),
            tasks: RwLock::new(HashMap::new()),
            asks: RwLock::new(HashMap::new()),
            flags: RwLock::new(HashMap::new()),
            // comment ids start at 1 so 0 can never be a valid id
            next_comment_id: AtomicUsize::new(1),
            // task ids start at 1 so 0 can never be a valid id
            next_task_id: AtomicUsize::new(1),
            // ask ids start at 1 so 0 can never be a valid id
            next_ask_id: AtomicUsize::new(1),
            // flag ids start at 1 so 0 can never be a valid id
            next_flag_id: AtomicUsize::new(1),
            // revisions start at 1 so 0 marks an item that predates tracking
            revision: AtomicU64::new(1),
            // no engine is fitted until the first search asks for one
            search_cache: Mutex::new(None),
            finish_requested: AtomicBool::new(false),
            // provenance appears only once detector seeding writes it
            provenance: RwLock::new(HashMap::new()),
            store: OnceLock::new(),
            events: OnceLock::new(),
        }
    }

    /// Installs the event sink forum mutations are reported on
    ///
    /// Called once by the builder when the run has an event sink; a second call
    /// is ignored since the sink is fixed for the forum's lifetime.
    ///
    /// # Arguments
    ///
    /// * `sender` - The channel to emit [`ThorPalEvent::ForumUpdated`] onto
    pub(crate) fn set_events(&self, sender: ThorPalEventSender) {
        // the sink is set at most once; a repeat install keeps the first
        let _ = self.events.set(sender);
    }

    /// Installs the session store the forum is persisted into after mutations
    ///
    /// Called once by the builder when the run has a session store; a second
    /// call is ignored since the store is fixed for the forum's lifetime.
    ///
    /// # Arguments
    ///
    /// * `store` - The session store to save `forum.json` into
    pub(crate) fn set_store(&self, store: Arc<SessionStore>) {
        // the store is set at most once; a repeat install keeps the first
        let _ = self.store.set(store);
    }

    /// Hands out the next revision to stamp onto a created or changed item
    fn next_revision(&self) -> u64 {
        // bump the counter and hand out the value it held
        self.revision.fetch_add(1, Ordering::SeqCst)
    }

    /// Mirrors the forum to disk and reports a mutation to any event sink
    ///
    /// Called after every post/comment creation or status change, once any
    /// write lock has been released (the disk flush re-acquires read locks).
    ///
    /// # Arguments
    ///
    /// * `target` - The post or comment that was just created or changed
    async fn after_mutation(&self, target: ParentRef) {
        // advance the revision once more now that the change is visible: items
        // stamp `updated` via next_revision() *before* they are inserted, so a
        // reader that scanned at that revision and missed the item must see the
        // counter move again or its cached "absent" verdict would stick until
        // some unrelated mutation (see ConstantTracker's presence cache)
        self.revision.fetch_add(1, Ordering::SeqCst);
        // mirror the updated forum to disk first so the dump never lags the event
        self.flush_forum_to_disk().await;
        // report the change without blocking; a closed channel is a no-op
        if let Some(tx) = self.events.get() {
            let _ = tx.send(ThorPalEvent::ForumUpdated { target });
        }
    }

    /// Mirrors the forum to disk and reports an ask/flag mutation on the sink
    ///
    /// The analyst-record twin of [`ForumState::after_mutation`]: asks and
    /// review flags have no [`ParentRef`] target, so they emit their own
    /// dedicated event instead of a [`ThorPalEvent::ForumUpdated`]. Like its
    /// twin, this must only be called once any write lock has been released.
    ///
    /// # Arguments
    ///
    /// * `event` - The dedicated event describing the mutation
    async fn after_analyst_mutation(&self, event: ThorPalEvent) {
        // advance the revision once more now that the change is visible, for
        // the same reader-cache reason after_mutation documents
        self.revision.fetch_add(1, Ordering::SeqCst);
        // mirror the updated forum to disk first so the dump never lags the event
        self.flush_forum_to_disk().await;
        // report the change without blocking; a closed channel is a no-op
        if let Some(tx) = self.events.get() {
            let _ = tx.send(event);
        }
    }

    /// Pulls the shared forum state out of a tool call's [`ToolContext`]
    ///
    /// The forum tools call this at the top of every invocation to recover the
    /// `Arc<ForumState>` that `ThorPal` seeded into the run's context. A missing
    /// value is a wiring bug, so it surfaces as an invalid-args tool error.
    ///
    /// # Arguments
    ///
    /// * `context` - The tool call context the run was prompted with
    pub(crate) fn from_context(
        context: &ToolContext,
    ) -> Result<Arc<ForumState>, ToolExecutionError> {
        // recover the shared state, mapping a missing slot to a tool error
        context
            .require::<Arc<ForumState>>()
            .map(Arc::clone)
            .map_err(|err| ToolExecutionError::invalid_args(err.to_string()))
    }

    /// Checks that a parent reference actually exists on this forum
    ///
    /// # Arguments
    ///
    /// * `parent` - The parent reference to check
    async fn check_parent_exists(&self, parent: &ParentRef) -> Result<(), ToolExecutionError> {
        match parent {
            ParentRef::Post(name) => {
                // check if a post with this name exists
                if self.posts.read().await.contains_key(name) {
                    Ok(())
                } else {
                    Err(ToolExecutionError::invalid_args(format!(
                        "No post named {name:?}"
                    )))
                }
            }
            ParentRef::Comment(id) => {
                // check if a comment with this id exists
                if self.comments.read().await.contains_key(id) {
                    Ok(())
                } else {
                    Err(ToolExecutionError::invalid_args(format!(
                        "No comment with id {id}"
                    )))
                }
            }
        }
    }

    /// Creates a new top-level post on this forum
    ///
    /// This backs the [`create_post`] tool: it validates the name, parses the
    /// kind, check-and-inserts the post under a single write lock, attaches any
    /// completion-gate tasks (questions only), mirrors the forum to disk, and
    /// returns the post plus an optional split advisory.
    ///
    /// # Arguments
    ///
    /// * `author` - The agent creating this post
    /// * `name` - The unique, space-free reference name for the post
    /// * `title` - A short title for the post
    /// * `body` - The body of the post
    /// * `kind` - The raw kind string ("statement" or "question")
    /// * `status` - The raw status string, only used when `kind` is a question
    /// * `tasks` - Completion-gate task descriptions to attach (questions only)
    async fn create_post(
        &self,
        author: String,
        name: String,
        title: String,
        body: String,
        kind: String,
        status: String,
        tasks: Vec<String>,
    ) -> Result<CreatePostResult, ToolExecutionError> {
        // reject any name containing whitespace since it must work as a #name reference
        if name.chars().any(char::is_whitespace) {
            return Err(ToolExecutionError::invalid_args(format!(
                "Post name {name:?} must not contain whitespace"
            )));
        }
        // parse the kind (and, for questions, the status) before touching storage
        let kind = Kind::parse(&kind, &status)?;
        // tasks are completion gates on questions, so an explicitly provided
        // list on a statement is a real mistake worth an error; validate the
        // descriptions too, all before the post is inserted
        if !tasks.is_empty() {
            if matches!(kind, Kind::Statement) {
                return Err(ToolExecutionError::invalid_args(
                    "tasks were provided but kind is \"statement\"; tasks only attach to \
                     questions"
                        .to_string(),
                ));
            }
            // a question born solved has an accepted answer, so its tasks could
            // never gate anything; make the contradiction an error
            if matches!(kind, Kind::Question(QuestionStatus::Solved)) {
                return Err(ToolExecutionError::invalid_args(
                    "tasks were provided but status is \"solved\"; create the question \
                     \"unsolved\" and complete its tasks before marking it solved"
                        .to_string(),
                ));
            }
            Self::validate_task_descriptions(&tasks)?;
        }
        // keep a copy of the author so the post and its tasks share attribution
        let task_author = author.clone();
        // build the post to insert
        let post = Post {
            name: name.clone(),
            author,
            title,
            body,
            kind,
            updated: self.next_revision(),
        };
        // check-and-insert under a single write lock to avoid a race between
        // two agents creating the same post name concurrently; scope the guard so it
        // is dropped before we flush to disk (flushing re-acquires read locks and
        // would otherwise deadlock against this held write guard)
        {
            let mut guard = self.posts.write().await;
            if guard.contains_key(&name) {
                return Err(ToolExecutionError::invalid_args(format!(
                    "A post named {name:?} already exists"
                )));
            }
            guard.insert(name, post.clone());
            // attach any completion-gate tasks before the post lock drops, so a
            // concurrent solve can never observe the question without its gates
            if !tasks.is_empty() {
                self.store_tasks(&task_author, &ParentRef::Post(post.name.clone()), tasks)
                    .await;
            }
        }
        tracing::debug!(post = ?post, "created post");
        // mirror and report the change now that the write lock is released
        self.after_mutation(ParentRef::Post(post.name.clone()))
            .await;
        // if this body is long, ride a non-fatal nudge along with the successful
        // result so the agent is encouraged to split into smaller posts next time
        let notice = if post.body.chars().count() > POST_BODY_SPLIT_HINT_CHARS {
            Some(format!(
                "This post is long ({} chars). Prefer several small posts, each \
                 stating a single fact, question, or problem, over one large post. \
                 Consider splitting future findings into separate posts.",
                post.body.chars().count()
            ))
        } else {
            None
        };
        Ok(CreatePostResult { post, notice })
    }

    /// Creates a comment replying to a post or another comment on this forum
    ///
    /// This backs the [`create_comment`] tool: it parses the parent/stance/kind,
    /// verifies the parent exists, hands out the next comment id, saves the
    /// comment, and mirrors the forum to disk.
    ///
    /// # Arguments
    ///
    /// * `author` - The agent creating this comment
    /// * `parent` - The raw parent reference ("#post-name" or "!123")
    /// * `body` - The body of the comment
    /// * `stance` - The raw stance string ("supports"/"disproves"/"neutral")
    /// * `kind` - The raw kind string ("statement" or "question")
    /// * `status` - The raw status string, only used when `kind` is a question
    async fn create_comment(
        &self,
        author: String,
        parent: String,
        body: String,
        stance: String,
        kind: String,
        status: String,
    ) -> Result<Comment, ToolExecutionError> {
        // parse the parent reference, stance, and kind before touching any storage
        let parent = ParentRef::parse(&parent)?;
        let stance = Stance::parse(&stance)?;
        let kind = Kind::parse(&kind, &status)?;
        // make sure the parent this comment replies to actually exists
        self.check_parent_exists(&parent).await?;
        // hand out the next comment id
        let id = self.next_comment_id.fetch_add(1, Ordering::SeqCst);
        // build and save this comment
        let comment = Comment {
            id,
            parent,
            author,
            body,
            stance,
            kind,
            updated: self.next_revision(),
        };
        self.comments.write().await.insert(id, comment.clone());
        tracing::debug!(comment = ?comment, "created comment");
        // mirror and report the change now that the write lock is released
        self.after_mutation(ParentRef::Comment(id)).await;
        Ok(comment)
    }

    /// Seeds a pre-built post onto the forum, bypassing the LLM tool plumbing
    ///
    /// This mirrors [`ForumState::create_post`]'s check-and-insert but takes
    /// already-typed fields (the detector seeder builds them directly) and skips
    /// the split-hint advisory and stdout noise. Used to pre-populate the forum
    /// with detector findings before the agent loop starts.
    ///
    /// # Arguments
    ///
    /// * `author` - The author to attribute the post to (e.g. `"detector"`)
    /// * `name` - The unique, space-free reference name for the post
    /// * `title` - A short title for the post
    /// * `body` - The body of the post
    /// * `kind` - Whether this post states something or asks a question
    #[instrument(skip_all, err(Debug))]
    pub async fn seed_post(
        &self,
        author: String,
        name: String,
        title: String,
        body: String,
        kind: Kind,
    ) -> Result<Post, ToolExecutionError> {
        // reject any name containing whitespace since it must work as a #name reference
        if name.chars().any(char::is_whitespace) {
            return Err(ToolExecutionError::invalid_args(format!(
                "Post name {name:?} must not contain whitespace"
            )));
        }
        // build the post to insert
        let post = Post {
            name: name.clone(),
            author,
            title,
            body,
            kind,
            updated: self.next_revision(),
        };
        // check-and-insert under a single write lock, rejecting a duplicate name;
        // scope the guard so it drops before flushing to disk (flushing re-acquires
        // read locks and would otherwise deadlock against this held write guard)
        {
            let mut guard = self.posts.write().await;
            if guard.contains_key(&name) {
                return Err(ToolExecutionError::invalid_args(format!(
                    "A post named {name:?} already exists"
                )));
            }
            guard.insert(name, post.clone());
        }
        // mirror and report the change now that the write lock is released
        self.after_mutation(ParentRef::Post(post.name.clone()))
            .await;
        Ok(post)
    }

    /// Seeds a pre-built comment onto the forum, bypassing the LLM tool plumbing
    ///
    /// This mirrors [`ForumState::create_comment`]'s parent-check and
    /// id-assignment but takes an already-typed [`ParentRef`]/[`Stance`] and skips
    /// the stdout noise. Used to attach detector findings under their seeded posts
    /// before the agent loop starts.
    ///
    /// # Arguments
    ///
    /// * `author` - The author to attribute the comment to (e.g. `"detector"`)
    /// * `parent` - The post or comment this comment replies to
    /// * `body` - The body of the comment
    /// * `stance` - How this comment's evidence relates to its parent
    /// * `kind` - Whether this comment states something or asks a question
    #[instrument(skip_all, err(Debug))]
    pub async fn seed_comment(
        &self,
        author: String,
        parent: ParentRef,
        body: String,
        stance: Stance,
        kind: Kind,
    ) -> Result<Comment, ToolExecutionError> {
        // make sure the parent this comment replies to actually exists
        self.check_parent_exists(&parent).await?;
        // hand out the next comment id
        let id = self.next_comment_id.fetch_add(1, Ordering::SeqCst);
        // build and save this comment
        let comment = Comment {
            id,
            parent,
            author,
            body,
            stance,
            kind,
            updated: self.next_revision(),
        };
        self.comments.write().await.insert(id, comment.clone());
        // mirror and report the change now that the write lock is released
        self.after_mutation(ParentRef::Comment(id)).await;
        Ok(comment)
    }

    /// Seeds a detector post and records its structured provenance
    ///
    /// Builds on [`ForumState::seed_post`]; the provenance record is keyed by
    /// the created post and re-flushed to disk so `forum.json` always carries
    /// it. The record never joins any model-visible view.
    ///
    /// # Arguments
    ///
    /// * `author` - The author to attribute the post to (e.g. `"detector"`)
    /// * `name` - The unique, space-free reference name for the post
    /// * `title` - A short title for the post
    /// * `body` - The body of the post
    /// * `kind` - Whether this post states something or asks a question
    /// * `shape` - The seeding shape (`bucket`, `rollup`, `question`, ...)
    /// * `question_origin` - What derived a seeded question, when one did
    /// * `findings` - The structured facts of the findings the post carries
    #[allow(clippy::too_many_arguments)]
    #[instrument(skip_all, err(Debug))]
    pub async fn seed_detector_post(
        &self,
        author: String,
        name: String,
        title: String,
        body: String,
        kind: Kind,
        shape: &str,
        question_origin: Option<String>,
        findings: Vec<FindingProvenance>,
    ) -> Result<Post, ToolExecutionError> {
        // seed the post itself first so a duplicate name changes nothing
        let post = self.seed_post(author, name, title, body, kind).await?;
        // record the provenance under the node the post actually became
        let node = ParentRef::Post(post.name.clone());
        let record = NodeProvenance {
            node: node.clone(),
            shape: shape.to_owned(),
            question_origin,
            findings,
        };
        self.provenance.write().await.insert(node, record);
        // re-flush so the on-disk forum carries the provenance immediately
        self.flush_forum_to_disk().await;
        Ok(post)
    }

    /// Seeds a detector comment and records its structured provenance
    ///
    /// Builds on [`ForumState::seed_comment`]; the provenance record is keyed
    /// by the created comment's id and re-flushed to disk. The record never
    /// joins any model-visible view.
    ///
    /// # Arguments
    ///
    /// * `author` - The author to attribute the comment to (e.g. `"detector"`)
    /// * `parent` - The post or comment this comment replies to
    /// * `body` - The body of the comment
    /// * `stance` - How this comment's evidence relates to its parent
    /// * `kind` - Whether this comment states something or asks a question
    /// * `shape` - The seeding shape (normally `finding`)
    /// * `findings` - The structured facts of the findings the comment carries
    #[instrument(skip_all, err(Debug))]
    pub async fn seed_detector_comment(
        &self,
        author: String,
        parent: ParentRef,
        body: String,
        stance: Stance,
        kind: Kind,
        shape: &str,
        findings: Vec<FindingProvenance>,
    ) -> Result<Comment, ToolExecutionError> {
        // seed the comment first so a bad parent changes nothing
        let comment = self
            .seed_comment(author, parent, body, stance, kind)
            .await?;
        // record the provenance under the id the comment actually got
        let node = ParentRef::Comment(comment.id);
        let record = NodeProvenance {
            node: node.clone(),
            shape: shape.to_owned(),
            question_origin: None,
            findings,
        };
        self.provenance.write().await.insert(node, record);
        // re-flush so the on-disk forum carries the provenance immediately
        self.flush_forum_to_disk().await;
        Ok(comment)
    }

    /// Seeds a user-specified question onto the forum as an open question post
    ///
    /// This is the programmatic entry point a caller uses to plant a human's own
    /// question before or around the agent loop, so the main agent picks it up and
    /// delegates it just like any question it raised itself. It builds on
    /// [`ForumState::seed_post`] with a `Kind::Question(QuestionStatus::Unsolved)`
    /// kind and derives the space-free reference name from the title by replacing
    /// spaces with dashes.
    ///
    /// # Arguments
    ///
    /// * `title` - A short, non-empty human-readable title for the question
    /// * `body` - The full text of the question to answer
    ///
    /// # Errors
    ///
    /// Returns an error if `title` is empty (or only whitespace), since the title
    /// doubles as the question's reference name.
    #[instrument(skip_all, err(Debug))]
    pub async fn add_user_question(
        &self,
        title: String,
        body: String,
    ) -> Result<Post, ToolExecutionError> {
        // reject an empty title outright since it doubles as the reference name
        if title.trim().is_empty() {
            return Err(ToolExecutionError::invalid_args(
                "A user question title must not be empty".to_string(),
            ));
        }
        // a fresh user question always starts open so the control loop drives it
        let kind = Kind::Question(QuestionStatus::Unsolved);
        // derive the space-free reference name from the title by dashing its spaces
        let name = title.replace(' ', "-");
        // seed the question onto the forum, attributed to the human operator
        self.seed_post(USER_AUTHOR.to_string(), name, title, body, kind)
            .await
    }

    /// Adds a human operator's comment under a post or another comment
    ///
    /// This is the programmatic entry point a caller (e.g. a TUI) uses to let an
    /// analyst join the discussion: the comment is attributed to the human
    /// operator, so agents see it like any other comment when they browse the
    /// forum. It builds on [`ForumState::seed_comment`].
    ///
    /// # Arguments
    ///
    /// * `parent` - The post or comment this comment replies to
    /// * `body` - The text of the comment
    /// * `stance` - How this comment relates to what it is replying to
    /// * `kind` - Whether this comment states something or asks a question
    ///
    /// # Errors
    ///
    /// Returns an error if `body` is empty (or only whitespace) or the parent
    /// does not exist.
    #[instrument(name = "ForumState::add_user_comment", skip_all, err(Debug))]
    pub async fn add_user_comment(
        &self,
        parent: ParentRef,
        body: String,
        stance: Stance,
        kind: Kind,
    ) -> Result<Comment, ToolExecutionError> {
        // reject an empty comment outright since it carries nothing for the agents
        if body.trim().is_empty() {
            return Err(ToolExecutionError::invalid_args(
                "A user comment must not be empty".to_string(),
            ));
        }
        // seed the comment onto the forum, attributed to the human operator
        self.seed_comment(USER_AUTHOR.to_string(), parent, body, stance, kind)
            .await
    }

    /// Validates a batch of task descriptions before they are attached
    ///
    /// # Arguments
    ///
    /// * `descriptions` - The task descriptions to validate
    fn validate_task_descriptions(descriptions: &[String]) -> Result<(), ToolExecutionError> {
        // an empty batch is a mistake worth surfacing rather than a silent no-op
        if descriptions.is_empty() {
            return Err(ToolExecutionError::invalid_args(
                "At least one task description must be provided".to_string(),
            ));
        }
        // bound the batch so one call cannot flood a question's checklist
        if descriptions.len() > MAX_TASKS_PER_ADD {
            return Err(ToolExecutionError::invalid_args(format!(
                "At most {MAX_TASKS_PER_ADD} tasks may be added per call (got {})",
                descriptions.len()
            )));
        }
        // every description must say something checkable
        if let Some(index) = descriptions.iter().position(|desc| desc.trim().is_empty()) {
            return Err(ToolExecutionError::invalid_args(format!(
                "Task description {} must not be empty",
                index + 1
            )));
        }
        Ok(())
    }

    /// Checks that a kind can still take new completion-gate tasks
    ///
    /// Tasks gate the transition to solved, so a statement can never carry
    /// them, and an already-solved question must be reopened (set back to
    /// "unsolved" or "needs_evidence") first -- a task attached after the
    /// answer was accepted could never gate anything.
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of the post or comment the tasks would attach to
    /// * `target` - The target reference, used to name it in errors
    fn check_task_target(kind: Kind, target: &ParentRef) -> Result<(), ToolExecutionError> {
        // name the target the way its own error messages already do
        let (noun, reference) = match target {
            ParentRef::Post(name) => ("Post", format!("{name:?}")),
            ParentRef::Comment(id) => ("Comment", id.to_string()),
        };
        match kind {
            // statements have no status for tasks to ever gate
            Kind::Statement => Err(ToolExecutionError::invalid_args(format!(
                "{noun} {reference} is a statement, not a question; tasks only attach to \
                 questions"
            ))),
            // a solved question's answer is already accepted, so new gates on
            // it would be dead; force an explicit reopen first
            Kind::Question(QuestionStatus::Solved) => {
                Err(ToolExecutionError::invalid_args(format!(
                    "{noun} {reference} is already solved; set it back to \"unsolved\" or \
                     \"needs_evidence\" with set_status before adding completion-gate tasks"
                )))
            }
            // any still-gateable question can take new tasks
            Kind::Question(_) => Ok(()),
        }
    }

    /// Stamps a fresh revision onto the post or comment a task change touched
    ///
    /// Task mutations bump their owning question's revision so the change reads
    /// as the question's latest activity (driving a viewer's "latest" tracking).
    /// A dangling target is silently skipped; it cannot happen in practice since
    /// posts and comments are never deleted.
    ///
    /// # Arguments
    ///
    /// * `target` - The question post or comment whose revision to bump
    async fn touch_target(&self, target: &ParentRef) {
        match target {
            ParentRef::Post(name) => {
                // bump the owning post's revision under a scoped write lock
                if let Some(post) = self.posts.write().await.get_mut(name) {
                    post.updated = self.next_revision();
                }
            }
            ParentRef::Comment(id) => {
                // bump the owning comment's revision under a scoped write lock
                if let Some(comment) = self.comments.write().await.get_mut(id) {
                    comment.updated = self.next_revision();
                }
            }
        }
    }

    /// Mints new tasks with fresh ids and revision stamps, without storing them
    ///
    /// # Arguments
    ///
    /// * `author` - Who to attribute the new tasks to
    /// * `parent` - The question post or comment the tasks gate
    /// * `descriptions` - The pre-validated task descriptions to mint from
    fn mint_tasks(&self, author: &str, parent: &ParentRef, descriptions: Vec<String>) -> Vec<Task> {
        // build each task around the next id and a fresh revision, open until
        // explicitly completed
        descriptions
            .into_iter()
            .map(|description| Task {
                id: self.next_task_id.fetch_add(1, Ordering::SeqCst),
                parent: parent.clone(),
                author: author.to_owned(),
                description,
                completed: None,
                updated: self.next_revision(),
            })
            .collect()
    }

    /// Mints and stores tasks under a question the caller has already checked
    ///
    /// The caller MUST hold the owning post's or comment's write lock: that
    /// lock is what makes the question check and the insert atomic against a
    /// concurrent solve (the solved gate runs under the same lock), and it
    /// fixes the owner-before-tasks lock order this forum relies on to stay
    /// deadlock-free.
    ///
    /// # Arguments
    ///
    /// * `author` - Who to attribute the new tasks to
    /// * `parent` - The question post or comment the tasks gate
    /// * `descriptions` - The pre-validated task descriptions to attach
    async fn store_tasks(
        &self,
        author: &str,
        parent: &ParentRef,
        descriptions: Vec<String>,
    ) -> Vec<Task> {
        // mint the tasks with fresh ids and revisions
        let created = self.mint_tasks(author, parent, descriptions);
        // store them under a scoped tasks write lock (owner lock already held)
        {
            let mut guard = self.tasks.write().await;
            for task in &created {
                guard.insert(task.id, task.clone());
            }
        }
        created
    }

    /// Adds completion-gate tasks to an existing question post or comment
    ///
    /// The typed core shared by the [`add_tasks`] tool, the detector seeder, and
    /// a human operator: it validates the descriptions, then checks the target
    /// and attaches the tasks under the owning question's write lock -- the same
    /// lock the solved gate takes -- so a concurrent solve can never land
    /// between the check and the insert, and mirrors/reports the change once.
    ///
    /// # Arguments
    ///
    /// * `author` - Who to attribute the new tasks to
    /// * `target` - The question post or comment the tasks gate
    /// * `descriptions` - The task descriptions to attach
    ///
    /// # Errors
    ///
    /// Returns an error if the descriptions are empty, oversized, or blank, or
    /// the target does not exist, is a statement, or is already solved.
    #[instrument(name = "ForumState::add_tasks_to", skip_all, err(Debug))]
    pub(crate) async fn add_tasks_to(
        &self,
        author: String,
        target: ParentRef,
        descriptions: Vec<String>,
    ) -> Result<Vec<Task>, ToolExecutionError> {
        // validate the batch before touching any storage
        Self::validate_task_descriptions(&descriptions)?;
        // check the target and attach the tasks under the owning question's
        // write lock, so a concurrent solve (which gates under the same lock)
        // can never land between the check and the insert
        let created = match &target {
            ParentRef::Post(name) => {
                // grab the post, rejecting an unknown name or an ungateable kind
                let mut guard = self.posts.write().await;
                let post = guard.get_mut(name).ok_or_else(|| {
                    ToolExecutionError::invalid_args(format!("No post named {name:?}"))
                })?;
                Self::check_task_target(post.kind, &target)?;
                // attach the tasks, then bump the owning question so the new
                // checklist reads as its latest activity
                let created = self.store_tasks(&author, &target, descriptions).await;
                post.updated = self.next_revision();
                created
            }
            ParentRef::Comment(id) => {
                // grab the comment, rejecting an unknown id or an ungateable kind
                let mut guard = self.comments.write().await;
                let comment = guard.get_mut(id).ok_or_else(|| {
                    ToolExecutionError::invalid_args(format!("No comment with id {id}"))
                })?;
                Self::check_task_target(comment.kind, &target)?;
                // attach the tasks, then bump the owning question so the new
                // checklist reads as its latest activity
                let created = self.store_tasks(&author, &target, descriptions).await;
                comment.updated = self.next_revision();
                created
            }
        };
        tracing::debug!(tasks = ?created, "added tasks");
        // mirror and report the change now that every write lock is released
        self.after_mutation(target).await;
        Ok(created)
    }

    /// Adds completion-gate tasks to an existing question on this forum
    ///
    /// This backs the [`add_tasks`] tool: it parses the question reference and
    /// delegates to [`ForumState::add_tasks_to`].
    ///
    /// # Arguments
    ///
    /// * `author` - The agent creating these tasks
    /// * `question` - The raw question reference ("#post-name" or "!123")
    /// * `tasks` - The task descriptions to attach
    async fn add_tasks(
        &self,
        author: String,
        question: String,
        tasks: Vec<String>,
    ) -> Result<Vec<Task>, ToolExecutionError> {
        // parse the question reference before touching any storage
        let target = ParentRef::parse(&question)?;
        // delegate to the typed core
        self.add_tasks_to(author, target, tasks).await
    }

    /// Seeds pre-built completion-gate tasks under a question, bypassing the
    /// LLM tool plumbing
    ///
    /// This mirrors [`ForumState::add_tasks_to`] for the detector seeder, which
    /// attaches concrete check tasks under the questions it derives before the
    /// agent loop starts.
    ///
    /// # Arguments
    ///
    /// * `author` - The author to attribute the tasks to (e.g. `"detector"`)
    /// * `target` - The question post or comment the tasks gate
    /// * `descriptions` - The task descriptions to attach
    #[instrument(skip_all, err(Debug))]
    pub async fn seed_tasks(
        &self,
        author: String,
        target: ParentRef,
        descriptions: Vec<String>,
    ) -> Result<Vec<Task>, ToolExecutionError> {
        // the typed core already does exactly what seeding needs
        self.add_tasks_to(author, target, descriptions).await
    }

    /// Adds a human operator's completion-gate task under a question
    ///
    /// This is the programmatic entry point a caller (e.g. a TUI) uses to let an
    /// analyst add a checklist item the agents must satisfy before the question
    /// can be marked solved. It builds on [`ForumState::add_tasks_to`].
    ///
    /// # Arguments
    ///
    /// * `target` - The question post or comment the task gates
    /// * `description` - What must be checked or done
    ///
    /// # Errors
    ///
    /// Returns an error if `description` is empty (or only whitespace) or the
    /// target does not exist or is a statement.
    #[instrument(name = "ForumState::add_user_task", skip_all, err(Debug))]
    pub async fn add_user_task(
        &self,
        target: ParentRef,
        description: String,
    ) -> Result<Task, ToolExecutionError> {
        // seed the task onto the forum, attributed to the human operator
        let mut created = self
            .add_tasks_to(USER_AUTHOR.to_string(), target, vec![description])
            .await?;
        // a one-description batch always creates exactly one task
        Ok(created.remove(0))
    }

    /// Marks a task complete, citing the comment that satisfied it
    ///
    /// The typed core shared by the [`complete_task`] tool and a human operator:
    /// the cited comment must already exist (any comment qualifies, not just one
    /// under the task's question) and a task can only be completed once, so the
    /// audit trail of how each gate was met never changes after the fact.
    ///
    /// # Arguments
    ///
    /// * `author` - Who to credit with completing the task
    /// * `task` - The numeric id of the task to complete
    /// * `comment` - The id of the comment holding the completing information
    ///
    /// # Errors
    ///
    /// Returns an error if the task or cited comment does not exist, or the
    /// task is already completed.
    #[instrument(name = "ForumState::complete_task_by", skip_all, err(Debug))]
    pub async fn complete_task_by(
        &self,
        author: String,
        task: usize,
        comment: usize,
    ) -> Result<Task, ToolExecutionError> {
        // the cited comment must exist so the completion's audit trail resolves
        if !self.comments.read().await.contains_key(&comment) {
            return Err(ToolExecutionError::invalid_args(format!(
                "No comment with id {comment}"
            )));
        }
        // complete the task under a scoped write lock so the guard drops before
        // the owning question's revision bump re-locks posts/comments
        let (updated, parent) = {
            let mut guard = self.tasks.write().await;
            // the task must exist to be completed
            let found = guard.get_mut(&task).ok_or_else(|| {
                ToolExecutionError::invalid_args(format!("No task with id {task}"))
            })?;
            // a completion is permanent so the audit trail never silently moves
            if let Some(existing) = &found.completed {
                return Err(ToolExecutionError::invalid_args(format!(
                    "Task {task} is already completed by {} citing !{}; completions are \
                     permanent",
                    existing.author, existing.comment
                )));
            }
            // record how this task was completed and stamp a fresh revision
            found.completed = Some(TaskCompletion { comment, author });
            found.updated = self.next_revision();
            (found.clone(), found.parent.clone())
        };
        tracing::debug!(task = ?updated, "completed task");
        // bump the owning question so the completion reads as its latest activity
        self.touch_target(&parent).await;
        // mirror and report the change now that every write lock is released
        self.after_mutation(parent).await;
        Ok(updated)
    }

    /// Marks a task complete on this forum, citing the completing comment
    ///
    /// This backs the [`complete_task`] tool: it parses the comment reference
    /// and delegates to [`ForumState::complete_task_by`].
    ///
    /// # Arguments
    ///
    /// * `author` - The agent completing this task
    /// * `task` - The numeric id of the task to complete
    /// * `comment` - The raw citing-comment reference ("!123" or a bare id)
    async fn complete_task(
        &self,
        author: String,
        task: usize,
        comment: String,
    ) -> Result<Task, ToolExecutionError> {
        // parse the citing comment reference before touching any storage
        let comment = parse_comment_id(&comment)?;
        // delegate to the typed core
        self.complete_task_by(author, task, comment).await
    }

    /// Lists the incomplete tasks gating a question, sorted by id
    ///
    /// # Arguments
    ///
    /// * `target` - The question post or comment whose open tasks to list
    async fn incomplete_tasks_for(&self, target: &ParentRef) -> Vec<Task> {
        // collect the target's incomplete tasks from under a read lock
        let mut open: Vec<Task> = self
            .tasks
            .read()
            .await
            .values()
            .filter(|task| &task.parent == target && task.completed.is_none())
            .cloned()
            .collect();
        // sort by id so the gate's refusal lists them stably
        open.sort_by_key(|task| task.id);
        open
    }

    /// Counts the open questions and the incomplete tasks attached to them
    ///
    /// Used by the control loop's re-prompt so the agent sees how much work
    /// remains. Both counts come from one snapshot of the forum's read locks,
    /// so they are mutually consistent -- a re-prompt can never report
    /// incomplete tasks on open questions while claiming zero are open.
    pub async fn open_work_counts(&self) -> (usize, usize) {
        // snapshot every map once so both counts agree with each other
        let posts = self.posts.read().await;
        let comments = self.comments.read().await;
        let tasks = self.tasks.read().await;
        // count the question posts and comments that are still open
        let open = posts.values().filter(|post| post.kind.is_open()).count()
            + comments
                .values()
                .filter(|comment| comment.kind.is_open())
                .count();
        // count incomplete tasks whose owning question is still open
        let incomplete = tasks
            .values()
            .filter(|task| task.completed.is_none())
            .filter(|task| match &task.parent {
                ParentRef::Post(name) => posts.get(name).is_some_and(|post| post.kind.is_open()),
                ParentRef::Comment(id) => comments
                    .get(id)
                    .is_some_and(|comment| comment.kind.is_open()),
            })
            .count();
        (open, incomplete)
    }

    /// Errors when this run's analyst-ask budget is already spent
    ///
    /// The ask tool checks this before announcing an interactive ask so a live
    /// analyst is rarely prompted for an ask past the budget;
    /// [`ForumState::record_ask`] re-checks under its write lock for headless
    /// (ticket-less) asks only — an announced ask's human answer is never
    /// discarded at the cap boundary.
    pub(crate) async fn ensure_ask_capacity(&self) -> Result<(), ToolExecutionError> {
        // compare against the cap under a read lock
        if self.asks.read().await.len() >= MAX_ASKS_PER_RUN {
            return Err(ask_cap_error());
        }
        Ok(())
    }

    /// Records a resolved analyst ask on this forum
    ///
    /// Asks are recorded only once resolved -- an analyst answered, the
    /// recommended default was taken with no analyst attached, or the analyst
    /// declined -- so there is never a persisted pending state, and recorded
    /// asks never gate the `finish` tool.
    ///
    /// # Arguments
    ///
    /// * `ask` - The validated ask payload being resolved
    /// * `chosen` - The answer: a chosen option label, or the analyst's free text
    /// * `source` - Who or what produced the answer
    /// * `ticket` - The live ask ticket this resolution answers (0 when no
    ///   analyst UI was attached)
    #[instrument(name = "ForumState::record_ask", skip_all, err(Debug))]
    pub async fn record_ask(
        &self,
        ask: AskRequest,
        chosen: String,
        source: AnswerSource,
        ticket: u64,
    ) -> Result<AnalystAsk, ToolExecutionError> {
        // insert the resolved ask under a scoped write lock so the guard drops
        // before the disk flush re-acquires read locks
        let record = {
            let mut guard = self.asks.write().await;
            // re-check the cap under the write lock only for ticket-less
            // (headless) asks, where the pre-check and this insert run back to
            // back; a ticketed ask was already announced to a live analyst,
            // and their answer must never be discarded at the cap boundary
            // (concurrent conversations bound the overshoot to a handful)
            if ticket == 0 && guard.len() >= MAX_ASKS_PER_RUN {
                return Err(ask_cap_error());
            }
            // hand out the next ask id and stamp a fresh revision
            let id = self.next_ask_id.fetch_add(1, Ordering::SeqCst);
            let record = AnalystAsk {
                id,
                asked_by: ask.asked_by,
                question: ask.question,
                options: ask.options,
                recommended: ask.recommended,
                chosen,
                source,
                updated: self.next_revision(),
            };
            guard.insert(id, record.clone());
            record
        };
        tracing::debug!(ask = ?record, "recorded analyst ask");
        // mirror to disk and report the resolution now that the lock is released
        self.after_analyst_mutation(ThorPalEvent::AnalystAskResolved {
            id: ticket,
            ask: record.id,
            agent: record.asked_by.clone(),
            question: record.question.clone(),
            chosen: record.chosen.clone(),
            source: record.source.wire().to_owned(),
        })
        .await;
        Ok(record)
    }

    /// Raises a "needs analyst review" flag on this forum
    ///
    /// This backs the `flag_for_review` tool and the ask tool's auto-filed
    /// fallback flag. Flags are non-blocking bookkeeping: they never gate the
    /// `finish` tool and never pause the run.
    ///
    /// # Arguments
    ///
    /// * `author` - Who raised the flag (an agent display name, or "system")
    /// * `title` - A short title naming what needs review
    /// * `reason` - Why human eyes are needed
    /// * `reference` - The post/comment the flag concerns, if it anchors to one
    /// * `ask` - The recorded ask this flag was auto-filed for, if any
    ///
    /// # Errors
    ///
    /// Returns an error if `title` or `reason` is empty (or only whitespace),
    /// the referenced post/comment does not exist, or the per-run flag cap is
    /// already spent.
    #[instrument(name = "ForumState::add_review_flag", skip_all, err(Debug))]
    pub async fn add_review_flag(
        &self,
        author: String,
        title: String,
        reason: String,
        reference: Option<ParentRef>,
        ask: Option<usize>,
    ) -> Result<ReviewFlag, ToolExecutionError> {
        // a flag with no title or reason would be unreviewable noise, reject it
        if title.trim().is_empty() {
            return Err(ToolExecutionError::invalid_args(
                "`title` must not be empty".to_owned(),
            ));
        }
        if reason.trim().is_empty() {
            return Err(ToolExecutionError::invalid_args(
                "`reason` must not be empty".to_owned(),
            ));
        }
        // an anchored flag must point at something that actually exists
        if let Some(target) = &reference {
            self.check_parent_exists(target).await?;
        }
        // insert the flag under a scoped write lock so the guard drops before
        // the disk flush re-acquires read locks
        let record = {
            let mut guard = self.flags.write().await;
            // cap the queue so a looping model cannot flood the review queue
            if guard.len() >= MAX_FLAGS_PER_RUN {
                return Err(ToolExecutionError::invalid_args(format!(
                    "Review-flag limit reached ({MAX_FLAGS_PER_RUN} per run); fold this \
                     concern into an existing flagged area or a forum post instead."
                )));
            }
            // hand out the next flag id and stamp a fresh revision
            let id = self.next_flag_id.fetch_add(1, Ordering::SeqCst);
            let record = ReviewFlag {
                id,
                author,
                title,
                reason,
                reference,
                ask,
                updated: self.next_revision(),
            };
            guard.insert(id, record.clone());
            record
        };
        tracing::debug!(flag = ?record, "raised review flag");
        // mirror to disk and report the new flag now that the lock is released
        self.after_analyst_mutation(ThorPalEvent::ReviewFlagRaised {
            flag: record.id,
            agent: record.author.clone(),
            title: record.title.clone(),
            reference: record.reference.clone(),
        })
        .await;
        Ok(record)
    }

    /// Clones every recorded analyst ask, sorted by id for a stable view
    pub async fn asks_snapshot(&self) -> Vec<AnalystAsk> {
        // grab a snapshot of every recorded ask
        let mut all: Vec<AnalystAsk> = self.asks.read().await.values().cloned().collect();
        // sort by id so renders and views are stable
        all.sort_by_key(|ask| ask.id);
        all
    }

    /// Clones every raised review flag, sorted by id for a stable view
    pub async fn flags_snapshot(&self) -> Vec<ReviewFlag> {
        // grab a snapshot of every raised flag
        let mut all: Vec<ReviewFlag> = self.flags.read().await.values().cloned().collect();
        // sort by id so renders and views are stable
        all.sort_by_key(|flag| flag.id);
        all
    }

    /// Lists all posts on the forum, sorted by name for a stable view
    async fn list_posts(&self) -> Result<Vec<Post>, ToolExecutionError> {
        // grab a snapshot of all posts
        let mut all: Vec<Post> = self.posts.read().await.values().cloned().collect();
        // sort by name so the list is stable and easy to scan
        all.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(all)
    }

    /// Builds a structured view of the whole forum for an external viewer
    ///
    /// Every post comes back with its full nested comment thread, sorted by name
    /// like [`ForumState::render_forum`], together with a reference to whichever
    /// post or comment was most recently created or changed.
    pub async fn view(&self) -> ForumView {
        // grab a stable, name-sorted snapshot of every post
        let mut all_posts: Vec<Post> = self.posts.read().await.values().cloned().collect();
        all_posts.sort_by(|a, b| a.name.cmp(&b.name));
        // grab a single snapshot of all comments to build every thread from
        let all_comments = self.comments.read().await;
        // and all tasks so each question's checklist rides on its node
        let all_tasks = self.tasks.read().await;
        // find the most recently touched post or comment by its revision
        let latest_post = all_posts
            .iter()
            .max_by_key(|post| post.updated)
            .map(|post| (post.updated, ParentRef::Post(post.name.clone())));
        // and the most recently touched comment
        let latest_comment = all_comments
            .values()
            .max_by_key(|comment| comment.updated)
            .map(|comment| (comment.updated, ParentRef::Comment(comment.id)));
        // whichever of the two is newer is the latest activity overall
        let last_updated = latest_post
            .into_iter()
            .chain(latest_comment)
            .max_by_key(|(updated, _)| *updated)
            .map(|(_, target)| target);
        // build each post's full comment thread, with its task checklist
        let threads = all_posts
            .into_iter()
            .map(|post| {
                let parent = ParentRef::Post(post.name.clone());
                let comments = build_thread(&all_comments, &all_tasks, &parent);
                let tasks = tasks_of(&all_tasks, &parent);
                PostWithThread {
                    post,
                    tasks,
                    comments,
                }
            })
            .collect();
        // release the thread snapshots before the ask/flag reads re-lock
        drop(all_comments);
        drop(all_tasks);
        // bundle the threads with the latest activity and the analyst records
        ForumView {
            threads,
            last_updated,
            asks: self.asks_snapshot().await,
            flags: self.flags_snapshot().await,
        }
    }

    /// Resolves a `#post-name`/`!id` reference to its node with full thread
    ///
    /// This backs the [`follow_link`] tool and is also reused internally by
    /// [`ForumState::set_status`] and [`ForumState::list_questions`] to return the
    /// updated/matched node together with its nested reply tree.
    ///
    /// # Arguments
    ///
    /// * `reference` - The raw reference string ("#post-name" or "!123")
    async fn follow_link(&self, reference: String) -> Result<ForumNode, ToolExecutionError> {
        // figure out whether this reference points at a post or a comment
        match ParentRef::parse(&reference)? {
            ParentRef::Post(name) => {
                // look up the post itself
                let post = self.posts.read().await.get(&name).cloned().ok_or_else(|| {
                    ToolExecutionError::invalid_args(format!("No post named {name:?}"))
                })?;
                // build its full comment thread, with each question's tasks
                let all = self.comments.read().await;
                let all_tasks = self.tasks.read().await;
                let parent = ParentRef::Post(name);
                let comments = build_thread(&all, &all_tasks, &parent);
                let tasks = tasks_of(&all_tasks, &parent);
                Ok(ForumNode::Post(PostWithThread {
                    post,
                    tasks,
                    comments,
                }))
            }
            ParentRef::Comment(id) => {
                // look up the comment itself
                let all = self.comments.read().await;
                let comment = all.get(&id).cloned().ok_or_else(|| {
                    ToolExecutionError::invalid_args(format!("No comment with id {id}"))
                })?;
                // build its full reply thread, with each question's tasks
                let all_tasks = self.tasks.read().await;
                let replies = build_thread(&all, &all_tasks, &ParentRef::Comment(id));
                let tasks = tasks_of(&all_tasks, &ParentRef::Comment(id));
                Ok(ForumNode::Comment(CommentNode {
                    comment,
                    tasks,
                    replies,
                }))
            }
        }
    }

    /// Updates the resolution status of a question post or comment
    ///
    /// This backs the [`set_status`] tool: it parses the reference and new status
    /// and delegates to [`ForumState::set_question_status`].
    ///
    /// # Arguments
    ///
    /// * `reference` - The raw reference string ("#post-name" or "!123")
    /// * `status` - The raw new status string
    async fn set_status(
        &self,
        reference: String,
        status: String,
    ) -> Result<ForumNode, ToolExecutionError> {
        // parse the reference and the new status before touching any storage
        let target = ParentRef::parse(&reference)?;
        let new_status = QuestionStatus::parse(&status)?;
        // apply the typed status change
        self.set_question_status(target, new_status).await
    }

    /// Sets the resolution status of a question post or comment
    ///
    /// The typed entry point shared by the [`set_status`] tool and a human
    /// operator (e.g. a TUI): it mutates the target under a scoped write lock,
    /// stamps a fresh revision, mirrors and reports the change, and returns the
    /// updated node with its thread.
    ///
    /// # Arguments
    ///
    /// * `target` - The post or comment whose status to set
    /// * `new_status` - The status to set
    ///
    /// # Errors
    ///
    /// Returns an error if the target does not exist, is a statement, or still
    /// has incomplete completion-gate tasks when marking it solved.
    #[instrument(name = "ForumState::set_question_status", skip_all, err(Debug))]
    pub async fn set_question_status(
        &self,
        target: ParentRef,
        new_status: QuestionStatus,
    ) -> Result<ForumNode, ToolExecutionError> {
        // dispatch on whether the reference points at a post or a comment, mutating
        // the target's status under a scoped write lock so the guard drops before we
        // flush to disk (flushing re-acquires read locks)
        match target {
            ParentRef::Post(name) => {
                {
                    // grab the post, rejecting an unknown name or a non-question kind
                    let mut guard = self.posts.write().await;
                    let post = guard.get_mut(&name).ok_or_else(|| {
                        ToolExecutionError::invalid_args(format!("No post named {name:?}"))
                    })?;
                    let slot = post.kind.status_mut().ok_or_else(|| {
                        ToolExecutionError::invalid_args(format!(
                            "Post {name:?} is a statement, not a question, so it has no status"
                        ))
                    })?;
                    // a question with incomplete completion-gate tasks cannot be
                    // marked solved (blocked stays un-gated as the escape hatch);
                    // checking under this write lock is race-free since attaching
                    // tasks requires the same lock
                    if new_status == QuestionStatus::Solved {
                        let gate = ParentRef::Post(name.clone());
                        let incomplete = self.incomplete_tasks_for(&gate).await;
                        if !incomplete.is_empty() {
                            return Err(ToolExecutionError::invalid_args(gate_message(
                                &gate,
                                &incomplete,
                            )));
                        }
                    }
                    *slot = new_status;
                    // stamp a fresh revision so this reads as the latest activity
                    post.updated = self.next_revision();
                }
                // mirror and report the change now that the write lock is released
                self.after_mutation(ParentRef::Post(name.clone())).await;
                // hand back the updated post with its full thread so the caller sees it
                self.follow_link(format!("#{name}")).await
            }
            ParentRef::Comment(id) => {
                {
                    // grab the comment, rejecting an unknown id or a non-question kind
                    let mut guard = self.comments.write().await;
                    let comment = guard.get_mut(&id).ok_or_else(|| {
                        ToolExecutionError::invalid_args(format!("No comment with id {id}"))
                    })?;
                    let slot = comment.kind.status_mut().ok_or_else(|| {
                        ToolExecutionError::invalid_args(format!(
                            "Comment {id} is a statement, not a question, so it has no status"
                        ))
                    })?;
                    // a question with incomplete completion-gate tasks cannot be
                    // marked solved (blocked stays un-gated as the escape hatch);
                    // checking under this write lock is race-free since attaching
                    // tasks requires the same lock
                    if new_status == QuestionStatus::Solved {
                        let gate = ParentRef::Comment(id);
                        let incomplete = self.incomplete_tasks_for(&gate).await;
                        if !incomplete.is_empty() {
                            return Err(ToolExecutionError::invalid_args(gate_message(
                                &gate,
                                &incomplete,
                            )));
                        }
                    }
                    *slot = new_status;
                    // stamp a fresh revision so this reads as the latest activity
                    comment.updated = self.next_revision();
                }
                // mirror and report the change now that the write lock is released
                self.after_mutation(ParentRef::Comment(id)).await;
                // hand back the updated comment with its reply thread
                self.follow_link(format!("!{id}")).await
            }
        }
    }

    /// Lists question posts and comments on the forum filtered by status
    ///
    /// This backs the [`list_questions`] tool.
    ///
    /// # Arguments
    ///
    /// * `status` - The filter ("open"/"all"/a specific status, or empty for open)
    async fn list_questions(&self, status: String) -> Result<Vec<ForumNode>, ToolExecutionError> {
        // build a predicate over a question's status from the requested filter
        let filter = status.trim().to_ascii_lowercase();
        let matches = |question: QuestionStatus| -> bool {
            match filter.as_str() {
                "" | "open" => matches!(
                    question,
                    QuestionStatus::Unsolved | QuestionStatus::NeedsEvidence
                ),
                "all" => true,
                // any other value must be a specific status; reuse the same parser
                _ => QuestionStatus::parse(&filter)
                    .map(|wanted| wanted == question)
                    .unwrap_or(false),
            }
        };
        // reject a filter string that is neither a keyword nor a valid status
        if !matches!(filter.as_str(), "" | "open" | "all") {
            QuestionStatus::parse(&filter)?;
        }
        // collect matching question posts, each with its full thread, sorted by name
        let mut out = Vec::new();
        let mut matching_posts: Vec<Post> = self
            .posts
            .read()
            .await
            .values()
            .filter(|post| matches!(post.kind, Kind::Question(status) if matches(status)))
            .cloned()
            .collect();
        matching_posts.sort_by(|a, b| a.name.cmp(&b.name));
        for post in matching_posts {
            out.push(self.follow_link(format!("#{}", post.name)).await?);
        }
        // collect matching question comments, each with its reply thread, sorted by id
        let mut matching_ids: Vec<usize> = self
            .comments
            .read()
            .await
            .values()
            .filter(|comment| matches!(comment.kind, Kind::Question(status) if matches(status)))
            .map(|comment| comment.id)
            .collect();
        matching_ids.sort_unstable();
        for id in matching_ids {
            out.push(self.follow_link(format!("!{id}")).await?);
        }
        Ok(out)
    }

    /// Declares the run finished, refusing while any question is still open
    ///
    /// This backs the [`finish`] tool: it refuses (returning an error) while any
    /// question is open so the agent gets immediate feedback, otherwise records the
    /// finish request for the control loop to re-confirm.
    async fn finish(&self) -> Result<String, ToolExecutionError> {
        // refuse to finish while any question is still open so the agent gets
        // immediate feedback this turn instead of being silently re-prompted; use
        // list_questions to pull the open ones up and resolve or block each first
        let open = self.open_question_count().await;
        if open > 0 {
            return Err(ToolExecutionError::invalid_args(format!(
                "Cannot finish: {open} question(s) are still unsolved or need evidence. \
                 Resolve them (mark solved with evidence, or blocked) or answer them before finishing. \
                 Use list_questions with status \"open\" to see them."
            )));
        }
        // no questions remain open, so record the finish request; the control loop
        // re-confirms this before it actually stops the run
        self.finish_requested.store(true, Ordering::SeqCst);
        Ok("Finish acknowledged; no questions remain open. The run will end.".to_string())
    }

    /// Counts the question posts and comments that are still open
    ///
    /// Open means [`QuestionStatus::Unsolved`] or [`QuestionStatus::NeedsEvidence`].
    /// The control loop uses this to decide whether it may stop.
    pub async fn open_question_count(&self) -> usize {
        // count open question posts
        let open_posts = self
            .posts
            .read()
            .await
            .values()
            .filter(|post| post.kind.is_open())
            .count();
        // count open question comments
        let open_comments = self
            .comments
            .read()
            .await
            .values()
            .filter(|comment| comment.kind.is_open())
            .count();
        open_posts + open_comments
    }

    /// Whether the given text appears verbatim anywhere on the forum
    ///
    /// Scans post titles and bodies plus comment bodies with a case-sensitive
    /// substring match. The constant tracker uses this to decide whether a
    /// recovered constant still needs to be posted; verbatim matching is the
    /// point, since a decoded value only survives into the report exactly as
    /// it was recovered.
    ///
    /// # Arguments
    ///
    /// * `needle` - The text to look for
    pub async fn contains_text(&self, needle: &str) -> bool {
        // an empty needle matches nothing rather than everything
        if needle.is_empty() {
            return false;
        }
        // scan the post titles and bodies first, dropping the lock right after
        {
            let posts = self.posts.read().await;
            if posts
                .values()
                .any(|post| post.title.contains(needle) || post.body.contains(needle))
            {
                return true;
            }
        }
        // then scan the comment bodies
        let comments = self.comments.read().await;
        comments
            .values()
            .any(|comment| comment.body.contains(needle))
    }

    /// The forum's current revision counter
    ///
    /// Every mutation bumps the counter, so a consumer caching a derived
    /// verdict (e.g. the constant tracker's "not posted yet") can re-test only
    /// when the forum actually changed.
    pub fn current_revision(&self) -> u64 {
        // observe the counter without disturbing it
        self.revision.load(Ordering::SeqCst)
    }

    /// Searches posts and comments for a query, most relevant first
    ///
    /// Backs both the [`search_forum`] tool and the TUI board's search box with
    /// BM25 over post names/titles/bodies and comment bodies. The fitted engine
    /// is cached keyed by the forum revision and refitted lazily on the first
    /// search after a mutation (mirroring the constant tracker's revision-keyed
    /// presence cache), so a burst of queries over an unchanged forum pays for
    /// scoring only. Hits assemble straight from the live maps -- no snapshot
    /// clones. A search racing a mutation can reuse an engine that misses the
    /// item being written (the engine is at most one in-progress mutation
    /// behind the maps); the mutation's own revision bump forces a refit on
    /// the next search, so the lag never outlives one call. An empty or
    /// unmatched query returns no hits.
    ///
    /// # Arguments
    ///
    /// * `query` - The query to rank posts and comments against
    /// * `limit` - The most hits to return
    pub async fn search(&self, query: &str, limit: usize) -> Vec<SearchHit> {
        // an empty query or a zero limit can never match anything
        if query.trim().is_empty() || limit == 0 {
            return Vec::new();
        }
        // read the revision before locking the corpus: the cached engine can
        // only ever be fresher than the revision it is keyed by, never staler
        let revision = self.current_revision();
        // one fit at a time: a concurrent search waits here, then reuses it
        let mut cache = self.search_cache.lock().await;
        // hold the corpus read locks across scoring so hits assemble from the
        // same maps the engine's freshness was judged against (posts before
        // comments, matching every other multi-lock reader)
        let posts = self.posts.read().await;
        let comments = self.comments.read().await;
        // reuse the cached engine for this revision, refitting it if stale
        let engine = search::engine_at(&mut cache, revision, &posts, &comments);
        // release the cache before ranking so equal-revision searches overlap
        drop(cache);
        // rank with the fitted engine; only the top hits pay for snippets
        search::rank(&engine, &posts, &comments, query, limit)
    }

    /// Reads and clears the `finish` request flag set by the [`finish`] tool
    ///
    /// Clearing on read means a finish signal from one iteration cannot leak into
    /// the next, so the control loop only stops on a fresh request.
    pub fn take_finish_requested(&self) -> bool {
        // swap the flag back to false and report whatever it was
        self.finish_requested.swap(false, Ordering::SeqCst)
    }

    /// Whether the [`finish`] tool has requested the run end, without clearing it
    ///
    /// Unlike [`ForumState::take_finish_requested`], this only observes the flag,
    /// so an outside consumer (e.g. a TUI deciding to auto-generate a report) can
    /// check it without disturbing the control loop's take-semantics or drifting
    /// from the flag the last autosave persisted to disk.
    pub fn finish_requested(&self) -> bool {
        // observe the flag without consuming it
        self.finish_requested.load(Ordering::SeqCst)
    }

    /// Renders the entire forum -- every post with its full nested comment thread
    /// -- into a human-readable string for printing before ThorPal exits
    ///
    /// Posts are sorted by name so the dump is stable across runs, and each post's
    /// comment thread is built the same way `follow_link` builds it.
    pub async fn render_forum(&self) -> String {
        // start the dump with a banner so it stands out in the terminal
        let mut out = String::new();
        out.push_str("════════════════════════════════════════\n");
        out.push_str(" ThorPal Forum\n");
        out.push_str("════════════════════════════════════════\n");
        // grab a stable, name-sorted snapshot of every post
        let mut all_posts: Vec<Post> = self.posts.read().await.values().cloned().collect();
        all_posts.sort_by(|a, b| a.name.cmp(&b.name));
        // if nothing was ever posted then say so and bail out early, still
        // appending the analyst tail (an ask can precede any post)
        if all_posts.is_empty() {
            out.push_str("\n(no posts)\n");
            let asks = self.asks_snapshot().await;
            let flags = self.flags_snapshot().await;
            render_analyst_tail(&mut out, &asks, &flags);
            return out;
        }
        // grab a single snapshot of all comments to build every thread from
        let all_comments = self.comments.read().await;
        // and all tasks so question checklists render inline
        let all_tasks = self.tasks.read().await;
        // render each post followed by its full nested comment thread
        for post in &all_posts {
            render_post_thread(&mut out, post, &all_comments, &all_tasks, false);
        }
        // release the thread snapshots before the ask/flag reads re-lock
        drop(all_comments);
        drop(all_tasks);
        // close with the analyst-attention tail: recorded asks, then flags
        let asks = self.asks_snapshot().await;
        let flags = self.flags_snapshot().await;
        render_analyst_tail(&mut out, &asks, &flags);
        out
    }

    /// Renders the linked-mode spawn context for a sub-agent
    ///
    /// The roots (the references scanned out of the sub-agent's goal and task)
    /// are resolved to their posts -- a comment reference resolves to the post
    /// its thread hangs under -- then expanded breadth-first through the
    /// references embedded in each pulled thread's post and comment bodies, up
    /// to `depth` hops. The rendered threads are followed by a titles-only
    /// index of every post so anything outside the neighborhood stays one
    /// `follow_link` away. There is deliberately no hard size cap: the depth
    /// cap is the guard, and a neighborhood only grows with the question's
    /// actual evidence trail.
    ///
    /// Dangling references are skipped silently, and `None` comes back when the
    /// forum has no posts at all so the caller injects nothing.
    ///
    /// # Arguments
    ///
    /// * `roots` - The references scanned out of the sub-agent's goal and task
    /// * `depth` - How many link hops to expand beyond the root threads
    pub async fn render_linked_context(&self, roots: &[ParentRef], depth: usize) -> Option<String> {
        // grab a stable, name-sorted snapshot of every post for the index
        let mut all_posts: Vec<Post> = self.posts.read().await.values().cloned().collect();
        all_posts.sort_by(|a, b| a.name.cmp(&b.name));
        // an empty forum has nothing worth preloading, so inject nothing at all
        if all_posts.is_empty() {
            return None;
        }
        // grab a single snapshot of all comments to build every thread from
        let all_comments = self.comments.read().await;
        // and all tasks so question checklists render inline
        let all_tasks = self.tasks.read().await;
        // index the posts by name so the walk can resolve references cheaply
        let by_name: HashMap<&str, &Post> = all_posts
            .iter()
            .map(|post| (post.name.as_str(), post))
            .collect();
        // resolve a reference to the name of a post that actually exists,
        // sending comment references up to the post their thread hangs under
        let resolve = |reference: &ParentRef| -> Option<String> {
            match reference {
                ParentRef::Post(name) => by_name.contains_key(name.as_str()).then(|| name.clone()),
                ParentRef::Comment(id) => root_post_of(&all_comments, *id)
                    .filter(|name| by_name.contains_key(name.as_str())),
            }
        };
        // resolve the roots in first-seen order; these are the walk's hop 0
        let mut visited: HashSet<String> = HashSet::new();
        let mut ordered: Vec<String> = Vec::new();
        for root in roots {
            // keep the first sighting of each resolved root post
            if let Some(name) = resolve(root)
                && visited.insert(name.clone())
            {
                ordered.push(name);
            }
        }
        // expand the neighborhood breadth-first, one hop level per round
        let mut level = ordered.clone();
        for _ in 0..depth {
            // collect the next hop's new posts out of this level's bodies
            let mut next: Vec<String> = Vec::new();
            for name in &level {
                // scan the post body and every comment body in its thread
                let post = by_name[name.as_str()];
                let mut found = scan_references(&post.body);
                let thread =
                    build_thread(&all_comments, &all_tasks, &ParentRef::Post(name.clone()));
                scan_thread_references(&thread, &mut found);
                // resolve each hit, keeping only posts not already pulled
                for reference in &found {
                    if let Some(name) = resolve(reference)
                        && visited.insert(name.clone())
                    {
                        next.push(name);
                    }
                }
            }
            // stop early once a round discovers nothing new
            if next.is_empty() {
                break;
            }
            // name-sort each hop level so the rendered order is deterministic
            next.sort();
            ordered.extend(next.iter().cloned());
            level = next;
        }
        // render the neighborhood threads, roots first then hop by hop
        let mut out = String::new();
        if ordered.is_empty() {
            // no root resolved to a real post, so degrade to the index alone
            out.push_str("## Your assigned thread\n\n");
            out.push_str(
                "No forum references were found in your task; use the index below \
                 and follow_link to find relevant discussion.\n",
            );
        } else {
            out.push_str("## Your assigned thread and the posts it references\n");
            for name in &ordered {
                render_post_thread(
                    &mut out,
                    by_name[name.as_str()],
                    &all_comments,
                    &all_tasks,
                    false,
                );
            }
        }
        // append the titles-only index of every post, name-sorted, no bodies
        out.push_str("\n## All forum posts (index)\n\n");
        for post in &all_posts {
            out.push_str(&format!(
                "#{} [{}] — {} (by {})\n",
                post.name,
                post.kind.label(),
                post.title,
                post.author
            ));
        }
        Some(out)
    }

    /// Renders the entire forum as boxed ASCII-art blocks for the on-disk dump
    ///
    /// This mirrors [`ForumState::render_forum`]'s stable, name-sorted snapshot
    /// logic but emits the boxed layout written to the forum file rather than the
    /// compact indented layout printed to stdout.
    pub async fn render_forum_boxed(&self) -> String {
        // start the dump buffer
        let mut out = String::new();
        // grab a stable, name-sorted snapshot of every post
        let mut all_posts: Vec<Post> = self.posts.read().await.values().cloned().collect();
        all_posts.sort_by(|a, b| a.name.cmp(&b.name));
        // if nothing was ever posted then say so and bail out early
        if all_posts.is_empty() {
            out.push_str("(no posts)\n");
            return out;
        }
        // grab a single snapshot of all comments to build every thread from
        let all_comments = self.comments.read().await;
        // and all tasks so question checklists render in their boxes
        let all_tasks = self.tasks.read().await;
        // render each post as a box followed by its full nested comment thread
        for post in &all_posts {
            render_post_box(
                &mut out,
                post,
                &tasks_of(&all_tasks, &ParentRef::Post(post.name.clone())),
            );
            // build the post's top-level comments and their replies
            let thread = build_thread(
                &all_comments,
                &all_tasks,
                &ParentRef::Post(post.name.clone()),
            );
            // render each top-level comment, or note the post has no comments at all
            if thread.is_empty() {
                out.push_str("|\n");
                out.push_str("|-> [no child comments shown]\n");
            } else {
                for node in &thread {
                    render_comment_box(&mut out, node, "");
                }
            }
            // separate posts with a blank line so the dump stays readable
            out.push('\n');
        }
        out
    }

    /// Renders the machine-generated forum appendix attached below a report
    ///
    /// This mirrors [`ForumState::render_forum`]'s snapshot logic -- the posts
    /// are cloned out from under their read lock, and the comments are
    /// rendered while their read lock is held (the render is synchronous, so
    /// no await ever runs under the guard) -- then delegates to the pure
    /// [`render_report_appendix`] renderer under the fixed
    /// [`REPORT_APPENDIX_MAX_BYTES`] budget. `None` comes back when nothing on
    /// the forum qualifies for the appendix, so the caller attaches nothing.
    pub async fn report_appendix(&self) -> Option<String> {
        // grab a snapshot of every post for the renderer to partition
        let all_posts: Vec<Post> = self.posts.read().await.values().cloned().collect();
        // and of the analyst records so their sections reach the appendix
        let all_asks = self.asks_snapshot().await;
        let all_flags = self.flags_snapshot().await;
        // grab a single snapshot of all comments to build every thread from
        let all_comments = self.comments.read().await;
        // and all tasks so question checklists reach the appendix
        let all_tasks = self.tasks.read().await;
        // delegate to the pure renderer under the fixed size budget
        render_report_appendix(
            &all_posts,
            &all_comments,
            &all_tasks,
            &all_asks,
            &all_flags,
            REPORT_APPENDIX_MAX_BYTES,
        )
    }

    /// Persists the whole forum to the session store as `forum.json`
    ///
    /// Called after every post/comment mutation so the saved forum always
    /// mirrors the live one and a crash never loses more than the in-flight
    /// turn. This is a no-op when no session store was installed (e.g. a
    /// library consumer that wants no persistence). It is otherwise
    /// best-effort: any filesystem error is logged and swallowed so a failed
    /// write never fails the tool call that triggered it. It must be called
    /// only after any posts/comments write lock has been released, since it
    /// re-acquires read locks to snapshot the forum.
    async fn flush_forum_to_disk(&self) {
        // skip entirely when no session persistence was requested
        let Some(store) = self.store.get() else {
            return;
        };
        // snapshot the forum and save it, logging and swallowing any failure
        if let Err(err) = store.save_forum(&self.snapshot().await).await {
            tracing::warn!(?err, "failed to save forum snapshot to the session store");
        }
    }

    /// Captures the forum's contents into a serializable snapshot
    ///
    /// Clones every post, comment, and task out from under a read lock and reads
    /// the next-id and finish-request scalars, so the result can be serialized
    /// and later handed to [`ForumState::restore`] on a fresh forum.
    pub async fn snapshot(&self) -> ForumSnapshot {
        // clone every post, comment, and task value out from under read locks
        let posts = self.posts.read().await.values().cloned().collect();
        let comments = self.comments.read().await.values().cloned().collect();
        let tasks = self.tasks.read().await.values().cloned().collect();
        // and the analyst records so asks and flags survive a resume
        let asks = self.asks.read().await.values().cloned().collect();
        let flags = self.flags.read().await.values().cloned().collect();
        // clone the detector provenance in a stable node order so snapshots
        // serialize deterministically (the backing map is unordered)
        let mut detector_provenance: Vec<NodeProvenance> =
            self.provenance.read().await.values().cloned().collect();
        detector_provenance.sort_by(|a, b| match (&a.node, &b.node) {
            (ParentRef::Post(a), ParentRef::Post(b)) => a.cmp(b),
            (ParentRef::Comment(a), ParentRef::Comment(b)) => a.cmp(b),
            (ParentRef::Post(_), ParentRef::Comment(_)) => std::cmp::Ordering::Less,
            (ParentRef::Comment(_), ParentRef::Post(_)) => std::cmp::Ordering::Greater,
        });
        // read the scalars the control loop consults so they round-trip too
        ForumSnapshot {
            posts,
            comments,
            tasks,
            asks,
            flags,
            detector_provenance,
            next_comment_id: self.next_comment_id.load(Ordering::SeqCst),
            next_task_id: self.next_task_id.load(Ordering::SeqCst),
            next_ask_id: self.next_ask_id.load(Ordering::SeqCst),
            next_flag_id: self.next_flag_id.load(Ordering::SeqCst),
            finish_requested: self.finish_requested.load(Ordering::SeqCst),
        }
    }

    /// Restores a snapshot's contents onto this (assumed-empty) forum
    ///
    /// Bulk-loads the posts, comments, and tasks preserving their original names
    /// and ids -- unlike [`ForumState::seed_comment`], which hands out fresh ids
    /// -- then restores the next-id and finish-request scalars so the forum
    /// continues exactly where the snapshot left off. Intended to be called once
    /// on a freshly built forum before the resumed run loop starts.
    ///
    /// # Arguments
    ///
    /// * `snapshot` - The forum snapshot to load onto this forum
    pub async fn restore(&self, snapshot: ForumSnapshot) {
        // load the posts back keyed by their original unique names
        {
            let mut posts = self.posts.write().await;
            for post in snapshot.posts {
                posts.insert(post.name.clone(), post);
            }
        }
        // load the comments back keyed by their original numeric ids
        {
            let mut comments = self.comments.write().await;
            for comment in snapshot.comments {
                comments.insert(comment.id, comment);
            }
        }
        // load the tasks back keyed by their original numeric ids
        {
            let mut tasks = self.tasks.write().await;
            for task in snapshot.tasks {
                tasks.insert(task.id, task);
            }
        }
        // load the analyst asks back keyed by their original numeric ids
        {
            let mut asks = self.asks.write().await;
            for ask in snapshot.asks {
                asks.insert(ask.id, ask);
            }
        }
        // load the review flags back keyed by their original numeric ids
        {
            let mut flags = self.flags.write().await;
            for flag in snapshot.flags {
                flags.insert(flag.id, flag);
            }
        }
        // load the detector provenance back keyed by its described nodes
        {
            let mut provenance = self.provenance.write().await;
            for record in snapshot.detector_provenance {
                provenance.insert(record.node.clone(), record);
            }
        }
        // continue revisions past the newest restored item so later changes
        // still sort as the most recent activity
        let newest = {
            let posts = self.posts.read().await;
            let comments = self.comments.read().await;
            let tasks = self.tasks.read().await;
            let asks = self.asks.read().await;
            let flags = self.flags.read().await;
            posts
                .values()
                .map(|post| post.updated)
                .chain(comments.values().map(|comment| comment.updated))
                .chain(tasks.values().map(|task| task.updated))
                .chain(asks.values().map(|ask| ask.updated))
                .chain(flags.values().map(|flag| flag.updated))
                .max()
                .unwrap_or(0)
        };
        // never move the counter backwards if it is already further along
        self.revision.fetch_max(newest + 1, Ordering::SeqCst);
        // bump once more unconditionally: a legacy snapshot whose items all
        // carry updated=0 leaves the fetch_max above at the fresh forum's
        // starting revision, and a verdict cached against that pre-restore
        // revision (the search engine, the constant tracker's presence cache)
        // would otherwise still read as fresh after the bulk load
        self.revision.fetch_add(1, Ordering::SeqCst);
        // restore the scalars so ids continue and finish state is preserved
        self.next_comment_id
            .store(snapshot.next_comment_id, Ordering::SeqCst);
        // derive a safe next task id for snapshots written before tasks existed
        // (serde defaults their next_task_id to 0): never hand out an id a
        // restored task already holds, and never hand out the invalid id 0
        let max_task_id = self.tasks.read().await.keys().max().copied().unwrap_or(0);
        self.next_task_id
            .store(snapshot.next_task_id.max(max_task_id + 1), Ordering::SeqCst);
        // derive safe next ask/flag ids the same way for snapshots written
        // before analyst asks and review flags existed
        let max_ask_id = self.asks.read().await.keys().max().copied().unwrap_or(0);
        self.next_ask_id
            .store(snapshot.next_ask_id.max(max_ask_id + 1), Ordering::SeqCst);
        let max_flag_id = self.flags.read().await.keys().max().copied().unwrap_or(0);
        self.next_flag_id
            .store(snapshot.next_flag_id.max(max_flag_id + 1), Ordering::SeqCst);
        self.finish_requested
            .store(snapshot.finish_requested, Ordering::SeqCst);
        // mirror the restored forum to disk so the on-disk dump matches at once
        self.flush_forum_to_disk().await;
    }
}

/// Recursively builds the nested reply tree under a parent reference
///
/// # Arguments
///
/// * `all` - All comments currently on the forum
/// * `all_tasks` - All completion-gate tasks currently on the forum
/// * `parent` - The parent reference to build the reply tree for
fn build_thread(
    all: &HashMap<usize, Comment>,
    all_tasks: &HashMap<usize, Task>,
    parent: &ParentRef,
) -> Vec<CommentNode> {
    // find the direct children of this parent
    let mut children: Vec<Comment> = all
        .values()
        .filter(|comment| &comment.parent == parent)
        .cloned()
        .collect();
    // sort children oldest first so the thread reads in the order it was written
    children.sort_by_key(|comment| comment.id);
    // recursively build each child's own reply tree, with its task checklist
    children
        .into_iter()
        .map(|comment| {
            let id = comment.id;
            CommentNode {
                replies: build_thread(all, all_tasks, &ParentRef::Comment(id)),
                tasks: tasks_of(all_tasks, &ParentRef::Comment(id)),
                comment,
            }
        })
        .collect()
}

/// Collects the tasks attached to one question, sorted by id
///
/// # Arguments
///
/// * `all` - All completion-gate tasks currently on the forum
/// * `parent` - The question post or comment whose tasks to collect
fn tasks_of(all: &HashMap<usize, Task>, parent: &ParentRef) -> Vec<Task> {
    // find the tasks gating this question
    let mut tasks: Vec<Task> = all
        .values()
        .filter(|task| &task.parent == parent)
        .cloned()
        .collect();
    // sort by id so checklists always render in creation order
    tasks.sort_by_key(|task| task.id);
    tasks
}

/// Parses a citing-comment reference, accepting `!123` or a bare `123`
///
/// # Arguments
///
/// * `raw` - The raw comment reference to parse
fn parse_comment_id(raw: &str) -> Result<usize, ToolExecutionError> {
    // tolerate the `!` reference sigil so both spellings agents use parse
    let trimmed = raw.trim();
    let digits = trimmed.strip_prefix('!').unwrap_or(trimmed);
    // whatever remains must be the comment's numeric id
    digits.parse::<usize>().map_err(|_| {
        ToolExecutionError::invalid_args(format!(
            "Invalid comment reference {raw:?}; expected !<number> or a bare number"
        ))
    })
}

/// Builds the refusal message for solving a question with incomplete tasks
///
/// The first line is self-sufficient (a one-line consumer such as a TUI footer
/// shows only it); the task list and the escape-hatch guidance follow it.
///
/// # Arguments
///
/// * `target` - The question that cannot be solved yet
/// * `incomplete` - Its incomplete tasks, sorted by id
fn gate_message(target: &ParentRef, incomplete: &[Task]) -> String {
    // render the target the way agents reference it (#name or !id)
    let reference = match target {
        ParentRef::Post(name) => format!("#{name}"),
        ParentRef::Comment(id) => format!("!{id}"),
    };
    // the first line alone must carry the refusal and the way forward
    let mut out = format!(
        "Cannot mark {reference} solved: {} task(s) are incomplete. Complete each with \
         complete_task, citing the comment (!id) that contains the information that \
         satisfies it, then set the status again.\n",
        incomplete.len()
    );
    // list each incomplete task so the agent sees exactly what remains
    for task in incomplete {
        out.push_str(&format!("  task {}: {}\n", task.id, task.description));
    }
    // close with the inapplicable-task and truly-blocked escape hatches
    out.push_str(
        "If a task does not apply, post a comment under the question explaining why and \
         complete the task citing that comment. If the question truly cannot be answered, \
         set it \"blocked\" instead.",
    );
    out
}

/// Renders a question's task checklist into `out`, one line per task
///
/// Emits nothing when there are no tasks, so taskless questions render exactly
/// as they did before tasks existed.
///
/// # Arguments
///
/// * `out` - The buffer the rendered checklist is appended to
/// * `tasks` - The tasks to render, sorted by id
/// * `indent` - The indentation prefix for every emitted line
fn render_task_lines(out: &mut String, tasks: &[Task], indent: &str) {
    // a question without tasks renders no checklist at all
    if tasks.is_empty() {
        return;
    }
    // open the checklist with the rule agents must follow to clear it
    out.push_str(&format!(
        "{indent}Tasks (each must be completed with complete_task before this question can \
         be solved):\n"
    ));
    // render each task as a checkbox line, citing how it was completed
    for task in tasks {
        match &task.completed {
            Some(completion) => out.push_str(&format!(
                "{indent}  [x] task {}: {} (completed by {}, see !{})\n",
                task.id, task.description, completion.author, completion.comment
            )),
            None => out.push_str(&format!(
                "{indent}  [ ] task {}: {}\n",
                task.id, task.description
            )),
        }
    }
}

/// Walks a comment's parent chain up to the post its thread hangs under
///
/// A comment reference is resolved to its root post so rendered context
/// carries the claim the comment argues about and its sibling evidence, not an
/// isolated subtree. `None` when the comment does not exist or its parent
/// chain dangles, and a visited guard keeps a malformed parent cycle from
/// looping forever.
///
/// # Arguments
///
/// * `all` - All comments currently on the forum
/// * `id` - The comment id to resolve to its root post
fn root_post_of(all: &HashMap<usize, Comment>, id: usize) -> Option<String> {
    // guard against malformed parent cycles with a visited set
    let mut seen = HashSet::new();
    let mut current = id;
    loop {
        // a repeated id means a parent cycle, so give up rather than loop forever
        if !seen.insert(current) {
            return None;
        }
        // follow this comment's parent one step up the chain
        match &all.get(&current)?.parent {
            // a post parent is the root this comment's thread hangs under
            ParentRef::Post(name) => return Some(name.clone()),
            // a comment parent means there is more chain to climb
            ParentRef::Comment(parent) => current = *parent,
        }
    }
}

/// Recursively collects the references embedded in a comment thread's bodies
///
/// # Arguments
///
/// * `nodes` - The thread nodes whose bodies (and nested replies) to scan
/// * `refs` - The list scanned references are appended to
fn scan_thread_references(nodes: &[CommentNode], refs: &mut Vec<ParentRef>) {
    // scan each node's body and then descend into its replies
    for node in nodes {
        refs.extend(scan_references(&node.comment.body));
        scan_thread_references(&node.replies, refs);
    }
}

/// Renders one post and its full nested comment thread in the compact layout
///
/// This is the per-post block shared by [`ForumState::render_forum`],
/// [`ForumState::render_linked_context`], and [`render_report_appendix`].
///
/// # Arguments
///
/// * `out` - The buffer the rendered thread is appended to
/// * `post` - The post to render
/// * `all_comments` - All comments currently on the forum, to build the thread from
/// * `all_tasks` - All completion-gate tasks currently on the forum
/// * `redact_detector` - Whether to elide detector-authored bodies (headers kept)
fn render_post_thread(
    out: &mut String,
    post: &Post,
    all_comments: &HashMap<usize, Comment>,
    all_tasks: &HashMap<usize, Task>,
    redact_detector: bool,
) {
    // render the post header with its reference name, kind, title, and author
    out.push_str(&format!(
        "\n#{} [{}] — {} (by {})\n",
        post.name,
        post.kind.label(),
        post.title,
        post.author
    ));
    // filter a detector-authored statement body down to its evidence lines
    // when redacting; the file-identity post and detector QUESTIONS stay
    // verbatim (both are bounded and artifact-dense — hashes, endpoints,
    // container listings — exactly what the appendix exists to preserve)
    let exempt = post.name == "file-identity" || matches!(post.kind, Kind::Question(_));
    let body = if redact_detector && post.author == SEED_AUTHOR && !exempt {
        elide_detector_reasoning(&post.body)
    } else {
        post.body.clone()
    };
    // render each line of the post body indented under the header
    for line in body.lines() {
        out.push_str(&format!("  {line}\n"));
    }
    // render the question's task checklist, if it carries one
    render_task_lines(
        out,
        &tasks_of(all_tasks, &ParentRef::Post(post.name.clone())),
        "  ",
    );
    // build and render the post's top-level comments and their replies
    let thread = build_thread(all_comments, all_tasks, &ParentRef::Post(post.name.clone()));
    for node in &thread {
        render_comment_node(out, node, 1, redact_detector);
    }
}

/// Checks whether a comment thread contains at least one non-detector comment
///
/// Used to decide whether a detector-seeded statement post earned a slot in the
/// report appendix: a seed only matters once an agent actually weighed in on it.
///
/// # Arguments
///
/// * `nodes` - The thread nodes (comments plus their nested replies) to scan
fn thread_has_non_detector_comment(nodes: &[CommentNode]) -> bool {
    // check each node's author and then descend into its replies
    nodes.iter().any(|node| {
        node.comment.author != SEED_AUTHOR || thread_has_non_detector_comment(&node.replies)
    })
}

/// Renders the deterministic forum-evidence appendix attached below a report
///
/// Posts are partitioned into five sections rendered in order -- solved
/// questions, open/blocked questions, agent finding posts, adjudicated
/// detector seeds, and a last-priority digest of unreplicated detector
/// evidence -- name-sorted within each like [`ForumState::render_forum`].
/// Every qualifying post renders with its full nested comment thread;
/// detector-authored bodies keep their evidence lines (sites, artifacts)
/// with only the reasoning prose elided, while agent bodies are kept
/// verbatim and never truncated. A detector-seeded statement post renders in
/// full once its thread holds at least one non-detector comment; otherwise
/// its evidence lines still survive through the digest section, so a decoded
/// value recovered only by a detector can never silently vanish. The
/// file-identity post always renders in full with the finding posts — its
/// hash and timestamp lines are identity facts the digest's line filter
/// would drop.
///
/// Whole thread blocks are appended while they fit inside `budget`; a block
/// that would overflow is skipped entirely and counted in a closing omission
/// note pointing at the complete `forum.json`. `None` comes back when no post
/// qualifies for any section.
///
/// # Arguments
///
/// * `posts` - Every post on the forum
/// * `comments` - All comments currently on the forum, to build threads from
/// * `tasks` - All completion-gate tasks currently on the forum
/// * `asks` - Every recorded analyst ask, sorted by id
/// * `flags` - Every raised review flag, sorted by id
/// * `budget` - The size bound (in bytes) on the rendered appendix
pub(crate) fn render_report_appendix(
    posts: &[Post],
    comments: &HashMap<usize, Comment>,
    tasks: &HashMap<usize, Task>,
    asks: &[AnalystAsk],
    flags: &[ReviewFlag],
    budget: usize,
) -> Option<String> {
    // name-sort a snapshot of the posts so every section renders stably
    let mut sorted: Vec<&Post> = posts.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    // partition the posts into the appendix's four sections
    let mut solved: Vec<&Post> = Vec::new();
    let mut open: Vec<&Post> = Vec::new();
    let mut findings: Vec<&Post> = Vec::new();
    let mut adjudicated: Vec<&Post> = Vec::new();
    let mut unreplicated: Vec<&Post> = Vec::new();
    for post in sorted {
        match &post.kind {
            // solved questions keep every author, including detector seeds
            Kind::Question(QuestionStatus::Solved) => solved.push(post),
            // every remaining question lands in open/blocked with its status label
            Kind::Question(_) => open.push(post),
            // the file-identity post renders in full whatever its thread looks
            // like: its hash and timestamp lines are scoring-critical identity
            // facts the evidence digest's line filter would otherwise drop
            Kind::Statement if post.name == "file-identity" => findings.push(post),
            // agent-authored statements are findings, kept in full
            Kind::Statement if post.author != SEED_AUTHOR => findings.push(post),
            // a detector seed renders in full once an agent weighed in on it;
            // otherwise its evidence lines still survive via the digest below
            Kind::Statement => {
                let thread = build_thread(comments, tasks, &ParentRef::Post(post.name.clone()));
                if thread_has_non_detector_comment(&thread) {
                    adjudicated.push(post);
                } else {
                    unreplicated.push(post);
                }
            }
        }
    }
    // nothing qualified anywhere, so there is no appendix to attach
    if solved.is_empty()
        && open.is_empty()
        && findings.is_empty()
        && adjudicated.is_empty()
        && unreplicated.is_empty()
        && asks.is_empty()
        && flags.is_empty()
    {
        return None;
    }
    // open the appendix with its fixed banner and track skipped blocks
    let mut out = String::from(REPORT_APPENDIX_HEADER);
    let mut omitted: usize = 0;
    // the question sections render first, in their fixed order
    render_post_sections(
        &mut out,
        &mut omitted,
        vec![
            ("Solved questions", solved),
            ("Open / blocked questions", open),
        ],
        comments,
        tasks,
        budget,
    );
    // the analyst-attention sections render next, before the long finding
    // dumps: they are the human's follow-up queue, so they outrank the dumps
    // for budget priority
    render_analyst_sections(&mut out, &mut omitted, asks, flags, budget);
    // then the finding dumps and the adjudicated detector seeds
    render_post_sections(
        &mut out,
        &mut omitted,
        vec![
            ("Finding posts", findings),
            ("Detector seed adjudications", adjudicated),
        ],
        comments,
        tasks,
        budget,
    );
    // the fifth, last-priority section digests the detector seeds no agent
    // adjudicated: title plus evidence lines only, so a value recovered only
    // by a detector still reaches the report deterministically
    if !unreplicated.is_empty() {
        let header = "\n### Unreplicated detector evidence\n";
        let mut header_pending = true;
        for post in unreplicated {
            // digest off to the side to measure it against the budget
            let mut block = String::new();
            render_evidence_digest(&mut block, post, comments, tasks);
            let cost = block.len() + if header_pending { header.len() } else { 0 };
            if out.len() + cost > budget {
                omitted += 1;
                continue;
            }
            if header_pending {
                out.push_str(header);
                header_pending = false;
            }
            out.push_str(&block);
        }
    }
    // close with an omission note so a bounded appendix is never silently partial
    if omitted > 0 {
        out.push_str(&format!(
            "\n[{omitted} thread(s) omitted to stay within the appendix size bound; \
             the complete forum is in the session store's forum.json]\n"
        ));
    }
    Some(out)
}

/// Renders a run of post sections into the appendix under a shared budget
///
/// Whole thread blocks are appended while they fit inside `budget`; a block
/// that would overflow is skipped entirely and counted in `omitted`. A section
/// header is deferred until its first block actually fits, so a fully omitted
/// section never leaves a dangling empty header behind.
///
/// # Arguments
///
/// * `out` - The appendix buffer to append to
/// * `omitted` - The running count of blocks skipped for the budget
/// * `sections` - The titled post sections to render, in order
/// * `comments` - All comments currently on the forum, to build threads from
/// * `tasks` - All completion-gate tasks currently on the forum
/// * `budget` - The size bound (in bytes) on the rendered appendix
fn render_post_sections(
    out: &mut String,
    omitted: &mut usize,
    sections: Vec<(&str, Vec<&Post>)>,
    comments: &HashMap<usize, Comment>,
    tasks: &HashMap<usize, Task>,
    budget: usize,
) {
    for (title, section) in sections {
        // a section with no qualifying posts emits nothing at all
        if section.is_empty() {
            continue;
        }
        // defer the section header until its first block actually fits, so a
        // fully omitted section never leaves a dangling empty header behind
        let header = format!("\n### {title}\n");
        let mut header_pending = true;
        for post in section {
            // render the whole thread block off to the side to measure it
            let mut block = String::new();
            render_post_thread(&mut block, post, comments, tasks, true);
            // the first fitting block also pays for its section header
            let cost = block.len() + if header_pending { header.len() } else { 0 };
            // skip (and count) the whole block when it would overflow the budget
            if out.len() + cost > budget {
                *omitted += 1;
                continue;
            }
            // emit the section header once, then the block itself
            if header_pending {
                out.push_str(&header);
                header_pending = false;
            }
            out.push_str(&block);
        }
    }
}

/// Renders the analyst-attention sections into the appendix under the budget
///
/// Recorded asks render under "Analyst decisions" and review flags under
/// "Needs analyst review", with the same whole-block budget discipline as the
/// post sections: an overflowing block is skipped and counted in `omitted`,
/// and a section header is deferred until its first block fits.
///
/// # Arguments
///
/// * `out` - The appendix buffer to append to
/// * `omitted` - The running count of blocks skipped for the budget
/// * `asks` - Every recorded analyst ask, sorted by id
/// * `flags` - Every raised review flag, sorted by id
/// * `budget` - The size bound (in bytes) on the rendered appendix
fn render_analyst_sections(
    out: &mut String,
    omitted: &mut usize,
    asks: &[AnalystAsk],
    flags: &[ReviewFlag],
    budget: usize,
) {
    // render the recorded asks under their own deferred header
    if !asks.is_empty() {
        let header = "\n### Analyst decisions\n";
        let mut header_pending = true;
        for ask in asks {
            // render the block off to the side to measure it against the budget
            let mut block = String::new();
            render_ask(&mut block, ask);
            let cost = block.len() + if header_pending { header.len() } else { 0 };
            // skip (and count) the whole block when it would overflow the budget
            if out.len() + cost > budget {
                *omitted += 1;
                continue;
            }
            // emit the section header once, then the block itself
            if header_pending {
                out.push_str(header);
                header_pending = false;
            }
            out.push_str(&block);
        }
    }
    // then the review flags, under the same discipline
    if !flags.is_empty() {
        let header = "\n### Needs analyst review\n";
        let mut header_pending = true;
        for flag in flags {
            // render the block off to the side to measure it against the budget
            let mut block = String::new();
            render_flag(&mut block, flag);
            let cost = block.len() + if header_pending { header.len() } else { 0 };
            // skip (and count) the whole block when it would overflow the budget
            if out.len() + cost > budget {
                *omitted += 1;
                continue;
            }
            // emit the section header once, then the block itself
            if header_pending {
                out.push_str(header);
                header_pending = false;
            }
            out.push_str(&block);
        }
    }
}

/// Appends the compact analyst-attention tail to a rendered forum dump
///
/// Recorded analyst asks render first, then the review flags; nothing is
/// appended when the run recorded neither, so pre-feature dumps are unchanged.
///
/// # Arguments
///
/// * `out` - The forum dump buffer to append to
/// * `asks` - Every recorded analyst ask, sorted by id
/// * `flags` - Every raised review flag, sorted by id
fn render_analyst_tail(out: &mut String, asks: &[AnalystAsk], flags: &[ReviewFlag]) {
    // render the recorded asks first: they are decisions, not open work
    if !asks.is_empty() {
        out.push_str("\n──── Analyst decisions ────\n");
        for ask in asks {
            render_ask(out, ask);
        }
    }
    // then the review queue the analyst still needs to work through
    if !flags.is_empty() {
        out.push_str("\n──── Needs analyst review ────\n");
        for flag in flags {
            render_flag(out, flag);
        }
    }
}

/// Renders one recorded analyst ask as a compact multi-line block
///
/// # Arguments
///
/// * `out` - The buffer to append the block to
/// * `ask` - The recorded ask to render
fn render_ask(out: &mut String, ask: &AnalystAsk) {
    // one header line names the ask, its question, and who raised it
    out.push_str(&format!(
        "[ask {}] {} (asked by {})\n",
        ask.id, ask.question, ask.asked_by
    ));
    // each option renders on its own line with the recommended one marked
    for option in &ask.options {
        let marker = if option.label == ask.recommended {
            " (recommended)"
        } else {
            ""
        };
        out.push_str(&format!(
            "  - {}{marker}: {}\n",
            option.label, option.description
        ));
    }
    // close with the answer and who or what chose it
    out.push_str(&format!(
        "  → chosen: {:?} — {}\n",
        ask.chosen,
        ask.source.label()
    ));
}

/// Renders one review flag as a compact multi-line block
///
/// # Arguments
///
/// * `out` - The buffer to append the block to
/// * `flag` - The review flag to render
fn render_flag(out: &mut String, flag: &ReviewFlag) {
    // one header line names the flag, who raised it, and its anchor if any
    let anchor = match &flag.reference {
        Some(ParentRef::Post(name)) => format!(" → #{name}"),
        Some(ParentRef::Comment(id)) => format!(" → !{id}"),
        None => String::new(),
    };
    out.push_str(&format!(
        "[flag {}] {} (by {}){anchor}\n",
        flag.id, flag.title, flag.author
    ));
    // the reason renders indented under the header
    out.push_str(&format!("  {}\n", flag.reason));
    // note the recorded ask this flag was auto-filed for, if any
    if let Some(ask) = flag.ask {
        out.push_str(&format!("  (auto-filed for ask {ask})\n"));
    }
}

/// The error returned once a run's `ask_analyst` budget is spent
///
/// Tells the model how to proceed without the analyst instead of retrying.
pub(crate) fn ask_cap_error() -> ToolExecutionError {
    // point the model at the non-blocking fallback instead of a dead end
    ToolExecutionError::invalid_args(format!(
        "Ask limit reached ({MAX_ASKS_PER_RUN} per run): proceed on your best judgment \
         and use flag_for_review to mark the decision for later human review instead."
    ))
}

/// Renders an unadjudicated detector thread as a compact evidence digest
///
/// One header line (`#name — title`), then for each detector comment its
/// finding line plus any `Artifact (`/`Sites (` lines — nothing else, so the
/// section stays cheap while every recovered value survives.
///
/// # Arguments
///
/// * `out` - The buffer to append the digest to
/// * `post` - The unadjudicated detector post to digest
/// * `comments` - All comments on the forum, to pull the post's thread from
/// * `tasks` - All completion-gate tasks on the forum, to build the thread with
fn render_evidence_digest(
    out: &mut String,
    post: &Post,
    comments: &HashMap<usize, Comment>,
    tasks: &HashMap<usize, Task>,
) {
    // one compact header names the thread the digest came from
    out.push_str(&format!("\n#{} — {}\n", post.name, post.title));
    // the post body's own evidence lines come first (rollups carry them)
    for line in post.body.lines() {
        if line.starts_with("Artifact (") || line.starts_with("Sites (") || line.starts_with("- ") {
            out.push_str(&format!("  {line}\n"));
        }
    }
    // then each finding comment's first line and its artifact/site lines
    let thread = build_thread(comments, tasks, &ParentRef::Post(post.name.clone()));
    for node in &thread {
        let mut lines = node.comment.body.lines();
        if let Some(first) = lines.next() {
            out.push_str(&format!("  {first}\n"));
        }
        for line in lines {
            if line.starts_with("Artifact (") || line.starts_with("Sites (") {
                out.push_str(&format!("    {line}\n"));
            }
        }
    }
}

#[rig::tool_macro(
    description = "Creates a new top-level post on the shared forum to raise a single question, claim, or finding for other agents to discuss and provide evidence on. Prefer many small, targeted posts over one large post: each post should state a single fact, question, or problem. Split unrelated findings into separate posts. name must be unique and contain no spaces (used to reference this post elsewhere as #name). Set kind to \"statement\" for a fact/claim/finding or \"question\" to ask something that needs answering. For a question, set status to \"unsolved\" (no answer yet), \"solved\" (answered with enough evidence to accept), \"needs_evidence\" (answered but not yet trusted), or \"blocked\" (cannot be answered due to missing tools/information); a new question is normally \"unsolved\". status is ignored for statements. For a question you may also pass tasks: a list of short, concrete completion-gate checklist items the answer must cover (e.g. \"check the function at 0x401000\"); the question cannot be marked \"solved\" until every task is completed with complete_task citing an evidence comment. tasks are rejected for statements and for a question created directly as \"solved\"; use add_tasks to add them to an existing question later. The body supports Markdown -- use short paragraphs, bullet lists, and fenced code blocks for disassembly, hex dumps, or code."
)]
pub async fn create_post(
    #[rig(context)] context: &mut ToolContext,
    author: String,
    name: String,
    title: String,
    body: String,
    kind: String,
    status: String,
    tasks: Option<Vec<String>>,
) -> Result<CreatePostResult, ToolExecutionError> {
    // recover this run's shared forum and create the post on it
    let forum = ForumState::from_context(context)?;
    forum
        .create_post(
            author,
            name,
            title,
            body,
            kind,
            status,
            tasks.unwrap_or_default(),
        )
        .await
}

#[rig::tool_macro(
    description = "Comments on a post or replies to another comment. Keep each comment to a single point of evidence. parent is a reference: \"#post-name\" to comment on a post, or \"!123\" to reply to comment 123. Set stance to \"supports\", \"disproves\", or \"neutral\" to say how this comment's evidence relates to what it's replying to. Set kind to \"statement\" for an answer/evidence/finding (the usual case) or \"question\" to raise a follow-up question. For a question, set status to \"unsolved\", \"solved\", \"needs_evidence\", or \"blocked\" (normally \"unsolved\" for a new one); status is ignored for statements. The body supports Markdown -- use short paragraphs, bullet lists, and fenced code blocks for disassembly, hex dumps, or code."
)]
pub async fn create_comment(
    #[rig(context)] context: &mut ToolContext,
    author: String,
    parent: String,
    body: String,
    stance: String,
    kind: String,
    status: String,
) -> Result<Comment, ToolExecutionError> {
    // recover this run's shared forum and create the comment on it
    let forum = ForumState::from_context(context)?;
    forum
        .create_comment(author, parent, body, stance, kind, status)
        .await
}

#[rig::tool_macro(
    description = "Lists all posts on the shared forum so you can browse existing discussion before creating a new post."
)]
pub async fn list_posts(
    #[rig(context)] context: &mut ToolContext,
) -> Result<Vec<Post>, ToolExecutionError> {
    // recover this run's shared forum and list its posts
    let forum = ForumState::from_context(context)?;
    forum.list_posts().await
}

#[rig::tool_macro(
    description = "Looks up a post or comment by reference (\"#post-name\" or \"!123\") and returns it with its full nested reply thread. Use this to follow a reference mentioned in another post or comment."
)]
pub async fn follow_link(
    #[rig(context)] context: &mut ToolContext,
    reference: String,
) -> Result<ForumNode, ToolExecutionError> {
    // recover this run's shared forum and resolve the reference on it
    let forum = ForumState::from_context(context)?;
    forum.follow_link(reference).await
}

#[rig::tool_macro(
    description = "Searches the shared forum's posts and comments and returns the most relevant hits, best first. Use this before posting (and instead of walking the whole board) to find existing discussion of a value, address, API, or topic -- e.g. \"string decryption\", \"CreateRemoteThread\", \"0x401000\". Matching is lexical (BM25) over post names, titles, and bodies plus comment bodies, so search the literal terms you expect to appear; CamelCase and snake_case names also match by their words. Each hit carries its reference (\"#post-name\" or \"!123\"), a title (a comment hit shows its root post's title), a short snippet, kind/status, stance, author, and score -- enough to decide whether to pull the full thread with follow_link. limit caps the hits returned (default 8, at most 25)."
)]
pub async fn search_forum(
    #[rig(context)] context: &mut ToolContext,
    query: String,
    limit: Option<usize>,
) -> Result<Vec<SearchHit>, ToolExecutionError> {
    // an empty query would rank nothing meaningfully, so reject it loudly
    if query.trim().is_empty() {
        return Err(ToolExecutionError::invalid_args(
            "`query` must not be empty".to_owned(),
        ));
    }
    // clamp the limit so one call cannot flood the context with tail matches
    let limit = limit
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
        .clamp(1, MAX_SEARCH_LIMIT);
    // recover this run's shared forum and search it
    let forum = ForumState::from_context(context)?;
    Ok(forum.search(&query, limit).await)
}

#[rig::tool_macro(
    description = "Updates the resolution status of a question post or comment. reference is \"#post-name\" or \"!123\". status must be \"unsolved\" (no answer yet), \"solved\" (answered with enough evidence to accept), \"needs_evidence\" (answered but not yet trusted), or \"blocked\" (cannot be answered due to missing tools/information). Only works on posts/comments whose kind is a question; mark a question solved once you have posted an answer with supporting evidence as a comment under it. Marking \"solved\" is refused while the question has incomplete tasks -- complete each with complete_task (citing the evidence comment) first; \"blocked\" is not gated."
)]
pub async fn set_status(
    #[rig(context)] context: &mut ToolContext,
    reference: String,
    status: String,
) -> Result<ForumNode, ToolExecutionError> {
    // recover this run's shared forum and update the target's status on it
    let forum = ForumState::from_context(context)?;
    forum.set_status(reference, status).await
}

#[rig::tool_macro(
    description = "Adds completion-gate tasks (checklist items) to an existing question. question is a reference: \"#post-name\" or \"!123\", and must be a question that is not already solved (not a statement; reopen a solved question with set_status first). tasks is a list of short, concrete, checkable descriptions (e.g. \"check the function at 0x401000\", \"verify the decoded config at 0x4051a0\"), at most 20 per call. A question cannot be marked \"solved\" while any of its tasks is incomplete; each task is completed with complete_task, citing the comment that contains the information that satisfies it. Attach tasks when a question has concrete verification steps; not every question needs them."
)]
pub async fn add_tasks(
    #[rig(context)] context: &mut ToolContext,
    author: String,
    question: String,
    tasks: Vec<String>,
) -> Result<Vec<Task>, ToolExecutionError> {
    // recover this run's shared forum and add the tasks on it
    let forum = ForumState::from_context(context)?;
    forum.add_tasks(author, question, tasks).await
}

#[rig::tool_macro(
    description = "Marks a completion-gate task complete, citing the comment that contains the information that completed it. task is the numeric id shown as \"task <id>\" in its question's checklist. comment is the citing comment's reference (\"!123\" or a bare id) and must already exist -- post the evidence comment first, then complete the task citing it. A task can only be completed once. If a task does not apply, post a comment under the question explaining why and complete the task citing that comment."
)]
pub async fn complete_task(
    #[rig(context)] context: &mut ToolContext,
    author: String,
    task: usize,
    comment: String,
) -> Result<Task, ToolExecutionError> {
    // recover this run's shared forum and complete the task on it
    let forum = ForumState::from_context(context)?;
    forum.complete_task(author, task, comment).await
}

#[rig::tool_macro(
    description = "Lists question posts and comments on the forum filtered by status. status must be \"open\" (unsolved and needs_evidence, the default when empty), \"all\" (every question regardless of status), or a specific status (\"unsolved\", \"solved\", \"needs_evidence\", \"blocked\"). Use this to find the questions that still need answering."
)]
pub async fn list_questions(
    #[rig(context)] context: &mut ToolContext,
    status: String,
) -> Result<Vec<ForumNode>, ToolExecutionError> {
    // recover this run's shared forum and list its questions
    let forum = ForumState::from_context(context)?;
    forum.list_questions(status).await
}

#[rig::tool_macro(
    description = "Declares that you have finished: every question on the forum is resolved (solved or blocked) and you have no further questions to raise. Only call this once there is nothing left open and nothing left to ask; the run ends after this. If any question is still unsolved or needs_evidence, this refuses and tells you how many remain so you can resolve them first."
)]
pub async fn finish(
    #[rig(context)] context: &mut ToolContext,
) -> Result<String, ToolExecutionError> {
    // recover this run's shared forum and record the finish request on it
    let forum = ForumState::from_context(context)?;
    // nudge the model toward a descriptive session rename exactly once, and
    // only when finish would otherwise succeed, so the open-question refusal
    // keeps precedence and a failing rename can never deadlock the run
    if forum.open_question_count().await == 0 {
        // first rung: decoded constants the forum never carried are an
        // evidence-completeness gap of the same class as an open question, and
        // they must land before the rename derives a name from the findings;
        // the one-shot latch means a run that declines to post them finishes
        // on its next call rather than deadlocking
        if let Some(tracker) = ConstantTracker::from_context(context) {
            let unposted = tracker.unposted(&forum, MAX_FINISH_CONSTANTS).await;
            if !unposted.is_empty() && tracker.take_finish_nudge() {
                // list the constants verbatim so the model can post them as-is
                let list = unposted
                    .iter()
                    .map(|value| format!("`{value}`"))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(ToolExecutionError::invalid_args(format!(
                    "Before finishing, {} decoded constant(s) recovered by tools \
                     were never posted to the forum: {list}. Post each meaningful \
                     one verbatim (create_post or create_comment, citing where it \
                     was recovered), then call finish again.",
                    unposted.len(),
                )));
            }
        }
        if let Some(store) = SessionStore::try_from_context(context) {
            if store.needs_rename() && store.take_rename_nudge() {
                return Err(ToolExecutionError::invalid_args(
                    "Before finishing, rename this session: call rename_session with a \
                     short, descriptive, alphanumeric name derived from your findings \
                     (the malware family, or the most identifying behavior or IOC -- \
                     never an IP, MAC address, or hash), then call finish again.",
                ));
            }
            // nudge the model to review this session's scripts for library
            // promotion exactly once, only when a library is configured and the
            // session actually saved scripts; the one-shot latch means a run
            // that decides nothing qualifies finishes on its next call
            if ScriptLibrary::try_from_context(context).is_some()
                && store
                    .list_scripts()
                    .await
                    .map(|scripts| !scripts.is_empty())
                    .unwrap_or(false)
                && store.take_promote_nudge()
            {
                return Err(ToolExecutionError::invalid_args(
                    "Before finishing, review this session's scripts for the \
                     persistent library: delegate to the developer to list_scripts \
                     and promote_script any that proved useful and are generic \
                     (binary-specific inputs flow through args, with an honest \
                     description and usage), or decide none qualify -- then call \
                     finish again.",
                ));
            }
            // nudge the model to bank this run's durable generic lessons into
            // the cross-run memory library exactly once, last in the ladder so
            // the review reads the settled forum and the script decisions; the
            // one-shot latch means a run that decides nothing qualifies
            // finishes on its next call
            if MemoryLibrary::try_from_context(context).is_some() && store.take_memory_nudge() {
                return Err(ToolExecutionError::invalid_args(
                    "Before finishing, review this run for durable GENERIC lessons \
                     worth keeping across runs (a technique that worked, a workflow \
                     rule or pitfall, verified family intel): delegate to the \
                     researcher to save_memory each one (generic knowledge only -- \
                     no sample hashes, paths, or session facts; those stay on the \
                     forum), or decide none qualify -- then call finish again.",
                ));
            }
        }
    }
    forum.finish().await
}

/// Renders a single comment node and all of its nested replies into `out`
///
/// # Arguments
///
/// * `out` - The string buffer to append the rendered comment to
/// * `node` - The comment node (comment plus its reply tree) to render
/// * `depth` - How deep this comment sits in the thread, used for indentation
/// * `redact_detector` - Whether to elide detector-authored bodies (headers kept)
fn render_comment_node(out: &mut String, node: &CommentNode, depth: usize, redact_detector: bool) {
    // indent replies two spaces per level so the thread structure is visible
    let indent = "  ".repeat(depth);
    // render this comment's header line with its id, kind, stance, and author
    out.push_str(&format!(
        "{indent}!{} [{}] [{:?}] by {}\n",
        node.comment.id,
        node.comment.kind.label(),
        node.comment.stance,
        node.comment.author
    ));
    // filter a detector-authored body down to its evidence lines when
    // redacting — the Artifact/Sites values must survive into the appendix
    let body = if redact_detector && node.comment.author == SEED_AUTHOR {
        elide_detector_reasoning(&node.comment.body)
    } else {
        node.comment.body.clone()
    };
    // render each line of the body at the same indentation as the header
    for line in body.lines() {
        out.push_str(&format!("{indent}  {line}\n"));
    }
    // render the question's task checklist, if this comment carries one
    render_task_lines(out, &node.tasks, &format!("{indent}  "));
    // recurse into this comment's replies one level deeper
    for reply in &node.replies {
        render_comment_node(out, reply, depth + 1, redact_detector);
    }
}

/// Word-wraps `text` to at most `width` characters per line
///
/// Whitespace-separated words are packed greedily onto each line; a single word
/// longer than `width` is emitted on its own (over-long) line rather than being
/// split mid-word, since forum content often contains long hex/identifier runs
/// that would be meaningless if broken. An empty input yields a single empty line
/// so a blank body still renders as one (blank) row.
///
/// # Arguments
///
/// * `text` - The text to wrap
/// * `width` - The maximum number of characters per line
fn wrap(text: &str, width: usize) -> Vec<String> {
    // collect the wrapped lines as we pack words onto the current line
    let mut lines = Vec::new();
    // the line currently being built up word by word
    let mut current = String::new();
    // pack each whitespace-separated word onto the current line where it fits
    for word in text.split_whitespace() {
        // an empty current line just takes the word as-is
        if current.is_empty() {
            current.push_str(word);
        } else if current.chars().count() + 1 + word.chars().count() <= width {
            // the word plus a joining space still fits, so append it
            current.push(' ');
            current.push_str(word);
        } else {
            // the word does not fit, so flush the current line and start fresh
            lines.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    // flush whatever is left, always emitting at least one (possibly empty) line
    lines.push(current);
    lines
}

/// Builds a single boxed content row, padded to the box's inner width
///
/// # Arguments
///
/// * `content` - The text to place inside the box row
fn box_row(content: &str) -> String {
    // pad the content to the fixed inner width so every row's right border lines up
    format!("| {content:<width$} |\n", width = BOX_INNER_WIDTH)
}

/// Builds a horizontal box rule spanning the box's full width
fn box_rule() -> String {
    // the rule spans the inner width plus the single space of padding on each side
    format!("+{}+\n", "-".repeat(BOX_INNER_WIDTH + 2))
}

/// Renders a labelled, wrapped field as one or more boxed rows
///
/// The first row reads `label   : value`; any wrapped continuation rows are
/// indented to line up under the value. Used for the `name`/`author`/`title`/
/// `stance` header fields of a post or comment box.
///
/// # Arguments
///
/// * `out` - The buffer to append the rendered rows to
/// * `label` - The field label (e.g. `"title"`)
/// * `value` - The field value, wrapped across rows if it is long
fn render_field(out: &mut String, label: &str, value: &str) {
    // the label column is padded so the ": " separators of every field align
    let prefix = format!("{label:<7}: ");
    // the value wraps into whatever width is left after the label prefix
    let value_width = BOX_INNER_WIDTH - prefix.chars().count();
    // continuation rows are blank-padded to sit directly under the value column
    let indent = " ".repeat(prefix.chars().count());
    // wrap the value and emit the first row with the label, the rest indented
    for (idx, line) in wrap(value, value_width).into_iter().enumerate() {
        if idx == 0 {
            out.push_str(&box_row(&format!("{prefix}{line}")));
        } else {
            out.push_str(&box_row(&format!("{indent}{line}")));
        }
    }
}

/// Renders a wrapped body block into a `BODY` box section
///
/// Each source line of `body` is wrapped to the box width; blank source lines are
/// preserved as blank rows so paragraph breaks survive.
///
/// # Arguments
///
/// * `out` - The buffer to append the rendered rows to
/// * `body` - The post or comment body to render
fn render_body(out: &mut String, body: &str) {
    // open the BODY section with a rule, a header row, and a blank spacer row
    out.push_str(&box_rule());
    out.push_str(&box_row("BODY"));
    out.push_str(&box_row(""));
    // render each source line, wrapping long lines and preserving blank ones
    for line in body.lines() {
        if line.trim().is_empty() {
            // keep a blank source line as a blank row to preserve paragraph breaks
            out.push_str(&box_row(""));
        } else {
            // wrap this line to the box width and emit each wrapped piece as a row
            for wrapped in wrap(line, BOX_INNER_WIDTH) {
                out.push_str(&box_row(&wrapped));
            }
        }
    }
    // close the BODY section
    out.push_str(&box_rule());
}

/// Applies a two-part prefix to a multi-line block
///
/// The first line of `block` gets `first`, every remaining line gets `rest`. Used
/// to hang a rendered comment box off a `|->` connector while keeping its later
/// lines aligned underneath.
///
/// # Arguments
///
/// * `out` - The buffer to append the prefixed lines to
/// * `block` - The already-rendered, multi-line block to prefix
/// * `first` - The prefix for the block's first line
/// * `rest` - The prefix for every other line of the block
fn apply_prefix(out: &mut String, block: &str, first: &str, rest: &str) {
    // prefix the first line differently from the rest so the `|->` only appears once
    for (idx, line) in block.lines().enumerate() {
        let prefix = if idx == 0 { first } else { rest };
        out.push_str(&format!("{prefix}{line}\n"));
    }
}

/// Renders a task checklist into a `TASKS` box section
///
/// Emits nothing when there are no tasks, so taskless boxes render exactly as
/// they did before tasks existed.
///
/// # Arguments
///
/// * `out` - The buffer to append the rendered rows to
/// * `tasks` - The tasks to render, sorted by id
fn render_tasks_box(out: &mut String, tasks: &[Task]) {
    // a question without tasks renders no TASKS section at all
    if tasks.is_empty() {
        return;
    }
    // open the TASKS section with a rule, a header row, and a blank spacer row
    out.push_str(&box_rule());
    out.push_str(&box_row("TASKS"));
    out.push_str(&box_row(""));
    // render each task as a wrapped checkbox row citing how it was completed
    for task in tasks {
        let line = match &task.completed {
            Some(completion) => format!(
                "[x] task {}: {} (completed by {}, see !{})",
                task.id, task.description, completion.author, completion.comment
            ),
            None => format!("[ ] task {}: {}", task.id, task.description),
        };
        for wrapped in wrap(&line, BOX_INNER_WIDTH) {
            out.push_str(&box_row(&wrapped));
        }
    }
    // close the TASKS section
    out.push_str(&box_rule());
}

/// Renders a single post as a boxed `POST` block with its `BODY` section
///
/// # Arguments
///
/// * `out` - The buffer to append the rendered post to
/// * `post` - The post to render
/// * `tasks` - The post's completion-gate tasks, sorted by id
fn render_post_box(out: &mut String, post: &Post, tasks: &[Task]) {
    // open the POST box with a rule and a header row
    out.push_str(&box_rule());
    out.push_str(&box_row("POST"));
    out.push_str(&box_rule());
    // render the post's header fields
    render_field(out, "name", &post.name);
    render_field(out, "kind", &post.kind.label());
    render_field(out, "author", &post.author);
    render_field(out, "title", &post.title);
    // render the post body as its own BODY section
    render_body(out, &post.body);
    // render the question's task checklist as its own TASKS section
    render_tasks_box(out, tasks);
}

/// Renders a single comment node (and its nested replies) as boxed blocks
///
/// The comment is rendered into its own box, hung off a `|->` connector at the
/// given `indent`, and each reply is rendered one level deeper.
///
/// # Arguments
///
/// * `out` - The buffer to append the rendered comment thread to
/// * `node` - The comment node (comment plus its reply tree) to render
/// * `indent` - The prefix applied to this comment's connector, growing with depth
fn render_comment_box(out: &mut String, node: &CommentNode, indent: &str) {
    // render the comment's box into a scratch buffer so we can prefix every line
    let mut block = String::new();
    block.push_str(&box_rule());
    block.push_str(&box_row("COMMENT"));
    block.push_str(&box_rule());
    // render the comment's header fields, including its numeric id, kind, and stance
    render_field(&mut block, "id", &node.comment.id.to_string());
    render_field(&mut block, "kind", &node.comment.kind.label());
    render_field(&mut block, "author", &node.comment.author);
    render_field(&mut block, "stance", &format!("{:?}", node.comment.stance));
    // render the comment body as its own BODY section
    render_body(&mut block, &node.comment.body);
    // render the comment's task checklist as its own TASKS section
    render_tasks_box(&mut block, &node.tasks);
    // draw the connector line, then hang the box off a `|->` at this indent
    out.push_str(&format!("{indent}|\n"));
    let first = format!("{indent}|-> ");
    let rest = format!("{indent}    ");
    apply_prefix(out, &block, &first, &rest);
    // render this comment's replies one level deeper under it
    for reply in &node.replies {
        render_comment_box(out, reply, &rest);
    }
    // if this comment has no replies, note that so the thread's shape is explicit
    if node.replies.is_empty() {
        out.push_str(&format!("{rest}|\n"));
        out.push_str(&format!("{rest}|-> [no child comments shown]\n"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a fresh, isolated forum state for a single test
    ///
    /// Each test gets its own [`ForumState`] with no session store, so tests no
    /// longer share process-global storage and can run concurrently without the
    /// unique-prefix / shared-id workarounds the old statics forced.
    fn forum() -> ForumState {
        ForumState::new()
    }

    /// Detector provenance snapshots, restores, and never joins a model view
    #[tokio::test]
    async fn provenance_round_trips_and_stays_out_of_views() {
        let forum = forum();
        // seed a detector post and comment carrying structured provenance
        let finding = FindingProvenance {
            technique: "T1027".to_owned(),
            confidence: "likely".to_owned(),
            suspicion: 40,
            source: "native".to_owned(),
            detection: Some("packing".to_owned()),
            sites: 2,
            artifact_class: None,
            suppressed: false,
        };
        forum
            .seed_detector_post(
                "detector".to_owned(),
                "fn-00401000".to_owned(),
                "Packed stub".to_owned(),
                "Static detectors flagged this function.".to_owned(),
                Kind::Statement,
                "bucket",
                None,
                vec![finding.clone()],
            )
            .await
            .expect("the post should seed");
        forum
            .seed_detector_comment(
                "detector".to_owned(),
                ParentRef::Post("fn-00401000".to_owned()),
                "[T1027 Obfuscated Files] packed".to_owned(),
                Stance::Neutral,
                Kind::Statement,
                "finding",
                vec![finding],
            )
            .await
            .expect("the comment should seed");
        // the snapshot carries both records in a stable node order
        let snapshot = forum.snapshot().await;
        assert_eq!(snapshot.detector_provenance.len(), 2);
        assert!(matches!(
            &snapshot.detector_provenance[0].node,
            ParentRef::Post(name) if name == "fn-00401000"
        ));
        // the records survive a serde round trip and a restore
        let encoded = serde_json::to_string(&snapshot).expect("snapshot should serialize");
        let decoded: crate::thorpal::resume::ForumSnapshot =
            serde_json::from_str(&encoded).expect("snapshot should parse");
        let fresh = ForumState::new();
        fresh.restore(decoded).await;
        let again = fresh.snapshot().await;
        assert_eq!(again.detector_provenance, snapshot.detector_provenance);
        // a legacy snapshot without the field parses as empty provenance
        let legacy = r#"{"posts":[],"comments":[],"next_comment_id":1,"finish_requested":false}"#;
        let parsed: crate::thorpal::resume::ForumSnapshot =
            serde_json::from_str(legacy).expect("legacy snapshot should parse");
        assert!(parsed.detector_provenance.is_empty());
        // no model-visible view ever carries the structured records: neither
        // the whole-forum view nor a single post's thread serializes them
        let view = serde_json::to_value(forum.view().await).expect("view should serialize");
        let text = view.to_string();
        assert!(
            !text.contains("provenance"),
            "the forum view leaked: {text}"
        );
        assert!(!text.contains("suspicion"), "the forum view leaked: {text}");
    }

    #[tokio::test]
    async fn create_post_rejects_whitespace_in_name() {
        let forum = forum();
        // a name with a space must be rejected before it ever reaches storage
        let result = forum
            .create_post(
                "tester".to_owned(),
                "has space".to_owned(),
                "title".to_owned(),
                "body".to_owned(),
                "statement".to_owned(),
                String::new(),
                Vec::new(),
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn create_post_rejects_duplicate_name() {
        let forum = forum();
        // create the first post
        forum
            .create_post(
                "tester".to_owned(),
                "dup-post".to_owned(),
                "title".to_owned(),
                "body".to_owned(),
                "statement".to_owned(),
                String::new(),
                Vec::new(),
            )
            .await
            .expect("first post should succeed");
        // a second post with the same name must be rejected
        let result = forum
            .create_post(
                "tester".to_owned(),
                "dup-post".to_owned(),
                "title2".to_owned(),
                "body2".to_owned(),
                "statement".to_owned(),
                String::new(),
                Vec::new(),
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn create_post_hints_split_only_for_long_bodies() {
        let forum = forum();
        // a short body should create the post with no split advisory
        let short = forum
            .create_post(
                "tester".to_owned(),
                "short-post".to_owned(),
                "title".to_owned(),
                "a short body".to_owned(),
                "statement".to_owned(),
                String::new(),
                Vec::new(),
            )
            .await
            .expect("short post should be created");
        assert!(short.notice.is_none());
        // a body longer than the threshold should carry a non-fatal split nudge
        let long_body = "x".repeat(POST_BODY_SPLIT_HINT_CHARS + 1);
        let long = forum
            .create_post(
                "tester".to_owned(),
                "long-post".to_owned(),
                "title".to_owned(),
                long_body,
                "statement".to_owned(),
                String::new(),
                Vec::new(),
            )
            .await
            .expect("long post should still be created");
        assert!(long.notice.is_some());
    }

    #[tokio::test]
    async fn comment_thread_nests_by_parent() {
        let forum = forum();
        // create a post to comment on
        forum
            .create_post(
                "tester".to_owned(),
                "thread-post".to_owned(),
                "title".to_owned(),
                "body".to_owned(),
                "statement".to_owned(),
                String::new(),
                Vec::new(),
            )
            .await
            .expect("post should be created");
        // add a top-level comment on the post
        let top = forum
            .create_comment(
                "tester".to_owned(),
                "#thread-post".to_owned(),
                "top-level".to_owned(),
                "supports".to_owned(),
                "statement".to_owned(),
                String::new(),
            )
            .await
            .expect("top-level comment should be created");
        // reply to that comment
        forum
            .create_comment(
                "tester".to_owned(),
                format!("!{}", top.id),
                "reply".to_owned(),
                "disproves".to_owned(),
                "statement".to_owned(),
                String::new(),
            )
            .await
            .expect("reply should be created");
        // following the post's link should return the whole nested tree
        let node = forum
            .follow_link("#thread-post".to_owned())
            .await
            .expect("follow_link should resolve the post");
        let ForumNode::Post(thread) = node else {
            panic!("expected a post node");
        };
        assert_eq!(thread.comments.len(), 1);
        assert_eq!(thread.comments[0].comment.id, top.id);
        assert_eq!(thread.comments[0].replies.len(), 1);
        assert_eq!(
            thread.comments[0].replies[0].comment.stance,
            Stance::Disproves
        );
    }

    #[tokio::test]
    async fn create_comment_rejects_unknown_parent() {
        let forum = forum();
        // a reference to a post that was never created must be rejected
        let result = forum
            .create_comment(
                "tester".to_owned(),
                "#does-not-exist".to_owned(),
                "body".to_owned(),
                "neutral".to_owned(),
                "statement".to_owned(),
                String::new(),
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn create_comment_rejects_invalid_stance() {
        let forum = forum();
        // create a post so the parent reference itself is valid
        forum
            .create_post(
                "tester".to_owned(),
                "bad-stance-post".to_owned(),
                "title".to_owned(),
                "body".to_owned(),
                "statement".to_owned(),
                String::new(),
                Vec::new(),
            )
            .await
            .expect("post should be created");
        // an unrecognized stance must be rejected
        let result = forum
            .create_comment(
                "tester".to_owned(),
                "#bad-stance-post".to_owned(),
                "body".to_owned(),
                "maybe".to_owned(),
                "statement".to_owned(),
                String::new(),
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn follow_link_rejects_malformed_reference() {
        let forum = forum();
        // a reference missing the '#'/'!' prefix must be rejected
        let result = forum.follow_link("no-prefix".to_owned()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn render_forum_boxed_frames_post_and_comment() {
        let forum = forum();
        // seed a post and one comment so the boxed dump has content to render
        forum
            .seed_post(
                "tester".to_owned(),
                "boxed-post".to_owned(),
                "A boxed title".to_owned(),
                "Some body text".to_owned(),
                Kind::Statement,
            )
            .await
            .expect("seed post should succeed");
        forum
            .seed_comment(
                "commenter".to_owned(),
                ParentRef::Post("boxed-post".to_owned()),
                "evidence".to_owned(),
                Stance::Supports,
                Kind::Statement,
            )
            .await
            .expect("seed comment should succeed");
        // render the whole forum; this forum is isolated so nothing else interferes
        let rendered = forum.render_forum_boxed().await;
        // the POST framing and a BODY section must be present
        assert!(rendered.contains("| POST"), "expected a POST box header");
        assert!(rendered.contains(&box_rule()), "expected box rules");
        assert!(rendered.contains("| BODY"), "expected a BODY section");
        // the comment must render as its own box hung off a `|->` connector
        assert!(rendered.contains("| COMMENT"), "expected a COMMENT box");
        assert!(rendered.contains("|-> "), "expected a comment connector");
        // a leaf comment must note that it has no child comments
        assert!(
            rendered.contains("[no child comments shown]"),
            "expected the no-children note on a leaf comment"
        );
    }

    /// Builds a post struct literal for the pure appendix renderer tests
    ///
    /// # Arguments
    ///
    /// * `name` - The unique reference name for the post
    /// * `author` - The author to attribute the post to
    /// * `body` - The body of the post
    /// * `kind` - Whether this post states something or asks a question
    fn appendix_post(name: &str, author: &str, body: &str, kind: Kind) -> Post {
        Post {
            name: name.to_owned(),
            author: author.to_owned(),
            title: format!("{name} title"),
            body: body.to_owned(),
            kind,
            updated: 0,
        }
    }

    /// Builds a comment map entry for the pure appendix renderer tests
    ///
    /// # Arguments
    ///
    /// * `id` - The numeric id for the comment
    /// * `parent` - The post or comment this comment replies to
    /// * `author` - The author to attribute the comment to
    /// * `body` - The body of the comment
    fn appendix_comment(
        id: usize,
        parent: ParentRef,
        author: &str,
        body: &str,
    ) -> (usize, Comment) {
        (
            id,
            Comment {
                id,
                parent,
                author: author.to_owned(),
                body: body.to_owned(),
                stance: Stance::Supports,
                kind: Kind::Statement,
                updated: 0,
            },
        )
    }

    /// Agent bodies land verbatim, detector bodies are elided, sections ordered.
    #[test]
    fn report_appendix_preserves_artifact_bodies_verbatim() {
        // one post per appendix section, plus a detector seed nobody adjudicated
        let posts = vec![
            appendix_post(
                "solved-q",
                "main",
                "which C2 does this use?",
                Kind::Question(QuestionStatus::Solved),
            ),
            appendix_post(
                "agent-finding",
                "main",
                "XOR key 0x5A decodes the config blob",
                Kind::Statement,
            ),
            appendix_post("det-only", SEED_AUTHOR, "seed body A", Kind::Statement),
            appendix_post(
                "det-adjudicated",
                SEED_AUTHOR,
                "finding line B\nReasoning: long persuasion prose\nArtifact (Decoded Data): decoded-value-B",
                Kind::Statement,
            ),
            appendix_post(
                "open-q",
                "main",
                "what persists at boot?",
                Kind::Question(QuestionStatus::Unsolved),
            ),
        ];
        // an agent answer with a nested reply, a detector-only thread, and one
        // agent adjudication under the second detector seed
        let comments: HashMap<usize, Comment> = [
            appendix_comment(
                1,
                ParentRef::Post("solved-q".to_owned()),
                "strings",
                "downloadexec Success!",
            ),
            appendix_comment(
                2,
                ParentRef::Comment(1),
                "main",
                "confirmed against the decoded table",
            ),
            appendix_comment(
                3,
                ParentRef::Post("det-only".to_owned()),
                SEED_AUTHOR,
                "[T1000 Test] det-only finding\nReasoning: prose\nArtifact (Decoded Data): det-only-artifact",
            ),
            appendix_comment(
                4,
                ParentRef::Post("det-adjudicated".to_owned()),
                "main",
                "agent adjudication evidence",
            ),
        ]
        .into_iter()
        .collect();
        // render the appendix with an unbounded budget
        let out = render_report_appendix(&posts, &comments, &HashMap::new(), &[], &[], usize::MAX)
            .expect("the appendix should render");
        // the five sections must appear in their fixed order
        let solved = out.find("### Solved questions").expect("solved section");
        let open = out
            .find("### Open / blocked questions")
            .expect("open section");
        let findings = out.find("### Finding posts").expect("findings section");
        let adjudicated = out
            .find("### Detector seed adjudications")
            .expect("adjudications section");
        let unreplicated = out
            .find("### Unreplicated detector evidence")
            .expect("unreplicated section");
        assert!(
            solved < open
                && open < findings
                && findings < adjudicated
                && adjudicated < unreplicated
        );
        // both artifact sentinels must survive verbatim, nested reply included
        assert!(out.contains("downloadexec Success!"));
        assert!(out.contains("confirmed against the decoded table"));
        assert!(out.contains("XOR key 0x5A decodes the config blob"));
        // the solved and open questions must carry their status labels
        assert!(out.contains("#solved-q [Question · Solved]"));
        assert!(out.contains("#open-q [Question · Unsolved]"));
        // the unadjudicated detector seed survives as an evidence digest: its
        // finding line and artifact value, never its reasoning prose
        assert!(out.contains("#det-only"));
        assert!(out.contains("det-only finding"));
        assert!(out.contains("Artifact (Decoded Data): det-only-artifact"));
        assert!(!out.contains("Reasoning: prose"));
        // the adjudicated seed keeps its evidence lines, elides its reasoning,
        // and the agent's adjudicating comment survives verbatim
        assert!(out.contains("#det-adjudicated"));
        assert!(out.contains("finding line B"));
        assert!(out.contains("Artifact (Decoded Data): decoded-value-B"));
        assert!(out.contains("[reasoning elided"));
        assert!(!out.contains("long persuasion prose"));
        assert!(out.contains("agent adjudication evidence"));
    }

    /// The file-identity post renders in full with the finding posts even when
    /// no agent ever commented on it — never as a digest that would drop its
    /// hash and timestamp lines.
    #[test]
    fn report_appendix_keeps_the_file_identity_post_whole() {
        // a seed-authored identity post with an uncommented thread
        let posts = vec![appendix_post(
            "file-identity",
            SEED_AUTHOR,
            "SHA-256: abc123\nBuild timestamp: 2005-08-27 (REPRO)",
            Kind::Statement,
        )];
        let out = render_report_appendix(
            &posts,
            &HashMap::new(),
            &HashMap::new(),
            &[],
            &[],
            usize::MAX,
        )
        .expect("the appendix should render");
        // the whole body lands under Finding posts, not the evidence digest
        assert!(out.contains("### Finding posts"));
        assert!(out.contains("SHA-256: abc123"));
        assert!(out.contains("Build timestamp: 2005-08-27 (REPRO)"));
        assert!(!out.contains("### Unreplicated detector evidence"));
    }

    /// The budget drops whole thread blocks and counts them in the closing note.
    #[test]
    fn report_appendix_budget_drops_whole_blocks_with_note() {
        // two agent finding posts whose name sort fixes their render order
        let posts = vec![
            appendix_post("aa-first", "main", "first sentinel body", Kind::Statement),
            appendix_post("zz-second", "main", "second sentinel body", Kind::Statement),
        ];
        let comments = HashMap::new();
        // render unbounded first to measure where the second block starts
        let full = render_report_appendix(&posts, &comments, &HashMap::new(), &[], &[], usize::MAX)
            .expect("the unbounded appendix should render");
        let budget = full
            .find("\n#zz-second")
            .expect("the second block should be in the unbounded render");
        // a budget that fits exactly the first block must drop the second whole
        let bounded = render_report_appendix(&posts, &comments, &HashMap::new(), &[], &[], budget)
            .expect("the bounded appendix should render");
        assert!(bounded.contains("first sentinel body"));
        assert!(!bounded.contains("zz-second"));
        assert!(!bounded.contains("second sentinel body"));
        // the omission note must count the dropped thread
        assert!(bounded.contains("1 thread(s) omitted"));
    }

    /// An empty forum yields no appendix at all.
    #[test]
    fn report_appendix_empty_forum_is_none() {
        // an empty forum has nothing to attach, so no appendix renders at all
        assert!(
            render_report_appendix(&[], &HashMap::new(), &HashMap::new(), &[], &[], usize::MAX)
                .is_none()
        );
    }

    /// The async wrapper renders the appendix from live forum state.
    #[tokio::test]
    async fn report_appendix_renders_from_live_forum() {
        let forum = forum();
        // seed an agent finding with a supporting comment onto the live forum
        forum
            .seed_post(
                "main".to_owned(),
                "live-finding".to_owned(),
                "A live finding".to_owned(),
                "mutex Global\\abc123".to_owned(),
                Kind::Statement,
            )
            .await
            .expect("seed post should succeed");
        forum
            .seed_comment(
                "strings".to_owned(),
                ParentRef::Post("live-finding".to_owned()),
                "found at 0x401000".to_owned(),
                Stance::Supports,
                Kind::Statement,
            )
            .await
            .expect("seed comment should succeed");
        // seed a detector statement nobody adjudicated so it stays filtered out
        forum
            .seed_post(
                SEED_AUTHOR.to_owned(),
                "det-seed".to_owned(),
                "A detector seed".to_owned(),
                "seed body".to_owned(),
                Kind::Statement,
            )
            .await
            .expect("seed post should succeed");
        // the live method must snapshot the forum and render the appendix
        let out = forum
            .report_appendix()
            .await
            .expect("the live appendix should render");
        // the banner, the finding post, and the comment body must all appear
        assert!(out.contains("## Appendix — Forum Evidence (machine-generated)"));
        assert!(out.contains("mutex Global\\abc123"));
        assert!(out.contains("found at 0x401000"));
        // the unadjudicated detector seed survives as a digest header only:
        // its plain body line carries no evidence anchor, so nothing else prints
        assert!(out.contains("### Unreplicated detector evidence"));
        assert!(out.contains("#det-seed"));
        assert!(!out.contains("seed body"));
    }

    #[test]
    fn question_status_parse_handles_casing_and_default() {
        // an empty string defaults to Unsolved so a fresh question starts open
        assert_eq!(QuestionStatus::parse("").unwrap(), QuestionStatus::Unsolved);
        // casing is ignored and the several NeedsEvidence spellings all parse
        assert_eq!(
            QuestionStatus::parse("SOLVED").unwrap(),
            QuestionStatus::Solved
        );
        assert_eq!(
            QuestionStatus::parse("needs-evidence").unwrap(),
            QuestionStatus::NeedsEvidence
        );
        assert_eq!(
            QuestionStatus::parse("NeedsEvidence").unwrap(),
            QuestionStatus::NeedsEvidence
        );
        // an unrecognized status is rejected
        assert!(QuestionStatus::parse("maybe").is_err());
    }

    #[test]
    fn kind_parse_ignores_status_for_statements() {
        // a statement ignores whatever status string it is given
        assert_eq!(Kind::parse("statement", "solved").unwrap(), Kind::Statement);
        // a question carries the parsed status inside the variant
        assert_eq!(
            Kind::parse("question", "blocked").unwrap(),
            Kind::Question(QuestionStatus::Blocked)
        );
        // a question with no status defaults to Unsolved
        assert_eq!(
            Kind::parse("question", "").unwrap(),
            Kind::Question(QuestionStatus::Unsolved)
        );
        // an unrecognized kind is rejected
        assert!(Kind::parse("musing", "").is_err());
    }

    #[tokio::test]
    async fn set_status_rejects_statement_and_transitions_question() {
        let forum = forum();
        // a statement post has no status, so set_status must reject it
        forum
            .create_post(
                "tester".to_owned(),
                "status-statement".to_owned(),
                "title".to_owned(),
                "body".to_owned(),
                "statement".to_owned(),
                String::new(),
                Vec::new(),
            )
            .await
            .expect("statement post should be created");
        assert!(
            forum
                .set_status("#status-statement".to_owned(), "solved".to_owned())
                .await
                .is_err()
        );
        // a question post starts Unsolved and can be transitioned to Solved
        forum
            .create_post(
                "tester".to_owned(),
                "status-question".to_owned(),
                "title".to_owned(),
                "body".to_owned(),
                "question".to_owned(),
                String::new(),
                Vec::new(),
            )
            .await
            .expect("question post should be created");
        forum
            .set_status("#status-question".to_owned(), "solved".to_owned())
            .await
            .expect("transitioning a question should succeed");
        // following the post back must show the new Solved status
        let node = forum
            .follow_link("#status-question".to_owned())
            .await
            .unwrap();
        let ForumNode::Post(thread) = node else {
            panic!("expected a post node");
        };
        assert_eq!(thread.post.kind, Kind::Question(QuestionStatus::Solved));
    }

    #[tokio::test]
    async fn list_questions_filters_open_and_solved_by_status() {
        let forum = forum();
        // helper pulling the post names out of a list_questions result
        async fn names_for(forum: &ForumState, status: &str) -> Vec<String> {
            forum
                .list_questions(status.to_owned())
                .await
                .unwrap()
                .iter()
                .filter_map(|node| match node {
                    ForumNode::Post(thread) => Some(thread.post.name.clone()),
                    ForumNode::Comment(_) => None,
                })
                .collect()
        }
        // an open (Unsolved) question and a solved question
        forum
            .create_post(
                "tester".to_owned(),
                "open-q".to_owned(),
                "title".to_owned(),
                "body".to_owned(),
                "question".to_owned(),
                "unsolved".to_owned(),
                Vec::new(),
            )
            .await
            .expect("open question should be created");
        forum
            .create_post(
                "tester".to_owned(),
                "solved-q".to_owned(),
                "title".to_owned(),
                "body".to_owned(),
                "question".to_owned(),
                "solved".to_owned(),
                Vec::new(),
            )
            .await
            .expect("solved question should be created");
        // the "open" filter (Unsolved/NeedsEvidence) includes only the open one
        let open_names = names_for(&forum, "open").await;
        assert!(open_names.contains(&"open-q".to_owned()));
        assert!(!open_names.contains(&"solved-q".to_owned()));
        // the "solved" filter includes only the solved one
        let solved_names = names_for(&forum, "solved").await;
        assert!(solved_names.contains(&"solved-q".to_owned()));
        assert!(!solved_names.contains(&"open-q".to_owned()));
        // the open question also raises the live open count above zero
        assert_eq!(forum.open_question_count().await, 1);
    }

    #[tokio::test]
    async fn add_user_question_creates_open_question_from_title() {
        let forum = forum();
        // a user question should land as an open question authored by "user",
        // with its reference name derived from the title by dashing its spaces
        let title = "Does it beacon home".to_owned();
        let post = forum
            .add_user_question(title.clone(), "investigate egress".to_owned())
            .await
            .expect("a titled user question should be created");
        assert_eq!(post.author, USER_AUTHOR);
        assert_eq!(post.kind, Kind::Question(QuestionStatus::Unsolved));
        // the name must be space-free (spaces replaced with dashes)
        assert!(!post.name.contains(' '));
        assert_eq!(post.name, title.replace(' ', "-"));
    }

    #[tokio::test]
    async fn add_user_question_rejects_empty_title() {
        let forum = forum();
        // an empty (or whitespace-only) title has no reference name, so it must fail
        assert!(
            forum
                .add_user_question(String::new(), "body".to_owned())
                .await
                .is_err()
        );
        assert!(
            forum
                .add_user_question("   ".to_owned(), "body".to_owned())
                .await
                .is_err()
        );
    }

    /// contains_text scans post titles/bodies and comment bodies verbatim.
    #[tokio::test]
    async fn contains_text_scans_posts_and_comments() {
        let forum = forum();
        // an empty forum carries nothing, and an empty needle matches nothing
        assert!(!forum.contains_text("act=recive_message").await);
        assert!(!forum.contains_text("").await);
        // post a finding whose body carries a decoded constant
        forum
            .create_post(
                "tester".to_owned(),
                "c2-protocol".to_owned(),
                "Decoded C2 verbs".to_owned(),
                "the beacon sends act=recive_message every poll".to_owned(),
                "statement".to_owned(),
                String::new(),
                Vec::new(),
            )
            .await
            .expect("the finding post should be created");
        // body text is found verbatim; titles are scanned too
        assert!(forum.contains_text("act=recive_message").await);
        assert!(forum.contains_text("Decoded C2 verbs").await);
        // matching is case-sensitive: a cased mismatch is a miss
        assert!(!forum.contains_text("ACT=RECIVE_MESSAGE").await);
        // comment bodies are scanned as well
        forum
            .create_comment(
                "tester".to_owned(),
                "#c2-protocol".to_owned(),
                "also saw act=get_message in the second blob".to_owned(),
                "supports".to_owned(),
                "statement".to_owned(),
                String::new(),
            )
            .await
            .expect("the comment should be created");
        assert!(forum.contains_text("act=get_message").await);
    }

    /// search ranks posts and comments end to end with well-formed references.
    #[tokio::test]
    async fn search_finds_posts_and_comments_end_to_end() {
        let forum = forum();
        // an empty forum (and an empty query) return no hits rather than erroring
        assert!(forum.search("beacon", 10).await.is_empty());
        assert!(forum.search("", 10).await.is_empty());
        // post a finding and hang an evidence comment under it
        forum
            .create_post(
                "tester".to_owned(),
                "c2-beacon".to_owned(),
                "Beacon loop decoded".to_owned(),
                "the implant beacons home with act=recive_message".to_owned(),
                "statement".to_owned(),
                String::new(),
                Vec::new(),
            )
            .await
            .expect("the finding post should be created");
        forum
            .create_comment(
                "tester".to_owned(),
                "#c2-beacon".to_owned(),
                "the beacon interval is 60 seconds".to_owned(),
                "supports".to_owned(),
                "statement".to_owned(),
                String::new(),
            )
            .await
            .expect("the evidence comment should be created");
        // both the post and the comment match, with well-formed references
        let hits = forum.search("beacon", 10).await;
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().any(|hit| hit.reference == "#c2-beacon"));
        assert!(hits.iter().any(|hit| hit.reference == "!1"));
        // the comment hit carries its root post's title and its stance
        let comment_hit = hits.iter().find(|hit| hit.reference == "!1").unwrap();
        assert_eq!(comment_hit.title, "Beacon loop decoded");
        assert_eq!(comment_hit.stance, Some(Stance::Supports));
        // a snake_case query matches by its words thanks to the tokenizer
        let hits = forum.search("recive message", 10).await;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].reference, "#c2-beacon");
        // the limit caps the hits returned
        assert_eq!(forum.search("beacon", 1).await.len(), 1);
    }

    /// The cached engine refits whenever the forum's revision moves.
    #[tokio::test]
    async fn search_engine_cache_refreshes_across_mutations() {
        let forum = forum();
        // post a finding and search it so the engine cache primes on this corpus
        forum
            .create_post(
                "tester".to_owned(),
                "xor-loop".to_owned(),
                "XOR decode loop".to_owned(),
                "the loop xors the blob".to_owned(),
                "statement".to_owned(),
                String::new(),
                Vec::new(),
            )
            .await
            .expect("the finding post should be created");
        assert_eq!(forum.search("xor", 10).await.len(), 1);
        // a new comment with the term must show up in the very next search
        forum
            .create_comment(
                "tester".to_owned(),
                "#xor-loop".to_owned(),
                "the xor key is 0x5A".to_owned(),
                "supports".to_owned(),
                "statement".to_owned(),
                String::new(),
            )
            .await
            .expect("the evidence comment should be created");
        assert_eq!(forum.search("xor", 10).await.len(), 2);
        // a question joins the corpus so a status flip can be probed next
        forum
            .create_post(
                "tester".to_owned(),
                "xor-question".to_owned(),
                "Which xor key?".to_owned(),
                "is the xor key constant".to_owned(),
                "question".to_owned(),
                "unsolved".to_owned(),
                Vec::new(),
            )
            .await
            .expect("the question post should be created");
        assert_eq!(forum.search("xor", 10).await.len(), 3);
        // a non-text mutation (a status change) also moves the revision; the
        // refit must keep returning the same, still-correct hits
        forum
            .set_status("#xor-question".to_owned(), "solved".to_owned())
            .await
            .expect("the status change should succeed");
        assert_eq!(forum.search("xor", 10).await.len(), 3);
    }

    /// A restored forum is searchable even when the snapshot predates stamps.
    #[tokio::test]
    async fn search_finds_restored_posts_even_in_legacy_snapshots() {
        // build a source forum holding one finding worth searching for
        let source = forum();
        source
            .create_post(
                "tester".to_owned(),
                "rc4-config".to_owned(),
                "RC4 decoded config".to_owned(),
                "the config decrypts with rc4".to_owned(),
                "statement".to_owned(),
                String::new(),
                Vec::new(),
            )
            .await
            .expect("the finding post should be created");
        // age the snapshot into a legacy one: every stamp zeroed, as written
        // by builds that predate revision tracking
        let mut snapshot = source.snapshot().await;
        for post in &mut snapshot.posts {
            post.updated = 0;
        }
        for comment in &mut snapshot.comments {
            comment.updated = 0;
        }
        for task in &mut snapshot.tasks {
            task.updated = 0;
        }
        // prime a fresh forum's engine cache with a pre-restore search
        let restored = forum();
        assert!(restored.search("rc4", 10).await.is_empty());
        // restore the legacy snapshot and search again: the engine must refit
        restored.restore(snapshot).await;
        let hits = restored.search("rc4", 10).await;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].reference, "#rc4-config");
    }

    /// The revision counter moves with every mutation and is observable.
    #[tokio::test]
    async fn current_revision_moves_per_mutation() {
        let forum = forum();
        // creating a post bumps the revision past its starting point
        let before = forum.current_revision();
        forum
            .create_post(
                "tester".to_owned(),
                "rev-probe".to_owned(),
                "title".to_owned(),
                "body".to_owned(),
                "statement".to_owned(),
                String::new(),
                Vec::new(),
            )
            .await
            .expect("the probe post should be created");
        let after = forum.current_revision();
        assert!(after > before);
        // observing the counter does not advance it
        assert_eq!(forum.current_revision(), after);
    }

    #[tokio::test]
    async fn finish_refuses_while_a_question_is_open() {
        let forum = forum();
        // an open question must block finishing
        forum
            .create_post(
                "tester".to_owned(),
                "finish-open-q".to_owned(),
                "title".to_owned(),
                "body".to_owned(),
                "question".to_owned(),
                "unsolved".to_owned(),
                Vec::new(),
            )
            .await
            .expect("open question should be created");
        // finish must refuse and must NOT set the flag while a question is open
        assert!(forum.finish().await.is_err());
        assert!(!forum.take_finish_requested());
        // once the question is resolved, finish succeeds and sets the flag
        forum
            .set_status("#finish-open-q".to_owned(), "solved".to_owned())
            .await
            .expect("resolving the question should succeed");
        // this forum is isolated, so nothing else can leave a question open
        forum
            .finish()
            .await
            .expect("finish should succeed with nothing open");
        assert!(forum.take_finish_requested());
    }

    /// Seeds an unsolved question post with the given name for a task test
    ///
    /// # Arguments
    ///
    /// * `forum` - The forum to seed the question onto
    /// * `name` - The question post's unique reference name
    async fn question(forum: &ForumState, name: &str) {
        forum
            .seed_post(
                "tester".to_owned(),
                name.to_owned(),
                format!("title-{name}"),
                "a question body".to_owned(),
                Kind::Question(QuestionStatus::Unsolved),
            )
            .await
            .expect("question post should seed");
    }

    /// Task batches are validated and only question targets accept them
    #[tokio::test]
    async fn add_tasks_rejects_statements_unknown_targets_and_empty_descriptions() {
        let forum = forum();
        // seed a statement post so the target exists but is not a question
        post(&forum, "task-statement", "a plain statement").await;
        // tasks are completion gates on questions, so a statement is rejected
        let err = forum
            .add_tasks(
                "tester".to_owned(),
                "#task-statement".to_owned(),
                vec!["check something".to_owned()],
            )
            .await
            .expect_err("a statement target should be rejected");
        assert!(err.to_string().contains("tasks only attach to questions"));
        // a post that was never created is rejected outright
        assert!(
            forum
                .add_tasks(
                    "tester".to_owned(),
                    "#task-missing".to_owned(),
                    vec!["check something".to_owned()],
                )
                .await
                .is_err()
        );
        // seed a question so the remaining failures are all about the batch
        question(&forum, "task-batch-q").await;
        // an empty batch is a mistake worth an error, not a silent no-op
        assert!(
            forum
                .add_tasks("tester".to_owned(), "#task-batch-q".to_owned(), Vec::new())
                .await
                .is_err()
        );
        // a blank description says nothing checkable, so the batch is rejected
        assert!(
            forum
                .add_tasks(
                    "tester".to_owned(),
                    "#task-batch-q".to_owned(),
                    vec!["check something".to_owned(), "   ".to_owned()],
                )
                .await
                .is_err()
        );
        // a batch above the per-call bound is rejected before touching storage
        let flood: Vec<String> = (0..=MAX_TASKS_PER_ADD)
            .map(|idx| format!("task {idx}"))
            .collect();
        assert!(
            forum
                .add_tasks("tester".to_owned(), "#task-batch-q".to_owned(), flood)
                .await
                .is_err()
        );
    }

    /// Tasks cannot attach to already-solved questions, but blocked ones can
    #[tokio::test]
    async fn add_tasks_rejects_already_solved_questions() {
        let forum = forum();
        // seed a question and accept its answer so it is already solved
        question(&forum, "solved-task-q").await;
        forum
            .set_status("#solved-task-q".to_owned(), "solved".to_owned())
            .await
            .expect("solving the taskless question should succeed");
        // a gate added after the answer was accepted could never gate anything
        let err = forum
            .add_tasks(
                "tester".to_owned(),
                "#solved-task-q".to_owned(),
                vec!["check something".to_owned()],
            )
            .await
            .expect_err("a solved question should reject new tasks");
        assert!(err.to_string().contains("already solved"));
        // a blocked question stays gateable in case it is ever reopened
        question(&forum, "blocked-task-q").await;
        forum
            .set_status("#blocked-task-q".to_owned(), "blocked".to_owned())
            .await
            .expect("blocking the question should succeed");
        forum
            .add_tasks(
                "tester".to_owned(),
                "#blocked-task-q".to_owned(),
                vec!["check something".to_owned()],
            )
            .await
            .expect("a blocked question should accept new tasks");
        // creating a question directly as solved with tasks is the same bypass
        let err = forum
            .create_post(
                "tester".to_owned(),
                "born-solved-q".to_owned(),
                "title".to_owned(),
                "body".to_owned(),
                "question".to_owned(),
                "solved".to_owned(),
                vec!["check something".to_owned()],
            )
            .await
            .expect_err("a question born solved should reject tasks");
        assert!(err.to_string().contains("solved"));
        // the rejection happens before insertion, so no post was left behind
        assert!(
            forum
                .follow_link("#born-solved-q".to_owned())
                .await
                .is_err()
        );
    }

    /// Tasks on a statement post are rejected before the post is inserted
    #[tokio::test]
    async fn create_post_rejects_tasks_on_statements() {
        let forum = forum();
        // a statement with tasks is a real mistake, so the whole call must fail
        let result = forum
            .create_post(
                "tester".to_owned(),
                "tasked-statement".to_owned(),
                "title".to_owned(),
                "body".to_owned(),
                "statement".to_owned(),
                String::new(),
                vec!["check something".to_owned()],
            )
            .await;
        assert!(result.is_err());
        // the rejection happens before insertion, so no post was left behind
        let posts = forum.list_posts().await.expect("list_posts should succeed");
        assert!(posts.is_empty());
    }

    /// A question created with tasks carries them from the start
    #[tokio::test]
    async fn create_post_attaches_tasks_to_a_new_question() {
        let forum = forum();
        // create a question with two completion-gate tasks in one call
        forum
            .create_post(
                "tester".to_owned(),
                "tasked-q".to_owned(),
                "title".to_owned(),
                "body".to_owned(),
                "question".to_owned(),
                String::new(),
                vec!["first check".to_owned(), "second check".to_owned()],
            )
            .await
            .expect("question with tasks should be created");
        // following the post back must show both tasks on its node, in id order
        let node = forum
            .follow_link("#tasked-q".to_owned())
            .await
            .expect("follow_link should resolve the post");
        let ForumNode::Post(thread) = node else {
            panic!("expected a post node");
        };
        assert_eq!(thread.tasks.len(), 2);
        // a fresh forum hands out task ids starting at 1
        assert_eq!(thread.tasks[0].id, 1);
        assert_eq!(thread.tasks[1].id, 2);
        // each task gates the question post and is attributed to its creator
        for task in &thread.tasks {
            assert_eq!(task.parent, ParentRef::Post("tasked-q".to_owned()));
            assert_eq!(task.author, "tester");
            // a freshly created task is open until explicitly completed
            assert!(task.completed.is_none());
        }
        // the descriptions land in creation order
        assert_eq!(thread.tasks[0].description, "first check");
        assert_eq!(thread.tasks[1].description, "second check");
    }

    /// Completing a task requires the cited comment to actually exist
    #[tokio::test]
    async fn complete_task_requires_an_existing_comment() {
        let forum = forum();
        // seed a question gated by one task
        question(&forum, "cite-q").await;
        let created = forum
            .add_tasks(
                "tester".to_owned(),
                "#cite-q".to_owned(),
                vec!["check something".to_owned()],
            )
            .await
            .expect("the task should be added");
        // citing a comment that was never created must be rejected
        let err = forum
            .complete_task("tester".to_owned(), created[0].id, "!99".to_owned())
            .await
            .expect_err("a dangling citation should be rejected");
        assert!(err.to_string().contains("No comment with id"));
        // the failed completion must leave the task open
        let open = forum
            .incomplete_tasks_for(&ParentRef::Post("cite-q".to_owned()))
            .await;
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, created[0].id);
    }

    /// A completion records its citing comment and both reference forms parse
    #[tokio::test]
    async fn complete_task_records_the_citing_comment_and_author() {
        let forum = forum();
        // a question gated by two tasks, plus a comment to cite
        question(&forum, "complete-q").await;
        let created = forum
            .add_tasks(
                "tester".to_owned(),
                "#complete-q".to_owned(),
                vec!["first check".to_owned(), "second check".to_owned()],
            )
            .await
            .expect("the tasks should be added");
        let cited = comment(
            &forum,
            ParentRef::Post("complete-q".to_owned()),
            "the answer",
        )
        .await;
        // completing with the `!id` sigil form records the comment and author
        let completed = forum
            .complete_task(
                "finisher".to_owned(),
                created[0].id,
                format!("!{}", cited.id),
            )
            .await
            .expect("the sigil citation should complete the task");
        let completion = completed
            .completed
            .expect("the returned task should carry its completion");
        assert_eq!(completion.comment, cited.id);
        assert_eq!(completion.author, "finisher");
        // the bare-number citation form parses to the same comment id
        let completed = forum
            .complete_task("finisher".to_owned(), created[1].id, cited.id.to_string())
            .await
            .expect("the bare-number citation should complete the task");
        assert_eq!(
            completed
                .completed
                .expect("the second task should carry its completion")
                .comment,
            cited.id
        );
    }

    /// A completion is permanent, so a second attempt is rejected unchanged
    #[tokio::test]
    async fn complete_task_rejects_double_completion() {
        let forum = forum();
        // a question gated by one task, plus two comments to cite
        question(&forum, "double-q").await;
        let created = forum
            .add_tasks(
                "tester".to_owned(),
                "#double-q".to_owned(),
                vec!["check something".to_owned()],
            )
            .await
            .expect("the task should be added");
        let first = comment(
            &forum,
            ParentRef::Post("double-q".to_owned()),
            "first proof",
        )
        .await;
        let second = comment(
            &forum,
            ParentRef::Post("double-q".to_owned()),
            "second proof",
        )
        .await;
        // the first completion lands and cites the first comment
        forum
            .complete_task("tester".to_owned(), created[0].id, format!("!{}", first.id))
            .await
            .expect("the first completion should succeed");
        // a second completion is rejected, naming the original citation
        let err = forum
            .complete_task("rival".to_owned(), created[0].id, format!("!{}", second.id))
            .await
            .expect_err("a double completion should be rejected");
        let message = err.to_string();
        assert!(message.contains("already completed"));
        assert!(message.contains(&format!("!{}", first.id)));
        // the stored completion still points at the original comment and author
        let tasks = forum.tasks.read().await;
        let completion = tasks[&created[0].id]
            .completed
            .as_ref()
            .expect("the task should stay completed");
        assert_eq!(completion.comment, first.id);
        assert_eq!(completion.author, "tester");
    }

    /// The gate refuses solved while tasks are open but leaves blocked alone
    #[tokio::test]
    async fn set_status_refuses_solved_while_tasks_are_incomplete() {
        let forum = forum();
        // a question gated by two tasks, plus a comment to complete them with
        question(&forum, "gated-q").await;
        let created = forum
            .add_tasks(
                "tester".to_owned(),
                "#gated-q".to_owned(),
                vec!["first check".to_owned(), "second check".to_owned()],
            )
            .await
            .expect("the tasks should be added");
        let cited = comment(
            &forum,
            ParentRef::Post("gated-q".to_owned()),
            "the evidence",
        )
        .await;
        // solving must refuse while both tasks are still open
        let err = forum
            .set_status("#gated-q".to_owned(), "solved".to_owned())
            .await
            .expect_err("solving a gated question should be refused");
        let message = err.to_string();
        assert!(message.contains("Cannot mark #gated-q solved: 2 task(s) are incomplete"));
        assert!(message.contains("task 1:"));
        // blocked is the un-gated escape hatch, so it must succeed as-is
        forum
            .set_status("#gated-q".to_owned(), "blocked".to_owned())
            .await
            .expect("blocking a gated question should stay un-gated");
        // reopen the question so the final solve exercises the cleared gate
        forum
            .set_status("#gated-q".to_owned(), "unsolved".to_owned())
            .await
            .expect("reopening the question should succeed");
        // complete both tasks, citing the real evidence comment
        for task in &created {
            forum
                .complete_task("tester".to_owned(), task.id, format!("!{}", cited.id))
                .await
                .expect("completing a task should succeed");
        }
        // with every gate cleared, solving now succeeds
        forum
            .set_status("#gated-q".to_owned(), "solved".to_owned())
            .await
            .expect("solving should succeed once every task is complete");
    }

    /// Tasks on a question comment gate that comment's own status
    #[tokio::test]
    async fn comment_question_tasks_gate_the_comment() {
        let forum = forum();
        // a statement post hosting a question raised in a comment
        post(&forum, "host-post", "a finding").await;
        let asked = forum
            .create_comment(
                "tester".to_owned(),
                "#host-post".to_owned(),
                "but which key is used?".to_owned(),
                "neutral".to_owned(),
                "question".to_owned(),
                String::new(),
            )
            .await
            .expect("the question comment should be created");
        // gate the question comment with one task via its !id reference
        let created = forum
            .add_tasks(
                "tester".to_owned(),
                format!("!{}", asked.id),
                vec!["check the key schedule".to_owned()],
            )
            .await
            .expect("the task should be added");
        assert_eq!(created[0].parent, ParentRef::Comment(asked.id));
        // solving the comment must refuse while its task is open
        assert!(
            forum
                .set_status(format!("!{}", asked.id), "solved".to_owned())
                .await
                .is_err()
        );
        // reply with the evidence and complete the task citing that reply
        let reply = comment(&forum, ParentRef::Comment(asked.id), "the key is 0x5A").await;
        forum
            .complete_task("tester".to_owned(), created[0].id, format!("!{}", reply.id))
            .await
            .expect("completing the task should succeed");
        // with the gate cleared, the comment question can be solved
        forum
            .set_status(format!("!{}", asked.id), "solved".to_owned())
            .await
            .expect("solving should succeed once the task is complete");
    }

    /// Task changes read as their owning question's latest activity
    #[tokio::test]
    async fn task_mutations_bump_the_owning_question_revision() {
        let forum = forum();
        // an older question, then a newer post so the question is not latest
        question(&forum, "rev-q").await;
        post(&forum, "rev-newer", "newer activity").await;
        assert_eq!(
            forum.view().await.last_updated,
            Some(ParentRef::Post("rev-newer".to_owned()))
        );
        // adding a task bumps the owning question back to the latest activity
        let created = forum
            .add_tasks(
                "tester".to_owned(),
                "#rev-q".to_owned(),
                vec!["check something".to_owned()],
            )
            .await
            .expect("the task should be added");
        assert_eq!(
            forum.view().await.last_updated,
            Some(ParentRef::Post("rev-q".to_owned()))
        );
        // a citing comment lands elsewhere and becomes the latest activity
        let cited = comment(&forum, ParentRef::Post("rev-newer".to_owned()), "evidence").await;
        assert_eq!(
            forum.view().await.last_updated,
            Some(ParentRef::Comment(cited.id))
        );
        // completing the task bumps the owning question back to the latest
        forum
            .complete_task("tester".to_owned(), created[0].id, format!("!{}", cited.id))
            .await
            .expect("completing the task should succeed");
        assert_eq!(
            forum.view().await.last_updated,
            Some(ParentRef::Post("rev-q".to_owned()))
        );
    }

    /// Task checklists ride on their question nodes in views and links
    #[tokio::test]
    async fn view_and_follow_link_attach_tasks_to_their_nodes() {
        let forum = forum();
        // a question post gated by one task
        forum
            .create_post(
                "tester".to_owned(),
                "node-q".to_owned(),
                "title".to_owned(),
                "body".to_owned(),
                "question".to_owned(),
                String::new(),
                vec!["post task".to_owned()],
            )
            .await
            .expect("question with a task should be created");
        // a question comment under it, gated by its own task
        let asked = forum
            .create_comment(
                "tester".to_owned(),
                "#node-q".to_owned(),
                "a follow-up question".to_owned(),
                "neutral".to_owned(),
                "question".to_owned(),
                String::new(),
            )
            .await
            .expect("the question comment should be created");
        forum
            .add_tasks(
                "tester".to_owned(),
                format!("!{}", asked.id),
                vec!["comment task".to_owned()],
            )
            .await
            .expect("the comment task should be added");
        // the view hangs each checklist on its own node
        let view = forum.view().await;
        assert_eq!(view.threads.len(), 1);
        assert_eq!(view.threads[0].tasks.len(), 1);
        assert_eq!(view.threads[0].tasks[0].description, "post task");
        assert_eq!(view.threads[0].comments[0].tasks.len(), 1);
        assert_eq!(
            view.threads[0].comments[0].tasks[0].description,
            "comment task"
        );
        // following the post resolves the same checklists on the same nodes
        let node = forum
            .follow_link("#node-q".to_owned())
            .await
            .expect("follow_link should resolve the post");
        let ForumNode::Post(thread) = node else {
            panic!("expected a post node");
        };
        assert_eq!(thread.tasks[0].description, "post task");
        assert_eq!(thread.comments[0].tasks[0].description, "comment task");
        // following the comment resolves its own checklist directly
        let node = forum
            .follow_link(format!("!{}", asked.id))
            .await
            .expect("follow_link should resolve the comment");
        let ForumNode::Comment(reply) = node else {
            panic!("expected a comment node");
        };
        assert_eq!(reply.tasks[0].description, "comment task");
    }

    /// The compact dump renders checklists with open and completed boxes
    #[tokio::test]
    async fn render_forum_lists_task_checklists() {
        let forum = forum();
        // a question gated by one task, plus the comment that will satisfy it
        question(&forum, "render-q").await;
        forum
            .add_tasks(
                "tester".to_owned(),
                "#render-q".to_owned(),
                vec!["check the import table".to_owned()],
            )
            .await
            .expect("the task should be added");
        let cited = comment(
            &forum,
            ParentRef::Post("render-q".to_owned()),
            "the evidence",
        )
        .await;
        // the open task renders under the checklist header as an unchecked box
        let rendered = forum.render_forum().await;
        assert!(rendered.contains(
            "Tasks (each must be completed with complete_task before this question can be solved):"
        ));
        assert!(rendered.contains("[ ] task 1: check the import table"));
        // once completed, the same task renders checked with its citation
        forum
            .complete_task("tester".to_owned(), 1, format!("!{}", cited.id))
            .await
            .expect("completing the task should succeed");
        let rendered = forum.render_forum().await;
        assert!(rendered.contains(&format!(
            "[x] task 1: check the import table (completed by tester, see !{})",
            cited.id
        )));
    }

    /// The boxed dump frames checklists in a TASKS section only when present
    #[tokio::test]
    async fn render_forum_boxed_frames_tasks() {
        // a forum whose question is gated by one task
        let tasked = forum();
        question(&tasked, "boxed-task-q").await;
        tasked
            .add_tasks(
                "tester".to_owned(),
                "#boxed-task-q".to_owned(),
                vec!["check the stub".to_owned()],
            )
            .await
            .expect("the task should be added");
        // the boxed dump must frame the checklist in its own TASKS section
        let rendered = tasked.render_forum_boxed().await;
        assert!(rendered.contains("| TASKS"), "expected a TASKS box header");
        assert!(rendered.contains("[ ] task 1: check the stub"));
        // a forum without tasks renders no TASKS section anywhere
        let taskless = forum();
        question(&taskless, "boxed-taskless-q").await;
        let rendered = taskless.render_forum_boxed().await;
        assert!(
            !rendered.contains("| TASKS"),
            "expected no TASKS box header"
        );
    }

    /// Question task checklists survive into the report appendix
    #[test]
    fn report_appendix_renders_question_tasks() {
        // a solved question whose single gate was completed citing comment 1
        let posts = vec![appendix_post(
            "tasked-solved-q",
            "main",
            "which mutex is created?",
            Kind::Question(QuestionStatus::Solved),
        )];
        let comments: HashMap<usize, Comment> = [appendix_comment(
            1,
            ParentRef::Post("tasked-solved-q".to_owned()),
            "main",
            "mutex Global\\abc",
        )]
        .into_iter()
        .collect();
        // build the task map directly, completed the way the live forum records it
        let tasks: HashMap<usize, Task> = [(
            1,
            Task {
                id: 1,
                parent: ParentRef::Post("tasked-solved-q".to_owned()),
                author: "main".to_owned(),
                description: "check the mutex name".to_owned(),
                completed: Some(TaskCompletion {
                    comment: 1,
                    author: "main".to_owned(),
                }),
                updated: 0,
            },
        )]
        .into_iter()
        .collect();
        // render the appendix with an unbounded budget
        let out = render_report_appendix(&posts, &comments, &tasks, &[], &[], usize::MAX)
            .expect("the appendix should render");
        // the solved thread's block carries its completed checklist line
        assert!(out.contains("### Solved questions"));
        assert!(out.contains("[x] task 1: check the mutex name (completed by main, see !1)"));
    }

    /// Tasks and their id counter survive a snapshot/restore round-trip
    #[tokio::test]
    async fn snapshot_restore_preserves_tasks_and_next_task_id() {
        // seed a forum with a gated question, a citing comment, and a completion
        let source = forum();
        question(&source, "snap-q").await;
        let created = source
            .add_tasks(
                "tester".to_owned(),
                "#snap-q".to_owned(),
                vec!["check something".to_owned()],
            )
            .await
            .expect("the task should be added");
        let cited = comment(&source, ParentRef::Post("snap-q".to_owned()), "evidence").await;
        let completed = source
            .complete_task(
                "finisher".to_owned(),
                created[0].id,
                format!("!{}", cited.id),
            )
            .await
            .expect("completing the task should succeed");

        // snapshot the source forum, then restore it onto a fresh, empty forum
        let snapshot = source.snapshot().await;
        let restored = forum();
        restored.restore(snapshot).await;

        // the task comes back exactly as it was completed, field by field
        let tasks = restored.tasks.read().await;
        assert_eq!(tasks.len(), 1);
        let task = &tasks[&completed.id];
        assert_eq!(task.id, completed.id);
        assert_eq!(task.parent, ParentRef::Post("snap-q".to_owned()));
        assert_eq!(task.author, "tester");
        assert_eq!(task.description, "check something");
        assert_eq!(task.updated, completed.updated);
        let completion = task
            .completed
            .as_ref()
            .expect("the completion should survive the round-trip");
        assert_eq!(completion.comment, cited.id);
        assert_eq!(completion.author, "finisher");
        drop(tasks);
        // the next-task-id continues where the source left off, so a new task
        // does not collide with the restored one's id
        let next = restored
            .add_tasks(
                "tester".to_owned(),
                "#snap-q".to_owned(),
                vec!["another check".to_owned()],
            )
            .await
            .expect("a post-restore task should be added");
        assert!(next[0].id > completed.id);
    }

    /// Snapshots written before tasks existed still parse and restore
    #[tokio::test]
    async fn legacy_forum_json_without_tasks_still_restores() {
        // a pre-task forum.json carries no tasks and no next_task_id at all
        let legacy = r#"{
            "posts": [{
                "name": "legacy-q",
                "author": "tester",
                "title": "a legacy question",
                "body": "asked before tasks existed",
                "kind": {"question": "unsolved"},
                "updated": 1
            }],
            "comments": [],
            "next_comment_id": 1,
            "finish_requested": false
        }"#;
        let snapshot: ForumSnapshot =
            serde_json::from_str(legacy).expect("the legacy snapshot should parse");
        // the serde defaults leave the parsed snapshot taskless
        assert!(snapshot.tasks.is_empty());
        assert_eq!(snapshot.next_task_id, 0);
        // restoring it must leave a forum that hands out task ids from 1
        let restored = forum();
        restored.restore(snapshot).await;
        let created = restored
            .add_tasks(
                "tester".to_owned(),
                "#legacy-q".to_owned(),
                vec!["a modern task".to_owned()],
            )
            .await
            .expect("adding a task after a legacy restore should succeed");
        assert_eq!(created[0].id, 1);
        // a snapshot holding a task but a zeroed counter derives a safe next id
        let zeroed = r#"{
            "posts": [{
                "name": "legacy-q",
                "author": "tester",
                "title": "a legacy question",
                "body": "asked before tasks existed",
                "kind": {"question": "unsolved"},
                "updated": 1
            }],
            "comments": [],
            "tasks": [{
                "id": 5,
                "parent": {"Post": "legacy-q"},
                "author": "tester",
                "description": "an old task",
                "updated": 2
            }],
            "next_comment_id": 1,
            "next_task_id": 0,
            "finish_requested": false
        }"#;
        let snapshot: ForumSnapshot =
            serde_json::from_str(zeroed).expect("the zeroed-counter snapshot should parse");
        let restored = forum();
        restored.restore(snapshot).await;
        // the next task id continues past the restored task, never reusing 5
        let created = restored
            .add_tasks(
                "tester".to_owned(),
                "#legacy-q".to_owned(),
                vec!["a fresh task".to_owned()],
            )
            .await
            .expect("adding a task after the zeroed restore should succeed");
        assert_eq!(created[0].id, 6);
    }

    /// Builds a two-option ask payload for the analyst-record tests
    fn ask_request(question: &str) -> AskRequest {
        AskRequest {
            asked_by: "main".to_owned(),
            question: question.to_owned(),
            options: vec![
                AskOption {
                    label: "static".to_owned(),
                    description: "static first".to_owned(),
                },
                AskOption {
                    label: "dynamic".to_owned(),
                    description: "dynamic first".to_owned(),
                },
            ],
            recommended: "static".to_owned(),
        }
    }

    /// The ask cap refuses headless records but never an announced answer
    #[tokio::test]
    async fn ask_cap_spares_ticketed_records() {
        // spend the whole ask budget on the headless (ticket-less) path
        let forum = forum();
        for index in 0..10 {
            forum
                .record_ask(
                    ask_request(&format!("question {index}?")),
                    "static".to_owned(),
                    AnswerSource::AutoDefault,
                    0,
                )
                .await
                .expect("an in-budget ask should record");
        }
        // another headless ask is refused at the cap
        assert!(
            forum
                .record_ask(
                    ask_request("one too many?"),
                    "static".to_owned(),
                    AnswerSource::AutoDefault,
                    0,
                )
                .await
                .is_err()
        );
        // an already-announced (ticketed) ask's human answer still records
        let answered = forum
            .record_ask(
                ask_request("announced before the cap?"),
                "dynamic".to_owned(),
                AnswerSource::Analyst,
                42,
            )
            .await
            .expect("an announced ask's answer must never be discarded");
        assert_eq!(answered.source, AnswerSource::Analyst);
    }

    /// Recorded asks and review flags never gate the finish tool
    #[tokio::test]
    async fn finish_ignores_asks_and_flags() {
        // a forum whose only contents are an ask and a review flag
        let forum = forum();
        forum
            .record_ask(
                ask_request("which path?"),
                "static".to_owned(),
                AnswerSource::AutoDefault,
                0,
            )
            .await
            .expect("recording the ask should succeed");
        forum
            .add_review_flag(
                "main".to_owned(),
                "weak attribution".to_owned(),
                "one sample only".to_owned(),
                None,
                None,
            )
            .await
            .expect("raising the flag should succeed");
        // neither record counts as open work, so finish is accepted
        assert_eq!(forum.open_question_count().await, 0);
        forum
            .finish()
            .await
            .expect("finish must ignore asks and flags");
    }

    /// Asks and flags round-trip a snapshot and keep handing out fresh ids
    #[tokio::test]
    async fn snapshot_restore_preserves_asks_and_flags() {
        // seed a forum with one resolved ask and one anchored flag
        let source = forum();
        question(&source, "flagged-q").await;
        let ask = source
            .record_ask(
                ask_request("which path?"),
                "dynamic".to_owned(),
                AnswerSource::Analyst,
                7,
            )
            .await
            .expect("recording the ask should succeed");
        let flag = source
            .add_review_flag(
                "system".to_owned(),
                "Unanswered analyst question: which path?".to_owned(),
                "took the default".to_owned(),
                Some(ParentRef::Post("flagged-q".to_owned())),
                Some(ask.id),
            )
            .await
            .expect("raising the flag should succeed");
        // snapshot the source forum, then restore it onto a fresh, empty forum
        let snapshot = source.snapshot().await;
        let restored = forum();
        restored.restore(snapshot).await;
        // both records come back field by field
        let asks = restored.asks_snapshot().await;
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].id, ask.id);
        assert_eq!(asks[0].chosen, "dynamic");
        assert_eq!(asks[0].source, AnswerSource::Analyst);
        assert_eq!(asks[0].recommended, "static");
        let flags = restored.flags_snapshot().await;
        assert_eq!(flags.len(), 1);
        assert_eq!(flags[0].id, flag.id);
        assert_eq!(flags[0].author, "system");
        assert_eq!(
            flags[0].reference,
            Some(ParentRef::Post("flagged-q".to_owned()))
        );
        assert_eq!(flags[0].ask, Some(ask.id));
        // the next ask id continues past the restored one, never colliding
        let next = restored
            .record_ask(
                ask_request("another fork?"),
                "static".to_owned(),
                AnswerSource::AutoDefault,
                0,
            )
            .await
            .expect("a post-restore ask should record");
        assert!(next.id > ask.id);
    }

    /// Snapshots written before asks and flags existed still parse and restore
    #[tokio::test]
    async fn legacy_forum_json_without_asks_still_restores() {
        // a pre-feature forum.json carries no asks, flags, or their counters
        let legacy = r#"{
            "posts": [],
            "comments": [],
            "next_comment_id": 1,
            "finish_requested": false
        }"#;
        let snapshot: ForumSnapshot =
            serde_json::from_str(legacy).expect("the legacy snapshot should parse");
        // the serde defaults leave the parsed snapshot ask- and flag-less
        assert!(snapshot.asks.is_empty());
        assert!(snapshot.flags.is_empty());
        assert_eq!(snapshot.next_ask_id, 0);
        assert_eq!(snapshot.next_flag_id, 0);
        // restoring it must leave a forum that hands out ids from 1
        let restored = forum();
        restored.restore(snapshot).await;
        let ask = restored
            .record_ask(
                ask_request("first modern ask?"),
                "static".to_owned(),
                AnswerSource::AutoDefault,
                0,
            )
            .await
            .expect("recording after a legacy restore should succeed");
        assert_eq!(ask.id, 1);
        let flag = restored
            .add_review_flag(
                "main".to_owned(),
                "first modern flag".to_owned(),
                "needs eyes".to_owned(),
                None,
                None,
            )
            .await
            .expect("flagging after a legacy restore should succeed");
        assert_eq!(flag.id, 1);
    }

    /// Ask and flag mutations emit their dedicated events, not ForumUpdated
    #[tokio::test]
    async fn ask_and_flag_mutations_emit_dedicated_events() {
        // install an event sink before mutating
        let forum = forum();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        forum.set_events(tx);
        // record one ask and one flag
        let ask = forum
            .record_ask(
                ask_request("which path?"),
                "static".to_owned(),
                AnswerSource::Declined,
                42,
            )
            .await
            .expect("recording the ask should succeed");
        forum
            .add_review_flag(
                "system".to_owned(),
                "Unanswered analyst question: which path?".to_owned(),
                "declined".to_owned(),
                None,
                Some(ask.id),
            )
            .await
            .expect("raising the flag should succeed");
        drop(forum);
        // exactly the two dedicated events arrive, in mutation order
        match rx.recv().await.expect("the resolved event should arrive") {
            ThorPalEvent::AnalystAskResolved {
                id,
                ask: record,
                agent,
                chosen,
                source,
                ..
            } => {
                assert_eq!(id, 42);
                assert_eq!(record, ask.id);
                assert_eq!(agent, "main");
                assert_eq!(chosen, "static");
                assert_eq!(source, "declined");
            }
            other => panic!("expected an ask-resolved event, got {other:?}"),
        }
        match rx.recv().await.expect("the flag event should arrive") {
            ThorPalEvent::ReviewFlagRaised {
                agent,
                title,
                reference,
                ..
            } => {
                assert_eq!(agent, "system");
                assert!(title.starts_with("Unanswered analyst question"));
                assert_eq!(reference, None);
            }
            other => panic!("expected a flag-raised event, got {other:?}"),
        }
        assert!(rx.recv().await.is_none());
    }

    /// The appendix renders analyst sections between questions and findings
    #[test]
    fn report_appendix_orders_analyst_sections_before_findings() {
        // one solved question and one agent finding bracket the new sections
        let posts = vec![
            appendix_post(
                "solved-q",
                "main",
                "answered",
                Kind::Question(QuestionStatus::Solved),
            ),
            appendix_post("a-finding", "main", "finding body", Kind::Statement),
        ];
        let asks = vec![AnalystAsk {
            id: 1,
            asked_by: "main".to_owned(),
            question: "which path?".to_owned(),
            options: ask_request("which path?").options,
            recommended: "static".to_owned(),
            chosen: "static".to_owned(),
            source: AnswerSource::AutoDefault,
            updated: 3,
        }];
        let flags = vec![ReviewFlag {
            id: 1,
            author: "system".to_owned(),
            title: "Unanswered analyst question: which path?".to_owned(),
            reason: "took the default".to_owned(),
            reference: None,
            ask: Some(1),
            updated: 4,
        }];
        let out = render_report_appendix(
            &posts,
            &HashMap::new(),
            &HashMap::new(),
            &asks,
            &flags,
            usize::MAX,
        )
        .expect("the appendix should render");
        // the analyst sections sit after the questions, before the findings
        let solved = out.find("### Solved questions").expect("solved section");
        let decisions = out.find("### Analyst decisions").expect("ask section");
        let review = out.find("### Needs analyst review").expect("flag section");
        let findings = out.find("### Finding posts").expect("findings section");
        assert!(solved < decisions && decisions < review && review < findings);
        // the blocks carry the question, the marked default, and the answer
        assert!(out.contains("[ask 1] which path? (asked by main)"));
        assert!(out.contains("- static (recommended): static first"));
        assert!(out.contains("auto-default (no analyst attached)"));
        assert!(out.contains("[flag 1] Unanswered analyst question: which path? (by system)"));
        assert!(out.contains("(auto-filed for ask 1)"));
        // an appendix with only analyst records (no posts) still renders
        let only = render_report_appendix(
            &[],
            &HashMap::new(),
            &HashMap::new(),
            &asks,
            &flags,
            usize::MAX,
        )
        .expect("analyst records alone should still render an appendix");
        assert!(only.contains("### Analyst decisions"));
    }

    /// The forum dump closes with the analyst tail when records exist
    #[tokio::test]
    async fn render_forum_appends_the_analyst_tail() {
        // a forum with no posts but one ask and one flag still shows the tail
        let forum = forum();
        forum
            .record_ask(
                ask_request("which path?"),
                "static".to_owned(),
                AnswerSource::AutoDefault,
                0,
            )
            .await
            .expect("recording the ask should succeed");
        forum
            .add_review_flag(
                "main".to_owned(),
                "weak attribution".to_owned(),
                "one sample only".to_owned(),
                None,
                None,
            )
            .await
            .expect("raising the flag should succeed");
        let out = forum.render_forum().await;
        assert!(out.contains("(no posts)"));
        assert!(out.contains("Analyst decisions"));
        assert!(out.contains("Needs analyst review"));
        assert!(out.contains("[ask 1] which path? (asked by main)"));
        assert!(out.contains("[flag 1] weak attribution (by main)"));
    }

    /// Task mutations report their owning question on the event sink
    #[tokio::test]
    async fn task_mutations_emit_forum_updated_events() {
        // build the question and citing comment before the sink is installed
        let forum = forum();
        question(&forum, "evented-q").await;
        let cited = comment(&forum, ParentRef::Post("evented-q".to_owned()), "evidence").await;
        // install an event sink like the builder does for a run with one
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        forum.set_events(tx);
        // add a task, then complete it citing the pre-existing comment
        let created = forum
            .add_tasks_to(
                "tester".to_owned(),
                ParentRef::Post("evented-q".to_owned()),
                vec!["check something".to_owned()],
            )
            .await
            .expect("the task should be added");
        forum
            .complete_task_by("tester".to_owned(), created[0].id, cited.id)
            .await
            .expect("completing the task should succeed");
        // each task mutation reports the owning question, in order
        let mut targets = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                ThorPalEvent::ForumUpdated { target } => targets.push(target),
                other => panic!("unexpected event {other:?}"),
            }
        }
        assert_eq!(
            targets,
            vec![
                ParentRef::Post("evented-q".to_owned()),
                ParentRef::Post("evented-q".to_owned()),
            ]
        );
    }

    /// Task mutations persist a parseable forum.json with the task state
    #[tokio::test]
    async fn task_mutations_persist_tasks_to_the_session_store() {
        // claim a session store under a scratch root like a real run does
        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::thorpal::store::SessionStore::create(
            dir.path().to_path_buf(),
            Some("ForumTaskTest".to_owned()),
            std::path::PathBuf::from("bin/target"),
            "target".to_owned(),
            false,
            Vec::new(),
        )
        .await
        .expect("store should create");
        // install the store on an isolated forum like the builder does
        let forum = forum();
        forum.set_store(Arc::clone(&store));
        // a gated question plus the comment its completion will cite
        question(&forum, "persisted-q").await;
        let cited = comment(
            &forum,
            ParentRef::Post("persisted-q".to_owned()),
            "evidence",
        )
        .await;
        let created = forum
            .add_tasks(
                "tester".to_owned(),
                "#persisted-q".to_owned(),
                vec!["check something".to_owned()],
            )
            .await
            .expect("the task should be added");
        // the add must leave a forum.json holding the still-open task
        let path = dir.path().join("ForumTaskTest").join("forum.json");
        let saved = tokio::fs::read(&path)
            .await
            .expect("forum.json should exist");
        let snapshot: ForumSnapshot =
            serde_json::from_slice(&saved).expect("forum.json should parse");
        assert_eq!(snapshot.tasks.len(), 1);
        assert_eq!(snapshot.tasks[0].id, created[0].id);
        assert!(snapshot.tasks[0].completed.is_none());
        // completing the task must persist its completion too
        forum
            .complete_task("tester".to_owned(), created[0].id, format!("!{}", cited.id))
            .await
            .expect("completing the task should succeed");
        let saved = tokio::fs::read(&path)
            .await
            .expect("forum.json should exist");
        let snapshot: ForumSnapshot =
            serde_json::from_slice(&saved).expect("forum.json should parse");
        let completion = snapshot.tasks[0]
            .completed
            .as_ref()
            .expect("the saved task should be completed");
        assert_eq!(completion.comment, cited.id);
    }

    /// An analyst's task is attributed to the user and validated
    #[tokio::test]
    async fn add_user_task_attributes_to_user_and_validates() {
        let forum = forum();
        // a question for the analyst to gate and a statement that cannot be
        question(&forum, "user-task-q").await;
        post(&forum, "user-task-statement", "a plain statement").await;
        // a valid task lands on the question, attributed to the human operator
        let task = forum
            .add_user_task(
                ParentRef::Post("user-task-q".to_owned()),
                "verify the unpacking stub".to_owned(),
            )
            .await
            .expect("the user task should be created");
        assert_eq!(task.author, USER_AUTHOR);
        assert_eq!(task.parent, ParentRef::Post("user-task-q".to_owned()));
        // a blank description says nothing checkable, so it is rejected
        assert!(
            forum
                .add_user_task(ParentRef::Post("user-task-q".to_owned()), "   ".to_owned())
                .await
                .is_err()
        );
        // a statement target is rejected since tasks only gate questions
        assert!(
            forum
                .add_user_task(
                    ParentRef::Post("user-task-statement".to_owned()),
                    "check something".to_owned(),
                )
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn snapshot_restore_preserves_posts_comments_and_scalars() {
        // seed a forum with a post, a comment under it, and a requested finish
        let source = forum();
        source
            .create_post(
                "tester".to_owned(),
                "seed-post".to_owned(),
                "title".to_owned(),
                "body".to_owned(),
                "statement".to_owned(),
                String::new(),
                Vec::new(),
            )
            .await
            .expect("post should be created");
        let comment = source
            .create_comment(
                "tester".to_owned(),
                "#seed-post".to_owned(),
                "supporting evidence".to_owned(),
                "supports".to_owned(),
                "statement".to_owned(),
                String::new(),
            )
            .await
            .expect("comment should be created");

        // snapshot the source forum, then restore it onto a fresh, empty forum
        let snapshot = source.snapshot().await;
        let restored = forum();
        restored.restore(snapshot).await;

        // the post comes back keyed by its original name
        let posts = restored.posts.read().await;
        assert_eq!(posts.len(), 1);
        assert!(posts.contains_key("seed-post"));
        drop(posts);
        // the comment comes back with its original id preserved
        let comments = restored.comments.read().await;
        assert_eq!(comments.len(), 1);
        assert!(comments.contains_key(&comment.id));
        drop(comments);
        // the next-comment-id continues where the source left off, so a new
        // comment does not collide with the restored one's id
        let next = restored
            .create_comment(
                "tester".to_owned(),
                "#seed-post".to_owned(),
                "another comment".to_owned(),
                "neutral".to_owned(),
                "statement".to_owned(),
                String::new(),
            )
            .await
            .expect("a post-restore comment should be created");
        assert!(next.id > comment.id);
    }

    /// The view's latest activity follows creations and status changes
    #[tokio::test]
    async fn view_tracks_the_most_recently_updated_item() {
        // start from an isolated, empty forum
        let forum = forum();
        // an empty forum has nothing to point at
        assert!(forum.view().await.last_updated.is_none());
        // a question post, then a comment under it
        let post = forum
            .seed_post(
                "tester".to_owned(),
                "latest-q".to_owned(),
                "title".to_owned(),
                "body".to_owned(),
                Kind::Question(QuestionStatus::Unsolved),
            )
            .await
            .expect("post should be created");
        // a lone post is the latest activity
        assert_eq!(
            forum.view().await.last_updated,
            Some(ParentRef::Post("latest-q".to_owned()))
        );
        // comment on it
        let comment = forum
            .seed_comment(
                "tester".to_owned(),
                ParentRef::Post("latest-q".to_owned()),
                "evidence".to_owned(),
                Stance::Supports,
                Kind::Statement,
            )
            .await
            .expect("comment should be created");
        // the newer comment carries a higher revision and becomes the latest
        assert!(comment.updated > post.updated);
        assert_eq!(
            forum.view().await.last_updated,
            Some(ParentRef::Comment(comment.id))
        );
        // a status change on the older post makes it the latest again
        forum
            .set_question_status(
                ParentRef::Post("latest-q".to_owned()),
                QuestionStatus::Solved,
            )
            .await
            .expect("status change should succeed");
        // the post is now the latest activity
        let view = forum.view().await;
        assert_eq!(
            view.last_updated,
            Some(ParentRef::Post("latest-q".to_owned()))
        );
        // the view carries the thread with the comment nested under its post
        assert_eq!(view.threads.len(), 1);
        assert_eq!(view.threads[0].comments.len(), 1);
        assert!(view.threads[0].post.updated > comment.updated);
    }

    /// An analyst's comment is attributed to the user and validated
    #[tokio::test]
    async fn add_user_comment_attributes_to_user_and_validates() {
        // seed a post for the analyst to reply to
        let forum = forum();
        forum
            .seed_post(
                "tester".to_owned(),
                "user-reply".to_owned(),
                "title".to_owned(),
                "body".to_owned(),
                Kind::Statement,
            )
            .await
            .expect("post should be created");
        // a valid reply lands under the post, attributed to the human operator
        let comment = forum
            .add_user_comment(
                ParentRef::Post("user-reply".to_owned()),
                "check the second call site".to_owned(),
                Stance::Neutral,
                Kind::Statement,
            )
            .await
            .expect("user comment should be created");
        assert_eq!(comment.author, USER_AUTHOR);
        // the reply shows up under the post in the view
        let view = forum.view().await;
        assert_eq!(view.threads[0].comments[0].comment.id, comment.id);
        // an empty body or an unknown parent is rejected
        assert!(
            forum
                .add_user_comment(
                    ParentRef::Post("user-reply".to_owned()),
                    "   ".to_owned(),
                    Stance::Neutral,
                    Kind::Statement,
                )
                .await
                .is_err()
        );
        assert!(
            forum
                .add_user_comment(
                    ParentRef::Comment(9999),
                    "orphan".to_owned(),
                    Stance::Neutral,
                    Kind::Statement,
                )
                .await
                .is_err()
        );
    }

    /// Setting a status on a statement is rejected
    #[tokio::test]
    async fn set_question_status_rejects_statement() {
        // seed a statement post
        let forum = forum();
        forum
            .seed_post(
                "tester".to_owned(),
                "typed-statement".to_owned(),
                "title".to_owned(),
                "body".to_owned(),
                Kind::Statement,
            )
            .await
            .expect("post should be created");
        // a statement has no status to set
        assert!(
            forum
                .set_question_status(
                    ParentRef::Post("typed-statement".to_owned()),
                    QuestionStatus::Solved,
                )
                .await
                .is_err()
        );
    }

    /// Every mutation reports its target on the event sink, in order
    #[tokio::test]
    async fn mutations_emit_forum_updated_events() {
        // start from an isolated, empty forum
        let forum = forum();
        // install an event sink like the builder does for a run with one
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        forum.set_events(tx);
        // create a post, comment on it, then change its status
        forum
            .seed_post(
                "tester".to_owned(),
                "evented".to_owned(),
                "title".to_owned(),
                "body".to_owned(),
                Kind::Question(QuestionStatus::Unsolved),
            )
            .await
            .expect("post should be created");
        let comment = forum
            .add_user_comment(
                ParentRef::Post("evented".to_owned()),
                "a reply".to_owned(),
                Stance::Neutral,
                Kind::Statement,
            )
            .await
            .expect("comment should be created");
        forum
            .set_question_status(
                ParentRef::Post("evented".to_owned()),
                QuestionStatus::Blocked,
            )
            .await
            .expect("status change should succeed");
        // each mutation reports its target, in order
        let mut targets = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                ThorPalEvent::ForumUpdated { target } => targets.push(target),
                other => panic!("unexpected event {other:?}"),
            }
        }
        assert_eq!(
            targets,
            vec![
                ParentRef::Post("evented".to_owned()),
                ParentRef::Comment(comment.id),
                ParentRef::Post("evented".to_owned()),
            ]
        );
    }

    /// Every mutation persists a parseable forum.json into the session store
    #[tokio::test]
    async fn mutations_persist_forum_to_the_session_store() {
        // claim a session store under a scratch root like a real run does
        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::thorpal::store::SessionStore::create(
            dir.path().to_path_buf(),
            Some("ForumTest".to_owned()),
            std::path::PathBuf::from("bin/target"),
            "target".to_owned(),
            false,
            Vec::new(),
        )
        .await
        .expect("store should create");
        // install the store on an isolated forum like the builder does
        let forum = forum();
        forum.set_store(Arc::clone(&store));
        // a single mutation must leave a parseable forum.json behind
        forum
            .seed_post(
                "tester".to_owned(),
                "persisted".to_owned(),
                "title".to_owned(),
                "body".to_owned(),
                Kind::Statement,
            )
            .await
            .expect("post should be created");
        // read the saved forum back and check the post survived
        let saved = tokio::fs::read(dir.path().join("ForumTest").join("forum.json"))
            .await
            .expect("forum.json should exist");
        let snapshot: ForumSnapshot =
            serde_json::from_slice(&saved).expect("forum.json should parse");
        assert_eq!(snapshot.posts.len(), 1);
        assert_eq!(snapshot.posts[0].name, "persisted");
    }

    /// Restoring keeps revisions and later changes still sort as newest
    #[tokio::test]
    async fn restore_preserves_revisions_and_continues_past_them() {
        // build a source forum with a post and a comment under it
        let source = forum();
        source
            .seed_post(
                "tester".to_owned(),
                "rev-post".to_owned(),
                "title".to_owned(),
                "body".to_owned(),
                Kind::Statement,
            )
            .await
            .expect("post should be created");
        let comment = source
            .seed_comment(
                "tester".to_owned(),
                ParentRef::Post("rev-post".to_owned()),
                "evidence".to_owned(),
                Stance::Supports,
                Kind::Statement,
            )
            .await
            .expect("comment should be created");
        // restore onto a fresh forum whose own revision counter starts over
        let restored = forum();
        restored.restore(source.snapshot().await).await;
        // the restored items keep their revisions, so the latest is unchanged
        assert_eq!(
            restored.view().await.last_updated,
            Some(ParentRef::Comment(comment.id))
        );
        // a post-restore change sorts after everything that was restored
        let next = restored
            .add_user_comment(
                ParentRef::Post("rev-post".to_owned()),
                "later".to_owned(),
                Stance::Neutral,
                Kind::Statement,
            )
            .await
            .expect("post-restore comment should be created");
        assert!(next.updated > comment.updated);
    }

    /// Seeds a statement post with the given name and body for a test
    ///
    /// # Arguments
    ///
    /// * `forum` - The forum to seed the post onto
    /// * `name` - The post's unique reference name
    /// * `body` - The post's body text
    async fn post(forum: &ForumState, name: &str, body: &str) {
        forum
            .seed_post(
                "tester".to_owned(),
                name.to_owned(),
                format!("title-{name}"),
                body.to_owned(),
                Kind::Statement,
            )
            .await
            .expect("post should seed");
    }

    /// Seeds a neutral statement comment under a parent for a test
    ///
    /// # Arguments
    ///
    /// * `forum` - The forum to seed the comment onto
    /// * `parent` - The post or comment the comment replies to
    /// * `body` - The comment's body text
    async fn comment(forum: &ForumState, parent: ParentRef, body: &str) -> Comment {
        forum
            .seed_comment(
                "tester".to_owned(),
                parent,
                body.to_owned(),
                Stance::Neutral,
                Kind::Statement,
            )
            .await
            .expect("comment should seed")
    }

    /// The scanner finds references, trims punctuation, and dedupes in order.
    #[test]
    fn scan_references_finds_trims_and_dedupes() {
        // wrapped and repeated references plus plain words and junk tokens
        let text = "see (#alpha), then `!3` and #beta. #alpha again, not !abc or # or bare words";
        let refs = scan_references(text);
        // the junk is dropped and each reference keeps its first-seen position
        assert_eq!(
            refs,
            vec![
                ParentRef::Post("alpha".to_owned()),
                ParentRef::Comment(3),
                ParentRef::Post("beta".to_owned()),
            ]
        );
    }

    /// An empty forum preloads nothing at all.
    #[tokio::test]
    async fn render_linked_context_empty_forum_is_none() {
        let forum = forum();
        // with no posts there is nothing worth injecting
        let roots = [ParentRef::Post("missing".to_owned())];
        assert!(forum.render_linked_context(&roots, 2).await.is_none());
    }

    /// Unresolvable roots degrade to the fallback note plus the index.
    #[tokio::test]
    async fn render_linked_context_falls_back_to_index() {
        let forum = forum();
        post(&forum, "only-post", "a body with no refs").await;
        // a dangling root resolves to nothing, so only the index renders
        let roots = [ParentRef::Post("missing".to_owned())];
        let out = forum
            .render_linked_context(&roots, 2)
            .await
            .expect("a non-empty forum should render");
        assert!(out.contains("No forum references were found in your task"));
        assert!(out.contains("## All forum posts (index)"));
        // the index lists the post by title without rendering its body
        assert!(out.contains("#only-post [Statement] — title-only-post (by tester)"));
        assert!(!out.contains("a body with no refs"));
    }

    /// The walk follows references hop by hop and stops at the depth cap.
    ///
    /// Bodies prove whether a thread rendered, since the index lists every
    /// post's header line either way but never includes a body.
    #[tokio::test]
    async fn render_linked_context_caps_depth() {
        let forum = forum();
        // a three-post chain a -> b -> c linked through post bodies
        post(&forum, "a", "see #b").await;
        post(&forum, "b", "see #c").await;
        post(&forum, "c", "the end").await;
        // depth 1 pulls a and b but leaves c to the index
        let roots = [ParentRef::Post("a".to_owned())];
        let out = forum
            .render_linked_context(&roots, 1)
            .await
            .expect("the forum should render");
        assert!(out.contains("see #b"));
        assert!(out.contains("see #c"));
        // c's thread is not rendered, but the index still lists its title
        assert!(!out.contains("the end"));
        assert!(out.contains("#c [Statement] — title-c (by tester)"));
        // depth 0 pulls the assigned thread alone
        let out = forum
            .render_linked_context(&roots, 0)
            .await
            .expect("the forum should render");
        assert!(out.contains("see #b"));
        assert!(!out.contains("see #c"));
    }

    /// A reference cycle terminates with each post rendered exactly once.
    #[tokio::test]
    async fn render_linked_context_handles_cycles() {
        let forum = forum();
        // two posts referencing each other form a cycle
        post(&forum, "ping", "see #pong").await;
        post(&forum, "pong", "see #ping").await;
        let roots = [ParentRef::Post("ping".to_owned())];
        let out = forum
            .render_linked_context(&roots, 5)
            .await
            .expect("the forum should render");
        // each body renders exactly once despite the cycle and the deep cap
        assert_eq!(out.matches("see #pong").count(), 1);
        assert_eq!(out.matches("see #ping").count(), 1);
    }

    /// A comment root pulls its containing post's full thread, and references
    /// inside comment bodies are followed too.
    #[tokio::test]
    async fn render_linked_context_resolves_comments() {
        let forum = forum();
        post(&forum, "claim", "a bold claim").await;
        post(&forum, "aside", "linked only from a comment").await;
        // nest a reply under a comment so the root walk has a chain to climb
        let top = comment(&forum, ParentRef::Post("claim".to_owned()), "see #aside").await;
        let reply = comment(&forum, ParentRef::Comment(top.id), "nested evidence").await;
        // the nested reply as root resolves up to the claim post's thread
        let roots = [ParentRef::Comment(reply.id)];
        let out = forum
            .render_linked_context(&roots, 1)
            .await
            .expect("the forum should render");
        // the containing post's body and nested comments all render
        assert!(out.contains("a bold claim"));
        assert!(out.contains("nested evidence"));
        // the comment-body reference to #aside was followed as a hop
        assert!(out.contains("linked only from a comment"));
    }

    /// Rendered threads come roots-first, then name-sorted hop levels.
    #[tokio::test]
    async fn render_linked_context_orders_deterministically() {
        let forum = forum();
        // one root referencing two posts whose names sort against insert order
        post(&forum, "root", "see #zeta and #alpha").await;
        post(&forum, "zeta", "zeta-body").await;
        post(&forum, "alpha", "alpha-body").await;
        let roots = [ParentRef::Post("root".to_owned())];
        let out = forum
            .render_linked_context(&roots, 1)
            .await
            .expect("the forum should render");
        // the root renders first and the hop level renders name-sorted
        let root_at = out.find("see #zeta").expect("root should render");
        let alpha_at = out.find("alpha-body").expect("alpha should render");
        let zeta_at = out.find("zeta-body").expect("zeta should render");
        assert!(root_at < alpha_at && alpha_at < zeta_at);
    }
}
