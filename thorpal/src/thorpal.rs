//! A Thorium AI agent

use rig::agent::{ModelHandle, PromptResponse, WithBuilderTools};
use rig::client::{AgentClientExt, CompletionClient, ProviderClient};
use rig::completion::{Message, Prompt, PromptError};
use rig::memory::ConversationMemory;
use rig::providers::{anthropic, openai};
use rig::tool::builtin::ThinkTool;
use rig::tool::{Tool, ToolContext};
use rig::{Agent, AgentBuilder};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::convert::AsRef;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Semaphore;
use tracing::instrument;

use analyst::AnalystLink;
use coverage::CoverageLedger;
use events::ThorPalEventSender;
use memory::PersistingConversationMemory;
use resume::SessionSnapshot;
use store::library::ScriptLibrary;
use store::memories::MemoryLibrary;
use store::redact::LibraryRedactor;
use store::{RenameSession, SessionStore};
use tools::analyst::{AskAnalyst, FlagForReview};
use tools::constants::ConstantTracker;
use tools::forum::{
    AddTasks, CompleteTask, CreateComment, CreatePost, DEFAULT_FORUM_LINK_DEPTH, Finish,
    FollowLink, ForumContextMode, ForumContextSettings, ForumState, ListPosts, ListQuestions,
    SearchForum, SetStatus,
};
use tools::ghidra::{GhidraSession, GhidraSettings};
use tools::registry::{EnabledTools, Requirement};
use usage::UsageLedger;

pub use agents::{AgentConfig, Workflow};

mod agents;
pub mod analyst;
pub mod anonymize;
pub mod coverage;
pub mod detect;
pub mod events;
pub mod gate;
pub mod mcp;
mod memory;
mod report;
pub mod resume;
mod retry;
pub mod scrub;
pub mod seed;
pub mod session;
mod sides;
pub mod store;
pub mod tool_logs;
pub mod tools;
pub mod transcript;
pub mod usage;

/// The default max token budget for every agent's completion requests.
///
/// Anthropic rejects requests with no `max_tokens` set, and rig can only
/// infer a default for its own hardcoded Anthropic model id constants -- not
/// for LiteLLM aliases -- so every agent builder sets this explicitly.
///
/// Doubled from 8192 after an evaluation run showed nine agents (two main loops, seven
/// sub-agents in one session) died with a no-answer `finish_reason=Length`,
/// each death discarding a paid turn and burning retries or the whole run.
pub(crate) const DEFAULT_MAX_TOKENS: u64 = 16_384;

/// The max token budget for the final report's one-shot completion.
///
/// Reports truncated mid-sentence at 8192 in earlier evaluations and at
/// exactly 32,768 in a later one (a body table cut mid-row), and the report is a
/// single request at the very end of a run, so the larger ceiling costs
/// nothing until a report actually needs it. Kept under common 64k provider
/// output caps so the request can never be rejected outright.
pub(crate) const REPORT_MAX_TOKENS: u64 = 49_152;

/// The maximum number of control-loop iterations before `run` gives up.
///
/// Each iteration is a full main-agent run (itself bounded by rig's per-agent
/// turn cap). This is a safety backstop so a forum whose questions never fully
/// settle -- e.g. the agent keeps raising new ones -- cannot loop forever.
const MAX_ITERATIONS: usize = 10;

/// The stable conversation id the main agent's history is keyed by.
///
/// rig silently bypasses an agent's conversation memory unless a conversation
/// id is set (via the builder or per request), so the main agent is always
/// built with this id. It also keys the persisted history in a
/// [`SessionSnapshot`] so a resumed run reloads into the same conversation.
const MAIN_CONVERSATION_ID: &str = "main";

/// The base preamble every main ThorPal agent gets, describing the shared
/// forum. Anything passed to `ThorPalBuilder::preamble` is appended after
/// this rather than replacing it.
const MAIN_FORUM_PREAMBLE: &str = r##"You share findings with other agents
through a forum. Call list_posts to see what has already been discussed before
answering, and search_forum to find existing posts and comments about a
specific value, API, address, or topic before raising it again. Prefer many
small, targeted posts over one large post: each post should state a single
fact, question, or claim, and separate findings belong in separate posts.

Every post and comment has a kind: a "statement" (a fact, claim, or finding) or
a "question" (something that must be answered). Raise a question by creating a
post (or comment) with kind "question"; it starts with status "unsolved". Put
the answer to a question, and the evidence for it, as comment(s) under that
question, then update the question's status with set_status: "solved" once you
have an answer backed by enough evidence to accept, "needs_evidence" when you
have an answer you do not yet trust, or "blocked" when ThorPal cannot answer it
due to missing tools or information. Use create_post/create_comment for
statements when you have evidence -- set the stance argument to "supports",
"disproves", or "neutral", keeping each comment to a single point. Use
list_questions to see which questions are still open.

A question can carry tasks: completion-gate checklist items that must all be
completed before the question can be marked "solved". Attach them when a
question has concrete verification steps -- functions or addresses to check,
values to confirm -- either with create_post's tasks argument when raising the
question or with add_tasks on an existing one. Complete each task with
complete_task, citing the comment that contains the information that satisfies
it (post the evidence comment first). set_status refuses "solved" while any
task is incomplete, so confirm every task on a question is complete before
accepting a sub-agent's answer; if a task turns out not to apply, post a
comment explaining why and complete the task citing that comment.

Posts are referenced as "#post-name" and comments as "!123"; use follow_link to
pull up a reference you see mentioned in a post or comment body. Use "main" as
your author name on every post and comment you create.

You are the coordinator: you do not investigate the binary yourself, you drive
the sub-agents that do. Break the work down into concrete forum questions (kind
"question") and delegate each open question to the reverse_engineer sub-agent
(and to the researcher sub-agent when domain knowledge would help) by calling
its tool with the top-level goal and that specific question as the task. Whenever
the questions currently on the forum are not enough to fully answer the user's
request, generate new question posts to cover the gaps before delegating them.
When a sub-agent reports back, verify its answer and evidence on the forum and
set the question's status accordingly.

Swarms vs. single delegation: you can delegate in two ways.
- Call spawn_swarm to run several sub-agents at the same time when you have two
  or more open questions that are independent of each other -- none needs
  another's answer, and each targets a different function, address range, or
  behavior. For example, "what does each of these 5 unexplored functions do", or
  examining separate subsystems at once. Give each swarm task exactly one
  specific forum question so the members do not overlap or duplicate work.
- Call a sub-agent's own tool (e.g. reverse_engineer) for a single task, and
  always when a task depends on an earlier finding (e.g. recover a key, then
  decrypt strings with it), is one deep investigation of a single function or
  call chain, or follows up on a previous answer. Run dependent tasks one at a
  time, in order, feeding each one what the previous one found.
Either way, verify every sub-agent's answer on the forum before you accept it.

Do not accept a benign verdict for a tampered or anomalous binary until every
implant signal has a solved question explaining it: a non-verifying or grafted
signature, an impossible build timestamp, runtime hash-based symbol
resolution, and embedded foreign-format content each demand a question that
traces what consumes them, not a dismissal that the host looks legitimate.

Keep working until every question you can answer is "solved" (or "blocked" when
it truly cannot be answered), and until you have raised every question worth
asking about this binary. Only then call the finish tool to end the run -- it
refuses while any question is still unsolved or needs evidence."##;

/// The preamble paragraph appended when the session carries a placeholder name
/// the model is expected to replace with a descriptive one before finishing.
/// Only added when the operator did not name the session, so an explicitly
/// named session is never renamed by the model.
const RENAME_PREAMBLE: &str = "This analysis session was given a random placeholder \
name. Before you call finish, call rename_session once with a short, descriptive, \
easily spoken alphanumeric name based on your findings: the malware family if you \
identified one, or otherwise the most unique identifying behavior or IOC (e.g. \
\"TrojanizedChrome\" or \"XorLoaderBot\"). Never use an IP address, MAC address, \
hash, hex offset, or other raw indicator as the name.";

/// The preamble paragraph appended for the main agent when the run uses
/// linked-mode forum context, teaching it the link hygiene the mode depends
/// on: the references in a delegation's task text (and in the question posts
/// it writes) decide exactly what each sub-agent gets to see.
const MAIN_LINKED_CONTEXT_PREAMBLE: &str = "Sub-agents do not see the whole forum: \
each one is handed only its assigned question's thread, the posts that thread \
transitively references, and a titles-only index of every post. The references in \
the task text you delegate are what gets preloaded, so always include the \
question's #name in the task prompt of every delegation and every swarm task, and \
cite every related post (#name) and relevant evidence comment (!id) in the body of \
each question post you write -- those links decide what the sub-agent sees.";

/// The preamble section appended for the main agent when the run offers the
/// analyst-interaction tools, teaching when to ask and when to flag. Appended
/// outside `caller_preamble` so a resume re-derives it from the restored
/// workflow's `analyst_asks` instead of persisting it twice.
const MAIN_ANALYST_PREAMBLE: &str = "## Asking the analyst and flagging for review\n\n\
You have two tools for reaching the human analyst. ask_analyst poses ONE \
structured multiple-choice question and returns the analyst's answer: use it \
rarely, only at a genuine fork a human must decide -- scope or priority \
choices, environment-specific judgment (e.g. whether an artifact is expected \
in their environment), or whether to pursue an expensive avenue -- and never \
for anything evidence or your own tools can settle. Always designate the \
recommended option you would take on your own. When no analyst is attached the \
tool does not wait: it returns your recommended default and records the \
question for later review, so never call it expecting to pause an unattended \
run, and never re-ask an answered question. flag_for_review marks a finding, \
conclusion, or area as needing human review without pausing anything: use it \
for low-confidence conclusions worth human eyes, judgment calls you made \
alone, and anomalies you could not fully explain. Neither recorded questions \
nor review flags block the finish tool.";

/// The explicit LLM provider configuration every ThorPal run is built from
///
/// Each variant carries exactly the settings its provider needs, so a caller
/// (or a config file) always states which provider is in use rather than
/// ThorPal inferring one from the environment. The variant also carries the
/// model, since model ids are provider-specific.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
pub enum AiProviderConfig {
    /// The OpenAI API (or any endpoint speaking its protocol)
    OpenAi {
        /// The base url to talk to, or `None` for the official OpenAI API
        endpoint: Option<String>,
        /// The API key to authenticate with
        api_key: String,
        /// The model to talk to
        model: String,
    },
    /// A LiteLLM (OpenAI-compatible) gateway
    LiteLlm {
        /// The base url of the gateway
        endpoint: String,
        /// The API key to authenticate with, or `None` for a no-auth gateway
        api_key: Option<String>,
        /// The model alias to talk to
        model: String,
    },
    /// The Anthropic API
    Anthropic {
        /// The base url to talk to, or `None` for the official Anthropic API
        endpoint: Option<String>,
        /// The API key to authenticate with
        api_key: String,
        /// The model to talk to
        model: String,
    },
    /// AWS Bedrock, authenticating through the AWS credential chain or an
    /// explicit access key pair
    Bedrock {
        /// The AWS region to talk to Bedrock in, or `None` to use the chain's
        region: Option<String>,
        /// The AWS profile to resolve credentials from, or `None` for the
        /// default environment-driven chain (ignored when `region` is set)
        profile: Option<String>,
        /// Talk to Bedrock's Mantle (Anthropic Messages API) endpoint instead
        /// of the legacy Converse API; required for models only served there
        #[serde(default)]
        mantle: bool,
        /// An explicit AWS access key id, overriding the credential chain when
        /// paired with `secret_access_key`
        #[serde(default)]
        access_key_id: Option<String>,
        /// The secret access key for `access_key_id`
        #[serde(default)]
        secret_access_key: Option<String>,
        /// The session token accompanying the explicit access key pair, if any
        #[serde(default)]
        session_token: Option<String>,
        /// The model id to talk to
        model: String,
    },
}

impl AiProviderConfig {
    /// The model this provider config talks to
    #[must_use]
    pub fn model(&self) -> &str {
        // every variant carries its model; hand back whichever is active
        match self {
            AiProviderConfig::OpenAi { model, .. }
            | AiProviderConfig::LiteLlm { model, .. }
            | AiProviderConfig::Anthropic { model, .. }
            | AiProviderConfig::Bedrock { model, .. } => model,
        }
    }

    /// A copy of this config with every secret cleared, safe to persist
    ///
    /// Session snapshots are written to disk, so they must never carry an API
    /// key; a resumed run re-supplies the key through
    /// [`AiProviderConfig::with_api_key`]. A Bedrock config only carries a
    /// secret when an explicit access key pair was set; a resumed run falls
    /// back to the AWS credential chain since `with_api_key` cannot restore a
    /// multi-part credential.
    #[must_use]
    pub fn sanitized(&self) -> Self {
        // clear the key field on the variants that have one
        let mut sanitized = self.clone();
        match &mut sanitized {
            AiProviderConfig::OpenAi { api_key, .. }
            | AiProviderConfig::Anthropic { api_key, .. } => api_key.clear(),
            AiProviderConfig::LiteLlm { api_key, .. } => *api_key = None,
            AiProviderConfig::Bedrock {
                access_key_id,
                secret_access_key,
                session_token,
                ..
            } => {
                *access_key_id = None;
                *secret_access_key = None;
                *session_token = None;
            }
        }
        sanitized
    }

    /// Re-inject an API key into this config, e.g. when resuming a snapshot
    ///
    /// A `None` (or a key on a Bedrock config, which has no key field) leaves
    /// the config untouched.
    ///
    /// # Arguments
    ///
    /// * `key` - The API key to set, or `None` to keep the config as is
    #[must_use]
    pub fn with_api_key(mut self, key: Option<String>) -> Self {
        // only overwrite when a key was actually supplied
        if let Some(key) = key {
            match &mut self {
                AiProviderConfig::OpenAi { api_key, .. }
                | AiProviderConfig::Anthropic { api_key, .. } => *api_key = key,
                AiProviderConfig::LiteLlm { api_key, .. } => *api_key = Some(key),
                // bedrock authenticates through the AWS chain, not a key
                AiProviderConfig::Bedrock { .. } => (),
            }
        }
        self
    }

    /// Builds the backing [`AiClient`] this config describes
    ///
    /// This is async because an explicit-region Bedrock client eagerly loads
    /// the AWS config; the other providers build synchronously.
    pub async fn connect(&self) -> Result<AiClient, anyhow::Error> {
        match self {
            AiProviderConfig::OpenAi {
                endpoint, api_key, ..
            } => {
                // only override the builder's official-API default when an
                // explicit endpoint was configured
                let mut builder = openai::Client::builder().api_key(api_key);
                if let Some(endpoint) = endpoint {
                    builder = builder.base_url(endpoint);
                }
                Ok(AiClient::OpenAi(builder.build()?))
            }
            AiProviderConfig::LiteLlm {
                endpoint, api_key, ..
            } => {
                // a LiteLLM gateway speaks the OpenAI protocol; an empty key
                // still lets a gateway that does not require auth work
                let client = openai::Client::builder()
                    .base_url(endpoint)
                    .api_key(api_key.clone().unwrap_or_default())
                    .build()?;
                Ok(AiClient::OpenAi(client))
            }
            AiProviderConfig::Anthropic {
                endpoint, api_key, ..
            } => {
                // only override the builder's official-API default when an
                // explicit endpoint was configured
                let mut builder = anthropic::Client::builder().api_key(api_key);
                if let Some(endpoint) = endpoint {
                    builder = builder.base_url(endpoint);
                }
                Ok(AiClient::Anthropic(builder.build()?))
            }
            AiProviderConfig::Bedrock {
                region,
                profile,
                mantle,
                access_key_id,
                secret_access_key,
                session_token,
                ..
            } => {
                // an explicit key pair outranks the chain on both endpoints
                let credentials = access_key_id.clone().zip(secret_access_key.clone());
                if *mantle {
                    // Mantle resolves its region (URL + signing) at build time
                    let mut builder = rig::bedrock::mantle::MantleClient::builder();
                    if let Some(region) = region {
                        builder = builder.region(region);
                    }
                    if let Some(profile) = profile {
                        builder = builder.profile_name(profile);
                    }
                    if let Some((access_key_id, secret_access_key)) = credentials {
                        builder = builder.credentials(access_key_id, secret_access_key);
                        if let Some(session_token) = session_token.clone() {
                            builder = builder.session_token(session_token);
                        }
                    }
                    return Ok(AiClient::BedrockMantle(builder.build().await?));
                }
                // an explicit region builds eagerly (loading the AWS config
                // now); otherwise defer to rig-bedrock's lazy chain/profile
                // paths, which resolve the AWS config on first use
                let client = match (credentials, region, profile) {
                    (Some((access_key_id, secret_access_key)), Some(region), _) => {
                        let mut builder = rig::bedrock::client::ClientBuilder::default()
                            .region(region)
                            .credentials(access_key_id, secret_access_key);
                        if let Some(session_token) = session_token.clone() {
                            builder = builder.session_token(session_token);
                        }
                        builder.build().await
                    }
                    (Some((access_key_id, secret_access_key)), None, _) => {
                        rig::bedrock::client::Client::with_credentials(
                            access_key_id,
                            secret_access_key,
                            session_token.clone(),
                        )
                    }
                    (None, Some(region), _) => {
                        rig::bedrock::client::ClientBuilder::default()
                            .region(region)
                            .build()
                            .await
                    }
                    (None, None, Some(profile)) => {
                        rig::bedrock::client::Client::with_profile_name(profile)
                    }
                    (None, None, None) => rig::bedrock::client::Client::from_env()?,
                };
                Ok(AiClient::Bedrock(client))
            }
        }
    }

    /// Send a single prompt through this provider with no tools or sub-agents
    ///
    /// A minimal connectivity and credential check: connects the client, builds
    /// a bare agent (no tools, no preamble, no memory), and returns the model's
    /// reply. Errors keep the full provider error chain for diagnosis.
    ///
    /// # Arguments
    ///
    /// * `prompt` - The prompt to send to the model
    pub async fn check(&self, prompt: &str) -> Result<String, anyhow::Error> {
        // connect a client for this provider through its normal path
        let client = self.connect().await?;
        // a builder with no tools added yields an agent that can only reply with text
        let agent = client
            .agent(self.model())
            .max_tokens(DEFAULT_MAX_TOKENS)
            .build();
        // one bare completion round trip proves the provider is reachable
        let reply = agent.prompt(prompt).await?;
        Ok(reply)
    }
}

/// The backing LLM client: an OpenAI-compatible gateway, direct Anthropic, or AWS Bedrock
pub enum AiClient {
    /// An OpenAI-compatible client (the OpenAI API or e.g. a LiteLLM gateway)
    OpenAi(openai::Client),
    /// A direct Anthropic client
    Anthropic(anthropic::Client),
    /// An AWS Bedrock client authenticating through the AWS credential chain
    Bedrock(rig::bedrock::client::Client),
    /// An AWS Bedrock Mantle client speaking the Anthropic Messages API
    BedrockMantle(rig::bedrock::mantle::MantleClient),
}

impl AiClient {
    /// Builds an agent for this client using the given model string
    ///
    /// # Arguments
    ///
    /// * `model` - The model to talk to
    pub fn agent(&self, model: impl Into<String>) -> AgentBuilder {
        // dispatch to whichever provider client is active
        match self {
            AiClient::OpenAi(client) => client.agent(model),
            AiClient::Anthropic(client) => client.agent(model),
            AiClient::Bedrock(client) => client.agent(model),
            AiClient::BedrockMantle(client) => client.agent(model),
        }
    }

    /// Builds a type-erased completion model for one of this client's models
    ///
    /// Erases the provider-specific model so a single wrapper type covers
    /// every current and future provider; prompt caching is enabled wherever
    /// it is opt-in (Anthropic-protocol cache_control markers and Converse
    /// cachePoint blocks) since agent runs resend a large stable prefix
    /// (preamble + tools + growing history) every turn -- the OpenAI protocol
    /// providers cache automatically server-side and need no flag.
    ///
    /// # Arguments
    ///
    /// * `model` - The model to talk to
    pub(crate) fn erased_model(&self, model: impl Into<String>) -> ModelHandle {
        // dispatch to whichever provider client is active
        match self {
            AiClient::OpenAi(client) => ModelHandle::new(client.completion_model(model)),
            AiClient::Anthropic(client) => {
                ModelHandle::new(client.completion_model(model).with_prompt_caching())
            }
            AiClient::Bedrock(client) => {
                ModelHandle::new(client.completion_model(model).with_prompt_caching())
            }
            AiClient::BedrockMantle(client) => {
                ModelHandle::new(client.completion_model(model).with_prompt_caching())
            }
        }
    }
}

