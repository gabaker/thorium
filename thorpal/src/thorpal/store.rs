//! The on-disk session store every ThorPal run saves into and resumes from.
//!
//! Each session owns one directory under the sessions root (by default
//! `~/.thorium/sessions/<name>/`) holding everything needed to resume it:
//!
//! - `meta.json` -- the session's [`SessionMeta`] (name, naming provenance,
//!   creation time, and the analyzed binary)
//! - `session.json` -- the main agent's [`SessionSnapshot`] (sanitized provider
//!   config, preamble, Ghidra paths, workflow, and conversation history)
//! - `forum.json` -- the shared forum's [`ForumSnapshot`]
//! - `agents/<name>.json` -- one [`AgentRecord`] per sub-agent invocation
//! - `report.md` -- the most recently generated markdown report, if any
//!
//! Every file is written atomically (a `.tmp` sibling renamed over the target)
//! and incrementally as the run progresses, so a crash or power loss costs at
//! most the in-flight turn. A session the operator did not name is given a
//! random adjective-animal name and may be renamed once by the model (via the
//! [`rename_session`] tool) to something descriptive before it finishes.

pub mod library;
pub mod memories;
mod names;
pub mod redact;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Utc};
use rig::completion::Message;
use rig::tool::{ToolContext, ToolExecutionError};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use super::agents::budget::ExtensionRequest;
use super::resume::{ChatHistory, ForumSnapshot, SessionSnapshot, SideHistory};
use super::transcript::TranscriptLine;

/// The schema version the current [`SessionMeta`] serializer writes.
pub const META_VERSION: u32 = 1;

/// The file the main agent's session snapshot is stored in.
const SESSION_FILE: &str = "session.json";

/// The file the shared forum's snapshot is stored in.
const FORUM_FILE: &str = "forum.json";

/// The file the main agent's per-message conversation history is stored in.
const CHAT_FILE: &str = "chat.json";

/// The file the session's ordered display transcript is appended to.
const TRANSCRIPT_FILE: &str = "transcript.jsonl";

/// The file the session's metadata is stored in.
const META_FILE: &str = "meta.json";

/// The subdirectory each sub-agent invocation's record is stored under.
const AGENTS_DIR: &str = "agents";

/// The subdirectory each named side conversation's history is stored under.
const BTW_DIR: &str = "btw";

/// The longest a side-conversation name may be, so tab labels stay readable.
const MAX_BTW_NAME_LEN: usize = 32;

/// The subdirectory the session's Rhai analysis scripts are stored under.
const SCRIPTS_DIR: &str = "scripts";

/// The subdirectory files extracted from the sample are stored under.
const EXTRACTED_DIR: &str = "extracted";

/// The longest an extracted file's name may be, so names stay referenceable.
const MAX_EXTRACTED_NAME_LEN: usize = 128;

/// The file extension every stored Rhai script is saved with.
const SCRIPT_EXT: &str = "rhai";

/// The longest a script name may be, so names stay easy to type and reference.
const MAX_SCRIPT_NAME_LEN: usize = 64;

/// The file the session's most recently generated report is stored in.
const REPORT_FILE: &str = "report.md";

/// The longest a session name may be, so names stay easy to type and reference.
const MAX_NAME_LEN: usize = 48;

/// The shortest a model-chosen rename may be, so names stay meaningful.
const MIN_RENAME_LEN: usize = 3;

/// How many plain adjective-animal draws to try before adding a numeric suffix.
const MAX_AUTO_NAME_ATTEMPTS: usize = 32;

/// One extra sample analyzed alongside the primary in a multi-sample session.
///
/// Recorded in `meta.json` for the operator, mirroring the primary's
/// `binary`: the path the session analyzes (the staged sha copy under
/// `--anonymize`). The pre-staging source path is deliberately never
/// recorded -- an anonymized store must identify members by content hash
/// only, so it stays shareable at rest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtraSampleMeta {
    /// The path the session analyzes (the staged sha copy under --anonymize).
    pub binary: PathBuf,
}

/// A session's durable metadata, stored as `meta.json` in its directory.
///
/// This records where the session came from (the analyzed binary and its
/// Ghidra project name) and how it was named, so a resume can report the
/// session faithfully and the model-rename rules survive restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    /// The schema version this metadata was written with.
    pub version: u32,
    /// The session's current name (always matching its directory name).
    pub name: String,
    /// Whether the operator named this session explicitly (via `--name`).
    pub user_named: bool,
    /// Whether the model has already renamed this session.
    pub renamed: bool,
    /// When this session was created.
    pub created_at: DateTime<Utc>,
    /// The path to the binary this session analyzes.
    pub binary: PathBuf,
    /// The Ghidra project name derived from the binary.
    pub project_name: String,
    /// Whether this session analyzed an anonymized staged copy of the binary.
    #[serde(default)]
    pub anonymized: bool,
    /// The extra samples analyzed alongside the primary in a multi-sample
    /// session; empty (and omitted) for a classic single-sample run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_samples: Vec<ExtraSampleMeta>,
}

/// The durable record of a single sub-agent invocation.
///
/// Written to `agents/<file-safe-name>.json` when the sub-agent starts (so a
/// crash mid-flight still records what was running), after every completed
/// model turn (mirroring the in-flight partial transcript so a process death
/// loses at most the final turn), and once more when it finishes with its
/// full message transcript. Beyond serving as the session's audit trail of
/// who investigated what, a running-shaped record (`finished_at: None`) is
/// how a resume finds the delegations that were mid-flight when the process
/// died and continues them from their persisted partial transcripts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRecord {
    /// The unique per-invocation display name (e.g. `ReverseEngineer(Europa)`).
    pub name: String,
    /// The role this invocation ran as (e.g. `ReverseEngineer`).
    pub role: String,
    /// The overarching goal this delegation served.
    pub goal: String,
    /// The concrete task the sub-agent was given.
    pub task: String,
    /// The display name of the swarm this invocation ran under, if any.
    ///
    /// Older records predate this field, so it defaults to `None`.
    #[serde(default)]
    pub swarm: Option<String>,
    /// When this invocation started.
    pub started_at: DateTime<Utc>,
    /// When this invocation finished, or `None` while it is still running.
    pub finished_at: Option<DateTime<Utc>>,
    /// Whether this invocation succeeded, or `None` while it is still running.
    pub success: Option<bool>,
    /// The sub-agent's final text, when it succeeded.
    pub output: Option<String>,
    /// The error that stopped the sub-agent, when it failed.
    pub error: Option<String>,
    /// The invocation's full message transcript, filled in when it finishes.
    ///
    /// A failed invocation carries its salvaged partial transcript here, so a
    /// budget death no longer loses the work the agent had already done.
    #[serde(default)]
    pub messages: Vec<Message>,
    /// The extension requests this invocation made, granted and refused alike.
    ///
    /// Older records predate this field, so it defaults to empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<ExtensionRequest>,
    /// The salvaged handoff handed back to the parent when this invocation died.
    ///
    /// Older records predate this field, so it defaults to `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<String>,
    /// The parent's `ToolCallId` string for the delegation call that spawned
    /// this invocation (shared by every member of one `spawn_swarm` call), so
    /// a resume can match a dangling delegation call to its exact invocation.
    ///
    /// Older records predate this field, so it defaults to `None` (a resume
    /// then falls back to matching on role, goal, and task).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_call: Option<String>,
    /// The rig internal call id of the parent's delegation call, so a resumed
    /// invocation's events correlate with the dead run's transcript lines.
    ///
    /// Older records predate this field, so it defaults to `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_internal_call: Option<String>,
    /// Whether a later process resumed this invocation after a mid-flight
    /// interruption, so the audit trail shows which runs span a resume.
    ///
    /// Older records predate this field, so it defaults to `false`.
    #[serde(default)]
    pub resumed: bool,
    /// The death class when this invocation failed (`max_turns`, `context`,
    /// `cancelled`, or `other`), mirroring the durable transcript line so the
    /// continue path can tell crash-class deaths from exhaustion without
    /// re-reading the transcript.
    ///
    /// Older records predate this field, so it defaults to `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
}

impl AgentRecord {
    /// Creates the record written as a sub-agent invocation starts
    ///
    /// # Arguments
    ///
    /// * `name` - The unique per-invocation display name
    /// * `role` - The role this invocation runs as
    /// * `goal` - The overarching goal this delegation serves
    /// * `task` - The concrete task the sub-agent was given
    /// * `swarm` - The display name of the swarm this invocation runs under, if any
    pub fn started(
        name: impl Into<String>,
        role: impl Into<String>,
        goal: impl Into<String>,
        task: impl Into<String>,
        swarm: Option<&str>,
    ) -> Self {
        // stamp the start time now; the completion fields stay empty until done
        AgentRecord {
            name: name.into(),
            role: role.into(),
            goal: goal.into(),
            task: task.into(),
            swarm: swarm.map(str::to_owned),
            started_at: Utc::now(),
            finished_at: None,
            success: None,
            output: None,
            error: None,
            messages: Vec::new(),
            extensions: Vec::new(),
            handoff: None,
            parent_call: None,
            parent_internal_call: None,
            resumed: false,
            class: None,
        }
    }

    /// Completes this record with a successful invocation's output
    ///
    /// # Arguments
    ///
    /// * `output` - The sub-agent's final text
    /// * `messages` - The invocation's full message transcript
    pub fn finish_ok(&mut self, output: impl Into<String>, messages: Vec<Message>) {
        // stamp the completion and record the successful outcome
        self.finished_at = Some(Utc::now());
        self.success = Some(true);
        self.output = Some(output.into());
        self.messages = messages;
    }

    /// Completes this record with the error that stopped the invocation
    ///
    /// The salvaged partial transcript and the handoff handed back to the
    /// parent are persisted alongside the error, so a budget death keeps the
    /// work the agent had already done instead of just the error string.
    ///
    /// # Arguments
    ///
    /// * `error` - The error that stopped the sub-agent
    /// * `messages` - The salvaged partial transcript, as far as it got
    /// * `handoff` - The salvaged handoff returned to the parent, if one was built
    /// * `class` - The death class (`max_turns`, `context`, `cancelled`, `other`)
    pub fn finish_err(
        &mut self,
        error: impl Into<String>,
        messages: Vec<Message>,
        handoff: Option<String>,
        class: &str,
    ) {
        // stamp the completion and record the failure
        self.finished_at = Some(Utc::now());
        self.success = Some(false);
        self.error = Some(error.into());
        // keep the partial transcript and the handoff the parent received
        self.messages = messages;
        self.handoff = handoff;
        self.class = Some(class.to_owned());
    }
}

/// Render the delegations that died without finishing into re-queue lines
///
/// A record counts as dead-unfinished when it never finished at all (the
/// process it ran in was killed, so on resume nothing is still running) or
/// when it failed with a crash-class death (`cancelled`/`other` — provider
/// faults and kills). Exhaustion deaths (`max_turns`/`context`) are excluded:
/// they already handed their salvage back to the parent in-run, and re-running
/// them identically would die the same way.
///
/// Lines render in spawn order regardless of the input's order (the store's
/// loader returns newest first for resume matching), so re-queue nudges
/// always read alike across resumes.
///
/// # Arguments
///
/// * `records` - The session's persisted sub-agent records
pub fn dead_delegation_lines(records: &[AgentRecord]) -> Vec<String> {
    // render in spawn order whatever order the records arrived in
    let mut records: Vec<&AgentRecord> = records.iter().collect();
    records.sort_by(|a, b| a.started_at.cmp(&b.started_at));
    let mut lines: Vec<String> = Vec::new();
    for record in records {
        // decide whether this record is a re-queue candidate
        let dead = match (&record.finished_at, &record.success) {
            // never finished: the session's process died out from under it
            (None, _) => true,
            // failed in-run: only crash-class deaths are worth re-queueing
            (Some(_), Some(false)) => {
                matches!(record.class.as_deref(), Some("cancelled") | Some("other"))
            }
            // successes (and malformed records) are not re-queue candidates
            _ => false,
        };
        if !dead {
            continue;
        }
        // flatten the task onto one line and bound it for the prompt
        let task: String = record.task.split_whitespace().collect::<Vec<_>>().join(" ");
        let task: String = task.chars().take(500).collect();
        let mut line = format!("- {} ({}): {task}", record.role, record.name);
        // a salvaged handoff tells the re-run where the dead attempt got to
        if let Some(handoff) = &record.handoff {
            let handoff: String = handoff.split_whitespace().collect::<Vec<_>>().join(" ");
            let handoff: String = handoff.chars().take(300).collect();
            line.push_str(&format!(" [salvaged handoff: {handoff}]"));
        }
        lines.push(line);
    }
    lines
}

