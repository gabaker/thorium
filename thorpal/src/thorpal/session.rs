//! The reusable orchestration entry point shared by library callers.
//!
//! [`Session`] wraps a fully-built [`ThorPal`] and sequences the phases a run
//! goes through -- optional detector seeding, the question-driven agent loop, and
//! the end-of-run summaries plus Ghidra shutdown -- behind one small API. A
//! consumer such as `thorctl` builds a [`SessionConfig`], calls
//! [`Session::build`] (or the all-in-one [`Session::execute`]), and prints the
//! returned [`RunOutcome`], rather than re-implementing the orchestration.
//! Every session saves into a [`SessionStore`] under the sessions root
//! (`~/.thorium/sessions/<name>/` by default) as it progresses, and a saved
//! session is picked back up through [`Session::restore`].

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use rig::completion::Message;
use rig::tool::builtin::ThinkTool;

use super::agents::Workflow;
use super::events::ThorPalEventSender;
use super::memory::{
    RestoredConversation, dangling_calls, dangling_calls_joined, repair_interrupted_tail,
};
use super::resume::{ChatHistory, ForumSnapshot, SessionSnapshot, UsageTotals};
use super::store::library::{ScriptLibrary, ScriptMeta};
use super::store::memories::MemoryLibrary;
use super::store::redact::LibraryRedactor;
use super::store::{ExtraSampleMeta, SessionStore};
use super::tools::forum::ForumState;
use super::tools::ghidra::GhidraSettings;
use super::{AiProviderConfig, ThorPal};

/// The configuration a caller supplies to build and run a [`Session`].
///
/// This bundles everything a run needs that is not derivable from the code: the
/// explicit LLM provider config (which provider to talk to, how to authenticate
/// with it, and the model), the resolved Ghidra paths, the workflow of
/// sub-agents to attach, and a handful of optional knobs.
pub struct SessionConfig {
    /// The explicit LLM provider config every agent's client is built from.
    pub provider: AiProviderConfig,
    /// The resolved Ghidra paths this run analyzes with.
    pub ghidra: GhidraSettings,
    /// The sub-agents to attach to the main agent.
    pub workflow: Workflow,
    /// Whether to pre-seed the forum with the static detectors' findings.
    pub seed_detectors: bool,
    /// Whether detector seeding also derives targeted open questions from the
    /// strongest findings (only consulted when `seed_detectors` is on).
    pub seed_questions: bool,
    /// Extra preamble appended to the main agent's base forum instructions.
    pub preamble: Option<String>,
    /// The main agent loop's turn-cap override, or `None` for the default.
    pub main_max_turns: Option<usize>,
    /// The operator's explicit session name, or `None` to auto-generate an
    /// adjective-animal name (which the model may later rename descriptively).
    pub session_name: Option<String>,
    /// The root directory sessions are stored under, or `None` for the default
    /// `~/.thorium/sessions`.
    pub sessions_root: Option<PathBuf>,
    /// The directory the persistent cross-session script library lives in, or
    /// `None` for the default `~/.thorium/scripts`.
    pub scripts_dir: Option<PathBuf>,
    /// A YARA rules file or directory the seed phase scans the sample with,
    /// or `None` to skip the scan entirely.
    pub yara_rules: Option<PathBuf>,
    /// Whether this run analyzes an anonymized staged copy of the binary.
    pub anonymized: bool,
    /// Operator-supplied deny tokens scrubbed out of every model-visible
    /// library surface (family names and aliases, one per entry).
    ///
    /// The model never sees the list itself -- only its effects -- and the
    /// list is never persisted into the session store (family tokens at rest
    /// would defeat anonymize-at-rest), so a resume must re-supply it.
    pub redact_tokens: Vec<String>,
    /// The directory the persistent cross-run memory library lives in, or
    /// `None` for the default `~/.thorium/memories`.
    pub memories_dir: Option<PathBuf>,
    /// The name and aliases of the family under analysis, whose `family-intel`
    /// memories are hidden entirely from every model-visible memory surface.
    ///
    /// Like `redact_tokens`, the list is eval ground truth: the model never
    /// sees it and it is never persisted into the session store, so a resume
    /// must re-supply it. The tokens also join the deny-token scrub set.
    pub blind_families: Vec<String>,
    /// The operator-side record of a multi-sample session's extra samples
    ///
    /// The paths the session actually analyzes also live in
    /// `ghidra.extra_binaries` (which drives the bootstrap import and the per
    /// -sample seeding); this list mirrors them into `meta.json`. The two
    /// fields are set together by the CLI funnel and must stay in step.
    pub extra_samples: Vec<ExtraSampleMeta>,
    /// An optional event sink for observing the run, or `None` to emit nothing.
    ///
    /// When set, it is installed on the builder so the main agent and every
    /// sub-agent mirror their activity onto it; when left `None` -- the default
    /// -- the run emits nothing.
    pub events: Option<ThorPalEventSender>,
    /// An optional credential refresher run before every auth-pause probe, or
    /// `None` to reconnect with the config this run was built with (which
    /// still covers self-refreshing credential chains like AWS profiles).
    pub credential_refresher: Option<crate::thorpal::gate::CredentialRefresher>,
    /// An optional resolver minting the built-in Thorium MCP server's
    /// credentials, or `None` when this consumer cannot reach a Thorium API.
    ///
    /// Invoked lazily -- only when the workflow enables `thorium_mcp` and an
    /// agent references the built-in server. The resolved token is held only
    /// in memory and never persisted, so a resume must re-supply the resolver.
    pub thorium_mcp: Option<crate::thorpal::mcp::ThoriumMcpResolver>,
}

impl SessionConfig {
    /// Creates a config with the required inputs and sensible defaults
    ///
    /// Detector seeding (and its derived questions) is on by default, no extra
    /// preamble is added, the session is auto-named, and sessions land under
    /// the default root. Callers tweak the remaining fields directly before
    /// building a [`Session`].
    ///
    /// # Arguments
    ///
    /// * `provider` - The explicit LLM provider config to build clients from
    /// * `ghidra` - The resolved Ghidra paths this run analyzes with
    /// * `workflow` - The sub-agents to attach to the main agent
    pub fn new(provider: AiProviderConfig, ghidra: GhidraSettings, workflow: Workflow) -> Self {
        // start from the historical defaults, leaving the rest for the caller
        SessionConfig {
            provider,
            ghidra,
            workflow,
            seed_detectors: true,
            seed_questions: true,
            preamble: None,
            main_max_turns: None,
            session_name: None,
            sessions_root: None,
            scripts_dir: None,
            yara_rules: None,
            anonymized: false,
            redact_tokens: Vec::new(),
            memories_dir: None,
            blind_families: Vec::new(),
            extra_samples: Vec::new(),
            events: None,
            credential_refresher: None,
            thorium_mcp: None,
        }
    }
}