/// A boxed transformation applied to the main [`AgentBuilder`] at build time.
///
/// Each `ThorPalBuilder::tool` call records one of these so an arbitrary,
/// caller-supplied [`Tool`] can be added once the client-backed agent builder
/// actually exists in [`ThorPalBuilder::build`].
type ToolApplier =
    Box<dyn FnOnce(AgentBuilder<WithBuilderTools>) -> AgentBuilder<WithBuilderTools> + Send>;

/// A Thorium AI agent builder
pub struct ThorPalBuilder {
    /// The explicit provider config every agent's client is built from.
    provider: AiProviderConfig,
    /// The preamble applied to the main agent, kept around so `preamble()`
    /// can append to it rather than clobbering the base forum instructions.
    preamble: String,
    /// The caller's appended preamble on its own (without the base forum
    /// instructions), kept so it can be persisted into a session snapshot and
    /// re-applied verbatim on resume. `None` when the caller added none.
    caller_preamble: Option<String>,
    /// The resolved Ghidra paths, kept so they can be persisted into a session
    /// snapshot (the `ghidra` session below consumes them at build time).
    ghidra_settings: GhidraSettings,
    /// The shared forum state seeded into every run's tool context.
    forum: Arc<ForumState>,
    /// The live analyst link seeded into every run's tool context, through
    /// which an attached consumer answers `ask_analyst` questions.
    analyst: Arc<AnalystLink>,
    /// Whether this run offers the analyst-interaction tools (ask_analyst on
    /// the main agent, flag_for_review everywhere); adopted from the workflow.
    analyst_asks: bool,
    /// The shared Ghidra session seeded into every run's tool context.
    ghidra: Arc<GhidraSession>,
    /// The shared token-usage ledger, also handed to each sub-agent tool.
    usage: Arc<UsageLedger>,
    /// The shared function-coverage ledger, also handed to each sub-agent tool.
    coverage: Arc<CoverageLedger>,
    /// A YARA rules file or directory the seed phase scans the sample with.
    yara_rules: Option<std::path::PathBuf>,
    /// The caller-supplied tools to add to the main agent at build time.
    tools: Vec<ToolApplier>,
    /// The configs for the sub-agents to attach as agents-as-tools tools at build time.
    sub_agents: Vec<AgentConfig>,
    /// The MCP servers sub-agents may source tools from; adopted from the workflow.
    mcp_servers: BTreeMap<String, mcp::McpServerConfig>,
    /// Whether the built-in Thorium MCP server is enabled; adopted from the workflow.
    thorium_mcp: bool,
    /// The callback minting the built-in Thorium MCP server's credentials, or
    /// `None` when this consumer cannot reach a Thorium API.
    thorium_mcp_resolver: Option<mcp::ThoriumMcpResolver>,
    /// The max number of sub-agents that may be alive at once across a run.
    max_alive_sub_agents: usize,
    /// The model's context window in tokens, for sub-agent context warnings.
    context_window: u64,
    /// How much of the forum each sub-agent is handed at spawn time.
    forum_context: ForumContextMode,
    /// How many link hops linked-mode forum context follows.
    forum_link_depth: usize,
    /// An optional event sink for observing the run, or `None` to emit nothing.
    events: Option<ThorPalEventSender>,
    /// The session store this run saves into as it progresses, or `None` to
    /// disable session persistence entirely.
    store: Option<Arc<SessionStore>>,
    /// The persistent cross-session script library, or `None` to disable
    /// script promotion and library reuse entirely.
    library: Option<Arc<ScriptLibrary>>,
    /// The main loop's turn-cap override, or `None` for the default 100.
    main_max_turns: Option<usize>,
    /// The persistent cross-run memory library, or `None` to disable memory
    /// recall and saving entirely.
    memories: Option<Arc<MemoryLibrary>>,
    /// The render-time library redactor for blinded or deny-listed runs, or
    /// `None` to render library content verbatim.
    redactor: Option<Arc<LibraryRedactor>>,
    /// The caller's credential refresher run before auth-pause probes, or
    /// `None` to reconnect with the current config.
    credential_refresher: Option<gate::CredentialRefresher>,
}

/// Methods that are possible on the default no-tool builder.
impl ThorPalBuilder {
    /// Create a new thorpal builder
    ///
    /// The client is not built until [`ThorPalBuilder::build`]. The provider
    /// config states explicitly which LLM provider to talk to and carries
    /// everything needed to authenticate with it. The four shared state bundles
    /// (forum, Ghidra session, usage and coverage ledgers) are created here so
    /// this instance never touches the process-global state of another.
    ///
    /// # Arguments
    ///
    /// * `provider` - The explicit LLM provider config to build clients from
    /// * `ghidra` - The resolved Ghidra settings this instance analyzes with
    pub fn new(provider: AiProviderConfig, ghidra: GhidraSettings) -> Self {
        // keep the resolved Ghidra paths so they can be snapshotted; the session
        // below takes its own copy since it consumes the settings it is built from
        let ghidra_settings = ghidra.clone();
        // the session owns the settings; its program name seeds the coverage
        // ledger so a `program` arg naming the main program still counts as main
        let ghidra = GhidraSession::new(ghidra);
        let main_program = ghidra.main_program_name();
        // build a fresh, isolated set of shared state for this instance
        ThorPalBuilder {
            provider,
            preamble: MAIN_FORUM_PREAMBLE.to_owned(),
            caller_preamble: None,
            ghidra_settings,
            forum: Arc::new(ForumState::new()),
            analyst: Arc::new(AnalystLink::new()),
            analyst_asks: agents::workflow::default_analyst_asks(),
            ghidra,
            usage: Arc::new(UsageLedger::new()),
            coverage: Arc::new(CoverageLedger::new(main_program)),
            yara_rules: None,
            tools: Vec::new(),
            sub_agents: Vec::new(),
            mcp_servers: BTreeMap::new(),
            thorium_mcp: false,
            thorium_mcp_resolver: None,
            max_alive_sub_agents: agents::workflow::default_max_alive_sub_agents(),
            context_window: agents::workflow::default_context_window_tokens(),
            forum_context: ForumContextMode::default(),
            forum_link_depth: DEFAULT_FORUM_LINK_DEPTH,
            events: None,
            store: None,
            library: None,
            main_max_turns: None,
            memories: None,
            redactor: None,
            credential_refresher: None,
        }
    }

    /// Install a credential refresher run before every auth-pause probe
    ///
    /// A paused session asks this for a freshly-credentialed provider config
    /// (e.g. thorctl re-reads its config file and re-injects live secrets)
    /// before reconnecting and probing. Without one, the gate reconnects with
    /// the config it already holds, which still covers self-refreshing
    /// credential chains (AWS profiles/SSO).
    ///
    /// # Arguments
    ///
    /// * `refresher` - The refresher returning a fresh provider config
    pub fn credential_refresher(mut self, refresher: gate::CredentialRefresher) -> Self {
        self.credential_refresher = Some(refresher);
        self
    }

    /// Install an optional event sink for observing the run
    ///
    /// When set, the main agent and every sub-agent emit [`events::ThorPalEvent`]s
    /// (tool calls, tool results, assistant messages, usage, and the final
    /// response) onto this sender so an external consumer such as a TUI can
    /// display the run's progress. When left unset -- the default -- ThorPal emits
    /// nothing and behaves exactly as it does standalone.
    ///
    /// # Arguments
    ///
    /// * `sender` - The channel to emit run events onto
    pub fn events(mut self, sender: ThorPalEventSender) -> Self {
        // record the sink; it is cloned onto the ThorPal and its hooks in `build`
        self.events = Some(sender);
        self
    }

    /// Append to the preamble for this agent
    ///
    /// This is appended after the base forum instructions rather than
    /// replacing them, so the agent never loses its forum usage guidance.
    ///
    /// # Arguments
    ///
    /// * `preamble` - The preamble text to append
    pub fn preamble(mut self, preamble: impl AsRef<str>) -> Self {
        // append after the base instructions so the forum guidance is preserved
        self.preamble.push_str("\n\n");
        self.preamble.push_str(preamble.as_ref());
        // keep the caller's portion on its own so it can be snapshotted and
        // re-applied verbatim on resume without duplicating the base preamble
        self.caller_preamble = Some(preamble.as_ref().to_owned());
        self
    }

    /// Point the seed phase's YARA scan at an operator-supplied rules path
    ///
    /// # Arguments
    ///
    /// * `rules` - A `.yar`/`.yara` file or a directory of them
    pub fn yara_rules(mut self, rules: impl Into<std::path::PathBuf>) -> Self {
        // record the path; the seed phase scans with it when present
        self.yara_rules = Some(rules.into());
        self
    }

    /// Install the session store this run saves into as it progresses
    ///
    /// When set, the whole session is persisted incrementally: the main
    /// agent's conversation after every turn, the forum after every mutation,
    /// and each sub-agent invocation as it starts and finishes, so a crashed
    /// run can be resumed with minimal loss. Saving is best-effort: a write
    /// failure is logged and the run continues. When left unset, nothing is
    /// written to disk.
    ///
    /// # Arguments
    ///
    /// * `store` - The session store to save this run into
    pub fn session_store(mut self, store: Arc<SessionStore>) -> Self {
        // record the store; it is shared onto the ThorPal and tools in `build`
        self.store = Some(store);
        self
    }

    /// Install the persistent cross-session script library this run reuses
    ///
    /// When set, the script tools resolve names session-first with a library
    /// fallback, proven scripts can be promoted into the library for future
    /// runs, and detector-flagged library scripts run during the seeding
    /// phase. When left unset, the run behaves as if the library were empty
    /// and promotion is unavailable.
    ///
    /// # Arguments
    ///
    /// * `library` - The script library this run searches and promotes into
    /// Override the main agent loop's turn cap (default 100)
    ///
    /// An evaluation's hardest unit finished its analysis and then died at the
    /// hardcoded cap during wrap-up; long evaluations set this from the
    /// batch manifest instead of resuming for a fresh budget.
    ///
    /// # Arguments
    ///
    /// * `turns` - The main loop's maximum turns (floored at 10)
    pub fn main_max_turns(mut self, turns: usize) -> Self {
        // a sub-10 cap cannot finish even a trivial session
        self.main_max_turns = Some(turns.max(10));
        self
    }

    pub fn script_library(mut self, library: Arc<ScriptLibrary>) -> Self {
        // record the library; it is shared onto the ThorPal and tools in `build`
        self.library = Some(library);
        self
    }

    /// Install the persistent cross-run memory library this run recalls from
    ///
    /// When set, the memory tools can list, search, and read durable generic
    /// lessons saved by past runs (and save new ones), memory-enabled
    /// sub-agents get a memory index in their spawn prompts, and the finish
    /// tool nudges a one-time review of the run for lessons worth saving. When
    /// left unset, the run has no cross-run memory at all.
    ///
    /// # Arguments
    ///
    /// * `memories` - The memory library this run recalls from and saves into
    pub fn memory_library(mut self, memories: Arc<MemoryLibrary>) -> Self {
        // record the library; it is shared onto the ThorPal and tools in `build`
        self.memories = Some(memories);
        self
    }

    /// Set the render-time library redactor for a blinded or deny-listed run
    ///
    /// When set, every model-visible library surface (listings, reads,
    /// searches, run output, detector seeding) renders through it: blinded
    /// runs neutralize every library script name and the deny-token list
    /// scrubs free text. When left unset -- the default -- library content
    /// renders verbatim.
    ///
    /// # Arguments
    ///
    /// * `redactor` - The redactor this run renders library content through
    pub fn library_redactor(mut self, redactor: Arc<LibraryRedactor>) -> Self {
        // record the redactor; it is shared onto the tools in `build`
        self.redactor = Some(redactor);
        self
    }