/// Prunes the records a live revival already handles from a leftover set
///
/// `re_issued` seeds the call ids the revival's revived owners are instructed
/// to re-issue: the calls frozen dangling in consumed NEVER-FINISHED records'
/// transcripts (consumed finished records are merely replayed, so their
/// frozen calls are re-issued by nobody and must not seed the prune). A
/// never-finished record stamped with a re-issued id is being handled by its
/// revived owner -- transitively, since a pruned child's own frozen
/// transcript may hold grandchild calls. What remains in `records` afterwards
/// is exactly the set the revival leaves unhandled, fit to feed
/// [`dead_delegation_lines`].
///
/// # Arguments
///
/// * `records` - The leftover records the revival's matchers did not consume
/// * `re_issued` - The call ids the revival's revived owners will re-issue
pub(crate) fn prune_revived_descendants(
    records: &mut Vec<AgentRecord>,
    mut re_issued: std::collections::HashSet<String>,
) {
    // prune claimed descendants until a pass claims nothing new
    loop {
        // split off the never-finished records a re-issued call id claims
        let (pruned, kept): (Vec<AgentRecord>, Vec<AgentRecord>) =
            records.drain(..).partition(|record| {
                record.finished_at.is_none()
                    && record
                        .parent_call
                        .as_deref()
                        .is_some_and(|call| re_issued.contains(call))
            });
        *records = kept;
        // nothing claimed: every remaining record is genuinely unhandled
        if pruned.is_empty() {
            break;
        }
        // a pruned child's own frozen calls are re-issued transitively
        for record in pruned {
            re_issued.extend(
                super::memory::dangling_calls(&record.messages)
                    .into_iter()
                    .map(|call| call.id.as_str().to_owned()),
            );
        }
    }
}

/// Sets a corrupt store file aside under a timestamped name, best-effort
///
/// The bad file is renamed (never deleted) so a post-mortem can inspect it,
/// and the original name is freed for the degraded resume's next save. A
/// stamped name that is already taken (repeated corruption within the same
/// second) is suffixed `-1`, `-2`, ... so an earlier aside is never
/// clobbered. Returns the aside path when the rename succeeded.
///
/// # Arguments
///
/// * `dir` - The session directory holding the corrupt file
/// * `file` - The corrupt file's name within the directory
async fn set_aside_corrupt(dir: &Path, file: &str) -> Option<PathBuf> {
    // stamp the aside name so repeated corruption never clobbers evidence
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let mut aside = dir.join(format!("{file}.corrupt-{stamp}"));
    // suffix a same-second aside until the name is free, so a later aside
    // never clobbers the evidence an earlier one preserved
    let mut suffix = 0u32;
    while tokio::fs::try_exists(&aside).await.unwrap_or(false) {
        suffix += 1;
        aside = dir.join(format!("{file}.corrupt-{stamp}-{suffix}"));
    }
    // the rename is best-effort: a failure just leaves the bad file in place
    match tokio::fs::rename(dir.join(file), &aside).await {
        Ok(()) => Some(aside),
        Err(err) => {
            tracing::warn!(?err, file, "failed to set the corrupt store file aside");
            None
        }
    }
}

/// A minimal probe decoding only a store file's schema version
///
/// Used when a full decode fails: a file written by a newer build can fail
/// this build's decode because the schema changed shape, not because the file
/// is corrupt, so the version is probed on its own to tell "newer schema"
/// (a hard error -- setting a valid newer file aside would silently lose
/// history) apart from true corruption (which degrades with the file set
/// aside). Unknown fields are ignored, so any schema that still carries a
/// numeric `version` key probes cleanly.
#[derive(Deserialize)]
struct VersionProbe {
    /// The schema version the file claims to be written with.
    version: u32,
}

/// The on-disk store one session saves into and resumes from.
///
/// Shared as an `Arc` between the main agent (which saves `session.json`), the
/// forum (which saves `forum.json` after every mutation), the sub-agent runner
/// (which saves `agents/*.json`), and the rename tool. Renames swap the
/// directory on disk, so the current name lives behind a lock concurrent saves
/// resolve the directory through.
pub struct SessionStore {
    /// The root every session directory lives under (e.g. `~/.thorium/sessions`).
    root: PathBuf,
    /// The session's current name; held behind a lock because a rename swaps
    /// the directory out from under concurrent saves.
    name: RwLock<String>,
    /// Whether the operator named this session explicitly (via `--name`),
    /// which disables the model's rename tool entirely.
    user_named: bool,
    /// Whether the model has already renamed this session.
    renamed: AtomicBool,
    /// A one-shot latch so the `finish` tool nudges the model toward a rename
    /// exactly once rather than deadlocking a run whose renames keep failing.
    rename_nudged: AtomicBool,
    /// A one-shot latch so the `finish` tool nudges the model to review this
    /// session's scripts for library promotion exactly once per run.
    promote_nudged: AtomicBool,
    /// A one-shot latch so the `finish` tool nudges the model to bank this
    /// run's durable generic lessons into the memory library exactly once.
    memory_nudged: AtomicBool,
    /// Serializes writes into the session's `extracted/` workspace.
    ///
    /// Held across the budget check, the extraction itself, and the final
    /// rename on every tool path that writes into `extracted/` (extract_file
    /// and decompress), so concurrent extractions can neither race the same
    /// `.tmp` sibling nor leapfrog the total-budget check past the cap.
    extract_lock: tokio::sync::Mutex<()>,
    /// When this session was created, kept so `meta.json` can be rewritten.
    created_at: DateTime<Utc>,
    /// The analyzed binary, kept so `meta.json` can be rewritten.
    binary: PathBuf,
    /// The Ghidra project name, kept so `meta.json` can be rewritten.
    project_name: String,
    /// Whether this session analyzes an anonymized staged copy, kept so
    /// `meta.json` can be rewritten.
    anonymized: bool,
    /// The extra samples of a multi-sample session, kept so `meta.json` can
    /// be rewritten.
    extra_samples: Vec<ExtraSampleMeta>,
}

