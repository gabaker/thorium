//! On-disk session snapshots for saving and resuming a ThorPal run.
//!
//! A ThorPal run's durable state is its shared forum (persisted separately by
//! the session store as `forum.json`) plus the main agent's conversation
//! history. [`SessionSnapshot`] captures the conversation together with the
//! inputs needed to rebuild the [`crate::ThorPal`] the snapshot came from (the
//! sanitized LLM provider config, the caller's extra preamble, the resolved
//! Ghidra paths, and the workflow of sub-agents). The
//! [`crate::thorpal::store::SessionStore`] writes one after every main-agent
//! turn so a crashed run can be picked back up, and `thorctl analyze chat --resume`
//! loads one to continue the conversation.

use std::collections::BTreeMap;
use std::ops::AddAssign;
use std::path::Path;

use chrono::{DateTime, Utc};
use rig::completion::Message;
use serde::{Deserialize, Serialize};

use super::AiProviderConfig;
use super::agents::Workflow;
use super::tools::forum::{AnalystAsk, Comment, Post, ReviewFlag, Task};
use super::tools::ghidra::GhidraSettings;

/// The schema version the current serializer writes.
///
/// Bumped whenever [`SessionSnapshot`]'s shape changes incompatibly so a loader
/// can reject a snapshot it does not understand rather than misinterpret it.
///
/// v2: the `url`/`model` pair was replaced by an explicit, sanitized
/// [`AiProviderConfig`].
/// v3: the forum moved out to the session store's own `forum.json` and the
/// workflow moved in so a resume needs no workflow flag.
/// v4: the per-agent accumulated token usage moved into the snapshot so a
/// resume continues the run's running totals.
/// v5: the main conversation moved out to the session store's own `chat.json`
/// (saved after every message, not just settled turns); `messages` is written
/// empty and kept only so older snapshots still load.
/// v6: `ghidra.extra_binaries` carries a multi-sample session's sibling
/// samples (older snapshots deserialize it empty via the serde default).
pub const SNAPSHOT_VERSION: u32 = 6;

/// The oldest snapshot schema version the current loader still understands.
///
/// v3 snapshots predate the persisted usage map, which deserializes to empty
/// via `#[serde(default)]`, so they remain loadable without migration.
pub const MIN_SNAPSHOT_VERSION: u32 = 3;

/// The accumulated token usage for one agent, as persisted in a snapshot.
///
/// Mirrors [`rig::completion::Usage`] field-for-field but is owned by this
/// crate so the on-disk schema cannot drift when rig's type changes shape.
/// Cache semantics follow the provider that produced the counts: on the
/// Anthropic/Bedrock providers `input_tokens` is the uncached remainder only,
/// while on the OpenAI protocol it includes `cached_input_tokens`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTotals {
    /// The accumulated input ("prompt") tokens.
    #[serde(default)]
    pub input_tokens: u64,
    /// The accumulated output ("completion") tokens.
    #[serde(default)]
    pub output_tokens: u64,
    /// The accumulated total tokens as the provider reported them.
    #[serde(default)]
    pub total_tokens: u64,
    /// The accumulated input tokens read from a provider-managed cache.
    #[serde(default)]
    pub cached_input_tokens: u64,
    /// The accumulated input tokens written to a provider-managed cache.
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    /// The accumulated tool-use prompt tokens.
    #[serde(default)]
    pub tool_use_prompt_tokens: u64,
    /// The accumulated internal reasoning/"thinking" tokens.
    #[serde(default)]
    pub reasoning_tokens: u64,
}

impl From<rig::completion::Usage> for UsageTotals {
    /// Converts rig's live usage struct into the persistable mirror
    ///
    /// # Arguments
    ///
    /// * `usage` - The rig usage tally to persist
    fn from(usage: rig::completion::Usage) -> Self {
        // copy every counter across field-for-field
        UsageTotals {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            total_tokens: usage.total_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            tool_use_prompt_tokens: usage.tool_use_prompt_tokens,
            reasoning_tokens: usage.reasoning_tokens,
        }
    }
}