    /// Add a tool to the main agent
    ///
    /// The tool is not attached until [`ThorPalBuilder::build`]; this records a
    /// deferred applier so the client-backed agent builder can add it once it
    /// exists.
    ///
    /// # Arguments
    ///
    /// * `tool` - The tool to add to the main agent
    pub fn tool<T: Tool + 'static>(mut self, tool: T) -> Self {
        // record a closure that adds this tool to the agent builder at build time
        self.tools.push(Box::new(move |agent| agent.tool(tool)));
        self
    }

    /// Add a tool whose name and schema are only known at runtime to the main agent
    ///
    /// Like [`ThorPalBuilder::tool`], the tool is attached at build time, in the
    /// order it was added among the other caller-supplied tools.
    ///
    /// # Arguments
    ///
    /// * `tool` - The runtime-defined tool to add to the main agent
    #[must_use]
    pub fn dynamic_tool(mut self, tool: rig::tool::DynamicTool) -> Self {
        // record a closure that adds this tool to the agent builder at build time
        self.tools
            .push(Box::new(move |agent| agent.dynamic_tool(tool)));
        self
    }

    /// Attach a single configured sub-agent as an agents-as-tools tool
    ///
    /// # Arguments
    ///
    /// * `config` - The config for the sub-agent to attach
    pub fn agent(mut self, config: AgentConfig) -> Self {
        // record the config; the sub-agent is built in `build`
        self.sub_agents.push(config);
        self
    }

    /// Attach every sub-agent in a workflow as agents-as-tools tools
    ///
    /// # Arguments
    ///
    /// * `workflow` - The workflow whose sub-agents to attach
    pub fn workflow(mut self, workflow: Workflow) -> Self {
        // record every config; the sub-agents are built in `build`
        self.sub_agents.extend(workflow.agents);
        // adopt the workflow's cap on how many sub-agents may be alive at once
        self.max_alive_sub_agents = workflow.max_alive_sub_agents;
        // adopt the workflow's forum-context mode and link-following depth
        self.forum_context = workflow.forum_context;
        self.forum_link_depth = workflow.forum_link_depth;
        // adopt the workflow's model context window for sub-agent warnings
        self.context_window = workflow.context_window_tokens;
        // adopt the workflow's analyst-interaction gate
        self.analyst_asks = workflow.analyst_asks;
        // adopt the workflow's MCP servers and built-in Thorium server flag;
        // the referenced servers are connected in `build`
        self.mcp_servers.extend(workflow.mcp_servers);
        self.thorium_mcp = workflow.thorium_mcp;
        self
    }

    /// Install a resolver minting the built-in Thorium MCP server's credentials
    ///
    /// A workflow with `thorium_mcp: true` connects to the Thorium API's MCP
    /// endpoint at build time; this callback supplies the base url and token
    /// to connect with. It is invoked lazily -- only when an agent actually
    /// references the built-in server -- so runs that never touch it stay
    /// fully offline. The resolved token is held only in memory and never
    /// persisted, so a resume resolves it again.
    ///
    /// # Arguments
    ///
    /// * `resolver` - The callback resolving the url and token to connect with
    pub fn thorium_mcp_resolver(mut self, resolver: mcp::ThoriumMcpResolver) -> Self {
        // keep the resolver for the connect pass in `build`
        self.thorium_mcp_resolver = Some(resolver);
        self
    }

    /// Set the model context window sub-agent context warnings assume
    ///
    /// Sub-agents nearing this many tokens of conversation memory are ordered
    /// to flush findings to the forum (and clamped to forum tools at the
    /// critical threshold). `0` disarms context tracking entirely. Callers
    /// attaching a workflow usually configure this in the workflow yaml
    /// instead (`context_window_tokens`).
    ///
    /// # Arguments
    ///
    /// * `tokens` - The context window in tokens
    pub fn context_window(mut self, tokens: u64) -> Self {
        // adopt the explicit window; it applies to every sub-agent budget
        self.context_window = tokens;
        self
    }

    /// Set the max number of sub-agents that may be alive at once across a run
    ///
    /// This counts single delegations and swarm members together; any extra
    /// invocations wait for a running sub-agent to finish before starting.
    /// Values below 1 are raised to 1 so delegation can never stall entirely.
    ///
    /// # Arguments
    ///
    /// * `max` - The max number of sub-agents alive at once
    pub fn max_alive_sub_agents(mut self, max: usize) -> Self {
        // never allow zero slots or every delegation would wait forever
        self.max_alive_sub_agents = max.max(1);
        self
    }

    /// Build this agent, connecting the client and materializing every tool
    ///
    /// This is where the deferred work happens: the client is built from the
    /// explicit provider config, the main agent is assembled with the forum
    /// toolset, caller tools, and sub-agent tools, and the shared state is
    /// moved onto the finished [`ThorPal`]. Async because an explicit-region
    /// Bedrock client loads the AWS config eagerly.
    pub async fn build(mut self) -> Result<ThorPal, anyhow::Error> {
        // refuse to build an agent whose enabled tools need a Ghidra install
        // when none is configured
        for config in &self.sub_agents {
            for entry in config.resolve_tools()? {
                if entry.requires == Requirement::Ghidra && !self.ghidra_settings.has_install() {
                    anyhow::bail!(
                        "agent '{}' uses the Ghidra tool '{}' but no Ghidra install path is set",
                        config.name,
                        entry.name
                    );
                }
            }
        }
        // connect every MCP server the sub-agents reference and freeze each
        // one's tool list; unreferenced servers are never connected
        let mcp_connections = mcp::McpConnections::connect(
            self.thorium_mcp,
            &self.mcp_servers,
            &self.sub_agents,
            self.thorium_mcp_resolver.as_ref(),
        )
        .await?;
        // tee the event stream through a transcript persister when a store is
        // installed: a single background task receives every event in emit
        // order, appends the displayable subset to the store's transcript, and
        // forwards each event to the caller's original sink unchanged. the tee
        // becomes this run's sender *before* it is cloned into the forum, the
        // hooks, and the sub-agent tools below, so every emit site routes
        // through it with no signature changes anywhere.
        if let Some(store) = &self.store {
            // the internal channel the persister receives the raw stream on
            let (fan_tx, mut fan_rx) = tokio::sync::mpsc::unbounded_channel();
            // the caller's original sink, forwarded to in the same order
            let downstream = self.events.take();
            // clone the store handle for the detached persister task
            let store = Arc::clone(store);
            // spawn the persister; it ends once every sender clone is dropped
            tokio::spawn(async move {
                while let Some(event) = fan_rx.recv().await {
                    // persist the displayable subset in receive (== emit) order,
                    // so the file's line order is exactly the live render order;
                    // each line is stamped so a replay can pace proportionally
                    if let Some(item) = transcript::TranscriptItem::from_event(&event) {
                        let line = transcript::TranscriptLine::stamped(item);
                        if let Err(err) = store.append_transcript(&line).await {
                            tracing::warn!(?err, "failed to append a transcript item");
                        }
                    }
                    // forward the event to the caller's sink; a dropped
                    // receiver just means no consumer is listening anymore
                    if let Some(tx) = &downstream {
                        let _ = tx.send(event);
                    }
                }
            });
            // route every emit site through the tee from here on
            self.events = Some(fan_tx);
        }
        // connect the client the explicit provider config describes and seat
        // it behind the session-wide pause gate every agent's requests ride
        let ai_client = Arc::new(self.provider.connect().await?);
        let gate = gate::ProviderGate::new(
            ai_client,
            self.provider.clone(),
            self.credential_refresher.take(),
            self.events.clone(),
        );
        // every agent in the run talks to the provider's configured model
        let model = self.provider.model().to_owned();

        // build the conversation memory ourselves and keep a clone: the built
        // agent exposes no getter for its memory, so this handle is the only way
        // to read the history back out for a snapshot or seed it on resume. The
        // clone shares the same inner state, so both see every message. The
        // persisting wrapper mirrors the history to the store's `chat.json`
        // after every change, so a quit mid-turn loses nothing durable.
        let memory = PersistingConversationMemory::new(MAIN_CONVERSATION_ID, self.store.clone());

        // keep a workflow describing the attached sub-agents so a session
        // snapshot can rebuild the same delegation tools on resume
        let workflow = Workflow {
            agents: self.sub_agents.clone(),
            max_alive_sub_agents: self.max_alive_sub_agents,
            forum_context: self.forum_context,
            forum_link_depth: self.forum_link_depth,
            context_window_tokens: self.context_window,
            analyst_asks: self.analyst_asks,
            thorium_mcp: self.thorium_mcp,
            mcp_servers: self.mcp_servers.clone(),
        };

        // append the rename guidance when this run's session still carries a
        // placeholder name the model is expected to replace; this is added
        // after the caller's preamble (and never into `caller_preamble`) so a
        // resumed session re-derives it from its own store instead
        let mut preamble = self.preamble;
        // teach the main agent linked-mode link hygiene when this run preloads
        // forum context into sub-agent prompts; appended outside
        // `caller_preamble` so a resume re-derives it from the workflow instead
        if self.forum_context == ForumContextMode::Linked {
            preamble.push_str("\n\n");
            preamble.push_str(MAIN_LINKED_CONTEXT_PREAMBLE);
        }
        // teach the ask/flag discipline only when the run offers those tools,
        // so a gated run's preamble stays byte-identical to a pre-feature one;
        // appended outside `caller_preamble` so a resume re-derives it from
        // the restored workflow instead
        if self.analyst_asks {
            preamble.push_str("\n\n");
            preamble.push_str(MAIN_ANALYST_PREAMBLE);
        }
        let needs_rename = self
            .store
            .as_ref()
            .is_some_and(|store| store.needs_rename());
        if needs_rename {
            preamble.push_str("\n\n");
            preamble.push_str(RENAME_PREAMBLE);
        }

        // start with a barebones main agent carrying the full forum toolset,
        // including the question-status and finish tools that drive the loop;
        // its completion requests retry transient provider faults in place
        let mut agent = gate
            .retrying_agent(model.clone(), "main")
            .default_max_turns(self.main_max_turns.unwrap_or(100))
            .max_tokens(DEFAULT_MAX_TOKENS)
            .preamble(&preamble)
            .tool(CreatePost)
            .tool(CreateComment)
            .tool(ListPosts)
            .tool(SearchForum)
            .tool(FollowLink)
            .tool(SetStatus)
            .tool(ListQuestions)
            .tool(AddTasks)
            .tool(CompleteTask)
            .tool(Finish)
            // give the main agent the same scratch-thinking tool the sub-agents
            // carry so its reasoning surfaces to a consumer just like theirs
            .tool(ThinkTool)
            // a conversation id is required for rig to actually use the memory --
            // without one it is silently bypassed -- so set our stable id and
            // attach the memory handle we kept a clone of above
            .conversation(MAIN_CONVERSATION_ID)
            .memory(memory.clone());

        // only an auto-named session gets the rename tool, so the model can
        // never rename a session the operator named explicitly
        if needs_rename {
            agent = agent.tool(RenameSession);
        }

        // offer the analyst-interaction tools unless this run gated them off
        // (the knob evaluation owners pin for byte-identical comparability);
        // ask_analyst is main-agent only -- sub-agents get flag_for_review
        // through their own build
        if self.analyst_asks {
            agent = agent.tool(AskAnalyst).tool(FlagForReview);
        }

        // apply each caller-supplied tool in the order it was added
        for apply in self.tools {
            agent = apply(agent);
        }

        // cap how many sub-agents may be alive at once across single
        // delegations and swarm members alike
        let alive = Arc::new(Semaphore::new(self.max_alive_sub_agents));
        // the shared bare salvage summarizer run over dead sub-agents' partial
        // transcripts; built through the retrying wrapper so a throttle at
        // death time paces the post-mortem instead of killing it too
        let post_mortem = Arc::new(
            gate.retrying_agent(model.clone(), "post_mortem")
                .max_tokens(DEFAULT_MAX_TOKENS)
                .build(),
        );
        // order the configs so every delegation callee is built before its
        // callers; this also rejects unknown references, self-references, and
        // cycles for configs that never went through Workflow::validate
        let order = agents::workflow::build_order(&self.sub_agents)?;
        // map each tool name back to its config so delegate roles resolve
        let index_of: BTreeMap<String, usize> = self
            .sub_agents
            .iter()
            .enumerate()
            .map(|(index, config)| (config.tool_name(), index))
            .collect();
        // the built agents, their enabled toolsets, and their nested pools
        let mut built: BTreeMap<String, (Arc<Agent>, EnabledTools, Arc<Semaphore>)> =
            BTreeMap::new();
        // build every sub-agent callees-first so delegate tools always exist
        for index in order {
            let config = &self.sub_agents[index];
            // resolve the toolset once for this agent's context entry
            let enabled = EnabledTools::from_entries(&config.resolve_tools()?);
            // wrap each listed callee (already built, by topological order) as
            // a goal tool drawing on the callee's own nested pool, so a parent
            // waiting on its delegate can never deadlock the global cap
            let mut delegates = Vec::with_capacity(config.agents.len());
            for name in &config.agents {
                let (callee, callee_enabled, pool) = &built[name];
                let role = self.sub_agents[index_of[name]].name.clone();
                delegates.push(agents::tool::into_goal_tool(
                    Arc::clone(callee),
                    role,
                    callee_enabled.clone(),
                    // nested invocations run under the callee's own budget
                    self.sub_agents[index_of[name]].turn_budget(self.context_window),
                    Arc::clone(&self.usage),
                    Arc::clone(&self.coverage),
                    self.events.clone(),
                    self.store.clone(),
                    Arc::clone(pool),
                    Some(Arc::clone(&post_mortem)),
                ));
            }
            // resolve this agent's MCP toolset against the frozen server lists
            let mcp_tools = mcp_connections.agent_tools(config)?;
            // build the sub-agent once so every attachment point shares it
            let sub_agent = Arc::new(config.build(
                &gate,
                model.clone(),
                delegates,
                mcp_tools,
                self.forum_context,
                self.analyst_asks,
            )?);
            // record the roster this role's instances will actually be
            // offered, now that every attachment is on the built agent
            append_tools_manifest(&self.store, &config.name, &sub_agent).await;
            // the dedicated pool nested invocations of this agent draw from
            let pool = Arc::new(Semaphore::new(config.max_nested_alive.max(1)));
            built.insert(config.tool_name(), (sub_agent, enabled, pool));
        }
        // collect the sub-agents that may also be included in a swarm
        let mut swarm_members = Vec::with_capacity(self.sub_agents.len());
        // attach each chosen sub-agent as a goal-aware agents-as-tools tool in
        // the original config order, so the tool order the model sees is
        // unchanged, handing it this instance's shared usage and coverage
        // ledgers
        for config in &self.sub_agents {
            let (sub_agent, enabled, _) = &built[&config.tool_name()];
            // offer this sub-agent to swarms too if it has not opted out
            if config.swarmable() {
                swarm_members.push(agents::swarm::SwarmMember {
                    tool_name: config.tool_name(),
                    role: config.name.clone(),
                    description: config.description.clone(),
                    max_swarm_size: config.max_swarm_size,
                    agent: Arc::clone(sub_agent),
                    enabled: enabled.clone(),
                    budget: config.turn_budget(self.context_window),
                });
            }
            agent = agent.dynamic_tool(agents::tool::into_goal_tool(
                Arc::clone(sub_agent),
                config.name.clone(),
                enabled.clone(),
                config.turn_budget(self.context_window),
                Arc::clone(&self.usage),
                Arc::clone(&self.coverage),
                self.events.clone(),
                self.store.clone(),
                Arc::clone(&alive),
                Some(Arc::clone(&post_mortem)),
            ));
        }
        // retain each top-level delegation's pieces (what the tool closures
        // otherwise consume) so a resume can re-run a dangling delegation
        // call with exactly the agent, toolset, and budget a live call gets
        let mut specs = BTreeMap::new();
        for config in &self.sub_agents {
            let (sub_agent, enabled, _) = &built[&config.tool_name()];
            specs.insert(
                config.tool_name(),
                agents::tool::DelegationSpec {
                    agent: Arc::clone(sub_agent),
                    role: config.name.clone(),
                    enabled: enabled.clone(),
                    budget: config.turn_budget(self.context_window),
                    swarmable: config.swarmable(),
                    max_swarm_size: config.max_swarm_size,
                },
            );
        }
        let delegations = agents::tool::DelegationRegistry {
            specs,
            alive: Arc::clone(&alive),
            post_mortem: Some(Arc::clone(&post_mortem)),
        };
        // attach the swarm tool when at least one sub-agent can be swarmed
        if !swarm_members.is_empty() {
            agent = agent.dynamic_tool(agents::swarm::into_swarm_tool(
                swarm_members,
                Arc::clone(&self.usage),
                Arc::clone(&self.coverage),
                self.events.clone(),
                self.store.clone(),
                alive,
                self.max_alive_sub_agents,
                Some(post_mortem),
            ));
        }

        // report forum mutations on the run's event sink when one is installed,
        // so a consumer can keep a live view of the discussion
        if let Some(events) = &self.events {
            self.forum.set_events(events.clone());
            // announce pending analyst asks on the same sink, so an attached
            // consumer can render the question modal
            self.analyst.set_events(events.clone());
        }
        // persist the forum to the session store after every mutation when a
        // store is installed, so a crash never loses more than the in-flight turn
        if let Some(store) = &self.store {
            self.forum.set_store(Arc::clone(store));
        }

        // build the main agent first so its offered roster can be recorded
        let agent = agent.build();
        // record the main agent's roster under the one "main" role
        append_tools_manifest(&self.store, "main", &agent).await;
        // assemble the finished ThorPal, moving the shared state onto it
        Ok(ThorPal {
            dead_delegations: std::sync::Mutex::new(Vec::new()),
            gate,
            agent,
            memory,
            sides: tokio::sync::Mutex::new(BTreeMap::new()),
            conversation_id: MAIN_CONVERSATION_ID.to_owned(),
            provider: self.provider,
            caller_preamble: self.caller_preamble,
            ghidra_settings: self.ghidra_settings,
            workflow,
            store: self.store,
            library: self.library,
            memories: self.memories,
            redactor: self.redactor,
            forum: self.forum,
            analyst: self.analyst,
            // the constant tracker is always on; its caps bound its noise
            constants: Arc::new(ConstantTracker::new()),
            ghidra: self.ghidra,
            usage: self.usage,
            coverage: self.coverage,
            yara_rules: self.yara_rules,
            events: self.events,
            delegations,
            mcp: tokio::sync::Mutex::new(mcp_connections),
            interrupted: tokio::sync::Mutex::new(None),
        })
    }
}