impl SessionStore {
    /// The default sessions root: `~/.thorium/sessions`
    pub fn default_root() -> Result<PathBuf, anyhow::Error> {
        // resolve the home dir so sessions land in the user's `.thorium`
        let home = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("could not resolve a home directory for ~/.thorium"))?;
        Ok(home.join(".thorium").join("sessions"))
    }

    /// Creates a new session directory, claiming a unique name
    ///
    /// An explicit `name` is validated and claimed as-is -- a collision with an
    /// existing session is an error pointing the operator at `--resume`. When
    /// no name is given, random adjective-animal names are drawn until one is
    /// free, falling back to numeric suffixes if the plain draws keep
    /// colliding. The directory creation itself is the atomic claim, so two
    /// concurrent sessions can never share a name. The session's `meta.json`
    /// and `agents/` subdirectory are written before this returns.
    ///
    /// # Arguments
    ///
    /// * `root` - The root directory every session lives under
    /// * `name` - The operator's explicit session name, or `None` to generate one
    /// * `binary` - The path to the binary this session analyzes
    /// * `project_name` - The Ghidra project name derived from the binary
    /// * `anonymized` - Whether this session analyzes an anonymized staged copy
    /// * `extra_samples` - The extra samples of a multi-sample session, empty
    ///   for a classic single-sample run
    #[tracing::instrument(name = "SessionStore::create", skip_all, err(Debug))]
    pub async fn create(
        root: PathBuf,
        name: Option<String>,
        binary: PathBuf,
        project_name: String,
        anonymized: bool,
        extra_samples: Vec<ExtraSampleMeta>,
    ) -> Result<Arc<Self>, anyhow::Error> {
        // make sure the sessions root exists before claiming a directory in it
        tokio::fs::create_dir_all(&root).await?;
        // remember whether the operator picked this name so renames can be gated
        let user_named = name.is_some();
        // claim a directory for this session, erroring or retrying on collision
        let name = match name {
            Some(name) => {
                // refuse names that would be unsafe or unwieldy on disk
                validate_session_name(&name).map_err(|err| anyhow::anyhow!(err))?;
                // the directory creation is the atomic claim on this name
                match tokio::fs::create_dir(root.join(&name)).await {
                    Ok(()) => name,
                    // a collision on an explicit name points at resuming instead
                    Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                        anyhow::bail!(
                            "a session named '{name}' already exists; resume it with \
                             `--resume {name}` or pick a different --name"
                        );
                    }
                    Err(err) => return Err(err.into()),
                }
            }
            None => Self::claim_generated_name(&root).await?,
        };
        // assemble the store around the claimed name
        let store = Arc::new(SessionStore {
            root,
            name: RwLock::new(name),
            user_named,
            renamed: AtomicBool::new(false),
            rename_nudged: AtomicBool::new(false),
            promote_nudged: AtomicBool::new(false),
            memory_nudged: AtomicBool::new(false),
            extract_lock: tokio::sync::Mutex::new(()),
            created_at: Utc::now(),
            binary,
            project_name,
            anonymized,
            extra_samples,
        });
        // create the agents subdirectory so sub-agent records always have a home
        tokio::fs::create_dir_all(store.dir().await.join(AGENTS_DIR)).await?;
        // create the scripts subdirectory so saved scripts always have a home
        tokio::fs::create_dir_all(store.dir().await.join(SCRIPTS_DIR)).await?;
        // create the extracted subdirectory so extracted files always have a home
        tokio::fs::create_dir_all(store.dir().await.join(EXTRACTED_DIR)).await?;
        // write the initial metadata so the session is identifiable immediately
        store.save_meta().await?;
        Ok(store)
    }

    /// Draws random adjective-animal names until a directory claim succeeds
    ///
    /// # Arguments
    ///
    /// * `root` - The root directory to claim a session directory under
    async fn claim_generated_name(root: &Path) -> Result<String, anyhow::Error> {
        // bound the draws so a pathological sessions dir cannot loop forever
        for attempt in 0..(MAX_AUTO_NAME_ATTEMPTS * 2) {
            // draw a plain name first; add a numeric suffix once draws keep colliding
            let candidate = if attempt < MAX_AUTO_NAME_ATTEMPTS {
                names::random_name()
            } else {
                format!("{}-{}", names::random_name(), names::random_suffix())
            };
            // the directory creation is the atomic claim on this name
            match tokio::fs::create_dir(root.join(&candidate)).await {
                Ok(()) => return Ok(candidate),
                // another session holds this name, so draw again
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err.into()),
            }
        }
        anyhow::bail!("could not find a free auto-generated session name under {root:?}");
    }

    /// Opens a saved session, loading everything needed to resume it
    ///
    /// Reads the session's `meta.json`, `session.json`, and `forum.json`. A
    /// missing `forum.json` yields an empty forum (the session crashed before
    /// its first forum save); a missing `session.json` is an error since there
    /// is no conversation to resume. The returned store keeps saving into the
    /// same directory.
    ///
    /// # Arguments
    ///
    /// * `root` - The root directory every session lives under
    /// * `name` - The name of the session to open
    #[tracing::instrument(name = "SessionStore::open", skip_all, err(Debug))]
    pub async fn open(
        root: PathBuf,
        name: &str,
    ) -> Result<(Arc<Self>, SessionMeta, SessionSnapshot, ForumSnapshot), anyhow::Error> {
        // resolve the session's directory and make sure it exists
        let dir = root.join(name);
        if !dir.is_dir() {
            anyhow::bail!("no session named '{name}' exists under {}", root.display());
        }
        // load the metadata recorded when the session was created
        let encoded = tokio::fs::read(dir.join(META_FILE))
            .await
            .map_err(|err| anyhow::anyhow!("failed to read session '{name}' metadata: {err}"))?;
        let meta: SessionMeta = serde_json::from_slice(&encoded).map_err(|err| {
            anyhow::anyhow!(
                "session '{name}' has a corrupt {META_FILE} ({err}); the session \
                 cannot be resumed without its metadata"
            )
        })?;
        // load the main agent's snapshot; without it there is nothing to resume
        let snapshot = SessionSnapshot::load(dir.join(SESSION_FILE))
            .await
            .map_err(|err| {
                anyhow::anyhow!("session '{name}' has no loadable saved state yet: {err}")
            })?;
        // an empty forum, for sessions whose forum is missing or unreadable
        let empty_forum = || ForumSnapshot {
            posts: Vec::new(),
            comments: Vec::new(),
            tasks: Vec::new(),
            asks: Vec::new(),
            flags: Vec::new(),
            next_comment_id: 1,
            next_task_id: 1,
            next_ask_id: 1,
            next_flag_id: 1,
            finish_requested: false,
        };
        // load the forum, treating a missing file as an empty forum and a
        // corrupt one as recoverable: the bad file is set aside (never
        // clobbered) and the resume degrades to an empty forum rather than
        // bricking the whole session over one unparseable store file
        let forum = match tokio::fs::read(dir.join(FORUM_FILE)).await {
            Ok(encoded) => match serde_json::from_slice(&encoded) {
                Ok(forum) => forum,
                Err(err) => {
                    // set the corrupt file aside for post-mortems, best-effort
                    let aside = set_aside_corrupt(&dir, FORUM_FILE).await;
                    tracing::warn!(
                        ?err,
                        ?aside,
                        "session '{name}' has a corrupt {FORUM_FILE}; resuming with an empty forum"
                    );
                    empty_forum()
                }
            },
            // the session crashed before its first forum save; start it empty
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => empty_forum(),
            Err(err) => return Err(err.into()),
        };
        // rebuild the store from the metadata so saving and renaming continue
        let store = Arc::new(SessionStore {
            root,
            name: RwLock::new(meta.name.clone()),
            user_named: meta.user_named,
            renamed: AtomicBool::new(meta.renamed),
            rename_nudged: AtomicBool::new(false),
            promote_nudged: AtomicBool::new(false),
            memory_nudged: AtomicBool::new(false),
            extract_lock: tokio::sync::Mutex::new(()),
            created_at: meta.created_at,
            binary: meta.binary.clone(),
            project_name: meta.project_name.clone(),
            anonymized: meta.anonymized,
            extra_samples: meta.extra_samples.clone(),
        });
        // make sure the agents subdirectory survived (a crash may predate it)
        tokio::fs::create_dir_all(store.dir().await.join(AGENTS_DIR)).await?;
        // make sure the scripts subdirectory exists too (older sessions predate it)
        tokio::fs::create_dir_all(store.dir().await.join(SCRIPTS_DIR)).await?;
        // make sure the extracted subdirectory exists too (older sessions predate it)
        tokio::fs::create_dir_all(store.dir().await.join(EXTRACTED_DIR)).await?;
        Ok((store, meta, snapshot, forum))
    }

    /// Lists the names of every saved session under a sessions root
    ///
    /// Used to hint the operator at resumable sessions. A missing root simply
    /// yields no sessions.
    ///
    /// # Arguments
    ///
    /// * `root` - The root directory every session lives under
    pub async fn list(root: &Path) -> Result<Vec<String>, anyhow::Error> {
        // a missing root means no session was ever saved
        let mut entries = match tokio::fs::read_dir(root).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        // collect every subdirectory name as a session
        let mut sessions = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            // only directories are sessions; skip stray files
            if entry.file_type().await?.is_dir() {
                sessions.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
        // sort so the listing is stable across calls
        sessions.sort();
        Ok(sessions)
    }

    /// The session's current name
    pub async fn name(&self) -> String {
        // clone the name out from under the read lock
        self.name.read().await.clone()
    }

    /// The primary sample's SHA-256, when the project name records it
    ///
    /// Staged and anonymized samples derive their Ghidra project name from the
    /// primary binary's digest, so a bare 64-hex project name IS the sample
    /// identity; a hand-named or legacy project yields `None` over a guess.
    pub fn sample_sha(&self) -> Option<String> {
        // only a bare lowercase digest identifies the sample unambiguously
        let name = self.project_name.trim();
        (name.len() == 64 && name.chars().all(|c| c.is_ascii_hexdigit()))
            .then(|| name.to_ascii_lowercase())
    }

    /// The session's current directory
    pub async fn dir(&self) -> PathBuf {
        // resolve the directory under the read lock so a rename cannot race us
        self.root.join(&*self.name.read().await)
    }

    /// Whether the model should still rename this session before finishing
    ///
    /// True only for an auto-named session the model has not yet renamed; an
    /// operator-named session is never renamed by the model.
    pub fn needs_rename(&self) -> bool {
        !self.user_named && !self.renamed.load(Ordering::SeqCst)
    }

    /// Reads and sets the one-shot finish-time rename nudge
    ///
    /// Returns true exactly once, so the `finish` tool can prompt the model to
    /// rename the session a single time without ever deadlocking a run whose
    /// rename attempts keep failing validation.
    pub fn take_rename_nudge(&self) -> bool {
        // flip the latch and report whether we were the first to do so
        !self.rename_nudged.swap(true, Ordering::SeqCst)
    }

    /// Reads and sets the one-shot finish-time script-promotion nudge
    ///
    /// Returns true exactly once, so the `finish` tool can prompt the model to
    /// review this session's scripts for library promotion a single time
    /// without ever deadlocking a run that declines to promote anything.
    pub fn take_promote_nudge(&self) -> bool {
        // flip the latch and report whether we were the first to do so
        !self.promote_nudged.swap(true, Ordering::SeqCst)
    }

    /// Reads and sets the one-shot finish-time memory-review nudge
    ///
    /// Returns true exactly once, so the `finish` tool can prompt the model to
    /// review this run for durable generic lessons worth saving into the
    /// cross-run memory library a single time without ever deadlocking a run
    /// that decides nothing qualifies.
    pub fn take_memory_nudge(&self) -> bool {
        // flip the latch and report whether we were the first to do so
        !self.memory_nudged.swap(true, Ordering::SeqCst)
    }

    /// Renames this session, moving its directory on disk
    ///
    /// Validates the new name against the rename rules (see
    /// [`validate_rename`]), refuses when another session already holds it,
    /// then renames the directory while holding the name write lock so a
    /// concurrent save can never write into the vanished old directory. The
    /// session's `meta.json` is rewritten with the new name before returning.
    ///
    /// # Arguments
    ///
    /// * `new` - The new session name
    #[tracing::instrument(name = "SessionStore::rename", skip_all, err(Debug))]
    pub async fn rename(&self, new: &str) -> Result<String, anyhow::Error> {
        // refuse names that break the descriptive-and-speakable rules
        validate_rename(new).map_err(|err| anyhow::anyhow!(err))?;
        // hold the write lock across the whole swap so saves never race the move
        let mut name = self.name.write().await;
        // renaming to the current name is a no-op beyond marking the rename done
        if *name != new {
            // refuse to clobber another session that already holds this name
            let target = self.root.join(new);
            if tokio::fs::try_exists(&target).await? {
                anyhow::bail!("a session named '{new}' already exists; choose a different name");
            }
            // move the whole session directory to its new name
            tokio::fs::rename(self.root.join(&*name), &target).await?;
            // adopt the new name so every later save resolves the new directory
            *name = new.to_owned();
        }
        // record that the model has used its rename
        self.renamed.store(true, Ordering::SeqCst);
        // rewrite the metadata in place under the still-held lock
        let meta = self.meta_for(&name);
        write_json_in(&self.root.join(&*name), META_FILE, &meta).await?;
        Ok(name.clone())
    }

    /// Saves the main agent's session snapshot to `session.json`
    ///
    /// # Arguments
    ///
    /// * `snapshot` - The session snapshot to persist
    #[tracing::instrument(name = "SessionStore::save_session", skip_all, err(Debug))]
    pub async fn save_session(&self, snapshot: &SessionSnapshot) -> Result<(), anyhow::Error> {
        // resolve the directory under the name lock and write atomically
        write_json_in(&self.dir().await, SESSION_FILE, snapshot).await
    }

    /// Saves the main agent's conversation history to `chat.json`
    ///
    /// Called after every message the main agent exchanges (see
    /// `PersistingConversationMemory`), unlike the snapshot which only settles
    /// once per turn.
    ///
    /// # Arguments
    ///
    /// * `chat` - The conversation history to persist
    #[tracing::instrument(name = "SessionStore::save_chat", skip_all, err(Debug))]
    pub async fn save_chat(&self, chat: &ChatHistory) -> Result<(), anyhow::Error> {
        // resolve the directory under the name lock and write atomically
        write_json_in(&self.dir().await, CHAT_FILE, chat).await
    }

    /// Loads the main agent's conversation history from `chat.json`
    ///
    /// Returns `None` when the file does not exist (a pre-chat.json session,
    /// which resumes from the snapshot's embedded history instead) or when it
    /// is corrupt -- the bad file is set aside with a timestamped name and the
    /// resume degrades to the snapshot's history. A version from a newer build
    /// stays a hard error, since setting a valid file aside would lose data.
    #[tracing::instrument(name = "SessionStore::load_chat", skip_all, err(Debug))]
    pub async fn load_chat(&self) -> Result<Option<ChatHistory>, anyhow::Error> {
        // resolve the file path under the name lock
        let path = self.dir().await.join(CHAT_FILE);
        // read the raw bytes, mapping a missing file to None
        let encoded = match tokio::fs::read(&path).await {
            Ok(encoded) => encoded,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        // decode the history from JSON; a corrupt file is set aside and the
        // resume degrades to the snapshot's embedded history instead of
        // bricking (a schema from the FUTURE below stays a hard error, since
        // renaming a valid newer file aside would silently lose history)
        let chat: ChatHistory = match serde_json::from_slice(&encoded) {
            Ok(chat) => chat,
            Err(err) => {
                // a file a newer build wrote can fail this build's decode
                // because the schema changed shape, not because it is
                // corrupt: probe just the version and refuse newer history
                // loudly instead of setting the valid file aside
                if let Ok(probe) = serde_json::from_slice::<VersionProbe>(&encoded)
                    && probe.version > super::resume::CHAT_HISTORY_VERSION
                {
                    anyhow::bail!(
                        "chat.json was written by a newer build (chat history version {}; \
                         this build understands up to {})",
                        probe.version,
                        super::resume::CHAT_HISTORY_VERSION,
                    );
                }
                let aside = set_aside_corrupt(&self.dir().await, CHAT_FILE).await;
                tracing::warn!(
                    ?err,
                    ?aside,
                    "the session has a corrupt chat history; resuming from the snapshot's history"
                );
                return Ok(None);
            }
        };
        // refuse a history written by a schema this build does not understand
        if chat.version > super::resume::CHAT_HISTORY_VERSION {
            anyhow::bail!(
                "unsupported chat history version {} (this build understands up to {})",
                chat.version,
                super::resume::CHAT_HISTORY_VERSION,
            );
        }
        Ok(Some(chat))
    }

    /// Appends one line to the session's ordered display transcript
    ///
    /// The transcript is deliberately append-only (one JSON line per item,
    /// never rewritten), so a crash can at worst truncate the final line --
    /// which the loader skips -- and the file order is exactly the order the
    /// items were rendered live.
    ///
    /// # Arguments
    ///
    /// * `line` - The timestamped transcript line to append
    #[tracing::instrument(name = "SessionStore::append_transcript", skip_all, err(Debug))]
    pub async fn append_transcript(&self, line: &TranscriptLine) -> Result<(), anyhow::Error> {
        // serialize the line to a single JSON line
        let mut line = serde_json::to_string(line)?;
        line.push('\n');
        // resolve the file path under the name lock so renames stay safe
        let path = self.dir().await.join(TRANSCRIPT_FILE);
        // append the line, creating the file on first write
        let mut file = tokio::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .await?;
        tokio::io::AsyncWriteExt::write_all(&mut file, line.as_bytes()).await?;
        Ok(())
    }

    /// Loads the session's ordered display transcript from `transcript.jsonl`
    ///
    /// Returns an empty list when the file does not exist (a pre-transcript
    /// session). An unparseable line -- e.g. a final line torn by a crash, or
    /// an item variant from a newer build -- is skipped with a warning rather
    /// than blocking the resume. Lines written before timestamps existed load
    /// with `ts: None`.
    #[tracing::instrument(name = "SessionStore::load_transcript", skip_all, err(Debug))]
    pub async fn load_transcript(&self) -> Result<Vec<TranscriptLine>, anyhow::Error> {
        // resolve the file path under the name lock
        let path = self.dir().await.join(TRANSCRIPT_FILE);
        // read the whole file, mapping a missing file to an empty transcript
        let encoded = match tokio::fs::read_to_string(&path).await {
            Ok(encoded) => encoded,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        // parse each line, skipping (and warning on) anything unparseable
        let mut items = Vec::new();
        for line in encoded.lines() {
            // ignore blank lines outright
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str(line) {
                Ok(item) => items.push(item),
                Err(err) => tracing::warn!(?err, "skipping an unparseable transcript line"),
            }
        }
        Ok(items)
    }

    /// Saves the shared forum's snapshot to `forum.json`
    ///
    /// # Arguments
    ///
    /// * `forum` - The forum snapshot to persist
    #[tracing::instrument(name = "SessionStore::save_forum", skip_all, err(Debug))]
    pub async fn save_forum(&self, forum: &ForumSnapshot) -> Result<(), anyhow::Error> {
        // resolve the directory under the name lock and write atomically
        write_json_in(&self.dir().await, FORUM_FILE, forum).await
    }

    /// Saves a generated markdown report to `report.md`
    ///
    /// Each save silently overwrites the previous report, so the store always
    /// holds the most recently generated one.
    ///
    /// # Arguments
    ///
    /// * `report` - The generated markdown report to persist
    #[tracing::instrument(name = "SessionStore::save_report", skip_all, err(Debug))]
    pub async fn save_report(&self, report: &str) -> Result<(), anyhow::Error> {
        // resolve the directory under the name lock and write atomically
        write_bytes_in(&self.dir().await, REPORT_FILE, report.as_bytes()).await
    }

    /// Saves a sub-agent invocation's record under `agents/`
    ///
    /// The file is named after the invocation's display name, made file-safe
    /// (e.g. `ReverseEngineer(Europa)` becomes `ReverseEngineer-Europa.json`).
    ///
    /// # Arguments
    ///
    /// * `record` - The sub-agent invocation record to persist
    #[tracing::instrument(name = "SessionStore::save_agent", skip_all, err(Debug))]
    pub async fn save_agent(&self, record: &AgentRecord) -> Result<(), anyhow::Error> {
        // place the record in the agents subdir under a file-safe name
        let dir = self.dir().await.join(AGENTS_DIR);
        // the subdir normally exists, but recreate it in case it was removed
        tokio::fs::create_dir_all(&dir).await?;
        write_json_in(&dir, &format!("{}.json", file_safe(&record.name)), record).await
    }

    /// Loads every saved sub-agent invocation record from `agents/`
    ///
    /// Returns an empty list when the directory does not exist (a session that
    /// never delegated). An unparseable file is skipped with a warning rather
    /// than blocking the resume: records are per-invocation files, so one bad
    /// file only loses that invocation's history. Records are sorted newest
    /// first by start time, so a resume matching a dangling delegation call
    /// without a stored parent call id prefers the most recent invocation
    /// (consumers that want spawn order, like the re-queue renderer, sort
    /// their own candidates).
    #[tracing::instrument(name = "SessionStore::load_agents", skip_all, err(Debug))]
    pub async fn load_agents(&self) -> Result<Vec<AgentRecord>, anyhow::Error> {
        // resolve the agents subdir under the name lock
        let dir = self.dir().await.join(AGENTS_DIR);
        // walk the directory, mapping a missing one to an empty list
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        // parse each record, skipping (and warning on) anything unloadable
        let mut records = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            // only JSON files are agent records; ignore temp or stray files
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            // read and decode the record, skipping files this build can't use
            match tokio::fs::read(&path).await {
                Ok(encoded) => match serde_json::from_slice::<AgentRecord>(&encoded) {
                    Ok(record) => records.push(record),
                    Err(err) => {
                        tracing::warn!(?err, ?path, "skipping an unparseable agent record");
                    }
                },
                Err(err) => tracing::warn!(?err, ?path, "skipping an unreadable agent record"),
            }
        }
        // newest first, so call matching without an id prefers recent records
        records.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        Ok(records)
    }

    /// Saves a named side conversation's history under `btw/`
    ///
    /// The file is named after the side conversation (names are validated to be
    /// path safe before a side is ever created) and each save silently
    /// overwrites the previous one, so the store always holds the side's latest
    /// settled history.
    ///
    /// # Arguments
    ///
    /// * `side` - The side conversation history to persist
    #[tracing::instrument(name = "SessionStore::save_btw", skip_all, err(Debug))]
    pub async fn save_btw(&self, side: &SideHistory) -> Result<(), anyhow::Error> {
        // refuse names that could escape the btw directory
        validate_btw_name(&side.name).map_err(|msg| anyhow::anyhow!(msg))?;
        // place the history in the btw subdir, creating it on first save
        let dir = self.dir().await.join(BTW_DIR);
        tokio::fs::create_dir_all(&dir).await?;
        write_json_in(&dir, &format!("{}.json", side.name), side).await
    }

    /// Loads every saved side conversation's history from `btw/`
    ///
    /// Returns an empty list when the directory does not exist (a session that
    /// never opened a side conversation). An unparseable or newer-versioned
    /// file is skipped with a warning rather than blocking the resume.
    #[tracing::instrument(name = "SessionStore::load_btws", skip_all, err(Debug))]
    pub async fn load_btws(&self) -> Result<Vec<SideHistory>, anyhow::Error> {
        // resolve the btw subdir under the name lock
        let dir = self.dir().await.join(BTW_DIR);
        // walk the directory, mapping a missing one to an empty list
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        // parse each history, skipping (and warning on) anything unloadable
        let mut sides = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            // only JSON files are side histories; ignore temp or stray files
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            // read and decode the history, skipping files this build can't use
            match tokio::fs::read(&path).await {
                Ok(encoded) => match serde_json::from_slice::<SideHistory>(&encoded) {
                    // refuse a history written by a schema this build predates
                    Ok(side) if side.version > super::resume::SIDE_HISTORY_VERSION => {
                        tracing::warn!(
                            name = side.name,
                            version = side.version,
                            "skipping a side conversation from a newer schema"
                        );
                    }
                    Ok(side) => sides.push(side),
                    Err(err) => {
                        tracing::warn!(?err, ?path, "skipping an unparseable side conversation");
                    }
                },
                Err(err) => tracing::warn!(?err, ?path, "skipping an unreadable side conversation"),
            }
        }
        // keep the load order stable so restored tabs always appear alike
        sides.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(sides)
    }

    /// Deletes a named side conversation's history from `btw/`
    ///
    /// A missing file is a no-op so closing an unsaved side never errors.
    ///
    /// # Arguments
    ///
    /// * `name` - The side conversation name to delete
    #[tracing::instrument(name = "SessionStore::delete_btw", skip_all, err(Debug))]
    pub async fn delete_btw(&self, name: &str) -> Result<(), anyhow::Error> {
        // refuse names that could escape the btw directory
        validate_btw_name(name).map_err(|msg| anyhow::anyhow!(msg))?;
        // resolve the side's file path under the name lock
        let path = self.dir().await.join(BTW_DIR).join(format!("{name}.json"));
        // remove the file, treating a missing one as already deleted
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    /// Resolves the session's scripts directory, creating it if needed
    ///
    /// The directory is resolved per call (never cached) because the session
    /// name behind it can change under the rename lock.
    async fn scripts_dir(&self) -> Result<PathBuf, anyhow::Error> {
        // resolve the scripts subdir under the current session directory
        let dir = self.dir().await.join(SCRIPTS_DIR);
        // the subdir normally exists, but recreate it in case it was removed
        tokio::fs::create_dir_all(&dir).await?;
        Ok(dir)
    }

    /// Saves a Rhai script under `scripts/`, overwriting any previous version
    ///
    /// # Arguments
    ///
    /// * `name` - The bare script name (the `.rhai` extension is appended)
    /// * `source` - The script source to persist
    #[tracing::instrument(name = "SessionStore::save_script", skip_all, err(Debug))]
    pub async fn save_script(&self, name: &str, source: &str) -> Result<(), anyhow::Error> {
        // refuse names that could escape the scripts directory
        validate_script_name(name).map_err(|err| anyhow::anyhow!(err))?;
        // resolve the scripts dir under the name lock and write atomically
        let dir = self.scripts_dir().await?;
        write_bytes_in(&dir, &format!("{name}.{SCRIPT_EXT}"), source.as_bytes()).await
    }

    /// Loads a Rhai script's source from `scripts/`
    ///
    /// # Arguments
    ///
    /// * `name` - The bare script name (the `.rhai` extension is appended)
    #[tracing::instrument(name = "SessionStore::load_script", skip_all, err(Debug))]
    pub async fn load_script(&self, name: &str) -> Result<String, anyhow::Error> {
        // refuse names that could escape the scripts directory
        validate_script_name(name).map_err(|err| anyhow::anyhow!(err))?;
        // read the script's source from the scripts subdir
        let dir = self.scripts_dir().await?;
        tokio::fs::read_to_string(dir.join(format!("{name}.{SCRIPT_EXT}")))
            .await
            .map_err(|err| anyhow::anyhow!("failed to read script '{name}': {err}"))
    }

    /// Whether a Rhai script with this name is already saved under `scripts/`
    ///
    /// # Arguments
    ///
    /// * `name` - The bare script name (the `.rhai` extension is appended)
    #[tracing::instrument(name = "SessionStore::script_exists", skip_all, err(Debug))]
    pub async fn script_exists(&self, name: &str) -> Result<bool, anyhow::Error> {
        // refuse names that could escape the scripts directory
        validate_script_name(name).map_err(|err| anyhow::anyhow!(err))?;
        // probe for the script file in the scripts subdir
        let dir = self.scripts_dir().await?;
        Ok(tokio::fs::try_exists(dir.join(format!("{name}.{SCRIPT_EXT}"))).await?)
    }

    /// Lists the bare names of every Rhai script saved under `scripts/`
    #[tracing::instrument(name = "SessionStore::list_scripts", skip_all, err(Debug))]
    pub async fn list_scripts(&self) -> Result<Vec<String>, anyhow::Error> {
        // walk the scripts subdir collecting every saved script
        let dir = self.scripts_dir().await?;
        let mut entries = tokio::fs::read_dir(&dir).await?;
        let mut scripts = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            // only `.rhai` files are scripts; skip temp files and strays
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some(SCRIPT_EXT)
                && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
            {
                scripts.push(stem.to_owned());
            }
        }
        // sort so the listing is stable across calls
        scripts.sort();
        Ok(scripts)
    }

    /// Resolves the session's extracted-files directory, creating it if needed
    ///
    /// The directory is resolved per call (never cached) because the session
    /// name behind it can change under the rename lock.
    pub(crate) async fn extracted_dir(&self) -> Result<PathBuf, anyhow::Error> {
        // resolve the extracted subdir under the current session directory
        let dir = self.dir().await.join(EXTRACTED_DIR);
        // the subdir normally exists, but recreate it in case it was removed
        tokio::fs::create_dir_all(&dir).await?;
        Ok(dir)
    }

    /// Resolves the full path of one named file under `extracted/`
    ///
    /// The name is validated first, so the returned path can never escape the
    /// extracted directory. The file itself may or may not exist yet.
    ///
    /// # Arguments
    ///
    /// * `name` - The extracted file's bare name
    #[tracing::instrument(name = "SessionStore::extracted_path", skip_all, err(Debug))]
    pub async fn extracted_path(&self, name: &str) -> Result<PathBuf, anyhow::Error> {
        // refuse names that could escape the extracted directory
        validate_extracted_name(name).map_err(|err| anyhow::anyhow!(err))?;
        // resolve the extracted dir under the name lock and join the file name
        let dir = self.extracted_dir().await?;
        Ok(dir.join(name))
    }

    /// Lists the names of every file saved under `extracted/`
    #[tracing::instrument(name = "SessionStore::list_extracted", skip_all, err(Debug))]
    pub async fn list_extracted(&self) -> Result<Vec<String>, anyhow::Error> {
        // walk the extracted subdir collecting every finished file
        let dir = self.extracted_dir().await?;
        let mut entries = tokio::fs::read_dir(&dir).await?;
        let mut names = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            // only plain files are extracted outputs; skip strays and subdirs
            if !entry.file_type().await?.is_file() {
                continue;
            }
            // skip in-flight `.tmp` siblings a crashed extraction left behind
            if let Some(name) = entry.file_name().to_str()
                && !name.ends_with(".tmp")
            {
                names.push(name.to_owned());
            }
        }
        // sort so the listing is stable across calls
        names.sort();
        Ok(names)
    }

    /// Sums the sizes of every file saved under `extracted/`, in bytes
    ///
    /// Used to enforce the total extraction budget before another entry is
    /// extracted into this session's workspace. Stray `.tmp` siblings a
    /// crashed extraction left behind are skipped (like [`list_extracted`]
    /// skips them), so they can never eat the budget forever.
    ///
    /// [`list_extracted`]: SessionStore::list_extracted
    #[tracing::instrument(name = "SessionStore::extracted_usage", skip_all, err(Debug))]
    pub async fn extracted_usage(&self) -> Result<u64, anyhow::Error> {
        // walk the extracted subdir summing every file's size
        let dir = self.extracted_dir().await?;
        let mut entries = tokio::fs::read_dir(&dir).await?;
        let mut total = 0u64;
        while let Some(entry) = entries.next_entry().await? {
            // skip stray in-flight `.tmp` siblings so a crash never leaks budget
            if entry.file_name().to_string_lossy().ends_with(".tmp") {
                continue;
            }
            // count plain files only
            let meta = entry.metadata().await?;
            if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
        Ok(total)
    }

    /// Locks the extracted workspace for one write
    ///
    /// Every tool path that writes into `extracted/` holds this guard across
    /// its budget check, the extraction itself, and the final rename (see the
    /// `extract_lock` field).
    pub(crate) async fn lock_extracted(&self) -> tokio::sync::MutexGuard<'_, ()> {
        // hand the caller the guard; dropping it releases the workspace
        self.extract_lock.lock().await
    }

    /// Pulls the shared session store out of a tool call's [`ToolContext`]
    ///
    /// Mirrors `ForumState::from_context`: a missing store is a wiring bug, so
    /// it surfaces as an invalid-args tool error.
    ///
    /// # Arguments
    ///
    /// * `context` - The tool call context the run was prompted with
    pub(crate) fn from_context(
        context: &ToolContext,
    ) -> Result<Arc<SessionStore>, ToolExecutionError> {
        // recover the shared store, mapping a missing slot to a tool error
        context
            .require::<Arc<SessionStore>>()
            .map(Arc::clone)
            .map_err(|err| ToolExecutionError::invalid_args(err.to_string()))
    }

    /// Pulls the shared session store out of a [`ToolContext`], if one is set
    ///
    /// Unlike [`SessionStore::from_context`], a run without a store (e.g. a
    /// bare library consumer) is not an error -- callers simply skip the
    /// store-backed behavior.
    ///
    /// # Arguments
    ///
    /// * `context` - The tool call context the run was prompted with
    pub(crate) fn try_from_context(context: &ToolContext) -> Option<Arc<SessionStore>> {
        // a missing store just means no session persistence was configured
        context.require::<Arc<SessionStore>>().ok().map(Arc::clone)
    }

    /// Assembles the current metadata for this session under a given name
    ///
    /// # Arguments
    ///
    /// * `name` - The session name to stamp into the metadata
    fn meta_for(&self, name: &str) -> SessionMeta {
        SessionMeta {
            version: META_VERSION,
            name: name.to_owned(),
            user_named: self.user_named,
            renamed: self.renamed.load(Ordering::SeqCst),
            created_at: self.created_at,
            binary: self.binary.clone(),
            project_name: self.project_name.clone(),
            anonymized: self.anonymized,
            extra_samples: self.extra_samples.clone(),
        }
    }

    /// Writes the session's current metadata to `meta.json`
    async fn save_meta(&self) -> Result<(), anyhow::Error> {
        // resolve the name and directory under the read lock, then write
        let name = self.name.read().await;
        let meta = self.meta_for(&name);
        write_json_in(&self.root.join(&*name), META_FILE, &meta).await
    }
}