impl From<UsageTotals> for rig::completion::Usage {
    /// Converts a persisted usage tally back into rig's live usage struct
    ///
    /// # Arguments
    ///
    /// * `totals` - The persisted tally to rehydrate
    fn from(totals: UsageTotals) -> Self {
        // copy every counter across field-for-field
        rig::completion::Usage {
            input_tokens: totals.input_tokens,
            output_tokens: totals.output_tokens,
            total_tokens: totals.total_tokens,
            cached_input_tokens: totals.cached_input_tokens,
            cache_creation_input_tokens: totals.cache_creation_input_tokens,
            tool_use_prompt_tokens: totals.tool_use_prompt_tokens,
            reasoning_tokens: totals.reasoning_tokens,
        }
    }
}

impl AddAssign for UsageTotals {
    /// Folds another tally into this one, saturating rather than overflowing
    ///
    /// # Arguments
    ///
    /// * `other` - The tally to fold in
    fn add_assign(&mut self, other: Self) {
        // saturate each counter so pathological counts can never panic
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(other.cached_input_tokens);
        self.cache_creation_input_tokens = self
            .cache_creation_input_tokens
            .saturating_add(other.cache_creation_input_tokens);
        self.tool_use_prompt_tokens = self
            .tool_use_prompt_tokens
            .saturating_add(other.tool_use_prompt_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
    }
}

/// A serializable snapshot of one ThorPal forum's contents.
///
/// Holds every post, comment, and completion-gate task by value (all already
/// derive serde) plus the scalar pieces of forum state the control loop
/// consults -- the next comment/task ids to hand out and whether `finish` has
/// been requested -- so a restored forum resumes with identical ids and finish
/// state. Persisted by the session store as `forum.json` after every forum
/// mutation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForumSnapshot {
    /// Every post on the forum, in no particular order.
    pub posts: Vec<Post>,
    /// Every comment on the forum, in no particular order.
    pub comments: Vec<Comment>,
    /// Every completion-gate task on the forum, in no particular order
    /// (absent in snapshots written before tasks existed).
    #[serde(default)]
    pub tasks: Vec<Task>,
    /// Every recorded analyst ask, in no particular order (absent in
    /// snapshots written before analyst asks existed).
    #[serde(default)]
    pub asks: Vec<AnalystAsk>,
    /// Every "needs analyst review" flag, in no particular order (absent in
    /// snapshots written before review flags existed).
    #[serde(default)]
    pub flags: Vec<ReviewFlag>,
    /// The next id the forum would hand out to a newly created comment.
    pub next_comment_id: usize,
    /// The next id the forum would hand out to a newly created task (0 in
    /// snapshots written before tasks existed; restore derives a safe id).
    #[serde(default)]
    pub next_task_id: usize,
    /// The next id the forum would hand out to a newly recorded analyst ask
    /// (0 in snapshots written before asks existed; restore derives a safe id).
    #[serde(default)]
    pub next_ask_id: usize,
    /// The next id the forum would hand out to a newly raised review flag
    /// (0 in snapshots written before flags existed; restore derives a safe id).
    #[serde(default)]
    pub next_flag_id: usize,
    /// Whether the main agent had requested to finish when this was taken.
    pub finish_requested: bool,
}

/// The schema version the current [`ChatHistory`] serializer writes.
pub const CHAT_HISTORY_VERSION: u32 = 1;

/// The main agent's conversation history, stored as `chat.json`.
///
/// Unlike the snapshot (written once per settled turn), this file is rewritten
/// after every message the main agent exchanges, so quitting mid-orchestration
/// loses at most the final partial line of the model's work rather than the
/// whole conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatHistory {
    /// The schema version this history was written with.
    pub version: u32,
    /// The conversation id this history is keyed by.
    pub conversation_id: String,
    /// The settled conversation history, exactly as rig appended it.
    pub messages: Vec<Message>,
    /// The in-flight turn's messages captured so far, reconstructed from the
    /// main agent's per-model-turn hook. This is the turn's durable mid-turn
    /// record: a resume repairs the tail (dangling tool calls get synthetic
    /// results and it is closed assistant-side, since providers reject the
    /// raw shape -- see `repair_interrupted_tail`) and replays it into the
    /// model context, and the restore's history append then settles it into
    /// `messages` and clears this field. A settled turn clears it the same
    /// way, so it only ever carries the current in-flight turn.
    #[serde(default)]
    pub pending: Vec<Message>,
}