/// Appends a role's offered-tool manifest to the session transcript
///
/// The roster an agent is actually offered exists only at build time (group
/// names expand against the compiled-in registry, MCP servers advertise their
/// tools at connect time), so this line is what makes "offered but never
/// called" answerable offline. Best-effort like the resume barrier: a failed
/// roster read or append is logged and skipped so telemetry can never fail a
/// build. Appended once per built role per process run, which keeps roster
/// drift across resumes under different binaries visible.
///
/// # Arguments
///
/// * `store` - The session store to append to, when one is installed
/// * `role` - The role this roster belongs to (e.g. `"main"`)
/// * `agent` - The built agent whose provider-facing roster is recorded
async fn append_tools_manifest(store: &Option<Arc<SessionStore>>, role: &str, agent: &Agent) {
    // without a store there is no transcript to append to
    let Some(store) = store else {
        return;
    };
    // resolve the provider-facing roster the model will actually be offered
    let tools = match agent.tool_definitions(None).await {
        // reduce the definitions to sorted, deduplicated tool names
        Ok(defs) => {
            let names: std::collections::BTreeSet<String> =
                defs.into_iter().map(|def| def.name).collect();
            names.into_iter().collect::<Vec<String>>()
        }
        Err(err) => {
            tracing::warn!(?err, role, "failed to resolve the offered-tool roster");
            return;
        }
    };
    // stamp the manifest with the crate version that resolved this roster
    let item = transcript::TranscriptItem::ToolsOffered {
        role: role.to_owned(),
        tools,
        version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    let line = transcript::TranscriptLine::stamped(item);
    // append best-effort: a lost manifest costs telemetry, never the build
    if let Err(err) = store.append_transcript(&line).await {
        tracing::warn!(?err, role, "failed to record the offered-tool manifest");
    }
}

impl ThorPal {
    /// Appends a seeding pass's detector roster to the session transcript
    ///
    /// The registry compiled into this binary is what ran, so the roster is
    /// recorded per pass (per sample in a multi-sample session) -- that is
    /// what makes "detectors that never fired" answerable offline. Appended
    /// best-effort like the tools-offered manifest: a lost roster costs
    /// telemetry, never the seeding.
    ///
    /// # Arguments
    ///
    /// * `report` - The seeding pass's report naming each detection that ran
    /// * `sample` - The scoped sibling program seeded, or `None` for the primary
    #[instrument(name = "ThorPal::append_detectors_run", skip_all)]
    async fn append_detectors_run(&self, report: seed::SeedReport, sample: Option<&str>) {
        // without a store there is no transcript to append to
        let Some(store) = &self.store else {
            return;
        };
        // a pass that ran nothing (seeding disabled upstream) records nothing
        if report.roster.is_empty() {
            return;
        }
        // reduce the roster to its durable line form
        let detectors = report
            .roster
            .iter()
            .map(|run| transcript::DetectorRunLine {
                name: run.name.to_owned(),
                technique: run.technique.clone(),
                hits: run.hits,
                failed: run.failed,
            })
            .collect();
        let item = transcript::TranscriptItem::DetectorsRun {
            sample: sample.map(str::to_owned),
            detectors,
            version: env!("CARGO_PKG_VERSION").to_owned(),
        };
        let line = transcript::TranscriptLine::stamped(item);
        // append best-effort: a lost roster costs telemetry, never the run
        if let Err(err) = store.append_transcript(&line).await {
            tracing::warn!(?err, "failed to record the detector roster");
        }
    }
}

/// A Thorium AI agent
pub struct ThorPal {
    /// The pause gate holding the live provider client all agents share
    gate: Arc<gate::ProviderGate>,
    /// The main agent to talk to
    agent: Agent,
    /// A handle to the main agent's conversation memory, kept so its history can
    /// be read out for a snapshot or seeded on resume (the agent exposes none);
    /// the persisting wrapper mirrors every change to the store's `chat.json`
    memory: PersistingConversationMemory,
    /// The named side conversations forked from the main agent, each holding
    /// its own history and run through the main agent via a per-run history
    /// override so the main conversation's memory is never touched
    sides: sides::SideMap,
    /// The conversation id the main agent's history is keyed by
    conversation_id: String,
    /// The provider config every agent's client was built from, kept for
    /// snapshotting (sanitized of secrets before it is persisted)
    provider: AiProviderConfig,
    /// The caller's appended preamble on its own, kept for snapshotting, or
    /// `None` when the caller added none
    caller_preamble: Option<String>,
    /// The resolved Ghidra paths this instance analyzes with, kept for snapshotting
    ghidra_settings: GhidraSettings,
    /// The workflow of sub-agents attached to the main agent, kept for snapshotting
    workflow: Workflow,
    /// The session store this run saves into as it progresses, or `None` to
    /// disable session persistence
    store: Option<Arc<SessionStore>>,
    /// The persistent cross-session script library, or `None` to disable
    /// script promotion and library reuse
    library: Option<Arc<ScriptLibrary>>,
    /// The persistent cross-run memory library, or `None` to disable memory
    /// recall and saving
    memories: Option<Arc<MemoryLibrary>>,
    /// The render-time library redactor for blinded or deny-listed runs, or
    /// `None` to render library content verbatim
    redactor: Option<Arc<LibraryRedactor>>,
    /// The shared forum state seeded into every run's tool context
    forum: Arc<ForumState>,
    /// Re-queue lines for delegations that died unfinished in the run this
    /// session resumed from; drained into the first follow-up prompt so the
    /// continue loop re-delegates their tasks instead of leaving a permanent
    /// FAILED scar (empty on fresh runs)
    dead_delegations: std::sync::Mutex<Vec<String>>,
    /// The live analyst link seeded into every run's tool context, through
    /// which an attached consumer answers `ask_analyst` questions
    analyst: Arc<AnalystLink>,
    /// The shared decoded-constant tracker seeded into every run's tool
    /// context, so the agent hook can nudge about unposted constants
    constants: Arc<ConstantTracker>,
    /// The shared Ghidra session seeded into every run's tool context
    ghidra: Arc<GhidraSession>,
    /// The shared token-usage ledger across the main agent and every sub-agent
    usage: Arc<UsageLedger>,
    /// The shared function-coverage ledger across every agent
    coverage: Arc<CoverageLedger>,
    /// A YARA rules file or directory the seed phase scans the sample with
    yara_rules: Option<std::path::PathBuf>,
    /// An optional event sink the main agent's hook emits onto, or `None`
    events: Option<ThorPalEventSender>,
    /// The retained delegation machinery a resume re-runs dangling calls with
    delegations: agents::tool::DelegationRegistry,
    /// The MCP connections this run's agents dispatch tool calls through,
    /// held so the services stay alive until shutdown closes them
    mcp: tokio::sync::Mutex<mcp::McpConnections>,
    /// A restored run's interrupted turn, held until its dangling delegations
    /// are re-run and the turn is completed (see `resume_interrupted_turn`)
    interrupted: tokio::sync::Mutex<Option<InterruptedTurn>>,
}

/// An interrupted turn held across a restore, awaiting completion
///
/// Restored from `chat.json` when the dead run's in-flight tail left
/// delegation calls dangling: the tail stays pending on disk and the
/// delegations are re-run for real before the turn continues, so the
/// conversation ends up byte-identical to one that never stopped.
struct InterruptedTurn {
    /// The interrupted turn's raw tail (empty when a prior resume settled it)
    tail: Vec<Message>,
    /// The calls the interruption left dangling, in order
    dangling: Vec<memory::DanglingCall>,
}

impl ThorPal {
    /// Create a new builder for a Thorium pal
    ///
    /// # Arguments
    ///
    /// * `provider` - The explicit LLM provider config to build clients from
    /// * `ghidra` - The resolved Ghidra settings this instance analyzes with
    pub fn builder(provider: AiProviderConfig, ghidra: GhidraSettings) -> ThorPalBuilder {
        ThorPalBuilder::new(provider, ghidra)
    }

    /// Builds the tool context seeding this instance's shared state into a run
    ///
    /// Every top-level agent prompt attaches this so the forum and Ghidra tools
    /// recover this instance's `Arc`s via `require`, and the same context
    /// propagates into the sub-agents' own tool calls.
    fn tool_context(&self) -> ToolContext {
        // start from an empty context and insert clones of every shared bundle
        let mut context = ToolContext::new();
        context.insert(Arc::clone(&self.forum));
        // seed the analyst link so ask_analyst can wait on an attached
        // consumer, or take its default path when none is attached
        context.insert(Arc::clone(&self.analyst));
        // seed the constant tracker so the agent hook can scan tool results
        // for decoded constants and the finish tool can list unposted ones
        context.insert(Arc::clone(&self.constants));
        // seed the forum-context settings so sub-agent spawns know whether (and
        // how deep) to preload linked forum context into their prompts
        context.insert(ForumContextSettings {
            mode: self.workflow.forum_context,
            depth: self.workflow.forum_link_depth,
        });
        context.insert(Arc::clone(&self.ghidra));
        context.insert(Arc::clone(&self.usage));
        context.insert(Arc::clone(&self.coverage));
        // seed an empty enabled-toolset entry for the main agent, so a
        // caller-attached run_script degrades to no host bindings instead of
        // silently reusing whatever a sub-agent later inserts
        context.insert(EnabledTools::default());
        // seed the session store too, when persistence is on, so the rename
        // and finish tools can reach this run's store
        if let Some(store) = &self.store {
            context.insert(Arc::clone(store));
        }
        // seed the script library too, when one is configured, so the script
        // tools can search, run, and promote into it
        if let Some(library) = &self.library {
            context.insert(Arc::clone(library));
        }
        // seed the memory library too, when one is configured, so the memory
        // tools can list, search, read, and save cross-run lessons
        if let Some(memories) = &self.memories {
            context.insert(Arc::clone(memories));
        }
        // seed the library redactor too, when one is configured, so every
        // model-visible library surface renders through it
        if let Some(redactor) = &self.redactor {
            context.insert(Arc::clone(redactor));
        }
        // seed the pause gate so sub-agent death arms can tell a genuine
        // failure racing a teardown apart from one during normal operation
        context.insert(Arc::clone(&self.gate));
        context
    }

    /// Send a prompt and return only the assistant response.
    ///
    /// This preserves the original single-shot API, seeding the shared state into
    /// the run's tool context so the forum and Ghidra tools work. The whole
    /// session is autosaved once the turn settles, so an interactive consumer
    /// (e.g. the `thorctl` TUI) persists after every exchange.
    ///
    /// # Arguments
    ///
    /// * `prompt` - The prompt to send
    pub async fn prompt(&self, prompt: &str) -> Result<String, rig::completion::PromptError> {
        // a restored interrupted turn is completed first, so its delegations'
        // real results re-enter the conversation before this new message (an
        // interactive consumer normally drives the resume explicitly, making
        // this a no-op; it is the backstop for library callers)
        if self.has_interrupted_turn().await {
            self.resume_interrupted_turn().await?;
        }
        // put the user's message on the event stream first, so the persisted
        // transcript interleaves it ahead of the turn's activity
        if let Some(tx) = &self.events {
            let _ = tx.send(events::ThorPalEvent::UserMessage {
                text: prompt.to_owned(),
                scope: None,
            });
        }
        // prompt the main agent; transient provider faults are retried per
        // completion request by the retrying model every agent is built around
        let response = self.send_prompt(prompt).await?;
        // fold this run's usage into the shared ledger under the main agent
        self.usage.record("main", response.usage).await;
        // autosave the whole session now that this turn has settled, so a crash
        // between interactive turns can be resumed from here
        self.autosave_snapshot().await;
        // mirror the final response onto the event sink if a consumer is listening
        if let Some(tx) = &self.events {
            let _ = tx.send(events::ThorPalEvent::RunFinished {
                response: response.output.clone(),
                scope: None,
            });
        }
        // hand back just the assistant text to preserve the original API
        Ok(response.output)
    }

    /// Prompt the main agent once, surfacing a failure onto the event sink.
    ///
    /// This is the single place both [`ThorPal::prompt`] and [`ThorPal::run`]
    /// dispatch the main agent through, so the failure diagnostics live in
    /// exactly one spot. It builds the same prompt request both call sites
    /// used inline -- seeding the shared tool context and the activity hook,
    /// and asking for extended details so usage can be recorded. Transient
    /// provider faults are retried per completion request by the
    /// [`retry::RetryingModel`] every agent is built around, so an error
    /// surfacing here already survived the whole backoff budget; it is
    /// mirrored onto the event sink so a consumer (e.g. the `thorctl` TUI)
    /// can render it rather than appear to hang.
    ///
    /// # Arguments
    ///
    /// * `prompt` - The prompt message to send to the main agent (plain text
    ///   for a user turn, or a tool-results message continuing an interrupted
    ///   turn -- the same message shape rig itself sends mid-turn)
    #[instrument(name = "ThorPal::send_prompt", skip_all, err(Debug))]
    async fn send_prompt(&self, prompt: impl Into<Message>) -> Result<PromptResponse, PromptError> {
        let prompt = prompt.into();
        // mirror the turn's usage so a failure can still bill the ledger
        let turn_usage = agents::tool::TurnUsageMirror::default();
        // build the request exactly as the call sites did inline: the shared
        // tool context, the main agent's activity hook, and extended details
        let result = self
            .agent
            .prompt(prompt)
            .tool_context(self.tool_context())
            .add_hook(agents::tool::AgentEventHook {
                main_length_retry: std::sync::atomic::AtomicBool::new(true),
                agent: "main".to_string(),
                coverage: Arc::clone(&self.coverage),
                events: self.events.clone(),
                // the main agent runs no turn budget: no warnings or extensions
                budget: None,
                turn_usage: Some(turn_usage.clone()),
                // the main conversation keeps no per-invocation agent record
                record: None,
            })
            // record each model turn into chat.json as it completes, so a
            // quit mid-turn still leaves the conversation on disk
            .add_hook(memory::MainHistoryRecorder {
                memory: self.memory.clone(),
            })
            .extended_details()
            .await;
        match result {
            // a completed turn needs no diagnostics
            Ok(response) => Ok(response),
            // a failure here already spent the per-request retry budget
            Err(err) => {
                // a shutdown-time interruption freezes the turn for resume:
                // bill the spend and persist the ledger, but leave the pending
                // tail pending on disk and skip the failure event and salvage
                // entirely, so no durable debris marks the death and a resume
                // continues the turn as if it never stopped. the latch check
                // also freezes a genuine error racing the teardown (e.g. the
                // final budgeted call exhausting as the latch lands) -- during
                // shutdown every outcome is better frozen than settled
                if retry::is_shutdown_interrupt(&err) || self.gate.is_shutdown() {
                    tracing::info!("main turn interrupted by shutdown; freezing for resume");
                    // the tokens were really spent; the ledger must not lose them
                    self.usage.record("main", turn_usage.take()).await;
                    // persist the snapshot so the billed usage survives the exit
                    // (it is otherwise only saved when a turn settles)
                    self.autosave_snapshot().await;
                    return Err(err);
                }
                // log what we can see; the raw failing body is preserved by
                // rig's own ERROR-level `body` log, which the thorctl binaries
                // capture into the thorctl-ai log file
                tracing::warn!(
                    error = %err,
                    "main-agent prompt failed; see the thorctl-ai log for the raw provider body"
                );
                // bill the failed turn's model calls to the ledger so the
                // persisted totals match what the live events already counted
                self.usage.record("main", turn_usage.take()).await;
                // surface the failure onto the event sink so a consumer can show it
                if let Some(tx) = &self.events {
                    let _ = tx.send(events::ThorPalEvent::TurnFailed {
                        agent: "main".to_string(),
                        error: err.to_string(),
                        request_id: err.provider_request_id().map(str::to_owned),
                    });
                }
                // salvage the failed turn's paid work into settled history so
                // the next turn (and a resume) keeps the context
                self.salvage_failed_turn(&err).await;
                Err(err)
            }
        }
    }

    /// Salvage a failed main turn's captured work into settled history
    ///
    /// A failed turn used to discard everything it paid for: rig only appends
    /// to memory on settled turns, and the next successful turn clears the
    /// pending mirror. This drains the mirror instead, repairs its tail
    /// (synthetic results for dangling tool calls, a closing assistant note),
    /// and appends it as settled history -- so the next turn and any resume
    /// keep the context. An oversized-request fault skips the salvage: the
    /// tail is what overflowed, and replaying it would re-fail identically,
    /// so the drop is recorded in the session transcript as a durable
    /// breadcrumb and the emptied pending mirror is persisted explicitly so
    /// a resume cannot replay the oversized tail either.
    ///
    /// # Arguments
    ///
    /// * `error` - The error that ended the turn
    #[instrument(name = "ThorPal::salvage_failed_turn", skip_all)]
    async fn salvage_failed_turn(&self, error: &PromptError) {
        // drain the failed turn's captured messages from the pending mirror
        let tail = self.memory.take_pending_tail().await;
        if tail.is_empty() {
            return;
        }
        // an oversized history must not be replayed into the same failure
        if let PromptError::CompletionError(inner) = error
            && retry::is_context_length_error(inner)
        {
            // summarize the dropped shape so the warn and the transcript
            // record show what was lost without dumping message bodies
            let roles = tail
                .iter()
                .map(|message| match message {
                    Message::User { .. } => "user",
                    Message::Assistant { .. } => "assistant",
                    Message::System { .. } => "system",
                })
                .collect::<Vec<_>>()
                .join(", ");
            tracing::warn!(
                dropped = tail.len(),
                roles = %roles,
                "dropping an oversized failed turn; replaying it would overflow the context again"
            );
            // leave a durable transcript record of the drop: the explicit
            // persist below erases the tail from chat.json's pending, and
            // the tail's tool calls/results were already teed into the
            // transcript live, so this breadcrumb marks that the rest of
            // the in-flight turn was dropped and what shape it had
            if let Some(store) = &self.store {
                let item = transcript::TranscriptItem::TurnFailed {
                    agent: "main".to_owned(),
                    error: format!(
                        "dropped an oversized in-flight turn from the model context: \
                         {} messages (roles: {roles}); replaying it would overflow \
                         the context again",
                        tail.len()
                    ),
                    // a local context-overflow drop has no provider request
                    request_id: None,
                };
                let line = transcript::TranscriptLine::stamped(item);
                if let Err(err) = store.append_transcript(&line).await {
                    tracing::warn!(?err, "failed to record the dropped turn in the transcript");
                }
            }
            // the drain above emptied pending in memory only; persist so disk
            // agrees instead of resurrecting the oversized tail on resume
            self.memory.persist().await;
            return;
        }
        // close any dangling tool calls and end the tail assistant-side
        let Some(repaired) = memory::repair_interrupted_tail(tail, &error.to_string()) else {
            // the unrepairable tail is dropped without an append, so persist
            // the emptied pending ourselves to keep chat.json agreeing with
            // memory (take_pending_tail's contract for tail-dropping callers)
            self.memory.persist().await;
            return;
        };
        // settle the repaired tail so history, chat.json, and snapshots keep it
        let salvaged = repaired.len();
        match self.memory.append(&self.conversation_id, repaired).await {
            Ok(()) => {
                tracing::info!(
                    salvaged,
                    "salvaged the failed turn's work into settled history"
                );
                // persist the snapshot so a crash right after still keeps it
                self.autosave_snapshot().await;
            }
            // deliberately no persist here: the append failed, so the on-disk
            // pending is the only surviving copy of the tail -- keep it
            Err(err) => tracing::warn!(?err, "failed to append the salvaged turn"),
        }
    }

    /// Add a user-specified question to the shared forum.
    ///
    /// Plants a human operator's own question as an open question post before (or
    /// alongside) a `run`, so the main agent picks it up, delegates it to the
    /// sub-agents, and cannot `finish` until it is resolved. The question's
    /// space-free `#reference` name is derived automatically, so callers only
    /// supply a title and body.
    ///
    /// # Arguments
    ///
    /// * `title` - A short human-readable title for the question
    /// * `body` - The full text of the question to answer
    #[instrument(name = "ThorPal::add_question", skip_all, err(Debug))]
    pub async fn add_question(
        &self,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Result<(), anyhow::Error> {
        // seed the question onto this instance's forum; the returned post is unused
        self.forum
            .add_user_question(title.into(), body.into())
            .await?;
        Ok(())
    }

    /// Seed the forum with the static detectors' findings for this instance.
    ///
    /// Runs the ported detection engine over the target binary (via this
    /// instance's shared Ghidra connection) and turns each finding into a forum
    /// post/comment, so the agent opens with concrete points of interest. When
    /// `seed_questions` is set, the strongest findings also derive a bounded set
    /// of targeted open questions for the question loop to delegate. A primary
    /// whose Ghidra import failed (a multi-sample session tolerates that when a
    /// sibling seeded the project) skips its Ghidra-context pass -- the bridge
    /// holds a sibling's program, so the findings would be mis-attributed --
    /// and instead posts an annotation steering the model to the raw file
    /// tools, keeping only the correctly-attributed YARA pass over its bytes.
    /// This is best-effort enrichment: an error is returned for the caller to
    /// log and treat as non-fatal.
    ///
    /// # Arguments
    ///
    /// * `seed_questions` - Whether to also seed the derived questions
    #[instrument(name = "ThorPal::seed_detectors", skip_all, err(Debug))]
    pub async fn seed_detectors(&self, seed_questions: bool) -> Result<(), anyhow::Error> {
        // seed the sample's raw file identity first: it needs no Ghidra at all,
        // so the forum opens with the basic facts even when the import can
        // never succeed; failures are logged and skipped like any seed error
        let format = self.seed_identity_post(self.ghidra.binary(), None).await;
        // seed the container-payload question before any Ghidra work, so a
        // sample that is really a wrapper (MSI, CAB, ZIP, gzip, ...) opens
        // with the unpacking lead even when the import can never succeed
        if seed_questions && let Some(format) = format {
            self.seed_container_question(&format, self.ghidra.binary().to_path_buf(), None)
                .await;
            // the text analogue: a script/text sample opens with the
            // read_text-first triage lead (the two leads are format-disjoint)
            self.seed_script_question(&format, self.ghidra.binary().to_path_buf(), None)
                .await;
        }
        // surface any prior session's tooling for this exact binary before the
        // agents start from scratch (format-independent, needs no Ghidra)
        if seed_questions {
            self.seed_prior_toolkit_question(self.ghidra.binary(), None)
                .await;
        }
        // the detectors read the binary through Ghidra, so skip them without an install
        if !self.ghidra.has_install() {
            anyhow::bail!("skipping detectors: no Ghidra install path is set");
        }
        // announce the (potentially slow) initial import/analysis as a synthetic
        // tool call so a consumer can render it -- and stream its logs -- like
        // any other tool call; gated on the sink so an unobserved run is unchanged
        if let Some(tx) = &self.events {
            let _ = tx.send(events::ThorPalEvent::ToolCallStarted {
                agent: "ghidra".to_string(),
                call_id: tool_logs::SEED_ANALYSIS_CALL_ID.to_string(),
                tool: tool_logs::SEED_ANALYSIS_TOOL.to_string(),
                args: "{}".to_string(),
            });
        }
        // connect (and analyze, once) the shared Ghidra handle the agent later
        // reuses; a bootstrap failure propagates so the caller (Session::seed) can
        // log-and-swallow it, leaving a cold forum rather than crashing the run
        let result = self.ghidra.get().await;
        // close the synthetic scope either way so the consumer's entry settles
        if let Some(tx) = &self.events {
            let _ = tx.send(events::ThorPalEvent::ToolCallFinished {
                agent: "ghidra".to_string(),
                call_id: tool_logs::SEED_ANALYSIS_CALL_ID.to_string(),
                tool: tool_logs::SEED_ANALYSIS_TOOL.to_string(),
                result: match (&result, self.ghidra.extra_binaries().len()) {
                    // a multi-sample bootstrap names its program count
                    (Ok(_), 0) => "Ghidra project imported and analyzed".to_string(),
                    (Ok(_), extras) => format!(
                        "Ghidra project imported and analyzed ({} programs)",
                        extras + 1
                    ),
                    (Err(err), _) => format!("{err:#}"),
                },
                success: result.is_ok(),
            });
        }
        // a bootstrap that never produced a bridge can still carry per-member
        // records (classification-gated members, or real failures after some
        // were gated): a whole set of scripts/documents never starts the JVM
        // at all. Seed the Ghidra-free passes anyway so the forum opens with
        // the facts (annotation, YARA, the sample-set map) instead of staying
        // cold; a failure with NO records is a real environment problem (bad
        // install, locked project) and propagates like before.
        let main_program = self.ghidra.main_program_name();
        let ghidra = match result {
            Ok(ghidra) => Some(ghidra),
            Err(error) if self.ghidra.failed_import(&main_program).is_some() => {
                tracing::warn!(
                    ?error,
                    "Ghidra never came up for this sample set; seeding without it"
                );
                None
            }
            Err(error) => return Err(error),
        };
        // resolve the binary path this instance's Ghidra session was built with
        let binary = self.ghidra.binary().to_path_buf();
        // a primary whose import failed (or was gated) has no program behind
        // the connected bridge (it holds whichever sibling seeded the
        // project), so the Ghidra-context pass below would silently attribute
        // that SIBLING's code to the primary -- parity with seed_sample's guard
        if let Some(reason) = self.ghidra.failed_import(&main_program) {
            tracing::warn!(
                primary = %main_program,
                %reason,
                "skipping the primary's Ghidra detector pass; its import failed"
            );
            // annotate the forum so the model learns the primary was never
            // statically seeded and reaches for the raw file tools instead
            if let Err(error) =
                seed::seed_failed_primary_annotation(&self.forum, &main_program, &reason).await
            {
                tracing::warn!(?error, "failed to seed the failed-primary annotation");
            }
            // YARA reads the on-disk bytes directly, so its findings stay
            // correctly attributed no matter what Ghidra holds; keep that
            // half of the primary's pass
            if let Some(rules) = self.yara_rules.as_deref()
                && let Err(error) = seed::seed_yara_only(&self.forum, &binary, rules, None).await
            {
                tracing::warn!(?error, "the yara-only primary pass failed; skipping it");
            }
        } else if let Some(ghidra) = &ghidra {
            // gather any detector-flagged library scripts to run alongside the
            // native detections; a library failure degrades to a script-less seed
            let script_detectors = match &self.library {
                Some(library) => match library.detector_scripts().await {
                    Ok(detectors) => detectors
                        .into_iter()
                        .map(|(name, source)| detect::script_detections::ScriptDetector {
                            // render the provenance name exactly like
                            // list_scripts shows it (neutral under blinding)
                            name: match &self.redactor {
                                Some(redactor) => redactor.display_name(&name),
                                None => name,
                            },
                            source,
                            redactor: self.redactor.clone(),
                        })
                        .collect(),
                    Err(err) => {
                        tracing::warn!(?err, "failed to load detector scripts; skipping them");
                        Vec::new()
                    }
                },
                None => Vec::new(),
            };
            // build the host the scripts' bindings call into only when any will run
            let host = (!script_detectors.is_empty()).then(|| tools::scripts::ScriptHost {
                ghidra: Arc::clone(&self.ghidra),
                store: self.store.clone(),
                handle: tokio::runtime::Handle::current(),
            });
            // run the detectors and seed their findings onto this instance's forum
            let report = seed::seed_forum_from_detectors(
                ghidra,
                &self.forum,
                &binary,
                seed_questions,
                script_detectors,
                host,
                self.yara_rules.as_deref(),
                None,
            )
            .await?;
            // record which detectors ran so offline telemetry can tell a
            // detector that never fires from one that never ran
            self.append_detectors_run(report, None).await;
        }
        // a multi-sample session seeds each sibling the same way, namespaced
        // so the fixed post names cannot collide with the main pass's
        let extras = self.ghidra.extra_binaries().to_vec();
        if !extras.is_empty() {
            // open with a one-glance map of the whole sample set
            self.seed_sample_set_post(&extras).await;
            for (index, extra) in extras.iter().enumerate() {
                // sibling programs were named by file name at bootstrap, and
                // the scope prefix is order-derived (s2, s3, ...)
                let program = extra
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "sibling".to_owned());
                let scope = seed::SampleScope {
                    prefix: format!("s{}", index + 2),
                    program,
                };
                // one bad sibling must never sink the main sample's seeding
                if let Err(error) = self.seed_sample(extra, &scope, seed_questions).await {
                    tracing::warn!(
                        ?error,
                        sample = %extra.display(),
                        "failed to seed a sibling sample; skipping"
                    );
                }
            }
        }
        Ok(())
    }

    /// Seeds one sibling sample's identity, container lead, and detector pass
    ///
    /// Mirrors the main sample's seeding under the sample's own namespace: a
    /// scoped file-identity post, a scoped container question, and the full
    /// detector/YARA/emulation pass run against the sibling's program under an
    /// exclusive pin. Seeding runs strictly before the agent loop, so holding
    /// the pin across the whole pass has zero contention by construction.
    /// Detector scripts are deliberately not run for siblings: the Rhai
    /// bindings pin `op(None)`, so a script would silently read the main
    /// program under a sibling pin (a program-aware `ScriptHost` is the named
    /// follow-up).
    ///
    /// # Arguments
    ///
    /// * `binary` - The sibling sample's on-disk path
    /// * `scope` - The sample's post-name namespace and program name
    /// * `seed_questions` - Whether to also seed the derived questions
    #[instrument(name = "ThorPal::seed_sample", skip_all, err(Debug))]
    async fn seed_sample(
        &self,
        binary: &Path,
        scope: &seed::SampleScope,
        seed_questions: bool,
    ) -> Result<(), anyhow::Error> {
        // seed the sibling's identity post, keeping its format for the lead
        let format = self.seed_identity_post(binary, Some(scope)).await;
        // the container lead applies to a sibling exactly like the primary
        if seed_questions && let Some(format) = format {
            self.seed_container_question(&format, binary.to_path_buf(), Some(scope))
                .await;
            // and so does the script-triage lead for a text/script sibling
            self.seed_script_question(&format, binary.to_path_buf(), Some(scope))
                .await;
        }
        // a sibling's prior-session tooling surfaces exactly like the primary's
        if seed_questions {
            self.seed_prior_toolkit_question(binary, Some(scope)).await;
        }
        // a sibling whose bootstrap import failed has no program to pin; its
        // identity post above still seeds, and the set map names the failure
        if let Some(reason) = self.ghidra.failed_import(&scope.program) {
            tracing::warn!(
                sibling = %scope.program,
                %reason,
                "skipping the detector pass for a sibling that failed to import"
            );
            return Ok(());
        }
        // pin the sibling's program exclusively for the whole detector pass;
        // nothing else runs during seeding, so the hold cannot contend
        let op = self.ghidra.op(Some(&scope.program)).await?;
        // run the native detectors, YARA, and emulation against the sibling
        let report = seed::seed_forum_from_detectors(
            op.ghidra(),
            &self.forum,
            binary,
            seed_questions,
            Vec::new(),
            None,
            self.yara_rules.as_deref(),
            Some(scope),
        )
        .await?;
        // record the sibling pass's roster under its scoped program name
        self.append_detectors_run(report, Some(&scope.program))
            .await;
        Ok(())
    }

    /// Seeds the one-glance map of a multi-sample session's whole sample set
    ///
    /// Best-effort like the rest of seeding: a failed identity read drops to a
    /// name-only line and a failed post is logged and skipped.
    ///
    /// # Arguments
    ///
    /// * `extras` - The sibling samples imported alongside the primary
    #[instrument(name = "ThorPal::seed_sample_set_post", skip_all)]
    async fn seed_sample_set_post(&self, extras: &[PathBuf]) {
        // describe the primary and every sibling by program name, size, and sha
        let mut lines = Vec::with_capacity(extras.len() + 1);
        for (index, path) in std::iter::once(self.ghidra.binary())
            .chain(extras.iter().map(PathBuf::as_path))
            .enumerate()
        {
            // sibling programs are named by file name; the primary is s1
            let program = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "sample".to_owned());
            let role = match index {
                0 => "primary (the main program)".to_owned(),
                n => format!("sibling (seeded under the s{}- prefix)", n + 1),
            };
            // name a failed Ghidra import on its own line so no agent burns
            // turns pinning (or re-importing) a program that never loaded
            let import_note = match self.ghidra.failed_import(&program) {
                Some(reason) => {
                    // render the record honestly: a gated member was never
                    // attempted at all, the rest really failed an import
                    let described = seed::describe_import_failure(&reason);
                    format!(
                        "; NOT loaded in Ghidra {described} -- use the raw file \
                         tools with target='{program}' instead"
                    )
                }
                None => String::new(),
            };
            // a failed identity read keeps the line, just without the facts
            match tools::file::file_identity(path).await {
                Ok(info) => lines.push(format!(
                    "- program '{program}': {role}; {} bytes, sha256 {}{import_note}",
                    info.size, info.sha256
                )),
                Err(_) => lines.push(format!("- program '{program}': {role}{import_note}")),
            }
        }
        // a set where nothing loaded must not steer agents at Ghidra calls
        // that answer with failures; name the raw-tools path instead
        let any_loaded = std::iter::once(self.ghidra.main_program_name())
            .chain(extras.iter().map(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "sample".to_owned())
            }))
            .any(|program| self.ghidra.failed_import(&program).is_none());
        let steering = if any_loaded {
            "Pass a program name as the Ghidra tools' `program` argument to \
             examine it (list_programs shows what is loaded); the raw file \
             tools address a sibling via its file name as `target`."
        } else {
            "NONE of these samples could be loaded into Ghidra (each line names \
             why), so skip the Ghidra tools for them: the raw file tools \
             address each member via its file name as `target`, and a \
             native-executable payload carved into the workspace can still \
             seed Ghidra via import_child."
        };
        let body = format!(
            "This session analyzes {} related samples in one Ghidra project:\n{}\n\
             {steering} Analyze the samples AS A SET: trace how they reference \
             each other (shared keys, embedded names, loaded exports) rather \
             than reporting each in isolation.",
            extras.len() + 1,
            lines.join("\n"),
        );
        // the set map is an asserted fact about the session, so a statement
        if let Err(error) = self
            .forum
            .seed_detector_post(
                "detector".to_string(),
                "sample-set".to_string(),
                "Sample set: related samples analyzed together".to_string(),
                body,
                tools::forum::Kind::Statement,
                "annotation",
                None,
                Vec::new(),
            )
            .await
        {
            tracing::warn!(%error, "failed to seed the sample-set post; skipping");
        }
    }

    /// Seeds one sample's raw file-identity post, returning its format
    ///
    /// Needs no Ghidra at all, so the forum opens with the basic facts even
    /// when the import can never succeed; failures are logged and skipped like
    /// any seed error. A sibling's post carries an extra line naming its
    /// program and file-tool target so the model knows how to address it.
    ///
    /// # Arguments
    ///
    /// * `binary` - The sample to identify
    /// * `scope` - The sample's post-name namespace, or `None` for the primary
    #[instrument(name = "ThorPal::seed_identity_post", skip_all)]
    async fn seed_identity_post(
        &self,
        binary: &Path,
        scope: Option<&seed::SampleScope>,
    ) -> Option<String> {
        match tools::file::file_identity(binary).await {
            Ok(info) => {
                // render the identity facts into the post body
                let mut body = format!(
                    "File: {}\nSize: {} bytes\nMD5: {}\nSHA1: {}\nSHA256: {}\n\
                     Entropy: {:.2} bits/byte\nFormat: {}",
                    info.file_name,
                    info.size,
                    info.md5,
                    info.sha1,
                    info.sha256,
                    info.entropy,
                    info.format,
                );
                // surface the declared build time and, crucially, why it may
                // not be a real date (REPRO hash, future-dated, zeroed) — the
                // misread class three evaluation runs kept paying for
                match (&info.build_timestamp, &info.timestamp_anomaly) {
                    (Some(stamp), Some(anomaly)) => {
                        body.push_str(&format!("\nBuild timestamp: {stamp} — {anomaly}"));
                    }
                    (Some(stamp), None) => {
                        body.push_str(&format!("\nBuild timestamp: {stamp}"));
                    }
                    (None, Some(anomaly)) => {
                        body.push_str(&format!("\nBuild timestamp: {anomaly}"));
                    }
                    (None, None) => {}
                }
                // a sibling's post says exactly how to address the sample
                let (name, title) = match scope {
                    Some(scope) => {
                        body.push_str(&format!(
                            "\nImported as program '{}' — pass it as the Ghidra \
                             tools' `program` argument; the raw file tools \
                             address it via target: \"{}\"",
                            scope.program, info.file_name,
                        ));
                        (
                            format!("{}-file-identity", scope.prefix),
                            format!("Sample file identity — {}", scope.program),
                        )
                    }
                    None => (
                        "file-identity".to_string(),
                        "Sample file identity".to_string(),
                    ),
                };
                // seed the identity post; a duplicate name (a resumed seed) is
                // logged and skipped rather than failing the whole seeding
                if let Err(error) = self
                    .forum
                    .seed_detector_post(
                        "detector".to_string(),
                        name,
                        title,
                        body,
                        tools::forum::Kind::Statement,
                        "annotation",
                        None,
                        Vec::new(),
                    )
                    .await
                {
                    tracing::warn!(%error, "failed to seed the file identity post; skipping");
                }
                // keep the identified format for the container question
                Some(info.format)
            }
            Err(error) => {
                tracing::warn!(%error, "failed to read the sample's file identity; skipping");
                None
            }
        }
    }

    /// Seeds the container-payload question when a sample is a container
    ///
    /// Best-effort like the rest of seeding: the entry listing is attempted to
    /// enrich the question body, but any failure degrades to the bare question
    /// and a failed post is logged and skipped.
    ///
    /// # Arguments
    ///
    /// * `format` - The sample's identified format (see `file::identify_format`)
    /// * `binary` - The sample the question (and its listing) covers
    /// * `scope` - The sample's post-name namespace, or `None` for the primary
    #[instrument(name = "ThorPal::seed_container_question", skip_all)]
    async fn seed_container_question(
        &self,
        format: &str,
        binary: PathBuf,
        scope: Option<&seed::SampleScope>,
    ) {
        // bail fast on non-container formats before spending a listing pass
        if seed::container_question(format, None).is_none() {
            return;
        }
        // best-effort: list the container's entries to enrich the question body
        let listing = tokio::task::spawn_blocking(move || {
            tools::containers::list_container_sync(
                &binary,
                0,
                tools::containers::DEFAULT_ENTRIES,
                None,
            )
        })
        .await
        .ok()
        .and_then(Result::ok);
        // derive the question (the format already classified as a container)
        let Some(question) = seed::container_question(format, listing.as_ref()) else {
            return;
        };
        // seed it as an open question (with its capped check tasks) through the
        // shared seeding path so container questions behave like derived ones
        seed::seed_question_posts(&self.forum, vec![question], scope).await;
    }

    /// Seeds the script-triage question when a sample is a text/script file
    ///
    /// The text analogue of [`ThorPal::seed_container_question`], and
    /// best-effort the same way: the sample's first lines are attempted to
    /// enrich the question body, but any failure degrades to the bare question
    /// and a failed post is logged and skipped.
    ///
    /// # Arguments
    ///
    /// * `format` - The sample's identified format (see `file::classify_format`)
    /// * `binary` - The sample the question (and its preview) covers
    /// * `scope` - The sample's post-name namespace, or `None` for the primary
    #[instrument(name = "ThorPal::seed_script_question", skip_all)]
    async fn seed_script_question(
        &self,
        format: &str,
        binary: PathBuf,
        scope: Option<&seed::SampleScope>,
    ) {
        // bail fast on non-text formats before spending a preview read
        let Some(mut question) = seed::script_question(format) else {
            return;
        };
        // best-effort: quote a bounded preview of the sample's first lines so
        // the agents open with the script's own text, not just its format
        if let Ok(window) = tools::file::read_text_window(binary, 0, 1, 10, 800, None).await
            && !window.lines.is_empty()
        {
            let shown: Vec<String> = window
                .lines
                .iter()
                .map(|line| format!("{:>4}  {}", line.number, line.text))
                .collect();
            question
                .body
                .push_str(&format!("\nFirst lines:\n{}", shown.join("\n")));
        }
        // seed it as an open question (with its capped check tasks) through the
        // shared seeding path so script questions behave like derived ones
        seed::seed_question_posts(&self.forum, vec![question], scope).await;
    }

    /// Seeds the prior-toolkit lead when library scripts reference this sample
    ///
    /// Scans the persistent script library for entries whose description or
    /// source embed the sample's SHA-256 16-hex stem (full digests contain the
    /// stem, and session artifacts conventionally name programs by it), then
    /// seeds `q-prior-toolkit` listing the matches so reuse of a prior
    /// session's exact tooling is the default instead of an unprompted library
    /// search. Matches render through the blinding redactor like every other
    /// library surface, and entries the redactor hides never surface.
    /// Best-effort like the rest of seeding: any hash or library error
    /// degrades to seeding nothing.
    ///
    /// # Arguments
    ///
    /// * `binary` - The sample whose content hash keys the library scan
    /// * `scope` - The sample's post-name namespace, or `None` for the primary
    #[instrument(name = "ThorPal::seed_prior_toolkit_question", skip_all)]
    async fn seed_prior_toolkit_question(&self, binary: &Path, scope: Option<&seed::SampleScope>) {
        // a blinded run simulates NOVEL malware: surfacing prior tooling for
        // the same hash would memoize the evaluation corpus instead of
        // measuring analysis capability (operator methodology call,
        // 2026-10-24), so the lead is an unblinded re-analysis feature only
        if self.redactor.is_some() {
            return;
        }
        // no library means no prior work to surface
        let Some(library) = &self.library else {
            return;
        };
        // hash the sample bytes off-thread; staging names vary, digests do not
        let path = binary.to_path_buf();
        let digest = tokio::task::spawn_blocking(move || -> Result<String, std::io::Error> {
            use sha2::{Digest, Sha256};
            let bytes = std::fs::read(&path)?;
            Ok(format!("{:x}", Sha256::digest(&bytes)))
        })
        .await;
        let Ok(Ok(sha)) = digest else {
            return;
        };
        // the 16-hex stem matches both full digests and stem-named artifacts
        let stem = &sha[..16];
        let Ok(entries) = library.list().await else {
            return;
        };
        let mut matches = Vec::new();
        for entry in entries {
            // match the description first, then the script source itself;
            // blinded entries still surface -- just under their neutral ids,
            // exactly like the list_scripts surface (hides() flags every name
            // under --anonymize, so skipping on it would empty the lead)
            let mut hit = entry.description.contains(stem);
            if !hit && let Ok(source) = library.load(&entry.name).await {
                hit = source.contains(stem);
            }
            if hit {
                // render the name and description like every library surface
                let (name, description) = match &self.redactor {
                    Some(redactor) => (
                        redactor.display_name(&entry.name),
                        redactor.redact_text(&entry.description),
                    ),
                    None => (entry.name.clone(), entry.description.clone()),
                };
                matches.push((name, description));
            }
        }
        // seed the lead through the shared question path when anything matched
        if let Some(question) = seed::prior_toolkit_question(&matches) {
            seed::seed_question_posts(&self.forum, vec![question], scope).await;
        }
    }

    /// Get a shared handle to this instance's script library, if one is set
    ///
    /// Consumers (e.g. the `thorctl` TUI's manual `/promote` command) use this
    /// to publish a session script into the persistent library themselves.
    pub(crate) fn script_library(&self) -> Option<Arc<ScriptLibrary>> {
        // share the same library every tool call resolves against
        self.library.as_ref().map(Arc::clone)
    }

    /// Get a shared handle to this instance's library redactor, if one is set
    pub(crate) fn library_redactor(&self) -> Option<Arc<LibraryRedactor>> {
        // share the same redactor every tool call renders through
        self.redactor.as_ref().map(Arc::clone)
    }

    /// Returns a handle to this instance's shared forum
    ///
    /// Lets an external consumer (e.g. a TUI) read a structured view of the
    /// discussion and add a human operator's comments or status changes while a
    /// run is in progress.
    pub fn forum(&self) -> Arc<ForumState> {
        // share the same forum this instance's agents write to
        Arc::clone(&self.forum)
    }

    /// Returns a handle to this instance's live analyst link
    ///
    /// An interactive consumer (e.g. a TUI) calls `attach` on it before the
    /// run starts so `ask_analyst` waits on the human, answers pending asks
    /// through it, and `detach`es on teardown so no in-flight turn can wedge.
    pub fn analyst(&self) -> Arc<AnalystLink> {
        // share the same link this instance's ask tool waits on
        Arc::clone(&self.analyst)
    }

    /// Get a shared handle to this instance's Ghidra session
    ///
    /// Consumers use this to make their own best-effort Ghidra queries (e.g.
    /// call-graph enrichment for a visualization) over the same lazily-opened
    /// connection every tool call shares.
    pub fn ghidra(&self) -> Arc<GhidraSession> {
        // share the same connection this instance's tools analyze with
        Arc::clone(&self.ghidra)
    }

    /// The resolved Ghidra settings this instance was built with
    ///
    /// Exposes the analysis paths so the session can finalize (rename) the
    /// analysis folder once the Ghidra bridge has shut down.
    pub(crate) fn ghidra_settings(&self) -> &GhidraSettings {
        // hand back the settings kept around for snapshotting
        &self.ghidra_settings
    }

    /// Render the whole forum -- every post with its nested comment thread.
    ///
    /// Exposes this instance's forum dump so a caller can print it after a run.
    pub async fn render_forum(&self) -> String {
        // delegate to the shared forum state
        self.forum.render_forum().await
    }

    /// Captures the main agent's session state to a snapshot
    ///
    /// Bundles the reconstruction inputs (sanitized provider config, caller
    /// preamble, Ghidra paths, and the workflow) into a [`SessionSnapshot`]
    /// that [`crate::Session::restore`] can rebuild from. The conversation
    /// history is persisted separately as `chat.json` after every message (see
    /// [`PersistingConversationMemory`]), so the snapshot writes `messages`
    /// empty; the forum is likewise snapshotted separately (see
    /// [`ForumState::snapshot`]) and persisted as its own `forum.json`.
    pub async fn snapshot(&self) -> SessionSnapshot {
        // assemble the full snapshot with the inputs needed to rebuild the run
        SessionSnapshot {
            version: resume::SNAPSHOT_VERSION,
            // persist the provider config without its secrets; a resumed run
            // re-supplies them from the caller's live config
            provider: self.provider.sanitized(),
            preamble: self.caller_preamble.clone(),
            ghidra: self.ghidra_settings.clone(),
            workflow: self.workflow.clone(),
            conversation_id: self.conversation_id.clone(),
            // the history lives in chat.json as of v5; written empty here
            messages: Vec::new(),
            // persist every agent's accumulated token usage so a resume
            // continues the run's running totals
            usage: self.usage.snapshot().await,
            // persist the display-name tally so a resume mints fresh names
            // instead of clobbering this run's agent records
            name_counter: agents::naming::name_counter(),
        }
    }

    /// Autosaves the session and forum to the session store, best-effort
    ///
    /// A no-op when no session store was configured. A serialization or write
    /// failure is logged and swallowed rather than failing the run, matching the
    /// detector-seeding philosophy. Called after every settled turn, and once at
    /// build time (see [`crate::Session::build`]) so even a run that crashes
    /// before its first turn leaves a resumable session behind.
    pub(crate) async fn autosave_snapshot(&self) {
        // skip entirely when session persistence was not requested
        let Some(store) = self.store.as_ref() else {
            return;
        };
        // save the main agent's state, logging and swallowing any failure
        if let Err(err) = store.save_session(&self.snapshot().await).await {
            tracing::warn!(?err, "failed to autosave session snapshot");
        }
        // save the forum alongside it so the two files never drift far apart
        if let Err(err) = store.save_forum(&self.forum.snapshot().await).await {
            tracing::warn!(?err, "failed to autosave forum snapshot");
        }
    }

    /// Restores a forum snapshot onto this instance's (empty) forum
    ///
    /// Used by [`crate::Session::restore`] to reload a saved forum before the
    /// resumed run loop starts. Preserves original post names and comment ids.
    ///
    /// # Arguments
    ///
    /// * `snapshot` - The forum snapshot to load
    pub async fn restore_forum(&self, snapshot: resume::ForumSnapshot) {
        // delegate to the shared forum state's bulk loader
        self.forum.restore(snapshot).await;
        // announce the restored forum on the event sink so a consumer (e.g. the
        // thorctl message board) primes its view with the saved posts; the bulk
        // loader itself emits nothing per item, and the TUI only ever learns
        // about the forum through these events
        if let Some(tx) = &self.events {
            // read the restored forum back so its latest item can be targeted
            let view = self.forum.view().await;
            // only announce when the restored forum actually holds something
            if let Some(target) = view.last_updated {
                let _ = tx.send(events::ThorPalEvent::ForumUpdated { target });
            }
        }
    }

    /// Seeds the main agent's conversation memory with a saved history
    ///
    /// Appends the snapshot's messages under this instance's conversation id so
    /// the resumed run continues the same conversation.
    ///
    /// # Arguments
    ///
    /// * `messages` - The saved main-agent conversation history to seed
    pub async fn restore_memory(&self, messages: Vec<Message>) -> Result<(), anyhow::Error> {
        // append the history under our conversation id so rig loads it on resume
        self.memory
            .append(&self.conversation_id, messages)
            .await
            .map_err(anyhow::Error::from)
    }

    /// Restores a split conversation, holding an interrupted turn for resume
    ///
    /// The settled half seeds the conversation memory like
    /// [`ThorPal::restore_memory`]; an interrupted tail is re-installed as the
    /// pending half (chat.json keeps the dead run's exact two-halves shape)
    /// and stashed, so [`ThorPal::resume_interrupted_turn`] can re-run its
    /// dangling delegations for real before the turn continues.
    ///
    /// # Arguments
    ///
    /// * `restored` - The split conversation the session restore produced
    pub(crate) async fn restore_conversation(
        &self,
        restored: memory::RestoredConversation,
    ) -> Result<(), anyhow::Error> {
        match restored {
            // a fully settled history seeds memory exactly as before
            memory::RestoredConversation::Settled(messages) => self.restore_memory(messages).await,
            // an interrupted turn seeds without settling and is held for resume
            memory::RestoredConversation::Interrupted {
                settled,
                tail,
                dangling,
            } => {
                // seed the settled half and re-install the tail as pending
                self.memory
                    .seed(settled, tail.clone())
                    .await
                    .map_err(anyhow::Error::from)?;
                // stash the held turn for the resume entry point
                *self.interrupted.lock().await = Some(InterruptedTurn { tail, dangling });
                Ok(())
            }
        }
    }

    /// Seeds the re-queue lines for delegations that died unfinished
    ///
    /// The lines are drained into the FIRST follow-up prompt the resumed run
    /// issues, so the continue loop re-delegates the dead tasks exactly once;
    /// later iterations (and later resumes, which re-derive the lines from the
    /// store's agent records) see whatever is still unfinished then. Seeded by
    /// [`crate::Session::restore`] when no interrupted turn is held, and by
    /// the interrupted-turn revival from its leftover records otherwise.
    ///
    /// # Arguments
    ///
    /// * `lines` - The rendered re-queue lines, one per dead delegation
    pub fn restore_dead_delegations(&self, lines: Vec<String>) {
        // replace wholesale: each seeder runs once, before the first drain
        *self.dead_delegations.lock().expect("dead delegation lines") = lines;
    }

    /// Seeds the shared usage ledger with a saved per-agent tally
    ///
    /// Used by [`crate::Session::restore`] so a resumed run's usage summary and
    /// snapshots carry the whole session's totals, not just the new turns.
    ///
    /// # Arguments
    ///
    /// * `usage` - The persisted per-agent tallies to fold in
    pub async fn restore_usage(&self, usage: BTreeMap<String, resume::UsageTotals>) {
        // delegate to the shared usage ledger's bulk loader
        self.usage.restore(usage).await;
    }

    /// Forks a named side conversation off the main agent's settled history
    ///
    /// Copies the main conversation's settled messages (a mid-flight main
    /// turn's pending tail is deliberately not forked) into a fresh side
    /// conversation and persists it under `btw/<name>.json`. Returns `false`
    /// when a side with this name already exists -- the caller just switches to
    /// it -- and `true` when a fresh side was forked.
    ///
    /// # Arguments
    ///
    /// * `name` - The side conversation's name
    #[instrument(name = "ThorPal::fork_side", skip_all, err(Debug))]
    pub async fn fork_side(&self, name: &str) -> Result<bool, anyhow::Error> {
        // refuse names that could escape the btw directory or collide with main
        store::validate_btw_name(name).map_err(|msg| anyhow::anyhow!(msg))?;
        // hold the map lock across the check and insert so two concurrent
        // forks of the same name can never both claim it
        let mut sides = self.sides.lock().await;
        // an existing side is switched to, never re-forked over
        if sides.contains_key(name) {
            return Ok(false);
        }
        // copy the main conversation's settled history as the fork point
        let messages = self
            .memory
            .load(&self.conversation_id)
            .await
            .map_err(anyhow::Error::from)?;
        // assemble the fresh side's state and claim its name in the map
        let state = sides::SideState {
            messages,
            created_at: chrono::Utc::now(),
        };
        let state = Arc::new(tokio::sync::Mutex::new(state));
        sides.insert(name.to_owned(), Arc::clone(&state));
        // release the map before touching disk so other sides keep moving
        drop(sides);
        // persist the fork point, logging and swallowing a failure so a write
        // error degrades durability rather than failing the fork
        self.save_side(name, &*state.lock().await).await;
        Ok(true)
    }

    /// Runs one conversational turn inside a named side conversation
    ///
    /// The turn runs through the *same built main agent* -- identical preamble,
    /// tools, and shared state -- but with the side's own history supplied as a
    /// per-run override, so the main conversation's memory (and its
    /// `chat.json`) is never touched. The side's lock is held for the whole
    /// turn, serializing turns within one side while other sides and the main
    /// conversation run concurrently. Sub-agents spawned during the turn mint
    /// `btw:<name>/`-prefixed display names via the scope marker, so the whole
    /// turn's usage attributes to this side.
    ///
    /// # Arguments
    ///
    /// * `name` - The side conversation to run the turn in
    /// * `prompt` - The user's message for this turn
    #[instrument(name = "ThorPal::side_prompt", skip_all, err(Debug))]
    pub async fn side_prompt(&self, name: &str, prompt: &str) -> Result<String, anyhow::Error> {
        // the side's scoped ledger key and event attribution name
        let agent_key = format!("btw:{name}");
        // put the user's message on the event stream first, so the persisted
        // transcript interleaves it ahead of the turn's activity
        if let Some(tx) = &self.events {
            let _ = tx.send(events::ThorPalEvent::UserMessage {
                text: prompt.to_owned(),
                scope: Some(name.to_owned()),
            });
        }
        // fetch the side's state handle under a short map lock
        let state = {
            let sides = self.sides.lock().await;
            match sides.get(name) {
                Some(state) => Arc::clone(state),
                None => anyhow::bail!("no side conversation named '{name}' exists"),
            }
        };
        // hold the side's own lock for the whole turn so its turns serialize
        let mut state = state.lock().await;
        // seed the shared tool context plus this side's scope marker, so every
        // sub-agent and swarm spawned under this turn attributes to the side
        let mut context = self.tool_context();
        context.insert(agents::tool::ScopeMarker {
            prefix: agent_key.clone(),
        });
        // run the turn against the side's own history; the override bypasses
        // the main conversation's memory entirely (no load and no save)
        // mirror the turn's usage so a failure can still bill the ledger
        let turn_usage = agents::tool::TurnUsageMirror::default();
        let result = self
            .agent
            .prompt(prompt)
            .history(state.messages.clone())
            .tool_context(context)
            .add_hook(agents::tool::AgentEventHook {
                main_length_retry: std::sync::atomic::AtomicBool::new(true),
                agent: agent_key.clone(),
                coverage: Arc::clone(&self.coverage),
                events: self.events.clone(),
                // side conversations run no turn budget: no warnings or extensions
                budget: None,
                turn_usage: Some(turn_usage.clone()),
                // side conversations keep no per-invocation agent record
                record: None,
            })
            .extended_details()
            .await;
        // surface a failed turn onto the event sink so the side's tab shows it
        let response = match result {
            Ok(response) => response,
            Err(err) => {
                // a shutdown-time interruption freezes the side turn like the
                // main one: bill the spend and persist it, but skip the
                // failure event and salvage so no durable debris (or raw
                // marker text) marks the teardown; the side's in-flight tail
                // has no pending mirror, so it is lost either way
                if retry::is_shutdown_interrupt(&err) || self.gate.is_shutdown() {
                    tracing::info!(side = name, "side turn interrupted by shutdown; freezing");
                    self.usage.record(&agent_key, turn_usage.take()).await;
                    self.autosave_snapshot().await;
                    return Err(err.into());
                }
                // log what we can see; the raw failing body is preserved by
                // rig's own ERROR-level `body` log
                tracing::warn!(
                    side = name,
                    error = %err,
                    "side-conversation turn failed; see logs for the raw provider body"
                );
                // bill the failed turn's model calls under the side's key so
                // the persisted totals match the live per-turn events
                self.usage.record(&agent_key, turn_usage.take()).await;
                // mirror the failure so a consumer can render it in the side's tab
                if let Some(tx) = &self.events {
                    let _ = tx.send(events::ThorPalEvent::TurnFailed {
                        agent: agent_key,
                        error: err.to_string(),
                        request_id: err.provider_request_id().map(str::to_owned),
                    });
                }
                // salvage what the error carries beyond the side's settled
                // history (completion errors carry nothing; the budget-cap and
                // cancellation errors carry the canonical history)
                let tail = agents::salvage::salvaged_tail(&err, state.messages.len());
                // repair and settle the salvaged tail into the side's history
                if !tail.is_empty() {
                    if let Some(repaired) = memory::repair_interrupted_tail(tail, &err.to_string())
                    {
                        state.messages.extend(repaired);
                        // persist the side's salvaged history, best-effort
                        self.save_side(name, &state).await;
                    }
                }
                return Err(err.into());
            }
        };
        // fold the turn's new messages into the side's settled history
        state
            .messages
            .extend(response.messages.clone().unwrap_or_default());
        // fold this turn's usage into the shared ledger under the side's key
        self.usage.record(&agent_key, response.usage).await;
        // persist the side's settled history, best-effort
        self.save_side(name, &state).await;
        // autosave the snapshot too so the updated usage map lands on disk
        self.autosave_snapshot().await;
        // mirror the final response onto the event sink, scoped to this side
        if let Some(tx) = &self.events {
            let _ = tx.send(events::ThorPalEvent::RunFinished {
                response: response.output.clone(),
                scope: Some(name.to_owned()),
            });
        }
        Ok(response.output)
    }

    /// Generates (or revises) a short summary of a named side conversation
    ///
    /// The summarizer's token usage is recorded under the side's `btw:<name>`
    /// ledger key, so a side pays for its own summarization.
    ///
    /// # Arguments
    ///
    /// * `name` - The side conversation to summarize
    /// * `prior` - The previously generated summary being revised, if any
    /// * `feedback` - The analyst's feedback steering the revision, if any
    #[instrument(name = "ThorPal::summarize_side", skip_all, err(Debug))]
    pub async fn summarize_side(
        &self,
        name: &str,
        prior: Option<&str>,
        feedback: Option<&str>,
    ) -> Result<String, anyhow::Error> {
        // clone the side's settled history under a short lock
        let messages = {
            let sides = self.sides.lock().await;
            let state = match sides.get(name) {
                Some(state) => Arc::clone(state),
                None => anyhow::bail!("no side conversation named '{name}' exists"),
            };
            drop(sides);
            let state = state.lock().await;
            state.messages.clone()
        };
        // run the one-shot summarizer over the cloned history
        sides::summarize(self, name, &messages, prior, feedback).await
    }

    /// Injects a note into the main conversation as a persistent user message
    ///
    /// Used to bring an accepted side-conversation summary back into the main
    /// conversation: the note lands in the main agent's memory (persisting to
    /// `chat.json`) so the agent sees it on its next turn, and a scope-less
    /// `UserMessage` event is emitted so the transcript shows it on replay.
    /// If a main turn is in flight, the note appends ahead of that turn's
    /// settle -- harmless, since the next turn loads the full history.
    ///
    /// # Arguments
    ///
    /// * `text` - The note to inject into the main conversation
    #[instrument(name = "ThorPal::inject_main_note", skip_all, err(Debug))]
    pub async fn inject_main_note(&self, text: &str) -> Result<(), anyhow::Error> {
        // append the note to the main memory, which persists chat.json itself
        self.memory
            .append(&self.conversation_id, vec![Message::user(text)])
            .await
            .map_err(anyhow::Error::from)?;
        // put the note on the event stream so the transcript replays it
        if let Some(tx) = &self.events {
            let _ = tx.send(events::ThorPalEvent::UserMessage {
                text: text.to_owned(),
                scope: None,
            });
        }
        Ok(())
    }

    /// Closes a named side conversation, deleting its persisted history
    ///
    /// Closing a side that does not exist is a no-op so a double-close (e.g. a
    /// skip racing a tab close) never errors.
    ///
    /// # Arguments
    ///
    /// * `name` - The side conversation to close
    #[instrument(name = "ThorPal::close_side", skip_all, err(Debug))]
    pub async fn close_side(&self, name: &str) -> Result<(), anyhow::Error> {
        // drop the side's live state from the map
        self.sides.lock().await.remove(name);
        // delete its persisted history too, when persistence is on
        if let Some(store) = &self.store {
            store.delete_btw(name).await?;
        }
        Ok(())
    }

    /// Seeds the side-conversation map with saved histories on resume
    ///
    /// # Arguments
    ///
    /// * `saved` - The persisted side histories to rehydrate
    pub async fn restore_sides(&self, saved: Vec<resume::SideHistory>) {
        // rehydrate each saved side into the live map
        let mut sides = self.sides.lock().await;
        for side in saved {
            sides.insert(
                side.name,
                Arc::new(tokio::sync::Mutex::new(sides::SideState {
                    messages: side.messages,
                    created_at: side.created_at,
                })),
            );
        }
    }

    /// The names of every live side conversation, in stable sorted order
    pub async fn side_names(&self) -> Vec<String> {
        // the map is a BTreeMap, so its keys iterate in sorted order
        self.sides.lock().await.keys().cloned().collect()
    }

    /// Persists one side conversation's settled history, best-effort
    ///
    /// A no-op when no session store was configured; a write failure is logged
    /// and swallowed so durability degrades rather than failing the turn.
    ///
    /// # Arguments
    ///
    /// * `name` - The side conversation's name
    /// * `state` - The side's state to persist
    async fn save_side(&self, name: &str, state: &sides::SideState) {
        // skip entirely when session persistence was not requested
        let Some(store) = self.store.as_ref() else {
            return;
        };
        // assemble the persistable history from the live state
        let side = resume::SideHistory {
            version: resume::SIDE_HISTORY_VERSION,
            name: name.to_owned(),
            created_at: state.created_at,
            messages: state.messages.clone(),
        };
        // save it, logging and swallowing any failure
        if let Err(err) = store.save_btw(&side).await {
            tracing::warn!(?err, side = name, "failed to save side conversation");
        }
    }

    /// The model id this instance's provider talks to.
    pub fn model(&self) -> &str {
        // defer to the provider config this instance was built from
        self.provider.model()
    }

    /// A controller for observing and steering the session's pause state.
    pub fn pause_controller(&self) -> gate::PauseController {
        // hand out a handle onto the shared gate
        self.gate.controller()
    }

    /// Log the per-agent and total token-usage summary for this run.
    pub async fn log_usage(&self) {
        // delegate to the shared usage ledger
        self.usage.log_summary().await;
    }

    /// Log the per-agent and total function-coverage summary for this run.
    ///
    /// Fetches the total function count from Ghidra as the coverage
    /// denominator, then logs the breakdown. The count only ever asks an
    /// already-open bridge (anything else reports zero): this runs at session
    /// teardown, which must never boot a fresh JVM just to print coverage.
    pub async fn log_coverage(&self) {
        // the total is the denominator for the examined/total fraction
        let total = self.ghidra.total_function_count().await;
        // delegate to the shared coverage ledger
        self.coverage.log_summary(total).await;
    }

    /// Cleanly shut down this instance's Ghidra bridge and MCP connections,
    /// flushing analysis to disk.
    #[instrument(name = "ThorPal::shutdown", skip_all, err(Debug))]
    pub async fn shutdown(&self) -> Result<(), anyhow::Error> {
        // close the MCP connections first so stdio children die deterministically
        self.mcp.lock().await.shutdown().await;
        // delegate to the shared Ghidra session
        self.ghidra.shutdown().await
    }

    /// Whether a restored interrupted turn is still awaiting completion
    ///
    /// A consumer checks this after a restore to decide whether to drive
    /// [`ThorPal::resume_interrupted_turn`] (the TUI asks the operator first;
    /// headless continues automatically).
    pub async fn has_interrupted_turn(&self) -> bool {
        // the stash is cleared the moment the resume takes the turn
        self.interrupted.lock().await.is_some()
    }

    /// How many dangling delegation calls the held interrupted turn carries
    ///
    /// A consumer (e.g. the TUI's resume prompt) shows this so the operator
    /// knows how much work resuming will restart. Zero when no turn is held.
    pub async fn interrupted_delegations(&self) -> usize {
        // count only the calls the resume would actually re-run
        match self.interrupted.lock().await.as_ref() {
            Some(turn) => turn
                .dangling
                .iter()
                .filter(|call| self.is_delegation_tool(&call.tool))
                .count(),
            None => 0,
        }
    }

    /// The tool names of every delegation call this run attaches
    ///
    /// A consumer (e.g. the `thorctl` replay) uses this set to pair dangling
    /// delegation calls with the failure debris a resume supersedes.
    pub fn delegation_tool_names(&self) -> std::collections::BTreeSet<String> {
        // the workflow derives the set, swarm tool included
        self.workflow.delegation_tool_names()
    }

    /// Whether a tool name is one of this run's delegation calls
    ///
    /// # Arguments
    ///
    /// * `tool` - The tool name a dangling call carried
    fn is_delegation_tool(&self, tool: &str) -> bool {
        // every attached sub-agent's goal tool, plus the shared swarm tool
        self.delegations.specs.contains_key(tool) || tool == agents::swarm::SWARM_TOOL_NAME
    }

    /// Completes a restored interrupted turn, resuming its delegations
    ///
    /// The public entry an interactive consumer drives (and the loop entries
    /// call internally): the turn is completed through
    /// [`ThorPal::resume_interrupted_inner`], its usage is billed, the session
    /// autosaves, and the answer is mirrored out like any settled turn. A
    /// session with no held turn returns an empty answer as a no-op.
    #[instrument(name = "ThorPal::resume_interrupted_turn", skip_all, err(Debug))]
    pub async fn resume_interrupted_turn(&self) -> Result<String, PromptError> {
        // complete the held turn; a no-op when nothing is held
        let Some(response) = self.resume_interrupted_inner().await? else {
            return Ok(String::new());
        };
        // fold the turn's usage into the shared ledger under the main agent
        self.usage.record("main", response.usage).await;
        // autosave the whole session now that the turn has settled
        self.autosave_snapshot().await;
        // mirror the final response onto the event sink if a consumer is
        // listening, so the transcript shows the completed turn's answer
        if let Some(tx) = &self.events {
            let _ = tx.send(events::ThorPalEvent::RunFinished {
                response: response.output.clone(),
                scope: None,
            });
        }
        Ok(response.output)
    }

    /// Re-runs an interrupted turn's dangling delegations and continues it
    ///
    /// The heart of sub-agent resume. Each dangling call of the held tail is
    /// answered for real: a delegation whose record finished before the death
    /// reuses its saved answer, a running-shaped record resumes from its
    /// persisted partial transcript under the same display name, a call with
    /// no record starts fresh, an interrupted swarm reconstructs through
    /// [`agents::swarm::resume_swarm`], and every non-delegation call gets the
    /// same synthetic result the tail repair would have written. The tail is
    /// then settled and the turn continues by prompting the model with the
    /// real tool-results message -- the exact message rig itself would have
    /// sent next -- so the resumed conversation is byte-identical to one that
    /// never stopped. Returns `None` when no turn is held.
    ///
    /// The first follow-up's dead-delegation nudge is re-seeded here from the
    /// records the matchers left unconsumed (minus the consumed records'
    /// frozen descendants, which the revived owners re-issue themselves), so
    /// work this revival already handles is never re-queued.
    #[instrument(name = "ThorPal::resume_interrupted_inner", skip_all, err(Debug))]
    async fn resume_interrupted_inner(&self) -> Result<Option<PromptResponse>, PromptError> {
        // take the held turn so a crash mid-resume re-derives it from disk
        // (chat.json still holds the tail as pending) rather than re-entering
        let Some(turn) = self.interrupted.lock().await.take() else {
            return Ok(None);
        };
        tracing::info!(
            dangling = turn.dangling.len(),
            "resuming the interrupted turn's delegations"
        );
        // load the saved invocation records the dead run left behind; a load
        // failure degrades to fresh re-runs rather than failing the resume
        let mut records = match &self.store {
            Some(store) => store.load_agents().await.unwrap_or_else(|err| {
                tracing::warn!(
                    ?err,
                    "failed to load agent records; re-running delegations fresh"
                );
                Vec::new()
            }),
            None => Vec::new(),
        };
        // index the loaded records before the matchers consume any: the name,
        // whether it is running-shaped, and the calls frozen dangling in its
        // transcript -- enough for the leftover-derived nudge below without
        // holding a copy of every transcript across the revival
        let frozen: Vec<(String, bool, Vec<String>)> = records
            .iter()
            .map(|record| {
                // scan the record's transcript for its unanswered calls
                let calls = memory::dangling_calls(&record.messages)
                    .into_iter()
                    .map(|call| call.id.as_str().to_owned())
                    .collect();
                (record.name.clone(), record.finished_at.is_none(), calls)
            })
            .collect();
        // answer each dangling call in order, exactly like rig's own
        // sequential tool execution would have
        let mut results = Vec::with_capacity(turn.dangling.len());
        for call in &turn.dangling {
            // produce this call's result text through the matching path
            let text = if let Some(spec) = self.delegations.specs.get(&call.tool) {
                // a single delegation: parse the recorded {goal, prompt} args
                match agents::tool::parse_goal_args(&call.args) {
                    Some((goal, task)) => {
                        self.resume_delegation(spec, call, &goal, &task, &mut records)
                            .await
                    }
                    // damaged args cannot be re-run; answer synthetically
                    None => memory::interrupted_result_text("the session ended mid-turn"),
                }
            } else if call.tool == agents::swarm::SWARM_TOOL_NAME {
                // an interrupted swarm reconstructs member by member; grab a
                // member's internal call id first so the parent's open call
                // bubble can be closed by the finished event below
                let internal = records
                    .iter()
                    .find(|record| record.parent_call.as_deref() == Some(call.id.as_str()))
                    .and_then(|record| record.parent_internal_call.clone());
                // stamp the swarm call's identity into the shared member
                // context, exactly like the live dispatch boundary does
                let mut context = self.tool_context();
                context.insert(rig::tool::CurrentToolCall {
                    id: call.id.clone(),
                    provider: call.provider.clone(),
                    internal_call_id: internal
                        .clone()
                        .unwrap_or_else(|| call.id.as_str().to_owned()),
                });
                let report = agents::swarm::resume_swarm(
                    &self.delegations,
                    &call.args,
                    call.id.as_str(),
                    &mut records,
                    context,
                    &self.usage,
                    &self.coverage,
                    &self.events,
                    &self.store,
                )
                .await;
                // close the dead run's open spawn_swarm call with the report
                // -- unless a second shutdown interrupted the resume, which is
                // teardown noise the next resume supersedes, not an outcome
                if !report.starts_with(retry::SHUTDOWN_INTERRUPT_MARKER) {
                    self.emit_resumed_result(internal, &call.tool, &report, true);
                }
                report
            } else {
                // an ordinary tool call keeps today's synthetic answer
                memory::interrupted_result_text("the session ended mid-turn")
            };
            // answer the exact call id (and provider id) the model issued
            results.push(rig::message::UserContent::tool_result_for(
                call.id.clone(),
                call.provider.clone(),
                call.tool.clone(),
                vec![rig::message::ToolResultContent::text(text)],
            ));
        }
        // re-seed the dead-delegation nudge from the revival's leftovers:
        // whatever the matchers above consumed (stamped, legacy-fallback, and
        // swarm matches alike) is being handled live, so only the records
        // they left behind can still be dead
        let leftover: std::collections::HashSet<&str> =
            records.iter().map(|record| record.name.as_str()).collect();
        // a consumed never-finished record is revived and told to re-issue
        // the calls frozen in its transcript, so its children are not dead
        // either; a consumed FINISHED record is merely replayed -- nobody
        // re-issues its frozen calls -- so it must not seed the prune
        let re_issued: std::collections::HashSet<String> = frozen
            .into_iter()
            .filter(|(name, never_finished, _)| {
                *never_finished && !leftover.contains(name.as_str())
            })
            .flat_map(|(_, _, calls)| calls)
            .collect();
        store::prune_revived_descendants(&mut records, re_issued);
        let dead = store::dead_delegation_lines(&records);
        if !dead.is_empty() {
            tracing::info!(
                dead = dead.len(),
                "re-queueing delegations the revival leaves unhandled"
            );
        }
        self.restore_dead_delegations(dead);
        // settle the held tail now that its results exist: one durable step
        // moves it from pending to settled, and a crash after this point
        // re-enters through the settled-dangling detection instead
        if !turn.tail.is_empty() {
            self.memory
                .append(&self.conversation_id, turn.tail)
                .await
                .map_err(PromptError::MemoryError)?;
        }
        // continue the turn by prompting with the real tool-results message --
        // the same message rig would have sent next had the run survived; the
        // results message is deliberately NOT a UserMessage event (it is turn
        // machinery, not operator input)
        let results_message = Message::User { content: results };
        let response = match self.send_prompt(results_message.clone()).await {
            Ok(response) => response,
            Err(err) => {
                // a genuine death of the continuation must not strand the
                // settled tail's calls unanswered -- a shape providers reject
                // on every later turn. a shutdown interruption instead leaves
                // the shape frozen for the next resume (which rebuilds these
                // results from the now-finished records), and a turn that got
                // far enough to capture work was already repaired and settled
                // by the salvage pass inside send_prompt
                if !retry::is_shutdown_interrupt(&err) && !self.gate.is_shutdown() {
                    self.repair_stranded_results(results_message, &err).await;
                }
                return Err(err);
            }
        };
        Ok(Some(response))
    }

    /// Settles a failed continuation's real results if its calls are stranded
    ///
    /// Runs on a genuine (non-shutdown) failure of the resumed turn's
    /// continuation: when the turn died before its first completion, nothing
    /// captured the results message, so the freshly settled tail ends in
    /// unanswered tool calls -- a shape providers reject on every later turn.
    /// Appending the real results plus a closing note keeps the history valid
    /// and the delegations' paid-for work in the model's context. A no-op when
    /// the salvage pass already settled a captured partial turn (its repair
    /// answered the calls). Best-effort, like every salvage write.
    ///
    /// # Arguments
    ///
    /// * `results_message` - The tool-results message the failed turn carried
    /// * `error` - The error that ended the continuation
    async fn repair_stranded_results(&self, results_message: Message, error: &PromptError) {
        // read the settled history back; the pending mirror never holds the
        // results message (it is captured only on a completed model call)
        let history = match self.memory.load(&self.conversation_id).await {
            Ok(history) => history,
            Err(err) => {
                tracing::warn!(
                    ?err,
                    "failed to re-read history; leaving the stranded calls"
                );
                return;
            }
        };
        // a history whose calls are all answered was already repaired
        if memory::dangling_calls(&history).is_empty() {
            return;
        }
        // close the stranded calls with their real results and a closing note
        let closing = vec![
            results_message,
            Message::Assistant {
                id: None,
                content: vec![rig::message::AssistantContent::text(format!(
                    "[turn interrupted: {error}. The delegation results above are \
                     preserved context; continue from here.]"
                ))],
            },
        ];
        match self.memory.append(&self.conversation_id, closing).await {
            Ok(()) => {
                tracing::info!("settled the failed continuation's delegation results");
                // persist the snapshot so the repair survives the exit
                self.autosave_snapshot().await;
            }
            Err(err) => {
                tracing::warn!(?err, "failed to settle the stranded delegation results");
            }
        }
    }

    /// Resumes (or reconstructs) one dangling single-delegation call
    ///
    /// Dispatches on the matching record's state: finished records reuse
    /// their saved outcome verbatim, running-shaped records resume from their
    /// persisted partial transcripts under the same display name, and a call
    /// with no record starts fresh. The parent's open call bubble is closed
    /// with a finished event when the dead run's internal call id is known.
    ///
    /// # Arguments
    ///
    /// * `spec` - The delegation's retained agent, toolset, and budget
    /// * `call` - The dangling call being answered
    /// * `goal` - The goal the call carried
    /// * `task` - The task the call carried
    /// * `records` - The unmatched saved records, consumed on a match
    async fn resume_delegation(
        &self,
        spec: &agents::tool::DelegationSpec,
        call: &memory::DanglingCall,
        goal: &str,
        task: &str,
        records: &mut Vec<store::AgentRecord>,
    ) -> String {
        // match the call to the invocation the dead run recorded for it
        let record =
            agents::tool::take_matching_record(records, call.id.as_str(), &spec.role, goal, task);
        // a finished record's outcome is reused verbatim, never re-run; its
        // result line usually persisted live before the death, but the tee is
        // a detached task, so re-emitting here closes the bubble when that
        // write was lost (a duplicate line is skipped by the replay pairing)
        if let Some(record) = &record
            && record.finished_at.is_some()
        {
            let internal = record.parent_internal_call.clone();
            let run = agents::tool::rerun_from_record(record.clone());
            let text = delegation_result_text(&run);
            self.emit_resumed_result(internal, &call.tool, &text, run.output.is_ok());
            return text;
        }
        // remember the dead run's internal call id before the record moves,
        // so the parent's open call bubble can be closed below
        let internal = record
            .as_ref()
            .and_then(|record| record.parent_internal_call.clone());
        // a running-shaped record resumes; a recordless call starts fresh
        let invocation = match record {
            Some(record) => agents::tool::Invocation::Resumed { record },
            None => agents::tool::Invocation::Fresh {
                role: spec.role.clone(),
                goal: goal.to_owned(),
                task: task.to_owned(),
                swarm: None,
            },
        };
        // stamp the dangling call's identity into the context, exactly like
        // the live dispatch boundary does, so the record this run saves can
        // be matched back to the same call by any later resume
        let mut context = self.tool_context();
        context.insert(rig::tool::CurrentToolCall {
            id: call.id.clone(),
            provider: call.provider.clone(),
            internal_call_id: internal
                .clone()
                .unwrap_or_else(|| call.id.as_str().to_owned()),
        });
        let run = agents::tool::run_sub_agent(
            &spec.agent,
            invocation,
            context,
            &spec.enabled,
            spec.budget,
            &self.usage,
            &self.coverage,
            &self.events,
            &self.store,
            &self.delegations.alive,
            self.delegations.post_mortem.as_deref(),
        )
        .await;
        // close the dead run's open delegation call with the real outcome --
        // unless a second shutdown interrupted the resume, which is teardown
        // noise the next resume supersedes, not an outcome worth a line
        let interrupted = run.output.as_ref().is_err_and(|error| {
            error
                .to_string()
                .starts_with(retry::SHUTDOWN_INTERRUPT_MARKER)
        });
        let text = delegation_result_text(&run);
        if !interrupted {
            self.emit_resumed_result(internal, &call.tool, &text, run.output.is_ok());
        }
        text
    }

    /// Emits the finished event closing a dead run's open delegation call
    ///
    /// The dead run persisted a `ToolCallStarted` line for the delegation; a
    /// resumed result emitted under the same internal call id closes that open
    /// bubble in the TUI replay and lands the durable result line. Skipped
    /// when the id is unknown (a recordless call), leaving the bubble open.
    ///
    /// # Arguments
    ///
    /// * `internal` - The dead run's internal call id, when a record kept it
    /// * `tool` - The delegation tool's name
    /// * `result` - The result text answering the call
    /// * `success` - Whether the delegation succeeded
    fn emit_resumed_result(
        &self,
        internal: Option<String>,
        tool: &str,
        result: &str,
        success: bool,
    ) {
        // without the dead run's call id there is no bubble to close
        let Some(call_id) = internal else {
            return;
        };
        // mirror the result onto the event sink if a consumer is listening
        if let Some(tx) = &self.events {
            let _ = tx.send(events::ThorPalEvent::ToolCallFinished {
                agent: "main".to_owned(),
                call_id,
                tool: tool.to_owned(),
                result: result.to_owned(),
                success,
            });
        }
    }

    /// Drive the main agent until the forum's questions are resolved.
    ///
    /// This re-prompts the main agent in a loop: the first iteration uses the
    /// caller's prompt, and each later iteration nudges the agent to resolve the
    /// questions still open on the forum and raise any it has missed. The loop
    /// stops once the agent calls the `finish` tool AND no question remains
    /// unsolved or needing evidence, or once the iteration cap is hit as a
    /// backstop. It returns the last assistant response.
    ///
    /// # Arguments
    ///
    /// * `prompt` - The opening prompt for the first iteration
    #[instrument(name = "ThorPal::run", skip_all, err(Debug))]
    pub async fn run(&self, prompt: impl Into<String>) -> Result<String, anyhow::Error> {
        // drive the shared loop with the caller's opening prompt for iteration 1
        self.run_from(Some(prompt.into())).await
    }

    /// Drive the control loop of a restored run, with no new opening prompt.
    ///
    /// The first iteration uses the follow-up nudge like any later one, so a
    /// crashed or interrupted headless run can be continued over its restored
    /// forum and conversation without inventing a user message.
    #[instrument(name = "ThorPal::run_resumed", skip_all, err(Debug))]
    pub async fn run_resumed(&self) -> Result<String, anyhow::Error> {
        // drive the shared loop with no opening prompt
        self.run_from(None).await
    }

    /// Drive the control loop, optionally opening with a caller-supplied prompt.
    ///
    /// When `opening` is `Some`, the first iteration uses it verbatim; when
    /// `None` (a restored run), the first iteration uses the follow-up prompt
    /// like any later one. After each iteration settles, the whole session is
    /// autosaved when a session store is configured, so a crash loses at most
    /// the in-flight iteration.
    ///
    /// A turn-budget exhaustion does not end the loop: the salvage pass has
    /// already appended the repaired tail as settled history, so the next
    /// iteration re-prompts and resumes from the paid-for context (bounded by
    /// `MAX_ITERATIONS`); a run where *every* iteration burned its budget
    /// still returns an error rather than an empty answer. Every other failed
    /// iteration ends the loop: with the per-request retry budget below it, a
    /// surfaced error is deterministic (a request fault, an operator abort),
    /// so re-prompting would just re-send a strictly larger prompt into the
    /// same failure. A run with at least one settled iteration still returns
    /// its accumulated answer; a run where nothing ever settled returns the
    /// error so a headless caller exits nonzero instead of printing an empty
    /// response and exiting 0.
    ///
    /// # Arguments
    ///
    /// * `opening` - The opening prompt for iteration 1, or `None` to resume
    #[instrument(name = "ThorPal::run_from", skip_all, err(Debug))]
    async fn run_from(&self, opening: Option<String>) -> Result<String, anyhow::Error> {
        // the last assistant response, returned to the caller when the loop ends
        let mut response = String::new();
        // how many iterations settled, deciding partial-success vs failure
        let mut successes = 0usize;
        // the last turn-budget error the loop swallowed to re-prompt, kept so
        // an all-MaxTurns run can fail loud instead of returning nothing
        let mut last_turn_budget_error: Option<PromptError> = None;
        // a restored interrupted turn is completed before the loop, so the
        // resumed delegations' real results re-enter the conversation first
        if self.has_interrupted_turn().await {
            match self.resume_interrupted_inner().await {
                // another caller raced the stash away; the loop proceeds
                Ok(None) => (),
                // the completed turn counts as a settled iteration
                Ok(Some(extended)) => {
                    successes += 1;
                    // fold the turn's usage into the ledger under the main agent
                    self.usage.record("main", extended.usage).await;
                    // keep the answer and mirror it so the transcript shows it
                    response = extended.output;
                    if let Some(tx) = &self.events {
                        let _ = tx.send(events::ThorPalEvent::RunFinished {
                            response: response.clone(),
                            scope: None,
                        });
                    }
                    // autosave the settled turn before the loop takes over
                    self.autosave_snapshot().await;
                    // the completed turn may already satisfy the finish gate
                    let finished = self.forum.take_finish_requested();
                    let open = self.forum.open_question_count().await;
                    if finished && open == 0 {
                        return Ok(response);
                    }
                }
                // a failed completion routes through the shared decision table
                Err(err) => match iteration_outcome(&err, successes) {
                    // a budget death salvaged its tail; the loop re-prompts over it
                    IterationOutcome::Reprompt => {
                        tracing::warn!(
                            error = %err,
                            "the resumed turn hit its budget; re-prompting from the salvaged context"
                        );
                        last_turn_budget_error = Some(err);
                    }
                    // the process is shutting down again: frozen for the next resume
                    IterationOutcome::Interrupted => {
                        tracing::info!("the resumed turn was interrupted by shutdown again");
                        return Err(anyhow::Error::new(err)
                            .context("the run was interrupted by shutdown and can be resumed"));
                    }
                    // nothing settled, so the failure ends the run loud
                    IterationOutcome::Fatal | IterationOutcome::Truncate => {
                        return Err(
                            anyhow::Error::new(err).context("the resumed interrupted turn failed")
                        );
                    }
                },
            }
        }
        // re-prompt the agent until the forum settles or we hit the safety cap
        for iteration in 1..=MAX_ITERATIONS {
            // iteration 1 of a fresh run uses the caller's prompt; a resumed run
            // (no opening) and every later iteration nudge the agent to close out
            // whatever questions are still open on the forum
            let turn_prompt = match (iteration, &opening) {
                (1, Some(opening)) => {
                    // the caller's opening prompt is a user message, so put it
                    // on the event stream ahead of the iteration's activity
                    // (the auto-generated follow-up nudges are not)
                    if let Some(tx) = &self.events {
                        let _ = tx.send(events::ThorPalEvent::UserMessage {
                            text: opening.clone(),
                            scope: None,
                        });
                    }
                    opening.clone()
                }
                _ => self.followup_prompt().await,
            };
            // prompt the agent through the shared dispatcher, which captures
            // the token usage rig aggregated across this iteration's whole run
            let extended = match self.send_prompt(&turn_prompt).await {
                // a completed iteration feeds the running answer and status checks
                Ok(extended) => extended,
                // a failed iteration is handled per the shared decision table:
                // a turn-budget exhaustion re-prompts over its salvaged tail,
                // while every other surfaced error is deterministic (the retry
                // layer already rode out every transient fault), so re-prompting
                // would re-send a strictly larger prompt into the same failure
                Err(err) => match iteration_outcome(&err, successes) {
                    // the turn budget ran out mid-iteration: salvage already
                    // appended the repaired tail as settled history, so the
                    // next follow-up prompt resumes from the paid-for context
                    // (the MAX_ITERATIONS cap bounds how often this can recur)
                    IterationOutcome::Reprompt => {
                        tracing::warn!(
                            iteration,
                            error = %err,
                            "iteration hit its turn budget; re-prompting from the salvaged context"
                        );
                        // remember the swallowed error for the post-loop guard
                        last_turn_budget_error = Some(err);
                        continue;
                    }
                    // a run that settled nothing has nothing to return: fail
                    // loud so a headless caller exits nonzero
                    IterationOutcome::Fatal => {
                        return Err(
                            anyhow::Error::new(err).context("the run never completed an iteration")
                        );
                    }
                    // the process is shutting down with the iteration frozen
                    // for resume: surface the interruption so the headless
                    // caller exits as resumable instead of failed
                    IterationOutcome::Interrupted => {
                        tracing::info!(
                            iteration,
                            "run interrupted by shutdown; the session resumes from here"
                        );
                        return Err(anyhow::Error::new(err)
                            .context("the run was interrupted by shutdown and can be resumed"));
                    }
                    // keep a partially-successful run's accumulated answer and
                    // forum state; the wrapper already warned and emitted the
                    // TurnFailed event for the consumer's log
                    IterationOutcome::Truncate => {
                        tracing::error!(
                            iteration,
                            error = %err,
                            "iteration failed after retries; returning the run so far"
                        );
                        break;
                    }
                },
            };
            // count the settled iteration toward partial success
            successes += 1;
            // fold this iteration's usage into the shared ledger under the main agent
            self.usage.record("main", extended.usage).await;
            // keep the assistant text as the running answer
            response = extended.output;
            tracing::debug!(iteration, %response, "iteration response");
            // autosave the whole session now that this iteration has settled, so a
            // crash before the next iteration can be resumed from here
            self.autosave_snapshot().await;
            // read-and-clear the finish request so a stale signal from an earlier
            // iteration cannot terminate this one
            let finished = self.forum.take_finish_requested();
            // count how many questions are still unsolved or awaiting evidence
            let open = self.forum.open_question_count().await;
            tracing::debug!(iteration, finished, open, "iteration status");
            // stop once the agent has declared it is done and nothing is still open
            if finished && open == 0 {
                break;
            }
        }
        // a run where every iteration was swallowed as a turn-budget re-prompt
        // never settled anything: fail loud instead of returning an empty
        // response, naming the last budget error so the caller sees why
        if successes == 0 {
            return Err(exhausted_run_error(last_turn_budget_error));
        }
        Ok(response)
    }

    /// Builds the re-prompt used on every iteration after the first.
    ///
    /// Lists the questions still open on the forum (and how many completion-gate
    /// tasks on them are incomplete) so the agent knows exactly what is left, and
    /// instructs it to resolve them, raise any it has missed, and only then call
    /// `finish`.
    async fn followup_prompt(&self) -> String {
        // pull the open-question and incomplete-task counts in one consistent
        // snapshot so the re-prompt never contradicts itself
        let (open, incomplete) = self.forum.open_work_counts().await;
        let mut prompt = format!(
            "There are still {open} open question(s) on the forum (status unsolved or \
             needs_evidence). Call list_questions with status \"open\" to see them. \
             Delegate the questions that are independent of each other together in one \
             spawn_swarm call (one task per question), and delegate any question that \
             depends on another's answer to the reverse_engineer sub-agent on its own, one \
             at a time (calling its tool with the top-level goal and that question as the \
             task). Delegate questions that need domain or family knowledge, prior-run \
             techniques, or hypothesis framing to the researcher sub-agent the same way. \
             For each answer, verify it and post the supporting evidence as comment(s) under the question, and set_status \
             to \"solved\" (answer accepted with evidence) or \"blocked\" (cannot be \
             answered due to missing tools or information). If the open questions do not \
             fully cover the user's request, raise new question posts for the gaps and \
             delegate those too. Once every question is resolved and you have nothing left \
             to ask, call finish."
        );
        // call out standing completion gates so the agent clears them explicitly
        if incomplete > 0 {
            prompt.push_str(&format!(
                " {incomplete} task(s) on open questions are incomplete; each must be \
                 completed with complete_task, citing the comment holding the evidence \
                 that satisfies it, before its question can be marked solved."
            ));
        }
        // surface delegations that died unfinished in the run this session
        // resumed from, exactly once (the drain empties the list) -- the model
        // re-delegates through the normal tools with fresh names and budgets
        let dead =
            std::mem::take(&mut *self.dead_delegations.lock().expect("dead delegation lines"));
        if !dead.is_empty() {
            prompt.push_str(&format!(
                "\n\n{} earlier delegation(s) died without finishing (killed with the \
                 previous process or by a provider fault). Their tasks, from the \
                 session's delegation records:\n{}\nFirst check the forum (including \
                 any #salvage- posts) for work that was already completed or \
                 superseded; then re-delegate whatever remains as fresh, narrower \
                 tasks through the normal sub-agent tools. Do not re-run a task whose \
                 findings are already on the forum.",
                dead.len(),
                dead.join("\n")
            ));
        }
        prompt
    }
}