/// Opens the persistent script library at a configured or default root.
///
/// # Arguments
///
/// * `scripts_dir` - The configured library directory, or `None` for the
///   default `~/.thorium/scripts`
async fn open_library(scripts_dir: Option<PathBuf>) -> Result<Arc<ScriptLibrary>, anyhow::Error> {
    // fall back to the shared default root when no directory was configured
    let root = match scripts_dir {
        Some(root) => root,
        None => ScriptLibrary::default_root()?,
    };
    ScriptLibrary::open(root).await
}

/// Opens the persistent memory library at a configured or default root.
///
/// # Arguments
///
/// * `memories_dir` - The configured library directory, or `None` for the
///   default `~/.thorium/memories`
async fn open_memories(memories_dir: Option<PathBuf>) -> Result<Arc<MemoryLibrary>, anyhow::Error> {
    // fall back to the shared default root when no directory was configured
    let root = match memories_dir {
        Some(root) => root,
        None => MemoryLibrary::default_root()?,
    };
    MemoryLibrary::open(root).await
}

/// The result of a completed [`Session`] run.
///
/// Carries the main agent's final answer alongside the rendered forum, so a
/// caller can print both without reaching back into the [`Session`].
pub struct RunOutcome {
    /// The main agent's final assistant response, markdown-formatted.
    pub response: String,
    /// The whole forum rendered for display, every post with its comment thread.
    pub forum_rendered: String,
}

/// A built ThorPal run, ready to seed, drive, and finish.
///
/// Holds the composed [`ThorPal`] plus the config flags the orchestration phases
/// consult (whether to seed detectors) and the session's on-disk store. All of
/// the shared per-instance state lives on the wrapped `ThorPal`, so two
/// `Session`s never share a forum, Ghidra connection, or ledger.
pub struct Session {
    /// The composed ThorPal instance every phase drives.
    thorpal: ThorPal,
    /// The on-disk store this session saves into as it progresses.
    store: Arc<SessionStore>,
    /// Whether the seeding phase should run the static detectors.
    seed_detectors: bool,
    /// Whether detector seeding also derives targeted question posts.
    seed_questions: bool,
}

impl Session {
    /// Builds a fresh session, claiming a new session directory for it
    ///
    /// Claims a session directory under the sessions root (named by the
    /// operator via `session_name`, or auto-generated), then composes the main
    /// agent and sub-agents around it. Nothing touches Ghidra yet -- the
    /// connection opens lazily on the first tool call during seeding or the run.
    ///
    /// # Arguments
    ///
    /// * `config` - The fully-populated run configuration
    #[tracing::instrument(name = "Session::build", skip_all, err(Debug))]
    pub async fn build(mut config: SessionConfig) -> Result<Self, anyhow::Error> {
        // resolve the sessions root, defaulting under the user's ~/.thorium
        let root = match config.sessions_root.take() {
            Some(root) => root,
            None => SessionStore::default_root()?,
        };
        // claim this session's directory, generating a name when none was given
        let store = SessionStore::create(
            root,
            config.session_name.take(),
            config.ghidra.binary.clone(),
            config.ghidra.project_name.clone(),
            config.anonymized,
            std::mem::take(&mut config.extra_samples),
        )
        .await?;
        // compose the run around the freshly claimed store
        Self::build_with_store(config, store).await
    }

    /// Composes a session's agents around an already-claimed session store
    ///
    /// Shared by [`Session::build`] (which claims a fresh store) and
    /// [`Session::restore`] (which reopens a saved one): the workflow's
    /// sub-agents plus the built-in think tool are attached and the caller's
    /// extra preamble (if any) is appended.
    ///
    /// # Arguments
    ///
    /// * `config` - The fully-populated run configuration
    /// * `store` - The session store this run saves into
    async fn build_with_store(
        config: SessionConfig,
        store: Arc<SessionStore>,
    ) -> Result<Self, anyhow::Error> {
        // start a builder seeded with this run's provider config and Ghidra
        // paths, saving into the session store as the run progresses
        let mut builder =
            ThorPal::builder(config.provider, config.ghidra).session_store(Arc::clone(&store));
        // open the persistent script library, defaulting under ~/.thorium; a
        // failure (e.g. no resolvable home dir) degrades to a library-less run
        // rather than failing the whole session
        match open_library(config.scripts_dir).await {
            Ok(library) => builder = builder.script_library(library),
            Err(err) => {
                tracing::warn!(
                    ?err,
                    "failed to open the script library; running without it"
                );
            }
        }
        // open the persistent memory library too, with the same degrade-on-
        // failure policy: a run without one simply has no cross-run memory
        match open_memories(config.memories_dir).await {
            Ok(memories) => builder = builder.memory_library(memories),
            Err(err) => {
                tracing::warn!(
                    ?err,
                    "failed to open the memory library; running without it"
                );
            }
        }
        // install the library redactor when there is anything for one to do:
        // a blinded run neutralizes every library script and memory name, a
        // deny list scrubs family tokens out of library free text, and a
        // blind-family list hides that family's intel memories entirely
        if let Some(redactor) = LibraryRedactor::new(
            config.anonymized,
            config.redact_tokens,
            config.blind_families,
        ) {
            builder = builder.library_redactor(redactor);
        }
        // point the seed-time YARA scan at the configured rules, when set
        if let Some(rules) = config.yara_rules {
            builder = builder.yara_rules(rules);
        }
        // append the caller's extra preamble after the base forum instructions
        // raise the main loop's turn cap when the run configures one
        if let Some(turns) = config.main_max_turns {
            builder = builder.main_max_turns(turns);
        }
        if let Some(preamble) = config.preamble {
            builder = builder.preamble(preamble);
        }
        // install the caller's event sink when supplied so the run's activity is
        // mirrored out; without one the run emits nothing
        if let Some(events) = config.events {
            builder = builder.events(events);
        }
        // install the caller's credential refresher so an auth pause can pick
        // up a replacement token without restarting the session
        if let Some(refresher) = config.credential_refresher {
            builder = builder.credential_refresher(refresher);
        }
        // install the caller's Thorium MCP resolver so a workflow enabling the
        // built-in server can mint credentials at connect time
        if let Some(resolver) = config.thorium_mcp {
            builder = builder.thorium_mcp_resolver(resolver);
        }
        // attach the workflow's sub-agents and the built-in think tool, then build
        let thorpal = builder
            .workflow(config.workflow)
            .tool(ThinkTool)
            .build()
            .await?;
        // write the initial snapshot immediately so even a run that crashes
        // before its first turn settles leaves a resumable session behind
        thorpal.autosave_snapshot().await;
        Ok(Session {
            thorpal,
            store,
            seed_detectors: config.seed_detectors,
            seed_questions: config.seed_questions,
        })
    }

    /// The name of this session's on-disk store
    ///
    /// A consumer displays this so the operator knows what to pass to
    /// `--resume` later. The name can change mid-run when the model renames an
    /// auto-named session, so callers should re-read it rather than cache it.
    pub async fn session_name(&self) -> String {
        // read the current name off the shared store
        self.store.name().await
    }