/// Atomically writes a value as pretty JSON into a directory
///
/// The value is written to a `.tmp` sibling first and renamed over the target,
/// so a crash mid-write can never leave a truncated JSON file behind.
///
/// # Arguments
///
/// * `dir` - The directory to write the file into
/// * `file` - The target file name within `dir`
/// * `value` - The value to serialize
async fn write_json_in<T: Serialize>(
    dir: &Path,
    file: &str,
    value: &T,
) -> Result<(), anyhow::Error> {
    // serialize to pretty JSON so saved sessions stay human-inspectable
    let encoded = serde_json::to_vec_pretty(value)?;
    // hand the encoded bytes to the shared atomic writer
    write_bytes_in(dir, file, &encoded).await
}

/// Atomically writes raw bytes into a directory
///
/// The bytes are written to a `.tmp` sibling first and renamed over the target,
/// so a crash mid-write can never leave a truncated file behind.
///
/// # Arguments
///
/// * `dir` - The directory to write the file into
/// * `file` - The target file name within `dir`
/// * `bytes` - The bytes to write
async fn write_bytes_in(dir: &Path, file: &str, bytes: &[u8]) -> Result<(), anyhow::Error> {
    // write the bytes to a temp sibling in the same directory
    let tmp = dir.join(format!("{file}.tmp"));
    tokio::fs::write(&tmp, bytes).await?;
    // atomically move the finished file over the target
    tokio::fs::rename(&tmp, dir.join(file)).await?;
    Ok(())
}