/// The control loop's decision for a failed iteration
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IterationOutcome {
    /// Re-prompt on the next iteration from the salvaged context
    Reprompt,
    /// Fail the whole run: nothing has settled and the error is deterministic
    Fatal,
    /// Keep the partial run's accumulated answer and stop iterating
    Truncate,
    /// The process is shutting down: surface the interruption so the caller
    /// exits as resumable, leaving the frozen state for a later resume
    Interrupted,
}

/// Decides how the control loop responds to a failed iteration
///
/// A turn-budget exhaustion ([`PromptError::MaxTurnsError`]) is re-prompted:
/// the salvage pass already appended the repaired tail as settled history, so
/// the next follow-up prompt resumes from the paid-for context (bounded by
/// the loop's `MAX_ITERATIONS` cap). Every other surfaced error is
/// deterministic -- the per-request retry budget below already rode out the
/// transient faults -- so re-prompting would re-send a strictly larger prompt
/// into the same failure: with nothing settled the run fails loud, and with
/// settled iterations the accumulated answer is kept instead.
///
/// # Arguments
///
/// * `err` - The error that ended the iteration
/// * `successes` - How many iterations have settled so far
fn iteration_outcome(err: &PromptError, successes: usize) -> IterationOutcome {
    match err {
        // the turn budget ran out mid-iteration: the salvaged tail keeps the
        // paid-for work, so a fresh iteration continues instead of discarding it
        PromptError::MaxTurnsError { .. } => IterationOutcome::Reprompt,
        // the process is shutting down: the iteration's state froze for
        // resume, so surface the interruption instead of a failure
        _ if retry::is_shutdown_interrupt(err) => IterationOutcome::Interrupted,
        // a run that settled nothing has nothing to return: fail loud
        _ if successes == 0 => IterationOutcome::Fatal,
        // keep a partially-successful run's accumulated answer and forum state
        _ => IterationOutcome::Truncate,
    }
}