    /// The model id this session's provider talks to
    ///
    /// A resumed session keeps its saved model, so a consumer labeling cost or
    /// usage output reads it from here rather than the live config.
    pub fn model(&self) -> &str {
        // defer to the wrapped instance's provider config
        self.thorpal.model()
    }

    /// A controller for observing and steering the session's pause state
    ///
    /// A consumer watches [`crate::PauseStatus`] for its banner, calls
    /// `retry_now` after the operator fixes credentials, or `abort` to make
    /// the parked requests surface their errors instead of waiting.
    pub fn pause_controller(&self) -> crate::thorpal::gate::PauseController {
        // defer to the wrapped instance's shared gate
        self.thorpal.pause_controller()
    }

    /// A snapshot of the per-agent token usage recorded so far
    ///
    /// Sub-agents run as tool calls nested inside the main agent's loop but
    /// report their usage separately, so summing the per-agent totals yields a
    /// true grand total for the run without double-counting. A consumer prices
    /// these totals (e.g. for a cost summary) or displays the breakdown.
    pub async fn usage(&self) -> BTreeMap<String, UsageTotals> {
        // take a persistable copy of the shared ledger's tallies
        self.thorpal.usage.snapshot().await
    }

    /// Runs the optional detector-seeding phase before the agent loop
    ///
    /// When detector seeding is enabled, this runs the static detectors and turns
    /// their findings into forum posts — plus, unless derived questions are
    /// disabled, a bounded set of targeted open questions — so the agent opens
    /// with concrete points of interest. Seeding is best-effort enrichment: a
    /// failure is logged and swallowed rather than failing the run. When
    /// disabled, this is a no-op.
    #[tracing::instrument(name = "Session::seed", skip_all)]
    pub async fn seed(&self) {
        // honor the operator's opt-out: a disabled seed leaves a cold forum
        if !self.seed_detectors {
            tracing::info!("detector seeding disabled; forum left unseeded");
            return;
        }
        // seed the detectors' findings, logging and continuing past any failure so
        // the agent loop still runs against whatever (possibly empty) forum resulted
        if let Err(err) = self.thorpal.seed_detectors(self.seed_questions).await {
            tracing::warn!(
                ?err,
                "detector seeding failed; continuing without seeded posts"
            );
        }
    }