/// Validates a model-supplied script name
///
/// Script names become file names under the session's `scripts/` directory, so
/// they must stay short and path safe: ascii letters, digits, hyphens, and
/// underscores only, with no separators or extensions that could escape the
/// directory or collide with the appended `.rhai` extension.
///
/// # Arguments
///
/// * `name` - The script name to validate
pub fn validate_script_name(name: &str) -> Result<(), String> {
    // an empty name cannot be referenced and an overlong one is unwieldy
    if name.is_empty() || name.len() > MAX_SCRIPT_NAME_LEN {
        return Err(format!(
            "script names must be 1-{MAX_SCRIPT_NAME_LEN} characters long"
        ));
    }
    // keep names path safe so they can never escape the scripts directory
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(
            "script names may only contain ascii letters, digits, hyphens, and \
             underscores (no path separators or extensions)"
                .to_owned(),
        );
    }
    Ok(())
}

/// Validates the name of a file extracted into the session's `extracted/` dir
///
/// Extracted names become file names under the session's `extracted/`
/// directory and `target` arguments to every file tool, so they must stay
/// short and path safe: ascii letters, digits, dots, hyphens, and underscores
/// only, never starting with a dot, hyphen, or underscore. Path separators are
/// outside the charset, so traversal is unrepresentable.
///
/// # Arguments
///
/// * `name` - The extracted file name to validate
pub fn validate_extracted_name(name: &str) -> Result<(), String> {
    // an empty name cannot be referenced and an overlong one is unwieldy
    if name.is_empty() || name.len() > MAX_EXTRACTED_NAME_LEN {
        return Err(format!(
            "extracted file names must be 1-{MAX_EXTRACTED_NAME_LEN} characters long"
        ));
    }
    // keep names path safe so they can never escape the extracted directory
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
    {
        return Err(
            "extracted file names may only contain ascii letters, digits, dots, \
             hyphens, and underscores (no path separators)"
                .to_owned(),
        );
    }
    // a leading dot, hyphen, or underscore makes hidden or flag-like names
    if name.starts_with('.') || name.starts_with('-') || name.starts_with('_') {
        return Err(
            "extracted file names must not start with a dot, hyphen, or underscore".to_owned(),
        );
    }
    // the `.tmp` suffix is reserved for in-flight writes, which the listing
    // and budget deliberately skip -- a `.tmp` name would be unaddressable
    if name.ends_with(".tmp") {
        return Err(
            "extracted file names must not end in '.tmp' (reserved for in-flight writes)"
                .to_owned(),
        );
    }
    Ok(())
}

/// Validates an operator-supplied side-conversation name (the `/btw` command)
///
/// Side names become file names under the session's `btw/` directory and
/// `btw:<name>` usage-ledger keys, so they must stay short, path safe, and
/// unambiguous: lowercase ascii letters, digits, hyphens, and underscores only,
/// and never the reserved main conversation's name.
///
/// # Arguments
///
/// * `name` - The side conversation name to validate
pub fn validate_btw_name(name: &str) -> Result<(), String> {
    // an empty name cannot be referenced and an overlong one breaks tab labels
    if name.is_empty() || name.len() > MAX_BTW_NAME_LEN {
        return Err(format!(
            "side conversation names must be 1-{MAX_BTW_NAME_LEN} characters long"
        ));
    }
    // keep names path safe so they can never escape the btw directory, and
    // lowercase so ledger keys and tab labels stay visually consistent
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return Err(
            "side conversation names may only contain lowercase ascii letters, digits, \
             hyphens, and underscores"
                .to_owned(),
        );
    }
    // the main conversation's name is reserved so scopes can never collide
    if name == "main" {
        return Err("'main' is reserved for the main conversation".to_owned());
    }
    Ok(())
}

/// Validates an operator-supplied session name (the `--name` flag)
///
/// Names become directory names, so they must stay short and file/shell safe:
/// ascii letters, digits, hyphens, and underscores only.
///
/// # Arguments
///
/// * `name` - The session name to validate
pub fn validate_session_name(name: &str) -> Result<(), String> {
    // an empty name cannot be referenced and an overlong one is unwieldy
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return Err(format!(
            "session names must be 1-{MAX_NAME_LEN} characters long"
        ));
    }
    // keep names file and shell safe so they can be typed and resumed easily
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(
            "session names may only contain ascii letters, digits, hyphens, and underscores"
                .to_owned(),
        );
    }
    Ok(())
}