/// Whether a run-level error is a shutdown-time interruption
///
/// A consumer (e.g. headless `thorctl analyze`) maps an interrupted run onto
/// the "temporary failure, resume later" exit instead of a hard failure: the
/// session's state froze cleanly, so resuming it continues the interrupted
/// work. The check walks the error chain for the shutdown marker the gate's
/// abort arms stamp onto surfaced requests.
///
/// # Arguments
///
/// * `err` - The run-level error to classify
pub fn is_interrupted_error(err: &anyhow::Error) -> bool {
    // the marker survives every context wrap, so a chain walk finds it
    err.chain()
        .any(|cause| cause.to_string().contains(retry::SHUTDOWN_INTERRUPT_MARKER))
}

/// The parent-visible result text for a resumed delegation's outcome
///
/// Mirrors what rig injects for a live delegation: the sub-agent's answer on
/// success, and the error's model feedback (the salvaged handoff) on failure.
///
/// # Arguments
///
/// * `run` - The resumed delegation's outcome
fn delegation_result_text(run: &agents::tool::SubAgentRun) -> String {
    match &run.output {
        // a success hands its answer back verbatim
        Ok(text) => text.clone(),
        // a failure hands back its model feedback, as a live tool result would
        Err(error) => error
            .model_feedback()
            .map(str::to_owned)
            .unwrap_or_else(|| error.to_string()),
    }
}