    /// Plants a human operator's own question on the forum before the run
    ///
    /// The question lands as an open question post the main agent must resolve (or
    /// block) before it can finish, so a caller can steer the run toward a specific
    /// concern. This just forwards to [`ThorPal::add_question`].
    ///
    /// # Arguments
    ///
    /// * `title` - A short human-readable title for the question
    /// * `body` - The full text of the question to answer
    #[tracing::instrument(name = "Session::add_question", skip_all, err(Debug))]
    pub async fn add_question(
        &self,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Result<(), anyhow::Error> {
        // forward to the wrapped ThorPal's forum
        self.thorpal.add_question(title, body).await
    }

    /// Returns a shared handle to this session's lazily-opened Ghidra connection
    ///
    /// A consumer holds this to make its own best-effort Ghidra queries (e.g.
    /// call-graph enrichment for a visualization) alongside the agents' tool
    /// calls. The handle stays valid after the session is moved into a worker
    /// task.
    pub fn ghidra(&self) -> Arc<crate::thorpal::tools::ghidra::GhidraSession> {
        // share the wrapped instance's lazily-opened Ghidra connection
        self.thorpal.ghidra()
    }

    /// Returns a handle to this session's shared forum
    ///
    /// A consumer (e.g. a TUI) holds this to show a live, structured view of the
    /// discussion and to add a human operator's comments or status changes. The
    /// handle stays valid after the session is moved into a worker task.
    pub fn forum(&self) -> Arc<ForumState> {
        // forward to the wrapped ThorPal's forum
        self.thorpal.forum()
    }

    /// Returns a handle to this session's live analyst link
    ///
    /// An interactive consumer (e.g. a TUI) calls `attach` on it before the
    /// run starts so `ask_analyst` waits on the human, answers pending asks
    /// through it, and `detach`es on teardown so no in-flight turn can wedge.
    /// The handle stays valid after the session is moved into a worker task.
    pub fn analyst(&self) -> Arc<crate::thorpal::analyst::AnalystLink> {
        // forward to the wrapped ThorPal's analyst link
        self.thorpal.analyst()
    }

    /// Drives the agent loop and returns the final answer plus the rendered forum
    ///
    /// This runs the question-driven control loop to completion, then renders the
    /// forum so the caller can display both the answer and the discussion that
    /// produced it. It does not shut Ghidra down -- call [`Session::finish`] for
    /// the end-of-run summaries and shutdown.
    ///
    /// # Arguments
    ///
    /// * `prompt` - The opening prompt for the run's first iteration
    #[tracing::instrument(name = "Session::run", skip_all, err(Debug))]
    pub async fn run(&self, prompt: impl Into<String>) -> Result<RunOutcome, anyhow::Error> {
        // drive the agent until the forum's questions settle and it finishes
        let response = self.thorpal.run(prompt).await?;
        // render the whole forum so the caller can show the discussion behind the answer
        let forum_rendered = self.thorpal.render_forum().await;
        Ok(RunOutcome {
            response,
            forum_rendered,
        })
    }

    /// Drives a restored run's agent loop with no new opening prompt
    ///
    /// The continuation of a crashed or interrupted headless run: the loop
    /// opens on the follow-up nudge (the forum's still-open questions) instead
    /// of a user message, then behaves exactly like [`Session::run`].
    #[tracing::instrument(name = "Session::run_resumed", skip_all, err(Debug))]
    pub async fn run_resumed(&self) -> Result<RunOutcome, anyhow::Error> {
        // drive the agent over the restored forum until it settles
        let response = self.thorpal.run_resumed().await?;
        // render the whole forum so the caller can show the discussion behind the answer
        let forum_rendered = self.thorpal.render_forum().await;
        Ok(RunOutcome {
            response,
            forum_rendered,
        })
    }

    /// The tool names of every delegation call this session attaches
    ///
    /// A consumer (e.g. the `thorctl` replay) uses this set to pair dangling
    /// delegation calls with the failure debris a resume supersedes.
    pub fn delegation_tool_names(&self) -> std::collections::BTreeSet<String> {
        // defer to the wrapped instance's workflow
        self.thorpal.delegation_tool_names()
    }

    /// Whether a restored interrupted turn is still awaiting completion
    ///
    /// A consumer checks this after a restore: the TUI asks the operator
    /// before driving [`Session::resume_interrupted`], while headless runs
    /// complete the turn automatically inside [`Session::run`] and
    /// [`Session::run_resumed`].
    pub async fn has_interrupted_turn(&self) -> bool {
        // defer to the wrapped instance's held-turn stash
        self.thorpal.has_interrupted_turn().await
    }

    /// How many dangling delegation calls the held interrupted turn carries
    ///
    /// Zero when no interrupted turn is held. A consumer shows this so the
    /// operator knows how much work resuming will restart.
    pub async fn interrupted_delegations(&self) -> usize {
        // defer to the wrapped instance's held-turn stash
        self.thorpal.interrupted_delegations().await
    }

    /// Completes a restored interrupted turn, resuming its sub-agents
    ///
    /// Re-runs the turn's dangling delegations (reusing finished records,
    /// resuming mid-flight ones from their persisted partial transcripts),
    /// then continues the turn to completion and returns its answer alongside
    /// the rendered forum. A session with no held turn returns an empty
    /// answer as a no-op.
    #[tracing::instrument(name = "Session::resume_interrupted", skip_all, err(Debug))]
    pub async fn resume_interrupted(&self) -> Result<RunOutcome, anyhow::Error> {
        // complete the held turn through the wrapped instance
        let response = self.thorpal.resume_interrupted_turn().await?;
        // render the forum as it stands so the caller can show the discussion
        let forum_rendered = self.thorpal.render_forum().await;
        Ok(RunOutcome {
            response,
            forum_rendered,
        })
    }

    /// Runs a single conversational turn against the persistent agent and forum
    ///
    /// Unlike [`Session::run`] -- which drives the multi-iteration,
    /// forum-completion control loop -- this runs exactly one main-agent turn
    /// against the same persistent conversation memory and shared forum. Each
    /// call continues the same conversation, so a caller (such as an interactive
    /// TUI) can hold a multi-turn chat where context, the forum, and Ghidra
    /// state all accumulate across turns. The whole session is saved after the
    /// turn settles, so a crash between turns resumes from here. It returns
    /// promptly with the turn's answer plus the forum rendered as it stands
    /// after the turn.
    ///
    /// # Arguments
    ///
    /// * `msg` - The user's message for this conversational turn
    #[tracing::instrument(name = "Session::chat", skip_all, err(Debug))]
    pub async fn chat(&self, msg: impl Into<String>) -> Result<RunOutcome, anyhow::Error> {
        // run a single main-agent turn against the persistent conversation and forum
        let response = self.thorpal.prompt(&msg.into()).await?;
        // render the forum as it stands so the caller can show the current discussion
        let forum_rendered = self.thorpal.render_forum().await;
        Ok(RunOutcome {
            response,
            forum_rendered,
        })
    }

    /// Forks a named side conversation off the main agent's settled history
    ///
    /// Returns `false` when a side with this name already exists (the caller
    /// just switches to it) and `true` when a fresh side was forked. This just
    /// forwards to [`ThorPal::fork_side`].
    ///
    /// # Arguments
    ///
    /// * `name` - The side conversation's name
    #[tracing::instrument(name = "Session::fork_side", skip_all, err(Debug))]
    pub async fn fork_side(&self, name: &str) -> Result<bool, anyhow::Error> {
        // forward to the wrapped ThorPal's side map
        self.thorpal.fork_side(name).await
    }

    /// Runs a single conversational turn inside a named side conversation
    ///
    /// Unlike [`Session::chat`], the turn runs against the side's own forked
    /// history -- the main conversation's memory is never touched -- while still
    /// sharing the forum, Ghidra state, and sub-agents. This just forwards to
    /// [`ThorPal::side_prompt`].
    ///
    /// # Arguments
    ///
    /// * `name` - The side conversation to run the turn in
    /// * `msg` - The user's message for this turn
    #[tracing::instrument(name = "Session::side_chat", skip_all, err(Debug))]
    pub async fn side_chat(
        &self,
        name: &str,
        msg: impl Into<String>,
    ) -> Result<String, anyhow::Error> {
        // forward to the wrapped ThorPal's side runner
        self.thorpal.side_prompt(name, &msg.into()).await
    }

    /// Generates (or revises) a short summary of a named side conversation
    ///
    /// This just forwards to [`ThorPal::summarize_side`].
    ///
    /// # Arguments
    ///
    /// * `name` - The side conversation to summarize
    /// * `prior` - The previously generated summary being revised, if any
    /// * `feedback` - The analyst's feedback steering the revision, if any
    #[tracing::instrument(name = "Session::summarize_side", skip_all, err(Debug))]
    pub async fn summarize_side(
        &self,
        name: &str,
        prior: Option<&str>,
        feedback: Option<&str>,
    ) -> Result<String, anyhow::Error> {
        // forward to the wrapped ThorPal's summarizer
        self.thorpal.summarize_side(name, prior, feedback).await
    }

    /// Accepts a side conversation's summary into the main conversation
    ///
    /// Injects the summary as a persistent user message the main agent sees on
    /// its next turn (attributed to the side it came from), then closes the
    /// side and deletes its persisted history.
    ///
    /// # Arguments
    ///
    /// * `name` - The side conversation whose summary is being accepted
    /// * `summary` - The (possibly analyst-edited) summary to bring back
    #[tracing::instrument(name = "Session::accept_side", skip_all, err(Debug))]
    pub async fn accept_side(&self, name: &str, summary: &str) -> Result<(), anyhow::Error> {
        // inject the summary into the main conversation first, so a failure
        // closing the side can never lose the accepted findings
        self.thorpal
            .inject_main_note(&format!(
                "[Summary returned from side conversation '{name}']\n\n{summary}"
            ))
            .await?;
        // the side's findings are home, so the side itself can close
        self.thorpal.close_side(name).await
    }

    /// Closes a named side conversation without returning anything
    ///
    /// This just forwards to [`ThorPal::close_side`].
    ///
    /// # Arguments
    ///
    /// * `name` - The side conversation to close
    #[tracing::instrument(name = "Session::close_side", skip_all, err(Debug))]
    pub async fn close_side(&self, name: &str) -> Result<(), anyhow::Error> {
        // forward to the wrapped ThorPal's side map
        self.thorpal.close_side(name).await
    }

    /// The names of every live side conversation, in stable sorted order
    pub async fn side_names(&self) -> Vec<String> {
        // forward to the wrapped ThorPal's side map
        self.thorpal.side_names().await
    }

    /// Generates a markdown report over this session's conversation and forum
    ///
    /// Runs a bare one-shot agent over the session's conversation transcript and
    /// rendered forum, structured by the given template (a markdown file's
    /// contents describing the report's sections) or the builtin default when
    /// `None`. The generated report is always also saved into the session store
    /// as `report.md`, silently overwriting any previous report.
    ///
    /// # Arguments
    ///
    /// * `template` - A template overriding the builtin default, if any
    #[tracing::instrument(name = "Session::generate_report", skip_all, err(Debug))]
    pub async fn generate_report(&self, template: Option<&str>) -> Result<String, anyhow::Error> {
        // generate the report with a bare one-shot agent over the session state
        let report = super::report::generate(&self.thorpal, template).await?;
        // always persist a copy into the session store, silently overwriting
        self.store.save_report(&report).await?;
        Ok(report)
    }

    /// Promotes one of this session's scripts into the persistent library
    ///
    /// The manual escape hatch behind the TUI's `/promote` command: the
    /// analyst's description doubles as the usage docs and the script is never
    /// flagged as a detector (promote through the agent's `promote_script`
    /// tool for the full knobs). Returns the success message to show the
    /// analyst.
    ///
    /// # Arguments
    ///
    /// * `name` - The bare name of the session script to promote
    /// * `description` - What the script does and when a future run should use it
    #[tracing::instrument(name = "Session::promote_script", skip_all, err(Debug))]
    pub async fn promote_script(
        &self,
        name: &str,
        description: &str,
    ) -> Result<String, anyhow::Error> {
        // a run without a configured library has nowhere to promote into
        let library = self
            .thorpal
            .script_library()
            .ok_or_else(|| anyhow::anyhow!("no persistent script library is configured"))?;
        // a neutral-id name (what blinded runs render library names as)
        // publishes over the real stem it aliases
        let publish_as = match self.thorpal.library_redactor() {
            Some(redactor) => redactor
                .resolve_alias(&library, name)
                .await?
                .unwrap_or_else(|| name.to_owned()),
            None => name.to_owned(),
        };
        // the analyst's description doubles as the usage docs; never a detector
        let meta = ScriptMeta::promoted_now(
            description,
            description,
            false,
            self.store.name().await,
            self.store.sample_sha().unwrap_or_default(),
        );
        // run the same promotion flow the agent tool uses, always overwriting
        // since the analyst asked for this script explicitly
        super::tools::scripts::promote_to_library(
            &self.store,
            &library,
            name,
            &publish_as,
            meta,
            true,
        )
        .await
        .map_err(|err| anyhow::anyhow!(err))
    }

    /// Logs the end-of-run summaries and cleanly shuts Ghidra down
    ///
    /// Emits the per-agent token-usage and function-coverage summaries (the latter
    /// while Ghidra is still connected, so the total function count resolves), then
    /// shuts the Ghidra bridge down so the analyzed database is flushed to disk and
    /// can be reused by the next run. Once the bridge is down (its locks released)
    /// the analysis folder is renamed to this session's final name, best-effort.
    #[tracing::instrument(name = "Session::finish", skip_all, err(Debug))]
    pub async fn finish(&self) -> Result<(), anyhow::Error> {
        // log where the run spent its tokens
        self.thorpal.log_usage().await;
        // log how much of the binary was examined (needs Ghidra still connected)
        self.thorpal.log_coverage().await;
        // shut the bridge down last so the flushed database survives for reuse
        self.thorpal.shutdown().await?;
        // the bridge is down and its locks are released, so the analysis folder
        // can adopt this session's final name; a rename failure must never fail
        // the finish, so it is logged and swallowed inside
        self.finalize_project_dir().await;
        Ok(())
    }

    /// Renames the analysis folder to this session's final name and persists it
    ///
    /// Best-effort: run only after the Ghidra bridge shut down cleanly, so our
    /// own locks are released. When the folder moves, the session snapshot is
    /// re-saved with the new projects dir so a later resume reopens the renamed
    /// project.
    #[tracing::instrument(name = "Session::finalize_project_dir", skip_all)]
    async fn finalize_project_dir(&self) {
        // the folder adopts the session's final (possibly model-chosen) name
        let name = self.store.name().await;
        // attempt the rename against the settings this run analyzed with
        match super::tools::ghidra::finalize_analysis_dir(self.thorpal.ghidra_settings(), &name) {
            // the folder moved: persist the new path so resume finds it; the
            // snapshot is patched directly because this is the session's last
            // act (no later autosave can overwrite it with the stale path)
            Ok(Some(new_dir)) => {
                tracing::info!(
                    ?new_dir,
                    "renamed the Ghidra analysis folder to the session name"
                );
                // capture the current state and swap in the renamed folder
                let mut snapshot = self.thorpal.snapshot().await;
                snapshot.ghidra.projects_dir = new_dir;
                // persist the patched snapshot so a resume reopens the project
                if let Err(err) = self.store.save_session(&snapshot).await {
                    tracing::warn!(
                        ?err,
                        "failed to persist the renamed analysis folder; a resume \
                         will reimport instead of reopening the renamed project"
                    );
                }
            }
            // nothing to do (already named, never created, locked, or taken)
            Ok(None) => (),
            // leave the provisional folder in place on any rename failure
            Err(err) => {
                tracing::warn!(
                    ?err,
                    "failed to rename the analysis folder; keeping its provisional name"
                );
            }
        }
    }

    /// Builds, seeds, runs, summarizes, and shuts down in one call
    ///
    /// This is the simplest entry point for a caller (like `thorctl`) that just
    /// wants an answer: it performs the whole lifecycle and hands back the
    /// [`RunOutcome`]. The summaries and Ghidra shutdown run even when the agent
    /// loop errors, so a failed run still flushes its analysis and reports usage.
    ///
    /// # Arguments
    ///
    /// * `config` - The fully-populated run configuration
    /// * `prompt` - The opening prompt for the run's first iteration
    #[tracing::instrument(name = "Session::execute", skip_all, err(Debug))]
    pub async fn execute(
        config: SessionConfig,
        prompt: impl Into<String>,
    ) -> Result<RunOutcome, anyhow::Error> {
        // compose the run, then seed the forum before driving the agent
        let session = Session::build(config).await?;
        session.seed().await;
        // drive the loop, capturing the result so we can still finish on failure
        let outcome = session.run(prompt).await;
        // always emit the summaries and shut Ghidra down, even if the run errored,
        // so the analyzed database is flushed and the usage/coverage are reported
        if let Err(err) = session.finish().await {
            tracing::warn!(?err, "failed to finish the session cleanly");
        }
        outcome
    }

    /// Rebuilds a session from its opened store, restoring its saved state
    ///
    /// Reconstructs the same [`SessionConfig`] the saved run used -- the Ghidra
    /// paths, caller preamble, and workflow all come from the snapshot -- with
    /// detector seeding forced off (the saved forum already contains any seeded
    /// posts), builds the session around the reopened store, then reloads the
    /// forum and the main agent's conversation history. The snapshot's provider
    /// config was sanitized of secrets on disk, so the caller supplies a
    /// fully-credentialed `provider` from its live config instead. The rebuilt
    /// session keeps saving into the same directory, so continuing it stays
    /// crash-safe. Nothing touches Ghidra yet.
    ///
    /// # Arguments
    ///
    /// * `store` - The reopened session store to keep saving into
    /// * `snapshot` - The saved main-agent snapshot to rebuild from
    /// * `forum` - The saved forum contents to reload
    /// * `provider` - The fully-credentialed provider config to connect with
    /// * `events` - An optional event sink for observing the resumed run
    /// * `scripts_dir` - The live config's script library directory, if any
    ///   (the snapshot deliberately does not record it, so a resume always uses
    ///   the operator's current library)
    /// * `memories_dir` - The live config's memory library directory, if any
    ///   (like `scripts_dir`, deliberately never snapshotted)
    /// * `anonymized` - Whether the saved session was blinded (from its
    ///   `meta.json`), so library names keep rendering as neutral ids
    /// * `redact_tokens` - The operator's deny-token list, re-supplied per
    ///   invocation since it is never persisted into the session store
    /// * `blind_families` - The blinded family's name and aliases, re-supplied
    ///   per invocation since they are never persisted into the session store
    /// * `thorium_mcp` - The built-in Thorium MCP server's credential resolver,
    ///   re-supplied per invocation since tokens are never persisted either
    #[tracing::instrument(name = "Session::restore", skip_all, err(Debug))]
    #[allow(clippy::too_many_arguments)]
    pub async fn restore(
        store: Arc<SessionStore>,
        snapshot: SessionSnapshot,
        forum: ForumSnapshot,
        provider: AiProviderConfig,
        events: Option<ThorPalEventSender>,
        scripts_dir: Option<PathBuf>,
        memories_dir: Option<PathBuf>,
        credential_refresher: Option<crate::thorpal::gate::CredentialRefresher>,
        anonymized: bool,
        redact_tokens: Vec<String>,
        blind_families: Vec<String>,
        thorium_mcp: Option<crate::thorpal::mcp::ThoriumMcpResolver>,
    ) -> Result<Self, anyhow::Error> {
        // take the saved usage out before the config build consumes the snapshot
        let usage = snapshot.usage;
        // seed the display-name counter past the saved run's names, so this
        // resume's sub-agents never clobber the prior run's agent records or
        // merge their usage under an already-used key; the snapshot's tally is
        // only as fresh as the last settled turn, so a hard kill mid-turn
        // minted names past it -- floor the seed past every name the saved
        // records actually used
        let record_floor = match store.load_agents().await {
            Ok(records) => crate::thorpal::agents::naming::counter_floor(
                records.iter().map(|record| record.name.as_str()),
            ),
            Err(err) => {
                tracing::warn!(?err, "failed to read agent records for the name floor");
                0
            }
        };
        crate::thorpal::agents::naming::seed_counter(snapshot.name_counter.max(record_floor));
        // split the saved conversation into its settled history and -- when
        // the dead run left delegation calls dangling -- the interrupted turn
        // this resume completes for real; everything else is repaired and
        // settled exactly as before, so a quit mid-turn (even before the
        // first turn settled) keeps the work the interrupted turn paid for
        let delegation_tools = snapshot.workflow.delegation_tool_names();
        let restored = split_restored(
            store.load_chat().await?,
            snapshot.messages,
            &delegation_tools,
        );
        // remember whether a held interrupted turn will run a live revival:
        // the revival re-seeds the dead-delegation nudge itself from the
        // records its matchers leave unconsumed, so seeding here too would
        // re-queue tasks the revival already handles
        let held = matches!(
            &restored,
            crate::thorpal::memory::RestoredConversation::Interrupted { .. }
        );
        // rebuild the config the saved run used, with seeding off so a resume
        // never re-seeds the forum
        let mut config = SessionConfig::new(provider, snapshot.ghidra, snapshot.workflow);
        config.seed_detectors = false;
        config.preamble = snapshot.preamble;
        config.events = events;
        config.credential_refresher = credential_refresher;
        // the library locations come from the live config, never the snapshot
        config.scripts_dir = scripts_dir;
        config.memories_dir = memories_dir;
        // blinding state comes from the session's meta; the deny-token and
        // blind-family lists are re-supplied per invocation (they are never
        // persisted into the store)
        config.anonymized = anonymized;
        config.redact_tokens = redact_tokens;
        config.blind_families = blind_families;
        // the Thorium MCP resolver is re-supplied too: the snapshot keeps the
        // `thorium_mcp` flag but never the credentials it resolves
        config.thorium_mcp = thorium_mcp;
        // build the session with the same sub-agents and tools a fresh run uses,
        // saving into the same store the saved run used
        let session = Self::build_with_store(config, store).await?;
        // reload the forum's posts, comments, and loop scalars from the snapshot
        session.thorpal.restore_forum(forum).await;
        // seed the main agent's conversation history so the resumed run
        // continues it, holding any interrupted turn for the resume entry
        session.thorpal.restore_conversation(restored).await?;
        // seed the usage ledger so the session's token totals keep accumulating
        session.thorpal.restore_usage(usage).await;
        // reload any saved side conversations so their tabs can be rebuilt; a
        // load failure degrades to a session without them rather than failing
        match session.store.load_btws().await {
            Ok(sides) => session.thorpal.restore_sides(sides).await,
            Err(err) => {
                tracing::warn!(
                    ?err,
                    "failed to load side conversations; resuming without them"
                );
            }
        }
        // surface delegations that died unfinished (process kills and
        // provider-fault deaths) so the continue loop re-queues their tasks
        // instead of leaving a permanent FAILED scar; when a held turn is
        // about to be revived, the revival seeds this nudge itself from the
        // records its matchers do not consume, so seeding here would re-queue
        // work the revival already handles; a load failure degrades to
        // resuming without the nudge rather than failing the resume
        if !held {
            match session.store.load_agents().await {
                Ok(records) => {
                    let dead = crate::thorpal::store::dead_delegation_lines(&records);
                    if !dead.is_empty() {
                        tracing::info!(
                            dead = dead.len(),
                            "re-queueing delegations that died unfinished in the resumed run"
                        );
                        session.thorpal.restore_dead_delegations(dead);
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        ?err,
                        "failed to load agent records; resuming without re-queue"
                    );
                }
            }
        }
        // re-persist the restored state over the empty build-time autosave so a
        // resume that quits before its first turn cannot clobber the saved session
        session.thorpal.autosave_snapshot().await;
        // append the durable resume barrier: a replay hides the terminal
        // failure debris a dead run left before this line, and audit tooling
        // can count resumes mechanically; best-effort like every autosave
        let barrier = crate::thorpal::transcript::TranscriptLine::stamped(
            crate::thorpal::transcript::TranscriptItem::SessionResumed {
                interrupted: session.thorpal.has_interrupted_turn().await,
            },
        );
        if let Err(err) = session.store.append_transcript(&barrier).await {
            tracing::warn!(
                ?err,
                "failed to append the resume barrier to the transcript"
            );
        }
        Ok(session)
    }
}

/// Splits the resumed main-agent history into settled and interrupted halves
///
/// The per-message `chat.json` is authoritative whenever it recorded anything
/// at all -- settled messages, a captured partial turn, or both; only a
/// missing `chat.json` (a pre-v5 session) or one with both halves empty falls
/// back to the snapshot's embedded messages. The split then decides how the
/// resume treats a captured partial turn:
///
/// - A pending tail whose dangling calls include a **delegation** (a workflow
///   agent's goal tool or `spawn_swarm`) is an interrupted turn: the tail is
///   held raw (and stays pending on disk) so the resume can re-run those
///   delegations for real and answer the exact call ids.
/// - A settled history that itself ends with unanswered calls is a resume
///   that crashed between settling its tail and completing the turn (a shape
///   normal settling never produces): it re-enters as interrupted, and the
///   dangling results are rebuilt from the (by then finished) agent records.
///   The two halves are scanned as one stream, so a pending tail that merely
///   ANSWERS the settled calls settles normally instead of re-entering.
/// - Everything else is repaired exactly as before -- dangling calls get
///   synthetic results and the tail is closed assistant-side -- so the work
///   the interrupted turn paid for stays in the model's context.
///
/// # Arguments
///
/// * `chat` - The loaded chat history, or `None` when `chat.json` was absent
/// * `snapshot_messages` - The snapshot's embedded legacy history
/// * `delegation_tools` - The workflow's delegation tool names
fn split_restored(
    chat: Option<ChatHistory>,
    snapshot_messages: Vec<Message>,
    delegation_tools: &std::collections::BTreeSet<String>,
) -> RestoredConversation {
    // resolve which halves chat.json recorded, falling back to the snapshot's
    // embedded history (pre-v5 sessions) when it recorded nothing at all
    let (settled, pending) = match chat {
        Some(chat) if !chat.messages.is_empty() || !chat.pending.is_empty() => {
            (chat.messages, chat.pending)
        }
        _ => (snapshot_messages, Vec::new()),
    };
    // scan the two halves as one stream, so a pending tail that answers the
    // settled half's trailing calls never re-detects them as dangling
    let joint_dangling = dangling_calls_joined(&settled, &pending);
    // a dangling delegation anywhere in the join is an interrupted turn the
    // resume completes for real instead of answering synthetically
    if joint_dangling
        .iter()
        .any(|call| delegation_tools.contains(&call.tool))
    {
        tracing::info!(
            dangling = joint_dangling.len(),
            "holding an interrupted turn with dangling delegations for resume"
        );
        return RestoredConversation::Interrupted {
            settled,
            tail: pending,
            dangling: joint_dangling,
        };
    }
    // calls stranded unanswered in the SETTLED half (a resume crashed between
    // settling its tail and recording a completed turn) cannot be repaired by
    // the pending repair below; re-enter as interrupted so the exact ids get
    // answered (delegations above, everything here synthetically)
    if !joint_dangling.is_empty() {
        let settled_dangling = dangling_calls(&settled);
        let stranded = joint_dangling
            .iter()
            .any(|call| settled_dangling.iter().any(|other| other.id == call.id));
        if stranded {
            tracing::info!(
                dangling = joint_dangling.len(),
                "re-entering a resume that died between settling and completing its turn"
            );
            return RestoredConversation::Interrupted {
                settled,
                tail: pending,
                dangling: joint_dangling,
            };
        }
    }
    // everything else settles with the synthetic repair, exactly as before
    let settled_had_dangling = !dangling_calls(&settled).is_empty();
    let mut messages = settled;
    if !pending.is_empty() {
        match repair_interrupted_tail(pending.clone(), "the session ended mid-turn") {
            Some(repaired) => {
                tracing::info!(
                    salvaged = repaired.len(),
                    "replaying a repaired partial turn into the resumed history"
                );
                messages.extend(repaired);
            }
            // a tail with no assistant work usually preserves nothing -- but
            // when it is the answers to calls the settled half left dangling
            // (the join scan proved every call answered), dropping it would
            // re-strand them; keep it and close assistant-side by hand
            None if settled_had_dangling => {
                tracing::info!(
                    kept = pending.len(),
                    "keeping an assistant-less tail that answers settled calls"
                );
                messages.extend(pending);
                messages.push(Message::Assistant {
                    id: None,
                    content: vec![rig::completion::message::AssistantContent::text(
                        "[turn interrupted: the session ended mid-turn. The results \
                         above are preserved context; continue from here.]",
                    )],
                });
            }
            None => {
                tracing::warn!("skipping a captured partial turn with no assistant content")
            }
        }
    }
    RestoredConversation::Settled(messages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig::completion::message::{AssistantContent, Text, UserContent};

    use crate::thorpal::resume::CHAT_HISTORY_VERSION;

    /// Builds a chat history from its settled and pending halves
    ///
    /// # Arguments
    ///
    /// * `messages` - The settled conversation history
    /// * `pending` - The in-flight turn's captured messages
    fn chat(messages: Vec<Message>, pending: Vec<Message>) -> ChatHistory {
        ChatHistory {
            version: CHAT_HISTORY_VERSION,
            conversation_id: "main".to_owned(),
            messages,
            pending,
        }
    }

    /// Splits with no delegation tools and expects the settled outcome
    ///
    /// # Arguments
    ///
    /// * `chat` - The loaded chat history, or `None` when `chat.json` was absent
    /// * `snapshot_messages` - The snapshot's embedded legacy history
    fn settled(chat: Option<ChatHistory>, snapshot_messages: Vec<Message>) -> Vec<Message> {
        // no delegation tools means every tail settles through the repair
        match split_restored(chat, snapshot_messages, &std::collections::BTreeSet::new()) {
            RestoredConversation::Settled(messages) => messages,
            RestoredConversation::Interrupted { .. } => {
                panic!("a delegation-less split must settle")
            }
        }
    }

    /// Builds an assistant message carrying one named tool call
    ///
    /// # Arguments
    ///
    /// * `id` - The tool call id to mint the call under
    /// * `tool` - The called tool's name
    fn assistant_call(id: &str, tool: &str) -> Message {
        Message::Assistant {
            id: None,
            content: vec![AssistantContent::ToolCall(
                rig::completion::message::ToolCall {
                    id: rig::completion::message::ToolCallId::new_or_mint(id),
                    provider: None,
                    function: rig::completion::message::ToolFunction {
                        name: tool.to_owned(),
                        arguments: serde_json::json!({"goal": "g", "prompt": "p"}),
                    },
                    signature: None,
                    additional_params: None,
                },
            )],
        }
    }

    /// Collects every text block out of an assistant message
    ///
    /// # Arguments
    ///
    /// * `message` - The message to collect assistant text from
    fn assistant_text(message: &Message) -> String {
        match message {
            Message::Assistant { content, .. } => content
                .iter()
                .filter_map(|part| match part {
                    AssistantContent::Text(Text { text, .. }) => Some(text.as_str()),
                    _ => None,
                })
                .collect(),
            other => panic!("expected an assistant message, got {other:?}"),
        }
    }

    /// A first-iteration quit (no settled messages) still salvages the tail
    #[test]
    fn resume_salvages_first_iteration_pending() {
        // the guaranteed on-disk state for the whole first control-loop
        // iteration: settled messages empty, only the captured tail set
        let pending = vec![
            Message::User {
                content: vec![UserContent::text("analyze the sample")],
            },
            Message::Assistant {
                id: None,
                content: vec![AssistantContent::text("started tracing the loader")],
            },
        ];
        let resumed = settled(
            Some(chat(Vec::new(), pending)),
            vec![Message::user("legacy snapshot history")],
        );
        // the repaired tail is replayed, not discarded for the snapshot
        assert_eq!(resumed.len(), 2, "the repaired tail must be replayed");
        let text = assistant_text(resumed.last().expect("a closing message"));
        assert!(text.contains("started tracing the loader"));
        assert!(text.contains("turn interrupted"));
        // the snapshot fallback must not leak in alongside the salvage
        assert!(
            !resumed.iter().any(|message| matches!(
                message,
                Message::User { content }
                    if content.iter().any(|part| matches!(
                        part,
                        UserContent::Text(Text { text, .. }) if text.contains("legacy")
                    ))
            )),
            "the snapshot history must not mix into a chat.json resume"
        );
    }

    /// Settled chat messages are used as-is when nothing is pending
    #[test]
    fn resume_prefers_settled_chat_messages() {
        let resumed = settled(
            Some(chat(
                vec![Message::user("hello"), Message::assistant("hi")],
                Vec::new(),
            )),
            vec![Message::user("legacy snapshot history")],
        );
        // the settled history passes through untouched
        assert_eq!(resumed.len(), 2);
        assert_eq!(assistant_text(&resumed[1]), "hi");
    }

    /// A missing or fully empty chat.json falls back to the snapshot history
    #[test]
    fn resume_falls_back_to_snapshot_without_chat_history() {
        let snapshot = vec![Message::user("legacy snapshot history")];
        // a pre-v5 session has no chat.json at all
        let resumed = settled(None, snapshot.clone());
        assert_eq!(resumed.len(), 1);
        // a chat.json with both halves empty preserves the same fallback
        let resumed = settled(Some(chat(Vec::new(), Vec::new())), snapshot);
        assert_eq!(resumed.len(), 1);
    }

    /// A pending tail with no assistant work falls back to the settled half
    #[test]
    fn resume_skips_assistant_less_pending() {
        // only the user's half of a turn that never got a response
        let pending = vec![Message::User {
            content: vec![UserContent::text("analyze the sample")],
        }];
        let resumed = settled(
            Some(chat(Vec::new(), pending)),
            vec![Message::user("legacy snapshot history")],
        );
        // the unsalvageable tail is skipped, leaving the (empty) settled half
        assert!(
            resumed.is_empty(),
            "an assistant-less tail preserves nothing and must not pull in the snapshot"
        );
    }

    /// A pending tail with a dangling delegation call is held for resume
    #[test]
    fn split_holds_dangling_delegations_for_resume() {
        // the tail died right after issuing a delegation call
        let settled_history = vec![Message::user("hello"), Message::assistant("hi")];
        let pending = vec![
            Message::User {
                content: vec![UserContent::text("continue")],
            },
            assistant_call("call-1", "reverse_engineer"),
        ];
        let delegations = std::collections::BTreeSet::from(["reverse_engineer".to_owned()]);
        let restored = split_restored(
            Some(chat(settled_history.clone(), pending.clone())),
            Vec::new(),
            &delegations,
        );
        // the turn is held raw: settled untouched, tail unrepaired, call listed
        match restored {
            RestoredConversation::Interrupted {
                settled,
                tail,
                dangling,
            } => {
                assert_eq!(settled, settled_history);
                assert_eq!(tail, pending);
                assert_eq!(dangling.len(), 1);
                assert_eq!(dangling[0].tool, "reverse_engineer");
                assert_eq!(dangling[0].args["prompt"], "p");
            }
            RestoredConversation::Settled(_) => panic!("a dangling delegation must hold"),
        }
    }

    /// A dangling non-delegation call settles through the synthetic repair
    #[test]
    fn split_settles_dangling_plain_tools() {
        // the tail died right after issuing an ordinary tool call
        let pending = vec![
            Message::User {
                content: vec![UserContent::text("continue")],
            },
            assistant_call("call-1", "decomp_function"),
        ];
        let delegations = std::collections::BTreeSet::from(["reverse_engineer".to_owned()]);
        let restored = split_restored(Some(chat(Vec::new(), pending)), Vec::new(), &delegations);
        // the plain call gets a synthetic result and the tail settles
        match restored {
            RestoredConversation::Settled(messages) => {
                let text = assistant_text(messages.last().expect("a closing note"));
                assert!(text.contains("turn interrupted"));
            }
            RestoredConversation::Interrupted { .. } => {
                panic!("a plain dangling call must settle")
            }
        }
    }

    /// A pending tail that answers the settled half's calls settles normally
    ///
    /// This is the shape left by a resume that settled its tail, sent the
    /// results, and died before rig recorded a completed turn with assistant
    /// content: the join scan proves every call answered, and the otherwise
    /// assistant-less tail is kept (closed by hand) rather than dropped.
    #[test]
    fn split_keeps_assistant_less_answers_to_settled_calls() {
        // the settled half ends with the delegation call; the pending half is
        // only the user-side results message answering it
        let (call_message, call_id) = {
            let message = assistant_call("call-1", "reverse_engineer");
            let id = rig::completion::message::ToolCallId::new_or_mint("call-1");
            (message, id)
        };
        let settled_history = vec![Message::user("hello"), call_message];
        let pending = vec![Message::User {
            content: vec![UserContent::tool_result_for(
                call_id,
                None,
                "reverse_engineer",
                vec![rig::completion::message::ToolResultContent::text(
                    "the answer",
                )],
            )],
        }];
        let delegations = std::collections::BTreeSet::from(["reverse_engineer".to_owned()]);
        let restored = split_restored(
            Some(chat(settled_history, pending)),
            Vec::new(),
            &delegations,
        );
        // the join proves the call answered, so the tail settles with a close
        match restored {
            RestoredConversation::Settled(messages) => {
                // the kept results message answers the settled call
                assert!(matches!(
                    &messages[2],
                    Message::User { content }
                        if content.iter().any(|part| matches!(part, UserContent::ToolResult(_)))
                ));
                // the hand-written close ends the history assistant-side
                let text = assistant_text(messages.last().expect("a closing note"));
                assert!(text.contains("turn interrupted"));
            }
            RestoredConversation::Interrupted { .. } => {
                panic!("an answered settled call must not re-enter")
            }
        }
    }

    /// A settled history ending in unanswered calls re-enters as interrupted
    ///
    /// This is the shape left by a resume that settled its tail and died
    /// before completing the turn; normal settling never produces it.
    #[test]
    fn split_reenters_settled_dangling_calls() {
        // settled history ends with the delegation call still unanswered
        let settled_history = vec![
            Message::user("hello"),
            assistant_call("call-1", "reverse_engineer"),
        ];
        let delegations = std::collections::BTreeSet::from(["reverse_engineer".to_owned()]);
        let restored = split_restored(
            Some(chat(settled_history.clone(), Vec::new())),
            Vec::new(),
            &delegations,
        );
        // the resume re-enters with an empty tail and the dangling set rebuilt
        match restored {
            RestoredConversation::Interrupted {
                settled,
                tail,
                dangling,
            } => {
                assert_eq!(settled, settled_history);
                assert!(tail.is_empty(), "an already-settled tail must not resettle");
                assert_eq!(dangling.len(), 1);
            }
            RestoredConversation::Settled(_) => panic!("settled dangling must re-enter"),
        }
    }
}