/// The schema version the current [`SideHistory`] serializer writes.
pub const SIDE_HISTORY_VERSION: u32 = 1;

/// One named side conversation's settled history, stored as `btw/<name>.json`.
///
/// A side conversation is forked from the main agent's settled history and then
/// grows its own turns without ever touching the main conversation's
/// `chat.json`. One of these files is rewritten after every settled side turn,
/// so quitting mid-turn loses at most that turn's in-flight tail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SideHistory {
    /// The schema version this history was written with.
    pub version: u32,
    /// The side conversation's name (always matching its file stem).
    pub name: String,
    /// When this side conversation was forked from the main agent.
    pub created_at: DateTime<Utc>,
    /// The settled side history: the fork-time copy of the main conversation
    /// plus every settled side turn's new messages.
    pub messages: Vec<Message>,
}

/// A serializable snapshot of a ThorPal session's main-agent state.
///
/// This is everything (beyond the forum, stored alongside it) needed to
/// rebuild the session and continue it: the reconstruction inputs the builder
/// would otherwise consume and drop (the provider config, the caller's
/// appended `preamble`, the Ghidra paths, and the workflow of sub-agents) plus
/// the main agent's conversation history keyed by `conversation_id`. The
/// provider config is persisted sanitized -- an API key is deliberately never
/// written to disk and must be re-supplied on resume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    /// The schema version this snapshot was written with.
    pub version: u32,
    /// The sanitized LLM provider config every agent was built from.
    pub provider: AiProviderConfig,
    /// The caller's extra preamble appended after the base forum instructions,
    /// or `None` when the run added none.
    pub preamble: Option<String>,
    /// The resolved Ghidra paths this run analyzes with.
    pub ghidra: GhidraSettings,
    /// The workflow of sub-agents attached to the main agent, so a resume
    /// rebuilds the same delegation tools without a workflow flag.
    pub workflow: Workflow,
    /// The conversation id the main agent's history is keyed by.
    pub conversation_id: String,
    /// The main agent's conversation history, round-tripped as serde.
    ///
    /// As of v5 the history lives in the session store's own `chat.json`
    /// (saved after every message) and this is written empty; it is kept so
    /// pre-v5 snapshots still load and resume as the chat.json fallback.
    #[serde(default)]
    pub messages: Vec<Message>,
    /// The accumulated token usage of every agent, keyed by agent name, so a
    /// resume continues the run's running totals (empty on pre-v4 snapshots).
    #[serde(default)]
    pub usage: BTreeMap<String, UsageTotals>,
    /// How many sub-agent display names the run had handed out at save time.
    ///
    /// Seeded back into the naming counter on resume so the resumed run mints
    /// fresh names instead of clobbering the prior run's agent records and
    /// merging distinct invocations under one usage key. Older snapshots
    /// deserialize it as 0 via the serde default (a best-effort resume).
    #[serde(default)]
    pub name_counter: usize,
}

/// One plain-text entry of a saved conversation's transcript.
///
/// A rendered view of the raw [`Message`] history for consumers (like the
/// `thorctl` TUI) that want to redisplay a resumed conversation without
/// depending on rig's message types. Tool calls, tool results, and reasoning
/// parts are omitted -- only the text the user and assistant exchanged remains.
#[derive(Debug, Clone)]
pub enum TranscriptEntry {
    /// A message the user sent.
    User(String),
    /// A text reply the assistant produced.
    Assistant(String),
}