/// Builds the error for a run where no iteration ever settled
///
/// Only an all-[`PromptError::MaxTurnsError`] run reaches the loop's end with
/// nothing settled (any other failure returns or breaks out immediately), so
/// the last swallowed budget error is surfaced with context naming why the
/// run has no answer to return.
///
/// # Arguments
///
/// * `last_turn_budget_error` - The last turn-budget error the loop swallowed
fn exhausted_run_error(last_turn_budget_error: Option<PromptError>) -> anyhow::Error {
    // surface the swallowed budget error when one was captured; nothing else
    // can reach the guard, but a bare error beats a panic if that ever changes
    let err = match last_turn_budget_error {
        Some(err) => anyhow::Error::new(err),
        None => anyhow::anyhow!("no iteration ever ran"),
    };
    // name the run-level failure so a headless caller's exit explains itself
    err.context("the run never completed an iteration: the model hit the turn budget every time")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Build a sub-agent config with the given enabled tools
    ///
    /// # Arguments
    ///
    /// * `name` - The name to give this agent
    /// * `tools` - The tool groups and/or tools this agent gets
    fn agent(name: &str, tools: Vec<&str>) -> AgentConfig {
        AgentConfig {
            name: name.to_owned(),
            description: "desc".to_owned(),
            preamble: "preamble".to_owned(),
            tools: tools.into_iter().map(String::from).collect(),
            disabled_tools: Vec::new(),
            read_only: false,
            max_turns: 1,
            max_extension_turns: None,
            warn_turns: None,
            max_swarm_size: 1,
            agents: Vec::new(),
            max_nested_alive: 6,
        }
    }

    /// Build a ThorPal builder against a local url with an optional Ghidra install
    ///
    /// # Arguments
    ///
    /// * `ghidra_install` - The Ghidra install to configure if any
    fn builder(ghidra_install: Option<PathBuf>) -> ThorPalBuilder {
        // build settings for a dummy binary; nothing touches disk until a tool runs
        let settings = GhidraSettings::from_binary("bin/target", "GhidraProjects", ghidra_install);
        // an explicit litellm provider config pointed at a dead local port
        let provider = AiProviderConfig::LiteLlm {
            endpoint: "http://127.0.0.1:1".to_owned(),
            api_key: Some("key".to_owned()),
            model: "model".to_owned(),
        };
        ThorPalBuilder::new(provider, settings)
    }

    /// The build appends one offered-tool manifest per role, plus `"main"`
    #[tokio::test]
    async fn build_records_offered_tool_manifests() {
        // a store the build's manifests land in
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SessionStore::create(
            dir.path().to_path_buf(),
            None,
            PathBuf::from("bin/target"),
            "target".to_owned(),
            false,
            Vec::new(),
        )
        .await
        .expect("store should create");
        // build a run holding one file-tool agent with the store installed
        let built = builder(None)
            .agent(agent("A", vec!["file"]))
            .session_store(Arc::clone(&store))
            .build()
            .await
            .expect("build should succeed");
        drop(built);
        // exactly two manifests landed: the role's, then the main agent's
        let lines = store.load_transcript().await.expect("load");
        let manifests: Vec<_> = lines
            .iter()
            .filter_map(|line| match &line.item {
                transcript::TranscriptItem::ToolsOffered {
                    role,
                    tools,
                    version,
                } => Some((role.clone(), tools.clone(), version.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(manifests.len(), 2, "one manifest per role plus main");
        // the role's roster carries its whole registry group, in sorted
        // order, plus the always-on orchestration tools beyond it
        let (role, tools, version) = &manifests[0];
        assert_eq!(role, "A");
        for entry in tools::registry::group("file") {
            assert!(
                tools.iter().any(|name| name == entry.name),
                "the file tool '{}' must be offered",
                entry.name
            );
        }
        assert!(
            tools.len() > tools::registry::group("file").len(),
            "always-on orchestration tools must be offered too"
        );
        assert!(
            tools.windows(2).all(|pair| pair[0] < pair[1]),
            "the roster must be sorted and deduplicated"
        );
        assert_eq!(version, env!("CARGO_PKG_VERSION"));
        // the main agent's roster lands under the reserved "main" role
        let (role, tools, version) = &manifests[1];
        assert_eq!(role, "main");
        assert!(!tools.is_empty(), "the main agent is never tool-less");
        assert!(
            tools.windows(2).all(|pair| pair[0] < pair[1]),
            "the roster must be sorted and deduplicated"
        );
        assert_eq!(version, env!("CARGO_PKG_VERSION"));
    }

    /// MCP reference failures surface at build time with actionable errors,
    /// even on builder paths that never ran `Workflow::validate`.
    #[tokio::test]
    async fn mcp_reference_failures_surface_at_build() {
        // an agent referencing an unconfigured server fails the build cleanly
        let result = builder(None)
            .agent(agent("A", vec!["mcp:ghost"]))
            .build()
            .await;
        let error = format!("{:#}", result.err().expect("build should fail"));
        assert!(error.contains("unconfigured MCP server 'ghost'"));
        // referencing the built-in Thorium server without its flag fails too
        let result = builder(None)
            .agent(agent("A", vec!["mcp:thorium"]))
            .build()
            .await;
        let error = format!("{:#}", result.err().expect("build should fail"));
        assert!(error.contains("thorium_mcp"));
        // a workflow enabling the built-in server still needs a resolver
        let workflow = Workflow {
            agents: vec![agent("A", vec!["mcp:thorium"])],
            max_alive_sub_agents: 1,
            forum_context: ForumContextMode::default(),
            forum_link_depth: 2,
            context_window_tokens: 0,
            analyst_asks: true,
            thorium_mcp: true,
            mcp_servers: BTreeMap::new(),
        };
        let result = builder(None).workflow(workflow).build().await;
        let error = format!("{:#}", result.err().expect("build should fail"));
        assert!(error.contains("resolver"));
    }

    /// Forking a side copies the main history without touching chat.json,
    /// an injected note lands only in the main conversation, and closing a
    /// side deletes its persisted history
    #[tokio::test]
    async fn side_conversations_never_touch_main_chat() {
        // build a persisted ThorPal around a scratch session store
        let dir = tempfile::tempdir().expect("tempdir");
        let store = SessionStore::create(
            dir.path().to_path_buf(),
            None,
            PathBuf::from("bin/target"),
            "target".to_owned(),
            false,
            Vec::new(),
        )
        .await
        .expect("store should create");
        let thorpal = builder(None)
            .session_store(Arc::clone(&store))
            .build()
            .await
            .expect("thorpal should build");
        // seed the main conversation with a settled history
        thorpal
            .restore_memory(vec![Message::user("hello"), Message::assistant("hi")])
            .await
            .expect("restore");
        // remember the persisted main history before any side activity
        let before = store.load_chat().await.expect("load").expect("chat");
        assert_eq!(before.messages.len(), 2);
        // a fresh fork copies the settled history and claims the name
        assert!(thorpal.fork_side("crypto").await.expect("fork"));
        // forking the same name again just switches to it
        assert!(!thorpal.fork_side("crypto").await.expect("refork"));
        // the fork landed on disk with the copied history
        let sides = store.load_btws().await.expect("load sides");
        assert_eq!(sides.len(), 1);
        assert_eq!(sides[0].name, "crypto");
        assert_eq!(sides[0].messages.len(), 2);
        assert_eq!(thorpal.side_names().await, ["crypto"]);
        // the main conversation's chat.json is byte-identical to before
        let after = store.load_chat().await.expect("load").expect("chat");
        assert_eq!(after.messages, before.messages);
        // an invalid side name is refused before anything is claimed
        assert!(thorpal.fork_side("main").await.is_err());
        // an accepted note appends to the main conversation only
        thorpal
            .inject_main_note("[Summary returned from side conversation 'crypto']\n\nfindings")
            .await
            .expect("inject");
        let after = store.load_chat().await.expect("load").expect("chat");
        assert_eq!(after.messages.len(), 3);
        let sides = store.load_btws().await.expect("load sides");
        assert_eq!(sides[0].messages.len(), 2);
        // closing the side drops it from the map and from disk
        thorpal.close_side("crypto").await.expect("close");
        assert!(thorpal.side_names().await.is_empty());
        assert!(store.load_btws().await.expect("load sides").is_empty());
    }

    /// An agent using the Ghidra tools fails to build without a Ghidra install
    #[tokio::test]
    async fn ghidra_agent_requires_install() {
        // try to build a ThorPal whose sub-agent needs the Ghidra tools
        let result = builder(None)
            .agent(agent("ReverseEngineer", vec!["ghidra"]))
            .build()
            .await;
        // the build must fail and name the offending agent
        let error = result
            .err()
            .expect("build should fail without a ghidra install");
        assert!(error.to_string().contains("ReverseEngineer"));
    }

    /// Delegating agents build callees-first regardless of config order
    #[tokio::test]
    async fn delegating_agents_build() {
        // list the callee after its caller so only a topological build works
        let mut caller = agent("ReverseEngineer", vec![]);
        caller.agents = vec!["developer".to_owned()];
        let callee = agent("Developer", vec!["transforms", "scripts"]);
        // the build must order the developer first and attach its goal tool
        let result = builder(None).agent(caller).agent(callee).build().await;
        assert!(result.is_ok(), "{:?}", result.err());
    }

    /// A delegation cycle fails the build with a cycle error
    #[tokio::test]
    async fn delegation_cycles_fail_the_build() {
        // wire two agents into a cycle without going through a workflow load
        let mut alpha = agent("Alpha", vec![]);
        alpha.agents = vec!["beta".to_owned()];
        let mut beta = agent("Beta", vec![]);
        beta.agents = vec!["alpha".to_owned()];
        // the build itself must reject the cycle, naming the agents
        let error = builder(None)
            .agent(alpha)
            .agent(beta)
            .build()
            .await
            .err()
            .expect("a delegation cycle should fail the build");
        assert!(error.to_string().contains("cycle"));
    }

    /// Agents that do not use the Ghidra tools build without a Ghidra install
    #[tokio::test]
    async fn non_ghidra_agents_build_without_install() {
        // build a ThorPal whose only sub-agent does not use Ghidra
        let result = builder(None)
            .agent(agent("Researcher", vec![]))
            .build()
            .await;
        assert!(result.is_ok());
    }

    /// A single Ghidra tool enabled by name still demands a Ghidra install
    #[tokio::test]
    async fn single_ghidra_tool_requires_install() {
        // try to build a ThorPal whose sub-agent enables one Ghidra tool by name
        let result = builder(None)
            .agent(agent("ReverseEngineer", vec!["decomp_function"]))
            .build()
            .await;
        // the build must fail and name both the agent and the tool
        let error = result
            .err()
            .expect("build should fail without a ghidra install");
        assert!(error.to_string().contains("ReverseEngineer"));
        assert!(error.to_string().contains("decomp_function"));
    }

    /// Disabling every Ghidra tool lifts the Ghidra install requirement
    #[tokio::test]
    async fn fully_disabled_ghidra_group_builds_without_install() {
        // enable the ghidra group but disable every tool in it
        let mut config = agent("ReverseEngineer", vec!["ghidra"]);
        config.disabled_tools = tools::registry::group("ghidra")
            .iter()
            .map(|entry| entry.name.to_owned())
            .collect();
        // with no ghidra tool left enabled no install should be required
        let result = builder(None).agent(config).build().await;
        assert!(result.is_ok());
    }

    /// An agent using the Ghidra tools builds once a Ghidra install is set
    #[tokio::test]
    async fn ghidra_agent_builds_with_install() {
        // build a ThorPal whose sub-agent needs Ghidra with an install configured
        let result = builder(Some(PathBuf::from("/opt/ghidra")))
            .agent(agent("ReverseEngineer", vec!["ghidra"]))
            .build()
            .await;
        assert!(result.is_ok());
    }

    /// Seeding with a recorded failed primary skips the Ghidra-context pass
    /// and annotates the forum instead of mis-attributing a sibling's findings
    #[tokio::test]
    async fn seed_detectors_annotates_a_failed_primary() {
        use std::io::{BufRead, Write};
        // a fake bridge answering every one-line command with a success
        // envelope, so connect_to_port can mint a handle without real Ghidra
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            // the thread dies with the test process
            for stream in listener.incoming().flatten() {
                let mut reader = std::io::BufReader::new(stream.try_clone().expect("clone"));
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                let mut stream = stream;
                let _ = stream.write_all(b"{\"status\":\"success\",\"data\":{}}\n");
            }
        });
        let handle = ghidra_cli::Ghidra::connect_to_port(port)
            .await
            .expect("the fake bridge should answer the ping");
        // a ThorPal with an install configured, its connection cell already
        // holding the handle, and the primary ('target') recorded as failed
        let thorpal = builder(Some(PathBuf::from("/opt/fake-ghidra")))
            .build()
            .await
            .expect("thorpal should build");
        thorpal.ghidra.set_open_for_tests(handle).await;
        thorpal
            .ghidra
            .record_failed_import_for_tests("target", "No load spec found");
        // seeding must succeed without ever running the Ghidra-context pass
        // (that pass would issue bridge commands the fake cannot satisfy and
        // error out, so an Ok here proves the guard took the annotation arm)
        thorpal
            .seed_detectors(false)
            .await
            .expect("seeding should succeed");
        // the annotation post landed, steering to the raw file tools ...
        let view = thorpal.forum.view().await;
        let annotation = view
            .threads
            .iter()
            .find(|thread| thread.post.name == "primary-not-loaded")
            .expect("the failed-primary annotation should be posted");
        assert!(annotation.post.body.contains("NOT loaded in Ghidra"));
        assert!(annotation.post.body.contains("target='target'"));
        // ... and no detector bucket posts exist: the main pass was skipped
        assert!(
            view.threads.iter().all(|thread| {
                !thread.post.name.starts_with("fn-") && thread.post.name != "file-level"
            }),
            "no Ghidra-context detector pass should have run"
        );
    }

    /// Builds a turn-budget error like the one the agent runtime surfaces
    ///
    /// # Arguments
    ///
    /// * `max_turns` - The exhausted budget to report
    fn max_turns_error(max_turns: usize) -> PromptError {
        PromptError::MaxTurnsError {
            max_turns,
            chat_history: Box::new(vec![Message::user("task")]),
            prompt: Box::new(Message::user("pending")),
        }
    }

    /// A turn-budget exhaustion re-prompts; other errors keep the old contract
    #[test]
    fn iteration_outcome_decides_the_err_arm() {
        // a budget exhaustion re-prompts whether or not anything settled yet
        assert_eq!(
            iteration_outcome(&max_turns_error(10), 0),
            IterationOutcome::Reprompt
        );
        assert_eq!(
            iteration_outcome(&max_turns_error(10), 3),
            IterationOutcome::Reprompt
        );
        // any other error with nothing settled fails the whole run
        let cancelled = PromptError::PromptCancelled {
            chat_history: Vec::new(),
            reason: "operator abort".to_owned(),
        };
        assert_eq!(iteration_outcome(&cancelled, 0), IterationOutcome::Fatal);
        // and with settled iterations the partial run is kept instead
        assert_eq!(iteration_outcome(&cancelled, 2), IterationOutcome::Truncate);
    }

    /// An all-MaxTurns run errors loudly instead of returning an empty answer
    #[test]
    fn exhausted_run_error_names_the_budget() {
        // the guard surfaces the swallowed budget error with run-level context
        let err = exhausted_run_error(Some(max_turns_error(10)));
        let rendered = format!("{err:#}");
        assert!(rendered.contains("the run never completed an iteration"));
        assert!(rendered.contains("max turns limit: 10"));
        // the defensive no-error path still errors rather than panicking
        let bare = format!("{:#}", exhausted_run_error(None));
        assert!(bare.contains("the run never completed an iteration"));
    }
}