/// Validates a model-chosen session rename
///
/// Renames must be descriptive and easy to say out loud: strictly ascii
/// alphanumeric (so an IP, MAC address, or path can never become a name), at
/// least one letter, and not hash-like (a long string of pure hex digits).
///
/// # Arguments
///
/// * `name` - The proposed new session name
pub fn validate_rename(name: &str) -> Result<(), String> {
    // keep renames long enough to mean something and short enough to say
    if name.len() < MIN_RENAME_LEN || name.len() > MAX_NAME_LEN {
        return Err(format!(
            "session names must be {MIN_RENAME_LEN}-{MAX_NAME_LEN} characters long"
        ));
    }
    // strictly alphanumeric rejects IPs, MACs, paths, and anything unspeakable
    if !name.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(
            "session names must be alphanumeric only (no spaces, dots, colons, or punctuation); \
             pick a speakable name like 'TrojanizedChrome'"
                .to_owned(),
        );
    }
    // a name with no letters at all (e.g. a port or pid) says nothing
    if !name.chars().any(|c| c.is_ascii_alphabetic()) {
        return Err("session names must contain at least one letter".to_owned());
    }
    // a long run of pure hex digits is a hash or address, not a name
    if name.len() >= 8 && name.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(
            "session names must not be a hash, address, or other raw hex indicator; \
             describe the finding instead (e.g. the malware family or behavior)"
                .to_owned(),
        );
    }
    Ok(())
}

/// Maps a display name onto a file-safe form for `agents/` record files
///
/// Every character outside `[A-Za-z0-9_-]` becomes a hyphen, runs of hyphens
/// collapse, and leading/trailing hyphens are trimmed, so
/// `ReverseEngineer(Europa)` becomes `ReverseEngineer-Europa`.
///
/// # Arguments
///
/// * `name` - The display name to make file-safe
fn file_safe(name: &str) -> String {
    // map every unsafe character to a hyphen
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            out.push(c);
        } else if !out.ends_with('-') {
            // collapse runs of unsafe characters into a single hyphen
            out.push('-');
        }
    }
    // trim any leading/trailing hyphen the mapping left behind
    out.trim_matches('-').to_owned()
}