/// Renders a raw message history as plain user/assistant text entries
///
/// Joins each message's text parts and skips messages that carried no text at
/// all (e.g. a pure tool-call turn or a tool-result message), so a consumer can
/// replay or re-prompt over the conversation as simple chat bubbles. Shared by
/// [`SessionSnapshot::transcript`] and the side-conversation summarizer.
///
/// # Arguments
///
/// * `messages` - The raw conversation history to render
pub(crate) fn render_messages(messages: &[Message]) -> Vec<TranscriptEntry> {
    // walk the history, keeping only the text the two sides exchanged
    messages
        .iter()
        .filter_map(|message| match message {
            // join a user message's text parts, dropping tool results
            Message::User { content } => {
                let text = content
                    .iter()
                    .filter_map(|part| match part {
                        rig::message::UserContent::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                // a user message with no text (e.g. tool results) is skipped
                (!text.is_empty()).then(|| TranscriptEntry::User(text))
            }
            // join an assistant message's text parts, dropping tool calls
            Message::Assistant { content, .. } => {
                let text = content
                    .iter()
                    .filter_map(|part| match part {
                        rig::message::AssistantContent::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                // a pure tool-call turn carries no text and is skipped
                (!text.is_empty()).then(|| TranscriptEntry::Assistant(text))
            }
            // the system preamble is rebuilt on resume, not replayed
            Message::System { .. } => None,
        })
        .collect()
}

impl SessionSnapshot {
    /// Sums the per-agent usage map into one run-wide tally
    ///
    /// Consumers (like the `thorctl` TUI) seed their running token readout with
    /// this so a resumed session's totals continue instead of restarting at zero.
    pub fn total_usage(&self) -> UsageTotals {
        // fold every agent's tally into a fresh grand total
        let mut total = UsageTotals::default();
        for usage in self.usage.values() {
            total += *usage;
        }
        total
    }

    /// Renders the saved conversation as plain user/assistant text entries
    ///
    /// Joins each message's text parts and skips messages that carried no text
    /// at all (e.g. a pure tool-call turn or a tool-result message), so a
    /// consumer can replay the conversation as simple chat bubbles.
    pub fn transcript(&self) -> Vec<TranscriptEntry> {
        // render the saved history with the shared message renderer
        render_messages(&self.messages)
    }

    /// Loads and deserializes a snapshot from `path`
    ///
    /// Rejects a snapshot whose `version` the current build does not understand
    /// rather than risk misinterpreting another schema.
    ///
    /// # Arguments
    ///
    /// * `path` - The snapshot file to load
    #[tracing::instrument(name = "SessionSnapshot::load", skip_all, err(Debug))]
    pub async fn load(path: impl AsRef<Path>) -> Result<Self, anyhow::Error> {
        // read the raw bytes off disk
        let encoded = tokio::fs::read(path.as_ref()).await?;
        // decode the snapshot from JSON
        let snapshot: SessionSnapshot = serde_json::from_slice(&encoded)?;
        // refuse a snapshot written by an incompatible schema; older versions
        // down to the supported minimum load via serde defaults
        if snapshot.version < MIN_SNAPSHOT_VERSION || snapshot.version > SNAPSHOT_VERSION {
            anyhow::bail!(
                "unsupported session snapshot version {} (this build understands {}..={})",
                snapshot.version,
                MIN_SNAPSHOT_VERSION,
                SNAPSHOT_VERSION
            );
        }
        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thorpal::tools::ghidra::GhidraSettings;

    /// A snapshot survives a JSON round-trip with all of its state intact.
    #[test]
    fn session_snapshot_json_round_trips() {
        // give the workflow non-default forum-context settings so the
        // round-trip proves they persist
        let mut workflow = Workflow::builtin().expect("builtin workflow should parse");
        workflow.forum_context = crate::thorpal::tools::forum::ForumContextMode::Full;
        workflow.forum_link_depth = 7;
        // build a snapshot carrying every field, including a bit of history
        let snapshot = SessionSnapshot {
            version: SNAPSHOT_VERSION,
            provider: AiProviderConfig::LiteLlm {
                endpoint: "https://example.test".to_owned(),
                api_key: None,
                model: "some-model".to_owned(),
            },
            preamble: Some("extra preamble".to_owned()),
            ghidra: GhidraSettings::from_binary(
                "bin/target",
                "GhidraProjects",
                Some("/opt/ghidra".into()),
            ),
            workflow,
            conversation_id: "main".to_owned(),
            messages: vec![Message::user("hello"), Message::assistant("hi")],
            usage: BTreeMap::from([(
                "main".to_owned(),
                UsageTotals {
                    input_tokens: 10,
                    output_tokens: 20,
                    total_tokens: 42,
                    cached_input_tokens: 8,
                    cache_creation_input_tokens: 4,
                    tool_use_prompt_tokens: 1,
                    reasoning_tokens: 2,
                },
            )]),
            name_counter: 7,
        };

        // serialize then deserialize the snapshot through JSON
        let encoded = serde_json::to_vec(&snapshot).expect("snapshot should serialize");
        let decoded: SessionSnapshot =
            serde_json::from_slice(&encoded).expect("snapshot should deserialize");

        // the scalar state round-trips unchanged
        assert_eq!(decoded.version, snapshot.version);
        // the provider config round-trips with its endpoint and model intact
        match &decoded.provider {
            AiProviderConfig::LiteLlm {
                endpoint,
                api_key,
                model,
            } => {
                assert_eq!(endpoint, "https://example.test");
                assert_eq!(api_key, &None);
                assert_eq!(model, "some-model");
            }
            other => panic!("provider changed variant across the round-trip: {other:?}"),
        }
        assert_eq!(decoded.preamble, snapshot.preamble);
        assert_eq!(decoded.conversation_id, snapshot.conversation_id);
        // the workflow round-trips its sub-agents by name
        let names: Vec<&str> = decoded
            .workflow
            .agents
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(names, ["Researcher", "ReverseEngineer", "Developer"]);
        // the forum-context settings round-trip unchanged
        assert_eq!(
            decoded.workflow.forum_context,
            crate::thorpal::tools::forum::ForumContextMode::Full
        );
        assert_eq!(decoded.workflow.forum_link_depth, 7);
        // the conversation history round-trips message-for-message
        assert_eq!(decoded.messages, snapshot.messages);
        // the per-agent usage tallies round-trip counter-for-counter
        assert_eq!(decoded.usage, snapshot.usage);
    }

    /// A workflow saved before the forum-context fields existed deserializes
    /// with the default linked mode and link depth.
    #[test]
    fn workflow_forum_context_defaults_when_absent() {
        // strip the forum-context keys from a serialized workflow to mimic a
        // snapshot written before the fields existed
        let workflow = Workflow::builtin().expect("builtin workflow should parse");
        let mut value = serde_json::to_value(&workflow).expect("workflow should serialize");
        let map = value.as_object_mut().expect("workflow should be an object");
        map.remove("forum_context");
        map.remove("forum_link_depth");
        // the legacy JSON loads with the defaults filled in
        let decoded: Workflow =
            serde_json::from_value(value).expect("legacy workflow should deserialize");
        assert_eq!(
            decoded.forum_context,
            crate::thorpal::tools::forum::ForumContextMode::default()
        );
        assert_eq!(
            decoded.forum_link_depth,
            crate::thorpal::tools::forum::DEFAULT_FORUM_LINK_DEPTH
        );
    }

    /// A snapshot without a `usage` key (pre-v4) deserializes with an empty map.
    #[test]
    fn usage_defaults_to_empty_when_absent() {
        // build a v4 snapshot and strip the usage key from its JSON to mimic a
        // v3 file written before the field existed
        let snapshot = SessionSnapshot {
            version: SNAPSHOT_VERSION,
            provider: AiProviderConfig::LiteLlm {
                endpoint: "https://example.test".to_owned(),
                api_key: None,
                model: "some-model".to_owned(),
            },
            preamble: None,
            ghidra: GhidraSettings::from_binary("bin/target", "projects", None),
            workflow: Workflow::builtin().expect("builtin workflow should parse"),
            conversation_id: "main".to_owned(),
            messages: Vec::new(),
            usage: BTreeMap::new(),
            name_counter: 0,
        };
        let mut value = serde_json::to_value(&snapshot).expect("snapshot should serialize");
        value
            .as_object_mut()
            .expect("snapshot serializes to an object")
            .remove("usage");
        // the usage-less JSON still deserializes, defaulting to an empty map
        let decoded: SessionSnapshot =
            serde_json::from_value(value).expect("snapshot without usage should deserialize");
        assert!(decoded.usage.is_empty());
    }

    /// The per-agent map sums into one grand total, saturating on overflow.
    #[test]
    fn total_usage_sums_every_agent() {
        // build a snapshot with two agents' tallies to fold together
        let mut snapshot = SessionSnapshot {
            version: SNAPSHOT_VERSION,
            provider: AiProviderConfig::LiteLlm {
                endpoint: "https://example.test".to_owned(),
                api_key: None,
                model: "some-model".to_owned(),
            },
            preamble: None,
            ghidra: GhidraSettings::from_binary("bin/target", "projects", None),
            workflow: Workflow::builtin().expect("builtin workflow should parse"),
            conversation_id: "main".to_owned(),
            messages: Vec::new(),
            usage: BTreeMap::new(),
            name_counter: 0,
        };
        snapshot.usage.insert(
            "main".to_owned(),
            UsageTotals {
                input_tokens: 100,
                output_tokens: 10,
                total_tokens: 110,
                cached_input_tokens: 50,
                cache_creation_input_tokens: 25,
                tool_use_prompt_tokens: 0,
                reasoning_tokens: 5,
            },
        );
        snapshot.usage.insert(
            "ReverseEngineer(Europa)".to_owned(),
            UsageTotals {
                input_tokens: 1,
                output_tokens: 2,
                total_tokens: 3,
                cached_input_tokens: 4,
                cache_creation_input_tokens: 5,
                tool_use_prompt_tokens: 6,
                reasoning_tokens: 7,
            },
        );
        // the grand total is the field-wise sum of both agents
        let total = snapshot.total_usage();
        assert_eq!(total.input_tokens, 101);
        assert_eq!(total.output_tokens, 12);
        assert_eq!(total.total_tokens, 113);
        assert_eq!(total.cached_input_tokens, 54);
        assert_eq!(total.cache_creation_input_tokens, 30);
        assert_eq!(total.tool_use_prompt_tokens, 6);
        assert_eq!(total.reasoning_tokens, 12);
    }

    /// A side history survives a JSON round-trip with its messages intact.
    #[test]
    fn side_history_json_round_trips() {
        // build a side history carrying a forked message plus a side turn
        let side = SideHistory {
            version: SIDE_HISTORY_VERSION,
            name: "crypto".to_owned(),
            created_at: Utc::now(),
            messages: vec![Message::user("hello"), Message::assistant("hi")],
        };
        // serialize then deserialize the history through JSON
        let encoded = serde_json::to_vec(&side).expect("side history should serialize");
        let decoded: SideHistory =
            serde_json::from_slice(&encoded).expect("side history should deserialize");
        // every field round-trips unchanged
        assert_eq!(decoded.version, side.version);
        assert_eq!(decoded.name, side.name);
        assert_eq!(decoded.created_at, side.created_at);
        assert_eq!(decoded.messages, side.messages);
    }

    /// A snapshot from another schema version is refused on load.
    #[tokio::test]
    async fn load_rejects_other_versions() {
        // write a syntactically valid snapshot with a bumped version field
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("session.json");
        let mut snapshot = SessionSnapshot {
            version: SNAPSHOT_VERSION + 1,
            provider: AiProviderConfig::LiteLlm {
                endpoint: "https://example.test".to_owned(),
                api_key: None,
                model: "some-model".to_owned(),
            },
            preamble: None,
            ghidra: GhidraSettings::from_binary("bin/target", "projects", None),
            workflow: Workflow::builtin().expect("builtin workflow should parse"),
            conversation_id: "main".to_owned(),
            messages: Vec::new(),
            usage: BTreeMap::new(),
            name_counter: 0,
        };
        let encoded = serde_json::to_vec(&snapshot).expect("snapshot should serialize");
        tokio::fs::write(&path, encoded).await.expect("write");
        // the loader must refuse the unknown future version
        let err = SessionSnapshot::load(&path)
            .await
            .err()
            .expect("mismatched version should fail to load");
        assert!(err.to_string().contains("unsupported session snapshot"));
        // a version below the supported minimum is refused too
        snapshot.version = MIN_SNAPSHOT_VERSION - 1;
        let encoded = serde_json::to_vec(&snapshot).expect("snapshot should serialize");
        tokio::fs::write(&path, encoded).await.expect("write");
        let err = SessionSnapshot::load(&path)
            .await
            .err()
            .expect("too-old version should fail to load");
        assert!(err.to_string().contains("unsupported session snapshot"));
        // every version in the supported range loads cleanly
        for version in MIN_SNAPSHOT_VERSION..=SNAPSHOT_VERSION {
            snapshot.version = version;
            let encoded = serde_json::to_vec(&snapshot).expect("snapshot should serialize");
            tokio::fs::write(&path, encoded).await.expect("write");
            SessionSnapshot::load(&path)
                .await
                .expect("supported version should load");
        }
    }
}