#[rig::tool_macro(
    description = "Renames this analysis session to a short, descriptive, easily spoken name based on your findings -- for example the malware family you identified, or the most unique identifying behavior or IOC, like \"TrojanizedChrome\" or \"XorLoaderBot\". The name must be alphanumeric characters only (no spaces or punctuation). Never use an IP address, MAC address, hash, hex offset, or other raw indicator as the name. Call this once, right before you call finish."
)]
pub async fn rename_session(
    #[rig(context)] context: &mut ToolContext,
    name: String,
) -> Result<String, ToolExecutionError> {
    // recover this run's session store and rename the session on it
    let store = SessionStore::from_context(context)?;
    // map a validation or collision failure to an instructive tool error so
    // the model can correct its name and retry
    let name = store
        .rename(&name)
        .await
        .map_err(|err| ToolExecutionError::invalid_args(err.to_string()))?;
    Ok(format!("Session renamed to '{name}'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thorpal::AiProviderConfig;
    use crate::thorpal::resume::SNAPSHOT_VERSION;
    use crate::thorpal::tools::ghidra::GhidraSettings;
    use crate::thorpal::transcript::TranscriptItem;

    /// A corrupt forum.json degrades to an empty forum with the file set aside
    #[tokio::test]
    async fn corrupt_forum_degrades_and_is_set_aside() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = scratch_store(dir.path()).await;
        let name = store.name().await;
        // persist a valid snapshot so the session is resumable at all
        store.save_session(&snapshot()).await.expect("save");
        // clobber the forum with garbage, as a torn write would
        let session_dir = store.dir().await;
        tokio::fs::write(session_dir.join(FORUM_FILE), b"{not json")
            .await
            .expect("corrupt write");
        drop(store);
        // the open succeeds, degraded to an empty forum
        let (_store, _meta, _snapshot, forum) = SessionStore::open(dir.path().to_path_buf(), &name)
            .await
            .expect("a corrupt forum must not brick the resume");
        assert!(forum.posts.is_empty());
        // the bad file was renamed aside for post-mortems
        let mut aside = tokio::fs::read_dir(&session_dir).await.expect("read dir");
        let mut found = false;
        while let Some(entry) = aside.next_entry().await.expect("entry") {
            let file = entry.file_name().to_string_lossy().into_owned();
            if file.starts_with("forum.json.corrupt-") {
                found = true;
            }
        }
        assert!(found, "the corrupt forum must be set aside, not deleted");
    }

    /// A corrupt chat.json degrades to the snapshot history with the file aside
    #[tokio::test]
    async fn corrupt_chat_degrades_and_is_set_aside() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = scratch_store(dir.path()).await;
        // clobber the chat history with garbage
        let session_dir = store.dir().await;
        tokio::fs::write(session_dir.join(CHAT_FILE), b"{not json")
            .await
            .expect("corrupt write");
        // the load degrades to None (the snapshot's history) instead of erroring
        let chat = store
            .load_chat()
            .await
            .expect("a corrupt chat must degrade");
        assert!(chat.is_none());
        // the bad file was renamed aside for post-mortems
        assert!(!session_dir.join(CHAT_FILE).exists());
    }

    /// A chat.json from a newer build hard-errors instead of degrading
    #[tokio::test]
    async fn future_chat_schema_errors_instead_of_degrading() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = scratch_store(dir.path()).await;
        let session_dir = store.dir().await;
        // a plausible future schema: version bumped and `messages` reshaped,
        // so this build's full decode fails even though the file is valid
        tokio::fs::write(
            session_dir.join(CHAT_FILE),
            br#"{"version":2,"conversation_id":"main","messages":{"settled":[]},"pending":[]}"#,
        )
        .await
        .expect("future write");
        // the load refuses loudly instead of silently losing newer history
        let err = store
            .load_chat()
            .await
            .expect_err("a newer schema must be a hard error, not Ok(None)");
        assert!(err.to_string().contains("newer build"), "got: {err}");
        // the valid newer file must stay in place, never set aside
        assert!(session_dir.join(CHAT_FILE).exists());
        // and no aside copy may have been created for it
        let mut entries = tokio::fs::read_dir(&session_dir).await.expect("read dir");
        while let Some(entry) = entries.next_entry().await.expect("entry") {
            let file = entry.file_name().to_string_lossy().into_owned();
            assert!(
                !file.starts_with("chat.json.corrupt-"),
                "a newer-schema file must never be set aside, found {file}"
            );
        }
    }

    /// Two set-asides in the same second keep both evidence files
    #[tokio::test]
    async fn same_second_set_asides_keep_both_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        // set two corrupt copies aside back-to-back, within one stamp second
        tokio::fs::write(dir.path().join(CHAT_FILE), b"{bad one")
            .await
            .expect("first corrupt write");
        let first = set_aside_corrupt(dir.path(), CHAT_FILE)
            .await
            .expect("first aside");
        tokio::fs::write(dir.path().join(CHAT_FILE), b"{bad two")
            .await
            .expect("second corrupt write");
        let second = set_aside_corrupt(dir.path(), CHAT_FILE)
            .await
            .expect("second aside");
        // the second aside picked a fresh name instead of clobbering the first
        assert_ne!(first, second, "the second aside must not reuse the name");
        // both evidence files survive with their own contents
        let one = tokio::fs::read(&first).await.expect("first evidence");
        let two = tokio::fs::read(&second).await.expect("second evidence");
        assert_eq!(one, b"{bad one");
        assert_eq!(two, b"{bad two");
    }

    /// Builds a minimal session snapshot for store round-trip tests
    fn snapshot() -> SessionSnapshot {
        SessionSnapshot {
            version: SNAPSHOT_VERSION,
            provider: AiProviderConfig::LiteLlm {
                endpoint: "https://example.test".to_owned(),
                api_key: None,
                model: "some-model".to_owned(),
            },
            preamble: None,
            ghidra: GhidraSettings::from_binary("bin/target", "projects", None),
            workflow: crate::thorpal::agents::Workflow::builtin().expect("builtin workflow"),
            conversation_id: "main".to_owned(),
            messages: vec![Message::user("hello"), Message::assistant("hi")],
            usage: std::collections::BTreeMap::new(),
            name_counter: 0,
        }
    }

    /// A failed record keeps its salvaged transcript, extensions, and handoff.
    #[test]
    fn finish_err_round_trips_the_salvage() {
        // start a record exactly as the sub-agent runner does
        let mut record = AgentRecord::started(
            "ReverseEngineer(Io)",
            "ReverseEngineer",
            "understand the sample",
            "decompile the decryptor",
            None,
        );
        // record one granted and one refused extension along the way
        record.extensions = vec![
            ExtensionRequest {
                requested: 10,
                granted: 4,
                justification: "two functions left".to_owned(),
                at: Utc::now(),
            },
            ExtensionRequest {
                requested: 5,
                granted: 0,
                justification: "one constant to verify".to_owned(),
                at: Utc::now(),
            },
        ];
        // finish it as a death carrying the partial transcript and handoff
        record.finish_err(
            "MaxTurnsError: reached max turns limit: 14",
            vec![
                Message::user("task"),
                Message::assistant("partial findings"),
            ],
            Some("SUB-AGENT TERMINATED EARLY -- partial handoff".to_owned()),
            "max_turns",
        );
        // the record round-trips through its on-disk JSON form intact
        let encoded = serde_json::to_string(&record).expect("record should serialize");
        let decoded: AgentRecord = serde_json::from_str(&encoded).expect("record should parse");
        assert_eq!(decoded.success, Some(false));
        assert_eq!(decoded.messages.len(), 2);
        assert_eq!(decoded.extensions.len(), 2);
        assert_eq!(decoded.extensions[1].granted, 0);
        assert!(
            decoded
                .handoff
                .expect("handoff kept")
                .contains("TERMINATED")
        );
    }

    /// A legacy record predating the salvage fields still deserializes.
    #[test]
    fn legacy_agent_records_still_parse() {
        // the exact shape a pre-salvage record carried on disk
        let old = serde_json::json!({
            "name": "ReverseEngineer(Io)",
            "role": "ReverseEngineer",
            "goal": "understand the sample",
            "task": "decompile the decryptor",
            "started_at": Utc::now(),
            "finished_at": Utc::now(),
            "success": false,
            "output": null,
            "error": "MaxTurnsError: reached max turns limit: 50",
            "messages": [],
        });
        // the new fields default instead of failing the parse
        let record: AgentRecord = serde_json::from_value(old).expect("legacy record should parse");
        assert!(record.extensions.is_empty());
        assert!(record.handoff.is_none());
    }

    /// Old meta.json (no `extra_samples`) loads with an empty sample list.
    #[test]
    fn meta_extra_samples_default_and_round_trip() {
        // the exact shape every pre-multi-sample meta.json carries
        let old = serde_json::json!({
            "version": META_VERSION,
            "name": "quiet-otter",
            "user_named": false,
            "renamed": false,
            "created_at": "2026-10-01T00:00:00Z",
            "binary": "bin/target",
            "project_name": "target",
        });
        let meta: SessionMeta = serde_json::from_value(old).expect("old meta");
        assert!(meta.extra_samples.is_empty());
        // a populated sample list survives a round-trip
        let sample = ExtraSampleMeta {
            binary: PathBuf::from("staged/abc"),
        };
        let wire = serde_json::to_value(&sample).expect("serialize");
        let back: ExtraSampleMeta = serde_json::from_value(wire).expect("round trip");
        assert_eq!(back.binary, PathBuf::from("staged/abc"));
    }

    /// Metas written before anonymize-at-rest (with recorded staging paths)
    /// still parse, and rewriting them drops the retired path fields.
    #[test]
    fn legacy_original_binary_metas_still_parse_and_scrub() {
        // the exact shape an anonymized pre-0043 session recorded
        let old = serde_json::json!({
            "version": META_VERSION,
            "name": "quiet-otter",
            "user_named": false,
            "renamed": false,
            "created_at": "2026-10-01T00:00:00Z",
            "binary": "staged/abc",
            "project_name": "abc",
            "original_binary": "corpus/family/x.bin",
            "anonymized": true,
            "extra_samples": [
                {"binary": "staged/def", "original_binary": "corpus/family/y.bin"},
            ],
        });
        // the retired fields are ignored instead of failing the parse
        let meta: SessionMeta = serde_json::from_value(old).expect("legacy meta");
        assert_eq!(meta.extra_samples.len(), 1);
        // a rewrite of that meta carries no staging-source path anywhere
        let rewritten = serde_json::to_string(&meta).expect("serialize");
        assert!(!rewritten.contains("original_binary"));
        assert!(!rewritten.contains("corpus"));
    }

    /// Creates a store under a scratch root with an auto-generated name
    ///
    /// # Arguments
    ///
    /// * `root` - The scratch sessions root to create the store under
    async fn scratch_store(root: &Path) -> Arc<SessionStore> {
        SessionStore::create(
            root.to_path_buf(),
            None,
            PathBuf::from("bin/target"),
            "target".to_owned(),
            false,
            Vec::new(),
        )
        .await
        .expect("store should create")
    }

    /// Side histories save, load back sorted, and delete as no-ops when gone.
    #[tokio::test]
    async fn btw_histories_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = scratch_store(dir.path()).await;
        // a session that never opened a side conversation loads an empty list
        assert!(store.load_btws().await.expect("load").is_empty());
        // save two side conversations out of name order
        for name in ["zeta", "alpha"] {
            let side = crate::thorpal::resume::SideHistory {
                version: crate::thorpal::resume::SIDE_HISTORY_VERSION,
                name: name.to_owned(),
                created_at: chrono::Utc::now(),
                messages: vec![Message::user("hello"), Message::assistant("hi")],
            };
            store.save_btw(&side).await.expect("save");
        }
        // the saved sides load back sorted by name with their messages intact
        let sides = store.load_btws().await.expect("load");
        let names: Vec<&str> = sides.iter().map(|side| side.name.as_str()).collect();
        assert_eq!(names, ["alpha", "zeta"]);
        assert_eq!(sides[0].messages.len(), 2);
        // deleting one side leaves the other behind
        store.delete_btw("zeta").await.expect("delete");
        let sides = store.load_btws().await.expect("load");
        assert_eq!(sides.len(), 1);
        assert_eq!(sides[0].name, "alpha");
        // deleting a side that never existed is a no-op
        store.delete_btw("ghost").await.expect("missing delete");
    }

    /// Side names must stay short, path safe, and never the reserved main.
    #[test]
    fn btw_names_are_validated() {
        // simple lowercase names with digits, hyphens, and underscores pass
        assert!(validate_btw_name("crypto").is_ok());
        assert!(validate_btw_name("xor-stub_2").is_ok());
        // empty, overlong, uppercase, path-escaping, and reserved names fail
        assert!(validate_btw_name("").is_err());
        assert!(validate_btw_name(&"a".repeat(MAX_BTW_NAME_LEN + 1)).is_err());
        assert!(validate_btw_name("Crypto").is_err());
        assert!(validate_btw_name("../escape").is_err());
        assert!(validate_btw_name("main").is_err());
    }

    /// Auto-generated names claim unique directories even in bulk.
    #[tokio::test]
    async fn auto_names_are_unique() {
        let dir = tempfile::tempdir().expect("tempdir");
        // claim a batch of sessions and collect their names
        let mut names = std::collections::HashSet::new();
        for _ in 0..20 {
            let store = scratch_store(dir.path()).await;
            // every claim must yield a name no other session holds
            assert!(names.insert(store.name().await));
        }
    }

    /// An explicit name collision errors and points at resuming.
    #[tokio::test]
    async fn explicit_name_collision_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        // claim the name once
        SessionStore::create(
            dir.path().to_path_buf(),
            Some("MySession".to_owned()),
            PathBuf::from("bin/target"),
            "target".to_owned(),
            false,
            Vec::new(),
        )
        .await
        .expect("first claim should succeed");
        // the second claim must fail with a resume hint
        let err = SessionStore::create(
            dir.path().to_path_buf(),
            Some("MySession".to_owned()),
            PathBuf::from("bin/target"),
            "target".to_owned(),
            false,
            Vec::new(),
        )
        .await
        .err()
        .expect("second claim should fail");
        assert!(err.to_string().contains("--resume"));
    }

    /// Scripts save, load, list, and probe through the scripts folder.
    #[tokio::test]
    async fn scripts_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = scratch_store(dir.path()).await;
        // a fresh session starts with an empty scripts folder
        assert!(store.list_scripts().await.expect("list").is_empty());
        assert!(!store.script_exists("decode").await.expect("probe"));
        // save two scripts and read one back verbatim
        store
            .save_script("decode", "40 + 2")
            .await
            .expect("save decode");
        store.save_script("scan", "1 + 1").await.expect("save scan");
        assert_eq!(store.load_script("decode").await.expect("load"), "40 + 2");
        assert!(store.script_exists("decode").await.expect("probe"));
        // the listing is the bare names, sorted
        assert_eq!(
            store.list_scripts().await.expect("list"),
            ["decode", "scan"]
        );
        // saving again overwrites in place
        store.save_script("decode", "41 + 1").await.expect("resave");
        assert_eq!(store.load_script("decode").await.expect("load"), "41 + 1");
        // the files live under the session's scripts folder with the extension
        let path = store.dir().await.join("scripts").join("decode.rhai");
        assert!(path.is_file());
    }

    /// Script names that could escape the scripts folder are rejected.
    #[tokio::test]
    async fn bad_script_names_are_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = scratch_store(dir.path()).await;
        // separators, traversal, extensions, and empty names are all rejected
        for name in ["../escape", "a/b", "a\\b", "a.rhai", "", &"x".repeat(65)] {
            assert!(
                store.save_script(name, "1").await.is_err(),
                "{name:?} saved"
            );
            assert!(store.load_script(name).await.is_err(), "{name:?} loaded");
            assert!(store.script_exists(name).await.is_err(), "{name:?} probed");
        }
    }

    /// Extracted names reject traversal, hidden names, and bad charsets.
    #[test]
    fn extracted_names_are_validated() {
        // plain names with dots, hyphens, underscores, and digits pass
        assert!(validate_extracted_name("payload.exe").is_ok());
        assert!(validate_extracted_name("stage2_dropper-v1.bin").is_ok());
        assert!(validate_extracted_name("decompressed-0x40-gzip").is_ok());
        // empty, overlong, separator-bearing, and traversal names all fail
        assert!(validate_extracted_name("").is_err());
        assert!(validate_extracted_name(&"a".repeat(MAX_EXTRACTED_NAME_LEN + 1)).is_err());
        assert!(validate_extracted_name("a/b").is_err());
        assert!(validate_extracted_name("a\\b").is_err());
        assert!(validate_extracted_name("../escape").is_err());
        assert!(validate_extracted_name("with space").is_err());
        // leading dots, hyphens, and underscores make hidden/flag-like names
        assert!(validate_extracted_name(".hidden").is_err());
        assert!(validate_extracted_name("-flag").is_err());
        assert!(validate_extracted_name("_sneaky").is_err());
        // the .tmp suffix is reserved for in-flight writes the listing hides
        assert!(validate_extracted_name("payload.tmp").is_err());
        assert!(validate_extracted_name("x.bin.tmp").is_err());
        assert!(validate_extracted_name("payload.tmp2").is_ok());
    }

    /// Extracted files list, size, and resolve through the workspace.
    #[tokio::test]
    async fn extracted_workspace_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = scratch_store(dir.path()).await;
        // a fresh session starts with an empty, zero-byte workspace
        assert!(store.list_extracted().await.expect("list").is_empty());
        assert_eq!(store.extracted_usage().await.expect("usage"), 0);
        // write two files through resolved paths, plus a stray tmp sibling
        let payload = store.extracted_path("payload.bin").await.expect("path");
        tokio::fs::write(&payload, b"abcd").await.expect("write");
        let config = store.extracted_path("config.dat").await.expect("path");
        tokio::fs::write(&config, b"xy").await.expect("write");
        // a stray tmp sibling (a crashed write) lands beside the real files;
        // .tmp names are rejected by the validator, so build the path directly
        let tmp = store
            .extracted_dir()
            .await
            .expect("dir")
            .join("torn.bin.tmp");
        tokio::fs::write(&tmp, b"zz").await.expect("write");
        // the listing is sorted and skips the in-flight tmp sibling
        assert_eq!(
            store.list_extracted().await.expect("list"),
            ["config.dat", "payload.bin"]
        );
        // the usage skips the tmp sibling too, so a crash never leaks budget
        assert_eq!(store.extracted_usage().await.expect("usage"), 6);
        // bad names never resolve to a path at all
        for name in ["../escape", "a/b", ".hidden", ""] {
            assert!(
                store.extracted_path(name).await.is_err(),
                "{name:?} resolved"
            );
        }
    }

    /// A rename keeps the extracted workspace addressable at the new location.
    #[tokio::test]
    async fn rename_keeps_extracted_addressable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = scratch_store(dir.path()).await;
        store.save_session(&snapshot()).await.expect("save session");
        // write one extracted file before the rename
        let before = store.extracted_path("payload.bin").await.expect("path");
        tokio::fs::write(&before, b"abcd").await.expect("write");
        // rename the session, moving the whole directory
        store.rename("ContainerDropper").await.expect("rename");
        // the same name resolves under the new directory with its bytes intact
        let after = store.extracted_path("payload.bin").await.expect("path");
        assert!(after.starts_with(dir.path().join("ContainerDropper")));
        assert_eq!(tokio::fs::read(&after).await.expect("read"), b"abcd");
        assert_eq!(store.list_extracted().await.expect("list"), ["payload.bin"]);
    }

    /// Saved session, forum, and meta files round-trip through open.
    #[tokio::test]
    async fn save_then_open_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = scratch_store(dir.path()).await;
        let name = store.name().await;
        // save a snapshot and a forum with recognizable contents
        store.save_session(&snapshot()).await.expect("save session");
        let forum = ForumSnapshot {
            posts: Vec::new(),
            comments: Vec::new(),
            tasks: Vec::new(),
            asks: Vec::new(),
            flags: Vec::new(),
            next_comment_id: 7,
            next_task_id: 1,
            next_ask_id: 1,
            next_flag_id: 1,
            finish_requested: true,
        };
        store.save_forum(&forum).await.expect("save forum");
        // open the session back up and check everything survived
        let (_, meta, snapshot, forum) = SessionStore::open(dir.path().to_path_buf(), &name)
            .await
            .expect("open should succeed");
        assert_eq!(meta.name, name);
        assert!(!meta.user_named);
        assert_eq!(snapshot.messages.len(), 2);
        assert_eq!(forum.next_comment_id, 7);
        assert!(forum.finish_requested);
    }

    /// A missing forum.json opens as an empty forum rather than an error.
    #[tokio::test]
    async fn missing_forum_opens_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = scratch_store(dir.path()).await;
        let name = store.name().await;
        // save only the session snapshot, never the forum
        store.save_session(&snapshot()).await.expect("save session");
        let (_, _, _, forum) = SessionStore::open(dir.path().to_path_buf(), &name)
            .await
            .expect("open should succeed");
        // the forum starts empty with its scalars at their initial values
        assert!(forum.posts.is_empty());
        assert_eq!(forum.next_comment_id, 1);
        assert!(!forum.finish_requested);
    }

    /// A session with no saved snapshot cannot be opened.
    #[tokio::test]
    async fn open_without_snapshot_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = scratch_store(dir.path()).await;
        let name = store.name().await;
        // the store exists but no session.json was ever written
        let err = SessionStore::open(dir.path().to_path_buf(), &name)
            .await
            .err()
            .expect("open should fail");
        assert!(err.to_string().contains("no loadable saved state"));
    }

    /// Renames move the directory, update meta, and refuse collisions.
    #[tokio::test]
    async fn rename_moves_the_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = scratch_store(dir.path()).await;
        let old = store.name().await;
        // an auto-named session needs a rename until the model performs one
        assert!(store.needs_rename());
        store.save_session(&snapshot()).await.expect("save session");
        // rename the session to a descriptive name
        store.rename("TrojanizedChrome").await.expect("rename");
        assert!(!store.needs_rename());
        // the old directory is gone and the new one holds the session
        assert!(!dir.path().join(&old).exists());
        assert!(
            dir.path()
                .join("TrojanizedChrome")
                .join(SESSION_FILE)
                .exists()
        );
        // the session reopens under its new name with the rename recorded
        let (_, meta, _, _) = SessionStore::open(dir.path().to_path_buf(), "TrojanizedChrome")
            .await
            .expect("open renamed session");
        assert!(meta.renamed);
        // a second session cannot rename into the now-taken name
        let other = scratch_store(dir.path()).await;
        let err = other
            .rename("TrojanizedChrome")
            .await
            .err()
            .expect("collision should fail");
        assert!(err.to_string().contains("already exists"));
    }

    /// The rename rules reject raw indicators and accept descriptive names.
    #[test]
    fn rename_validation_rules() {
        // descriptive alphanumeric names are accepted
        assert!(validate_rename("TrojanizedChrome").is_ok());
        assert!(validate_rename("XorLoaderBot2").is_ok());
        // an IP address is punctuation-laden and rejected
        assert!(validate_rename("192.168.1.1").is_err());
        // a MAC address is rejected for its colons
        assert!(validate_rename("00:1A:2B:3C").is_err());
        // a hash-like pure-hex string is rejected even without punctuation
        assert!(validate_rename("DEADBEEFCAFE").is_err());
        // spaces are rejected so names stay single references
        assert!(validate_rename("Trojanized Chrome").is_err());
        // pure digits say nothing about the finding
        assert!(validate_rename("445566").is_err());
        // too-short names are rejected
        assert!(validate_rename("ab").is_err());
    }

    /// The finish-time rename nudge fires exactly once.
    #[tokio::test]
    async fn rename_nudge_is_one_shot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = scratch_store(dir.path()).await;
        // only the first take sees the nudge
        assert!(store.take_rename_nudge());
        assert!(!store.take_rename_nudge());
    }

    /// The finish-time script-promotion nudge fires exactly once.
    #[tokio::test]
    async fn promote_nudge_is_one_shot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = scratch_store(dir.path()).await;
        // only the first take sees the nudge
        assert!(store.take_promote_nudge());
        assert!(!store.take_promote_nudge());
    }

    /// The finish-time memory-review nudge fires exactly once.
    #[tokio::test]
    async fn memory_nudge_is_one_shot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = scratch_store(dir.path()).await;
        // only the first take sees the nudge
        assert!(store.take_memory_nudge());
        assert!(!store.take_memory_nudge());
    }

    /// Display names map onto file-safe record names.
    #[test]
    fn file_safe_names() {
        assert_eq!(
            file_safe("ReverseEngineer(Europa)"),
            "ReverseEngineer-Europa"
        );
        assert_eq!(file_safe("Swarm(Io) (2)"), "Swarm-Io-2");
        assert_eq!(file_safe("plain_name"), "plain_name");
    }

    /// A chat history round-trips through chat.json, and a missing file is None.
    #[tokio::test]
    async fn chat_history_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = scratch_store(dir.path()).await;
        // a store with no chat.json yet loads as None, not an error
        assert!(store.load_chat().await.expect("load").is_none());
        // save a history with settled and pending halves
        let chat = ChatHistory {
            version: crate::thorpal::resume::CHAT_HISTORY_VERSION,
            conversation_id: "main".to_owned(),
            messages: vec![Message::user("hello"), Message::assistant("hi")],
            pending: vec![Message::user("in flight")],
        };
        store.save_chat(&chat).await.expect("save chat");
        // the saved history loads back with both halves intact
        let loaded = store
            .load_chat()
            .await
            .expect("load")
            .expect("chat.json exists");
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.pending.len(), 1);
    }

    /// Transcript items append in order and a torn final line is skipped.
    #[tokio::test]
    async fn transcript_appends_and_skips_torn_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = scratch_store(dir.path()).await;
        // a store with no transcript yet loads as empty, not an error
        assert!(store.load_transcript().await.expect("load").is_empty());
        // append two stamped lines in a known order
        store
            .append_transcript(&TranscriptLine::stamped(TranscriptItem::User {
                text: "first".to_owned(),
                scope: None,
            }))
            .await
            .expect("append");
        store
            .append_transcript(&TranscriptLine::stamped(TranscriptItem::Assistant {
                text: "second".to_owned(),
                scope: None,
            }))
            .await
            .expect("append");
        // sneak in a timestamp-less line as an older build would have written
        let path = store.dir().await.join(TRANSCRIPT_FILE);
        let mut encoded = tokio::fs::read_to_string(&path).await.expect("read");
        encoded.push_str("{\"kind\":\"user\",\"text\":\"legacy\"}\n");
        // tear the file's final line as a crash mid-append would
        encoded.push_str("{\"kind\":\"user\",\"tex");
        tokio::fs::write(&path, encoded).await.expect("write");
        // the intact lines load in order and the torn line is skipped
        let lines = store.load_transcript().await.expect("load");
        assert_eq!(lines.len(), 3);
        assert!(matches!(&lines[0].item, TranscriptItem::User { text, .. } if text == "first"));
        assert!(lines[0].ts.is_some());
        assert!(
            matches!(&lines[1].item, TranscriptItem::Assistant { text, .. } if text == "second")
        );
        // the older build's line parses with no timestamp rather than erroring
        assert!(matches!(&lines[2].item, TranscriptItem::User { text, .. } if text == "legacy"));
        assert!(lines[2].ts.is_none());
    }

    /// Only unfinished and crash-class records render re-queue lines
    #[test]
    fn dead_delegation_lines_pick_crash_class_only() {
        // an unfinished record: the process died out from under it
        let killed = AgentRecord::started("RE(Io)", "RE", "goal", "trace the  loader\nchain", None);
        // a provider-fault death with a salvaged handoff
        let mut faulted =
            AgentRecord::started("Dev(Europa)", "Dev", "goal", "decode the config", None);
        faulted.finish_err(
            "401",
            Vec::new(),
            Some("got to  stage 2".to_owned()),
            "other",
        );
        // an exhaustion death: already handed its salvage back in-run
        let mut capped = AgentRecord::started("RE(Ganymede)", "RE", "goal", "sweep strings", None);
        capped.finish_err("turns", Vec::new(), None, "max_turns");
        // a success: nothing to re-queue
        let mut done = AgentRecord::started("RE(Callisto)", "RE", "goal", "done task", None);
        done.finish_ok("answer", Vec::new());
        // a legacy failed record with no class stays excluded (conservative)
        let mut legacy = AgentRecord::started("RE(Elara)", "RE", "goal", "old task", None);
        legacy.finish_err("died", Vec::new(), None, "other");
        legacy.class = None;
        let lines = dead_delegation_lines(&[killed, faulted, capped, done, legacy]);
        assert_eq!(lines.len(), 2, "{lines:?}");
        // whitespace flattens onto one line; the handoff rides along bounded
        assert_eq!(lines[0], "- RE (RE(Io)): trace the loader chain");
        assert_eq!(
            lines[1],
            "- Dev (Dev(Europa)): decode the config [salvaged handoff: got to stage 2]"
        );
    }

    /// Builds an assistant message holding one unanswered tool call
    ///
    /// # Arguments
    ///
    /// * `id` - The tool call id the message dangles
    /// * `tool` - The called tool's name
    fn frozen_call(id: &str, tool: &str) -> Message {
        Message::Assistant {
            id: None,
            content: vec![rig::message::AssistantContent::ToolCall(
                rig::message::ToolCall {
                    id: rig::message::ToolCallId::new_or_mint(id),
                    provider: None,
                    function: rig::message::ToolFunction {
                        name: tool.to_owned(),
                        arguments: serde_json::json!({}),
                    },
                    signature: None,
                    additional_params: None,
                },
            )],
        }
    }

    /// The revival prune drops handled chains and keeps unrelated records
    #[test]
    fn prune_revived_descendants_drops_handled_chains() {
        // the revived parent's transcript froze awaiting its nested call-9,
        // so the revival re-issues call-9 -- the seed of the prune
        let re_issued: std::collections::HashSet<String> =
            ["call-9".to_owned()].into_iter().collect();
        // the child (stamped with call-9) froze awaiting grandchild call-10
        let mut child = AgentRecord::started("Dev(Europa)", "Dev", "g", "child task", None);
        child.parent_call = Some("call-9".to_owned());
        child.messages = vec![frozen_call("call-10", "developer")];
        // the grandchild is stamped with the child's frozen call
        let mut grandchild = AgentRecord::started("Dev(Elara)", "Dev", "g", "grand task", None);
        grandchild.parent_call = Some("call-10".to_owned());
        // a finished child under a re-issued call is replayed context, not a
        // revival target: the prune must leave it for the nudge policy
        let mut done = AgentRecord::started("Dev(Io)", "Dev", "g", "done task", None);
        done.parent_call = Some("call-9".to_owned());
        done.finish_err("401", Vec::new(), None, "other");
        // an unrelated never-finished record must stay a nudge candidate
        let unrelated = AgentRecord::started("RE(Callisto)", "RE", "g", "other task", None);
        let mut leftovers = vec![child, grandchild, done, unrelated];
        prune_revived_descendants(&mut leftovers, re_issued);
        // the never-finished chain is pruned transitively; the finished
        // crash-class child and the unrelated record survive
        assert_eq!(leftovers.len(), 2, "{leftovers:?}");
        assert_eq!(leftovers[0].name, "Dev(Io)");
        assert_eq!(leftovers[1].name, "RE(Callisto)");
    }

    /// The agents loader is lenient and returns records newest first
    #[tokio::test]
    async fn load_agents_is_lenient_and_newest_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = scratch_store(dir.path()).await;
        // save two records (the second started later by construction order)
        let first = AgentRecord::started("RE(Io)", "RE", "g", "task one", None);
        store.save_agent(&first).await.expect("save first");
        let mut second = AgentRecord::started("Dev(Europa)", "Dev", "g", "task two", None);
        second.finish_err("boom", Vec::new(), None, "other");
        store.save_agent(&second).await.expect("save second");
        // drop a garbage file in the agents dir; the loader must skip it
        let agents = store.dir().await.join(AGENTS_DIR);
        tokio::fs::write(agents.join("junk.json"), b"{not json")
            .await
            .expect("write junk");
        let records = store.load_agents().await.expect("load");
        assert_eq!(records.len(), 2, "junk must be skipped");
        // newest first: the later-started record leads for resume matching
        assert_eq!(records[0].name, "Dev(Europa)");
        assert_eq!(records[1].name, "RE(Io)");
        // the failure class round-trips for the re-queue filter
        assert_eq!(records[0].class.as_deref(), Some("other"));
    }
}
