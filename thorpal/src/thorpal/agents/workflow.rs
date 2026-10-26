//! Configurable sub-agents loaded from an on-disk workflow file.
//!
//! A [`Workflow`] lists the sub-agents ThorPal attaches to its main agent as
//! agents-as-tools tools. Each [`AgentConfig`] describes one sub-agent's display
//! name prefix, description, preamble, and which compiled in tools it gets, so
//! the set of sub-agents can be changed without recompiling ThorPal.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::Path;

use anyhow::Context;
use rig::Agent;
use rig::agent::tool::DynamicTool;
use rig::tool::builtin::ThinkTool;
use serde::{Deserialize, Serialize};
use tracing::instrument;

use super::budget::{RequestMoreTurns, TurnBudget};
use crate::thorpal::gate::ProviderGate;
use crate::thorpal::mcp::{self, McpAgentTools, McpServerConfig, McpToolRef};
use crate::thorpal::tools::analyst::FlagForReview;
use crate::thorpal::tools::forum::{
    AddTasks, CompleteTask, CreateComment, CreatePost, DEFAULT_FORUM_LINK_DEPTH, FollowLink,
    ForumContextMode, ListPosts, ListQuestions, SearchForum, SetStatus,
};
use crate::thorpal::tools::registry::{self, ToolEntry};

/// The default max number of turns a sub-agent can take per task
fn default_max_turns() -> usize {
    10
}

/// The default max number of copies of an agent a single swarm may include
fn default_max_swarm_size() -> usize {
    4
}

/// The default max number of sub-agents that may be alive at once across a run
pub(crate) fn default_max_alive_sub_agents() -> usize {
    8
}

/// The default max number of nested invocations of an agent alive at once
fn default_max_nested_alive() -> usize {
    6
}

/// The default number of link hops linked-mode forum context follows
fn default_forum_link_depth() -> usize {
    DEFAULT_FORUM_LINK_DEPTH
}

/// The default model context window the sub-agent context warnings assume
pub(crate) fn default_context_window_tokens() -> u64 {
    // the Claude-class window; operators with a bigger model raise it in yaml
    200_000
}

/// The default for whether the analyst-interaction tools are offered
pub(crate) fn default_analyst_asks() -> bool {
    true
}

/// The preamble section appended for sub-agents when review flagging is on,
/// teaching the flag_for_review discipline alongside the forum guidance.
const FLAG_FOR_REVIEW_PREAMBLE: &str = "## Flagging for analyst review\n\n\
You have a flag_for_review tool: use it to hand the human analyst anything \
deserving their eyes -- a conclusion you are not confident in, a judgment call \
you made alone, or an anomaly you could not fully explain. Give it a short \
title, the reason human review is needed, and the #post-name or !comment-id it \
concerns when there is one, passing your own agent name as author. Flags never \
pause the run and never block finishing; they are a review queue for the \
analyst, not a question for other agents.";

/// The config for a single sub-agent in a workflow
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// The display name prefix for this agent (e.g. `ReverseEngineer`)
    ///
    /// Every invocation of this agent is given a unique display name made from
    /// this prefix and a moon of Jupiter (e.g. `ReverseEngineer(Europa)`), and
    /// the snake_case form of it is the tool name the main agent calls.
    pub name: String,
    /// A description of what this agent does, shown to the main agent
    pub description: String,
    /// The preamble (system prompt) for this agent
    pub preamble: String,
    /// The tool groups and/or individual tools this agent gets
    ///
    /// Each entry either names a tool group (e.g. `ghidra`), enabling every
    /// compiled in tool in that group, or a single tool (e.g. `bytewise_xor`).
    #[serde(default)]
    pub tools: Vec<String>,
    /// The individual tools removed after the `tools` entries are expanded
    ///
    /// This lets an agent enable a whole group while dropping specific tools
    /// from it (e.g. enable `ghidra` but disable `read`).
    #[serde(default)]
    pub disabled_tools: Vec<String>,
    /// Whether this agent is restricted to read-only tools
    ///
    /// When true, the tools flagged as mutating (the write-side Ghidra
    /// annotation tools) are stripped from this agent's resolved toolset after
    /// the `tools` and `disabled_tools` entries are applied. Defaults to false,
    /// so agents keep the write tools unless a workflow opts out.
    #[serde(default)]
    pub read_only: bool,
    /// The max number of turns this agent can take per task
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,
    /// The max number of extra turns this agent may grant itself per task
    ///
    /// Inside its warning window an agent can call `request_more_turns` with a
    /// justification; grants are clamped so one invocation never receives more
    /// than this many extra turns in total. `None` derives a default from
    /// `max_turns` (a quarter, clamped to 2..=25); `0` disables self-service
    /// extensions entirely (the tool is not attached).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_extension_turns: Option<usize>,
    /// How many remaining turns arm the wrap-up warning
    ///
    /// Once this many model calls (or fewer) remain, every request carries a
    /// budget warning directing the agent to post findings, write a final
    /// handoff, and request an extension only if justified. `None` derives a
    /// default from `max_turns` (a fifth, clamped to 2..=5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn_turns: Option<usize>,
    /// The max number of copies of this agent a single swarm may include
    ///
    /// A value of `0` opts this agent out of swarms entirely, leaving it only
    /// usable as a normal single sub-agent.
    #[serde(default = "default_max_swarm_size")]
    pub max_swarm_size: usize,
    /// The tool names of workflow sub-agents this agent may itself call
    ///
    /// Each entry is another agent's tool name (the snake_case form of its
    /// `name`, e.g. `developer`); that agent is attached to this one as a
    /// goal-aware delegation tool, exactly like sub-agents attach to the main
    /// agent. The delegation graph must be acyclic: self-references and cycles
    /// are rejected when the workflow loads.
    #[serde(default)]
    pub agents: Vec<String>,
    /// The max number of nested invocations of this agent alive at once
    ///
    /// Invocations started by *other sub-agents* (through the `agents` lists
    /// naming this agent) draw from this dedicated pool instead of the run's
    /// global `max_alive_sub_agents` cap, so a parent waiting on this agent
    /// can never deadlock the run. Must be at least 1 when any agent lists
    /// this one in its `agents`.
    #[serde(default = "default_max_nested_alive")]
    pub max_nested_alive: usize,
}

impl AgentConfig {
    /// Get the tool name the main agent calls this agent by
    ///
    /// This is the snake_case form of this agent's name, so `ReverseEngineer`
    /// becomes `reverse_engineer`.
    pub fn tool_name(&self) -> String {
        // allocate room for the name plus a few underscores
        let mut tool_name = String::with_capacity(self.name.len() + 4);
        // walk the name, splitting words on each uppercase character
        for (i, c) in self.name.chars().enumerate() {
            // start a new word on every uppercase char after the first
            if c.is_ascii_uppercase() && i > 0 {
                tool_name.push('_');
            }
            tool_name.push(c.to_ascii_lowercase());
        }
        tool_name
    }

    /// Whether this agent can be included in a swarm
    pub fn swarmable(&self) -> bool {
        // an agent with no swarm slots has opted out of swarms
        self.max_swarm_size > 0
    }

    /// The total extension allowance this agent may grant itself per task
    ///
    /// An explicit `max_extension_turns` wins; otherwise a quarter of
    /// `max_turns`, clamped to 2..=25. Zero disables self-service extensions.
    pub fn extension_allowance(&self) -> usize {
        // derive from the turn cap unless the workflow set an explicit value
        self.max_extension_turns
            .unwrap_or_else(|| (self.max_turns / 4).clamp(2, 25))
    }

    /// How many remaining turns arm this agent's wrap-up warning
    ///
    /// An explicit `warn_turns` wins; otherwise a fifth of `max_turns`,
    /// clamped to 2..=5. Either way the window is forced below the turn cap
    /// (and to at least 1) so the warning can actually fire before the end.
    pub fn warn_window(&self) -> usize {
        // derive from the turn cap unless the workflow set an explicit value
        let window = self
            .warn_turns
            .unwrap_or_else(|| (self.max_turns / 5).clamp(2, 5));
        // keep the window inside the budget so the warning precedes the cap
        window.min(self.max_turns.saturating_sub(1)).max(1)
    }

    /// Bundle this agent's resolved turn-budget knobs for one invocation
    ///
    /// # Arguments
    ///
    /// * `context_window` - The workflow-level context window in tokens
    pub fn turn_budget(&self, context_window: u64) -> TurnBudget {
        TurnBudget {
            max_turns: self.max_turns,
            max_extension_turns: self.extension_allowance(),
            warn_turns: self.warn_window(),
            context_window,
        }
    }

    /// Resolve this agent's enabled tools against the compiled in registry
    ///
    /// Each `tools` entry enables a whole group when it names one and a single
    /// tool otherwise; the `disabled_tools` entries are then removed, and a
    /// `read_only` agent drops every mutating tool last. The resolved entries
    /// come back sorted by name so the toolset the model sees is deterministic.
    pub fn resolve_tools(&self) -> Result<Vec<&'static ToolEntry>, anyhow::Error> {
        // key the enabled entries by name so groups, dupes, and disables collapse
        let mut resolved = BTreeMap::new();
        // enable each listed group or individual tool
        for name in &self.tools {
            // mcp: references resolve against live servers at build time, not
            // the compiled in registry
            if name.starts_with(mcp::MCP_PREFIX) {
                continue;
            }
            // a name enables a whole group when one matches it
            let members = registry::group(name);
            if members.is_empty() {
                // otherwise it must name a single compiled in tool
                match registry::find(name) {
                    Some(entry) => resolved.insert(entry.name, entry),
                    None => anyhow::bail!(
                        "agent '{}' enables unknown tool or tool group '{}'",
                        self.name,
                        name
                    ),
                };
            } else {
                // enable every tool in the matched group
                for entry in members {
                    resolved.insert(entry.name, entry);
                }
            }
        }
        // drop each explicitly disabled tool, rejecting unknown names as typos
        for name in &self.disabled_tools {
            // mcp: disables are applied against live servers at build time too
            if name.starts_with(mcp::MCP_PREFIX) {
                continue;
            }
            if registry::find(name).is_none() {
                anyhow::bail!("agent '{}' disables unknown tool '{}'", self.name, name);
            }
            resolved.remove(name.as_str());
        }
        // strip the write-side tools when this agent is declared read-only
        if self.read_only {
            resolved.retain(|_, entry| !entry.mutating);
        }
        Ok(resolved.into_values().collect())
    }

    /// Parse this agent's `mcp:` tool-list references
    ///
    /// Returns the parsed enables and disables. A disable must name a single
    /// tool (`mcp:<server>:<tool>`): disabling a whole server would just be a
    /// confusing way to not enable it.
    pub(crate) fn mcp_refs(&self) -> Result<(Vec<McpToolRef>, Vec<McpToolRef>), anyhow::Error> {
        // parse each mcp: entry in the enables list
        let mut enables = Vec::new();
        for name in &self.tools {
            if name.starts_with(mcp::MCP_PREFIX) {
                enables.push(mcp::parse_ref(name)?);
            }
        }
        // parse each mcp: entry in the disables list, requiring a tool part
        let mut disables = Vec::new();
        for name in &self.disabled_tools {
            if name.starts_with(mcp::MCP_PREFIX) {
                let reference = mcp::parse_ref(name)?;
                if reference.tool.is_none() {
                    anyhow::bail!(
                        "agent '{}' disables '{}'; disable single tools with \
                        mcp:<server>:<tool> (or drop the server from `tools` instead)",
                        self.name,
                        name
                    );
                }
                disables.push(reference);
            }
        }
        Ok((enables, disables))
    }

    /// Make sure this agent config is valid
    ///
    /// # Arguments
    ///
    /// * `mcp_servers` - The workflow's configured MCP servers
    /// * `thorium_mcp` - Whether the built-in Thorium MCP server is enabled
    fn validate(
        &self,
        mcp_servers: &BTreeMap<String, McpServerConfig>,
        thorium_mcp: bool,
    ) -> Result<(), anyhow::Error> {
        // an agent must have a name to build a tool name from
        if self.name.is_empty() {
            anyhow::bail!("workflow agent names must not be empty");
        }
        // tool names must be ascii alphanumeric to be accepted by LLM providers
        if !self.name.chars().all(|c| c.is_ascii_alphanumeric()) {
            anyhow::bail!(
                "workflow agent name '{}' must only contain ascii letters and digits",
                self.name
            );
        }
        // reject tool/group typos at load time instead of at agent build time
        self.resolve_tools()?;
        // reject references to servers this workflow never configures; the
        // tools themselves can only be checked against a live server at build
        let (enables, disables) = self.mcp_refs()?;
        for reference in enables.iter().chain(disables.iter()) {
            // the built-in Thorium server is referenced through its flag
            if reference.server == mcp::THORIUM_MCP_SERVER {
                if !thorium_mcp {
                    anyhow::bail!(
                        "agent '{}' references the built-in Thorium MCP server but \
                        `thorium_mcp` is not enabled in this workflow",
                        self.name
                    );
                }
            } else if !mcp_servers.contains_key(&reference.server) {
                anyhow::bail!(
                    "agent '{}' references unconfigured MCP server '{}'",
                    self.name,
                    reference.server
                );
            }
        }
        // a disable on a server this agent never enables would silently no-op
        // forever; reject the config debt at load time
        let enabled_servers: HashSet<&str> = enables
            .iter()
            .map(|reference| reference.server.as_str())
            .collect();
        for reference in &disables {
            if !enabled_servers.contains(reference.server.as_str()) {
                anyhow::bail!(
                    "agent '{}' disables MCP tools on server '{}' but never enables it",
                    self.name,
                    reference.server
                );
            }
        }
        Ok(())
    }

    /// Build this sub-agent from the shared provider gate and model
    ///
    /// # Arguments
    ///
    /// * `gate` - The shared provider gate every agent's requests ride
    /// * `model` - The model to talk to
    /// * `delegates` - The goal tools of the agents this one's `agents` lists
    /// * `mcp_tools` - The resolved MCP tool batches this agent attaches
    /// * `forum_context` - The forum-context mode this run spawns sub-agents under
    /// * `analyst_asks` - Whether this run offers the analyst-interaction tools
    pub fn build(
        &self,
        gate: &std::sync::Arc<ProviderGate>,
        model: impl Into<String>,
        delegates: Vec<DynamicTool>,
        mcp_tools: Vec<McpAgentTools>,
        forum_context: ForumContextMode,
        analyst_asks: bool,
    ) -> Result<Agent, anyhow::Error> {
        // resolve the toolset once for the tools and the generated preamble
        let resolved = self.resolve_tools()?;
        // append the generated host-function section for script-running agents,
        // so the model always knows exactly what its scripts can call
        let mut preamble = self.preamble.clone();
        if let Some(section) = script_bindings_preamble(&resolved) {
            preamble.push_str("\n\n");
            preamble.push_str(&section);
        }
        // append the generated memory-library section for memory-enabled
        // agents, so recall discipline always matches the resolved toolset
        if let Some(section) = memory_tools_preamble(&resolved) {
            preamble.push_str("\n\n");
            preamble.push_str(section);
        }
        // append the forum-reading guidance matching this run's context mode,
        // so one workflow preamble works under either mode
        preamble.push_str("\n\n");
        preamble.push_str(forum_reading_preamble(forum_context));
        // append the review-flag discipline when this run offers the tool, so
        // a gated run's preamble stays byte-identical to a pre-feature one
        if analyst_asks {
            preamble.push_str("\n\n");
            preamble.push_str(FLAG_FOR_REVIEW_PREAMBLE);
        }
        // start with a barebones agent configured from this config; its
        // completion requests retry transient provider faults in place and
        // park on the session-wide pause gate like every other agent's
        let mut agent = gate
            .retrying_agent(model, &self.name)
            .name(&self.tool_name())
            .description(&self.description)
            .default_max_turns(self.max_turns)
            .max_tokens(crate::thorpal::DEFAULT_MAX_TOKENS)
            .preamble(&preamble)
            // every sub-agent shares findings through the forum
            .tool(CreatePost)
            .tool(CreateComment)
            .tool(ListPosts)
            .tool(SearchForum)
            .tool(FollowLink)
            .tool(SetStatus)
            .tool(ListQuestions)
            .tool(AddTasks)
            .tool(CompleteTask)
            .tool(ThinkTool);
        // attach the self-service turn-extension tool only when this agent has
        // any allowance to grant itself
        if self.extension_allowance() > 0 {
            agent = agent.tool(RequestMoreTurns);
        }
        // attach the non-blocking review flag when this run enables the
        // analyst-interaction tools (sub-agents never get ask_analyst: they
        // run concurrently under turn budgets, so blocking them on a human
        // would burn budget; the coordinator asks on their behalf)
        if analyst_asks {
            agent = agent.tool(FlagForReview);
        }
        // add each compiled in tool this agent's config enables
        for entry in resolved {
            agent = (entry.apply)(agent);
        }
        // attach this agent's MCP tools, one registration per server so each
        // batch shares its server's sink and per-call timeout; the batch's
        // prefixed names are what the model sees
        for batch in mcp_tools {
            agent = agent.rmcp_tools_named_with_timeout(batch.tools, batch.sink, batch.timeout);
        }
        // attach each delegated sub-agent as a goal-aware tool, exactly like
        // the sub-agents attach to the main agent
        for delegate in delegates {
            agent = agent.dynamic_tool(delegate);
        }
        Ok(agent.build())
    }
}

/// Render the generated forum-reading guidance for a forum-context mode
///
/// Every sub-agent gets this section appended after its workflow preamble, so
/// the reading instructions always match how the run actually delivers forum
/// context: preloaded into the task prompt under `linked`, or pulled with
/// `list_posts` under `full`. The linked text explicitly supersedes any
/// `list_posts`-first instruction a user-authored preamble may still carry.
///
/// # Arguments
///
/// * `mode` - The forum-context mode this run spawns sub-agents under
fn forum_reading_preamble(mode: ForumContextMode) -> &'static str {
    // hand back the guidance matching how this run delivers forum context
    match mode {
        // linked mode preloads the assigned neighborhood into the task prompt
        ForumContextMode::Linked => {
            "## Reading the forum\n\n\
             Your task prompt contains a \"# Forum context\" section holding your \
             assigned thread, every post it transitively references (up to a \
             configured link depth), and a one-line index of every post on the \
             forum. Read that section first instead of calling list_posts; this \
             supersedes any earlier instruction to always start by calling \
             list_posts. Use follow_link to pull up any #post-name or !comment-id \
             reference that is not already included, check the index for related \
             work worth pulling before you post, and use search_forum to find \
             discussion of a specific value, API, or topic the links do not reach."
        }
        // full mode preloads nothing, so agents download the board themselves
        ForumContextMode::Full => {
            "## Reading the forum\n\n\
             Always start by calling list_posts to see what has already been \
             discussed before doing anything else. Use search_forum to find \
             existing posts and comments about a specific value, API, address, \
             or topic before raising it again."
        }
    }
}

/// Render the generated host-function preamble for a script-running agent
///
/// Only agents whose resolved toolset includes `run_script` get the section;
/// it lists the Rhai signature of every enabled tool that carries a binding,
/// so the model knows exactly what its scripts can call without a tool call.
///
/// # Arguments
///
/// * `resolved` - The agent's resolved toolset
fn script_bindings_preamble(resolved: &[&'static ToolEntry]) -> Option<String> {
    // only a script-running agent needs the host-function list
    if !resolved.iter().any(|entry| entry.name == "run_script") {
        return None;
    }
    // open the section with how the list is derived, so the model trusts it
    let mut section = String::from(
        "## Rhai host functions available to your scripts\n\n\
         Scripts you run with `run_script` can call these host functions. This \
         list is derived from your enabled tools, so it is authoritative for \
         this run:\n",
    );
    // collect the enabled tools that expose a binding into the script engine
    let signatures: Vec<&'static str> = resolved
        .iter()
        .filter_map(|entry| entry.rhai_signature)
        .collect();
    if signatures.is_empty() {
        // an agent can run scripts with no host bindings at all
        section.push_str("\n(none -- your scripts are pure Rhai over the `args` value)\n");
    } else {
        // render one compact bullet per binding
        for signature in signatures {
            section.push_str(&format!("\n- `{signature}`"));
        }
        section.push('\n');
    }
    // close with the args contract and where the full reference lives
    section.push_str(
        "\nScripts also receive an `args` constant holding the JSON value passed \
         to run_script. Call the rhai_docs tool for the full language reference \
         and each host function's arguments and return shape.",
    );
    Some(section)
}

/// Render the generated memory-library preamble for a memory-enabled agent
///
/// Only agents whose resolved toolset includes `search_memories` get the
/// section; it teaches the recall discipline, and agents that also hold
/// `save_memory` get the curation paragraph on top. The section is static
/// text -- the live memory index is injected per spawn into the task prompt,
/// where it can see memories saved earlier in the same run.
///
/// # Arguments
///
/// * `resolved` - The agent's resolved toolset
fn memory_tools_preamble(resolved: &[&'static ToolEntry]) -> Option<&'static str> {
    // only a memory-enabled agent needs the recall discipline
    if !resolved.iter().any(|entry| entry.name == "search_memories") {
        return None;
    }
    // the base section teaches recall; curators get the save paragraph too
    if resolved.iter().any(|entry| entry.name == "save_memory") {
        Some(MEMORY_RECALL_PREAMBLE_WITH_SAVE)
    } else {
        Some(MEMORY_RECALL_PREAMBLE)
    }
}

/// The recall-discipline section every memory-enabled agent gets
const MEMORY_RECALL_PREAMBLE: &str = "## Cross-run memory library\n\n\
    You have access to a persistent library of GENERIC knowledge saved by past \
    analysis runs: techniques, workflow rules, verified family intel, and \
    pitfalls. Your task prompt may open with a \"# Memory index\" section \
    listing each memory's name, kind, and summary. Consult the index (and \
    search_memories) BEFORE re-deriving a technique or asserting facts from \
    recall, and read_memory anything that looks relevant -- cite memories by \
    name when you use them. Memories are knowledge from PRIOR runs, not \
    evidence about this binary: verify a memory's claims against this sample \
    before promoting them to conclusions.";

/// The recall section plus the curation duty for `save_memory` holders
const MEMORY_RECALL_PREAMBLE_WITH_SAVE: &str = "## Cross-run memory library\n\n\
    You have access to a persistent library of GENERIC knowledge saved by past \
    analysis runs: techniques, workflow rules, verified family intel, and \
    pitfalls. Your task prompt may open with a \"# Memory index\" section \
    listing each memory's name, kind, and summary. Consult the index (and \
    search_memories) BEFORE re-deriving a technique or asserting facts from \
    recall, and read_memory anything that looks relevant -- cite memories by \
    name when you use them. Memories are knowledge from PRIOR runs, not \
    evidence about this binary: verify a memory's claims against this sample \
    before promoting them to conclusions.\n\n\
    You are also this library's curator: when a run surfaces a durable, \
    generic lesson (a technique that worked, a workflow rule, verified family \
    intel, a pitfall), save it with save_memory. Never save sample-specific \
    facts -- hashes, paths, decoded strings, or session details belong on the \
    forum, not in cross-run memory.";

/// Compute a build order over a workflow's delegation graph (callees first)
///
/// Validates the `agents` lists while ordering: unknown references, duplicate
/// entries, self-references, and cycles are all rejected, and any agent that
/// is delegated to must keep at least one nested-invocation slot. The returned
/// indexes order the agents so every callee is built before its callers.
///
/// # Arguments
///
/// * `agents` - The workflow agents to order
pub(crate) fn build_order(agents: &[AgentConfig]) -> Result<Vec<usize>, anyhow::Error> {
    // map each agent's tool name to its index so references resolve fast
    let index_of: HashMap<String, usize> = agents
        .iter()
        .enumerate()
        .map(|(index, agent)| (agent.tool_name(), index))
        .collect();
    // the reverse adjacency (callee -> callers) drives the topological order
    let mut callers_of: Vec<Vec<usize>> = vec![Vec::new(); agents.len()];
    // each agent starts with one unmet dependency per agent it delegates to
    let mut pending = vec![0usize; agents.len()];
    // resolve and validate every delegation edge
    for (index, agent) in agents.iter().enumerate() {
        // track this agent's references so duplicates are rejected as typos
        let mut seen = HashSet::with_capacity(agent.agents.len());
        for name in &agent.agents {
            // every reference must name another agent in this workflow
            let Some(&callee) = index_of.get(name) else {
                anyhow::bail!("agent '{}' delegates to unknown agent '{name}'", agent.name);
            };
            // an agent delegating to itself could never be built or terminate
            if callee == index {
                anyhow::bail!("agent '{}' may not list itself in `agents`", agent.name);
            }
            // the same delegate twice is a typo worth rejecting
            if !seen.insert(callee) {
                anyhow::bail!("agent '{}' lists agent '{name}' more than once", agent.name);
            }
            // a delegated-to agent with no nested slots could never run
            if agents[callee].max_nested_alive == 0 {
                anyhow::bail!(
                    "agent '{name}' is delegated to but has max_nested_alive 0; \
                     it needs at least one nested slot"
                );
            }
            // record the edge for the topological walk below
            callers_of[callee].push(index);
            pending[index] += 1;
        }
    }
    // seed the walk with every agent that delegates to nobody
    let mut ready: VecDeque<usize> = pending
        .iter()
        .enumerate()
        .filter(|(_, count)| **count == 0)
        .map(|(index, _)| index)
        .collect();
    // place agents callees-first, unblocking their callers as they land
    let mut order = Vec::with_capacity(agents.len());
    while let Some(index) = ready.pop_front() {
        order.push(index);
        // every caller of this agent has one fewer unmet dependency
        for &caller in &callers_of[index] {
            pending[caller] -= 1;
            if pending[caller] == 0 {
                ready.push_back(caller);
            }
        }
    }
    // any agent left unplaced sits on a delegation cycle
    if order.len() < agents.len() {
        let stuck: Vec<&str> = pending
            .iter()
            .enumerate()
            .filter(|(_, count)| **count > 0)
            .map(|(index, _)| agents[index].name.as_str())
            .collect();
        anyhow::bail!(
            "workflow agents form a delegation cycle involving: {}",
            stuck.join(", ")
        );
    }
    Ok(order)
}

/// The set of sub-agents ThorPal attaches to its main agent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    /// The sub-agents to attach to the main agent
    pub agents: Vec<AgentConfig>,
    /// The max number of sub-agents that may be alive at once across a run
    ///
    /// This counts single delegations and swarm members together; any extra
    /// invocations wait for a running sub-agent to finish before starting.
    #[serde(default = "default_max_alive_sub_agents")]
    pub max_alive_sub_agents: usize,
    /// How much of the forum each sub-agent is handed at spawn time
    ///
    /// `linked` (the default) renders the assigned question's thread plus
    /// everything it transitively references into the sub-agent's starting
    /// prompt, together with a titles-only index of every post; `full` preloads
    /// nothing and tells agents to call `list_posts` first instead.
    #[serde(default)]
    pub forum_context: ForumContextMode,
    /// How many link hops linked-mode forum context follows beyond the
    /// assigned thread (`0` preloads the assigned thread alone)
    #[serde(default = "default_forum_link_depth")]
    pub forum_link_depth: usize,
    /// The model's context window in tokens, for sub-agent context warnings
    ///
    /// Sub-agents whose conversation memory nears this size are ordered to
    /// flush findings to the forum (and clamped to forum tools at the
    /// critical threshold) before a context death loses them. Defaults to
    /// the Claude-class 200k; raise it for a bigger-window model, or set `0`
    /// to disarm context tracking entirely.
    #[serde(default = "default_context_window_tokens")]
    pub context_window_tokens: u64,
    /// Whether this run offers the analyst-interaction tools
    ///
    /// On (the default), the main agent gets `ask_analyst` and every agent
    /// gets `flag_for_review`, with matching preamble guidance. Off, the tool
    /// lists and preambles stay byte-identical to pre-feature runs -- the knob
    /// evaluation owners pin for strict A/B comparability. Persisted in the
    /// session snapshot so a resume keeps the same tool surface.
    #[serde(default = "default_analyst_asks")]
    pub analyst_asks: bool,
    /// Whether the built-in Thorium MCP server is enabled
    ///
    /// On, agents may reference the reserved server name `thorium`
    /// (`mcp:thorium`, `mcp:thorium:<tool>`) and the run connects to the
    /// Thorium API's `/api/mcp` endpoint with credentials the consumer
    /// resolves at build time. Off (the default), any `mcp:thorium` reference
    /// is rejected at load.
    #[serde(default)]
    pub thorium_mcp: bool,
    /// The MCP servers this workflow sources tools from, keyed by name
    ///
    /// Agents reference a server's tools in their `tools`/`disabled_tools`
    /// lists as `mcp:<server>` (every tool) or `mcp:<server>:<tool>` (one
    /// tool, by the name the server lists). The name `thorium` is reserved
    /// for the built-in Thorium server toggled by `thorium_mcp`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
}

/// The default workflow compiled into ThorPal
const BUILTIN_WORKFLOW: &str = include_str!("../../../workflow.yaml");

impl Workflow {
    /// Parse and validate the default workflow compiled into ThorPal
    ///
    /// This is the crate's `workflow.yaml`, used when a caller has no workflow
    /// file of its own to load.
    pub fn builtin() -> Result<Self, anyhow::Error> {
        // parse and validate the compiled in workflow
        Self::from_yaml(BUILTIN_WORKFLOW).context("invalid builtin workflow")
    }

    /// The tool names of every delegation call this workflow attaches
    ///
    /// One per sub-agent (its goal tool) plus the swarm tool. A resume uses
    /// this set to tell a dangling delegation call -- which it re-runs for
    /// real -- apart from an ordinary tool call, which only gets a synthetic
    /// interrupted result; the `thorctl` replay uses the same set to pair
    /// delegation calls across a resume.
    pub fn delegation_tool_names(&self) -> std::collections::BTreeSet<String> {
        // every agent's goal tool, plus the shared swarm tool
        let mut names: std::collections::BTreeSet<String> =
            self.agents.iter().map(AgentConfig::tool_name).collect();
        names.insert(super::swarm::SWARM_TOOL_NAME.to_owned());
        names
    }

    /// Parse and validate a workflow from a yaml string
    ///
    /// # Arguments
    ///
    /// * `yaml` - The yaml to parse a workflow from
    pub fn from_yaml(yaml: &str) -> Result<Self, anyhow::Error> {
        // parse the workflow from yaml
        let workflow: Workflow =
            serde_norway::from_str(yaml).context("failed to parse workflow yaml")?;
        // make sure this workflow is valid before using it
        workflow.validate()?;
        Ok(workflow)
    }

    /// Load and validate a workflow from a yaml file on disk
    ///
    /// # Arguments
    ///
    /// * `path` - The path to the workflow yaml file to load
    #[instrument(name = "Workflow::load", skip_all, err(Debug))]
    pub async fn load(path: impl AsRef<Path>) -> Result<Self, anyhow::Error> {
        // borrow the path once for reading and error context
        let path = path.as_ref();
        // read the raw yaml off disk
        let yaml = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("failed to read workflow file {}", path.display()))?;
        // parse and validate the workflow
        Self::from_yaml(&yaml).with_context(|| format!("invalid workflow file {}", path.display()))
    }

    /// Make sure this workflow is valid
    fn validate(&self) -> Result<(), anyhow::Error> {
        // a workflow with no agents leaves the main agent nobody to delegate to
        if self.agents.is_empty() {
            anyhow::bail!("workflows must contain at least one agent");
        }
        // a run with no sub-agent slots could never delegate anything
        if self.max_alive_sub_agents == 0 {
            anyhow::bail!("workflow max_alive_sub_agents must be at least 1");
        }
        // validate each configured MCP server before the agents that use them
        for (name, server) in &self.mcp_servers {
            // the built-in Thorium server owns its reserved name
            if name == mcp::THORIUM_MCP_SERVER {
                anyhow::bail!(
                    "MCP server name 'thorium' is reserved for the built-in Thorium \
                    server; enable it with `thorium_mcp: true` instead"
                );
            }
            // a server shadowing a compiled in group or tool name would make
            // tool references ambiguous to readers
            if !registry::group(name).is_empty() || registry::find(name).is_some() {
                anyhow::bail!(
                    "MCP server name '{name}' collides with a compiled in tool or tool group"
                );
            }
            server.validate(name)?;
        }
        // track the tool names we have seen so duplicates can be rejected
        let mut seen = HashSet::with_capacity(self.agents.len());
        // validate each agent and make sure its tool name is unique
        for agent in &self.agents {
            agent.validate(&self.mcp_servers, self.thorium_mcp)?;
            if !seen.insert(agent.tool_name()) {
                anyhow::bail!(
                    "workflow agent name '{}' is used more than once",
                    agent.name
                );
            }
        }
        // reject bad delegation graphs (unknown names, self-references, and
        // cycles) at load time instead of at agent build time
        build_order(&self.agents)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal agent config with the given name
    ///
    /// # Arguments
    ///
    /// * `name` - The name to give this agent
    fn agent(name: &str) -> AgentConfig {
        AgentConfig {
            name: name.to_owned(),
            description: "desc".to_owned(),
            preamble: "preamble".to_owned(),
            tools: Vec::new(),
            disabled_tools: Vec::new(),
            read_only: false,
            max_turns: default_max_turns(),
            max_extension_turns: None,
            warn_turns: None,
            max_swarm_size: default_max_swarm_size(),
            agents: Vec::new(),
            max_nested_alive: default_max_nested_alive(),
        }
    }

    /// Build a workflow from a list of agents with the default limits
    ///
    /// # Arguments
    ///
    /// * `agents` - The agents to put in this workflow
    fn workflow(agents: Vec<AgentConfig>) -> Workflow {
        Workflow {
            agents,
            max_alive_sub_agents: default_max_alive_sub_agents(),
            forum_context: ForumContextMode::default(),
            forum_link_depth: default_forum_link_depth(),
            context_window_tokens: default_context_window_tokens(),
            analyst_asks: default_analyst_asks(),
            thorium_mcp: false,
            mcp_servers: BTreeMap::new(),
        }
    }

    /// The builtin workflow parses into the standard sub-agents.
    #[test]
    fn builtin_workflow_parses() {
        // parse the workflow file compiled into the crate
        let workflow = Workflow::builtin().expect("builtin workflow.yaml should be valid");
        // the researcher, reverse engineer, and developer are the standard sub-agents
        let names: Vec<&str> = workflow.agents.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["Researcher", "ReverseEngineer", "Developer"]);
        // the builtin states the Claude-class context window explicitly
        assert_eq!(workflow.context_window_tokens, 200_000);
        // a workflow that omits the knob gets the same default, and an
        // explicit value (or zero, disarming context tracking) parses
        let yaml = "agents:\n  - name: A\n    description: d\n    preamble: p\n";
        let defaulted = Workflow::from_yaml(yaml).expect("default should parse");
        assert_eq!(defaulted.context_window_tokens, 200_000);
        let yaml =
            "context_window_tokens: 0\nagents:\n  - name: A\n    description: d\n    preamble: p\n";
        let disarmed = Workflow::from_yaml(yaml).expect("zero should parse");
        assert_eq!(disarmed.context_window_tokens, 0);
        // every agent reads the memory library; only the analysis agents get
        // the binary-facing registry groups
        assert_eq!(workflow.agents[0].tools, ["memory"]);
        assert_eq!(
            workflow.agents[1].tools,
            [
                "containers",
                "decode",
                "file",
                "ghidra",
                "memory",
                "transforms"
            ]
        );
        assert_eq!(
            workflow.agents[2].tools,
            [
                "containers",
                "decode",
                "file",
                "ghidra",
                "memory",
                "transforms",
                "scripts"
            ]
        );
        // the researcher is the memory library's only curator: everyone else
        // has the write side disabled
        assert!(workflow.agents[0].disabled_tools.is_empty());
        assert_eq!(workflow.agents[1].disabled_tools, ["save_memory"]);
        assert_eq!(workflow.agents[2].disabled_tools, ["save_memory"]);
        // no builtin agent opts out of the write-side tools
        assert!(!workflow.agents[0].read_only);
        assert!(!workflow.agents[1].read_only);
        assert!(!workflow.agents[2].read_only);
        // the turn caps match the original hardcoded agents
        assert_eq!(workflow.agents[0].max_turns, 10);
        assert_eq!(workflow.agents[1].max_turns, 100);
        assert_eq!(workflow.agents[2].max_turns, 50);
        // every builtin agent sets its extension allowance and warning window
        assert_eq!(workflow.agents[0].extension_allowance(), 4);
        assert_eq!(workflow.agents[0].warn_window(), 3);
        assert_eq!(workflow.agents[1].extension_allowance(), 25);
        assert_eq!(workflow.agents[1].warn_window(), 5);
        assert_eq!(workflow.agents[2].extension_allowance(), 12);
        assert_eq!(workflow.agents[2].warn_window(), 5);
        // every agent can be swarmed, the reverse engineer most widely
        assert_eq!(workflow.agents[0].max_swarm_size, 2);
        assert_eq!(workflow.agents[1].max_swarm_size, 6);
        assert_eq!(workflow.agents[2].max_swarm_size, 2);
        assert_eq!(workflow.max_alive_sub_agents, 8);
        // the researcher and reverse engineer can both call the developer,
        // and a stuck reverse engineer can call the researcher directly
        assert_eq!(workflow.agents[0].agents, ["developer"]);
        assert_eq!(workflow.agents[1].agents, ["developer", "researcher"]);
        assert!(workflow.agents[2].agents.is_empty());
        // the researcher declares its nested pool explicitly (sized to the
        // reverse engineer's swarm cap); the developer keeps the default
        assert_eq!(workflow.agents[0].max_nested_alive, 6);
        assert_eq!(workflow.agents[2].max_nested_alive, 6);
        // the researcher preamble teaches memory-first recall, and the old
        // intro typo stays fixed
        assert!(workflow.agents[0].preamble.contains("search_memories"));
        for agent in &workflow.agents {
            assert!(
                !agent.preamble.contains("You are are"),
                "agent '{}' preamble carries the old typo",
                agent.name
            );
        }
        // the builtin ships no MCP servers and leaves the built-in Thorium
        // server off
        assert!(!workflow.thorium_mcp);
        assert!(workflow.mcp_servers.is_empty());
    }

    /// MCP servers parse from yaml with references gated at load time.
    #[test]
    fn mcp_servers_parse_and_gate_references() {
        // a workflow with both transports, the built-in server, and references
        let yaml = r#"
thorium_mcp: true
mcp_servers:
  intel:
    url: https://intel.example/mcp
    headers:
      Authorization: Bearer ${INTEL_TOKEN}
    timeout_secs: 120
    optional: true
  sandbox:
    command: /usr/local/bin/my-mcp
    args: ["--mode", "analysis"]
    env:
      API_KEY: ${SANDBOX_KEY}
    read_only: true
agents:
  - name: A
    description: d
    preamble: p
    tools: ["mcp:thorium", "mcp:intel", "mcp:sandbox:detonate", memory]
    disabled_tools: ["mcp:intel:scan_url"]
"#;
        let parsed = Workflow::from_yaml(yaml).expect("mcp servers should parse");
        // the built-in server flag and both configs landed
        assert!(parsed.thorium_mcp);
        let intel = &parsed.mcp_servers["intel"];
        assert_eq!(intel.url.as_deref(), Some("https://intel.example/mcp"));
        assert_eq!(intel.timeout_secs, Some(120));
        assert!(intel.optional);
        assert!(!intel.read_only);
        let sandbox = &parsed.mcp_servers["sandbox"];
        assert_eq!(sandbox.command.as_deref(), Some("/usr/local/bin/my-mcp"));
        assert_eq!(sandbox.args, ["--mode", "analysis"]);
        assert!(sandbox.read_only);
        assert!(!sandbox.optional);
        assert_eq!(sandbox.timeout_secs, None);
        // mcp references resolve no compiled in tools; plain names still do
        let resolved = parsed.agents[0].resolve_tools().expect("resolve");
        assert!(resolved.iter().all(|entry| entry.group == "memory"));
        // the references parse into their server and tool parts
        let (enables, disables) = parsed.agents[0].mcp_refs().expect("refs");
        assert_eq!(enables.len(), 3);
        assert_eq!(enables[2].server, "sandbox");
        assert_eq!(enables[2].tool.as_deref(), Some("detonate"));
        assert_eq!(disables.len(), 1);
        // the serialized workflow keeps ${VAR} references literal, so secrets
        // never land in a session snapshot
        let round = serde_norway::to_string(&parsed).expect("serialize");
        assert!(round.contains("${INTEL_TOKEN}"));
        assert!(round.contains("${SANDBOX_KEY}"));
        let reparsed = Workflow::from_yaml(&round).expect("round trip");
        assert_eq!(reparsed.mcp_servers.len(), 2);
        // a workflow that omits both keys gets the defaults
        let yaml = "agents:\n  - name: A\n    description: d\n    preamble: p\n";
        let defaulted = Workflow::from_yaml(yaml).expect("defaults should parse");
        assert!(!defaulted.thorium_mcp);
        assert!(defaulted.mcp_servers.is_empty());
    }

    /// Bad MCP references are rejected when the workflow loads.
    #[test]
    fn bad_mcp_references_are_rejected() {
        // referencing a server the workflow never configures is a typo
        let mut config = agent("Top");
        config.tools = vec!["mcp:ghost".to_owned()];
        let error = workflow(vec![config]).validate().unwrap_err().to_string();
        assert!(error.contains("unconfigured MCP server 'ghost'"));
        // referencing the built-in server requires its flag
        let mut config = agent("Top");
        config.tools = vec!["mcp:thorium".to_owned()];
        let error = workflow(vec![config]).validate().unwrap_err().to_string();
        assert!(error.contains("thorium_mcp"));
        // flipping the flag on makes the same reference valid
        let mut config = agent("Top");
        config.tools = vec!["mcp:thorium".to_owned()];
        let mut flow = workflow(vec![config]);
        flow.thorium_mcp = true;
        assert!(flow.validate().is_ok());
        // a disable must name a single tool, not a whole server
        let mut config = agent("Top");
        config.tools = vec!["mcp:thorium".to_owned()];
        config.disabled_tools = vec!["mcp:thorium".to_owned()];
        let mut flow = workflow(vec![config]);
        flow.thorium_mcp = true;
        let error = flow.validate().unwrap_err().to_string();
        assert!(error.contains("mcp:<server>:<tool>"));
        // a disable on a server the agent never enables is dead config
        let mut config = agent("Top");
        config.disabled_tools = vec!["mcp:thorium:start_tree".to_owned()];
        let mut flow = workflow(vec![config]);
        flow.thorium_mcp = true;
        let error = flow.validate().unwrap_err().to_string();
        assert!(error.contains("never enables"));
        // malformed references are rejected with the expected shapes
        for bad in ["mcp:", "mcp::x", "mcp:a:", "mcp:a:b:c"] {
            let mut config = agent("Top");
            config.tools = vec![bad.to_owned()];
            assert!(
                workflow(vec![config]).validate().is_err(),
                "'{bad}' should be rejected"
            );
        }
    }

    /// Bad MCP server configs are rejected when the workflow loads.
    #[test]
    fn bad_mcp_server_configs_are_rejected() {
        /// Build a workflow holding one named server config
        ///
        /// # Arguments
        ///
        /// * `name` - The name to configure the server under
        /// * `server` - The server config to validate
        fn with_server(name: &str, server: McpServerConfig) -> Workflow {
            let mut flow = workflow(vec![agent("Top")]);
            flow.mcp_servers.insert(name.to_owned(), server);
            flow
        }
        /// A minimal valid http server config to mutate per case
        fn http() -> McpServerConfig {
            McpServerConfig {
                command: None,
                args: Vec::new(),
                env: BTreeMap::new(),
                url: Some("https://x.example/mcp".to_owned()),
                headers: BTreeMap::new(),
                timeout_secs: None,
                optional: false,
                read_only: false,
            }
        }
        // a well-formed config under a well-formed name passes
        assert!(with_server("intel", http()).validate().is_ok());
        // the built-in Thorium server owns its reserved name
        let error = with_server("thorium", http())
            .validate()
            .unwrap_err()
            .to_string();
        assert!(error.contains("reserved"));
        // a server may not shadow a compiled in group or tool name
        assert!(with_server("ghidra", http()).validate().is_err());
        assert!(with_server("read_file", http()).validate().is_err());
        // names must be lowercase snake case without double underscores
        assert!(with_server("Intel", http()).validate().is_err());
        assert!(with_server("1intel", http()).validate().is_err());
        assert!(with_server("in__tel", http()).validate().is_err());
        assert!(with_server("in-tel", http()).validate().is_err());
        // exactly one transport must be set
        let mut server = http();
        server.command = Some("/bin/mcp".to_owned());
        assert!(with_server("intel", server).validate().is_err());
        let mut server = http();
        server.url = None;
        assert!(with_server("intel", server).validate().is_err());
        // cross-transport field mixes are likely typos
        let mut server = http();
        server.args = vec!["--x".to_owned()];
        assert!(with_server("intel", server).validate().is_err());
        let mut server = http();
        server.command = Some("/bin/mcp".to_owned());
        server.url = None;
        server.headers = BTreeMap::from([("X".to_owned(), "y".to_owned())]);
        assert!(with_server("intel", server).validate().is_err());
        // a zero call timeout could never complete a call
        let mut server = http();
        server.timeout_secs = Some(0);
        assert!(with_server("intel", server).validate().is_err());
    }

    /// Delegation lists are validated and ordered callees-first.
    #[test]
    fn delegation_graphs_validate_and_order() {
        // a valid chain orders every callee before its callers
        let mut top = agent("Top");
        top.agents = vec!["middle".to_owned()];
        let mut middle = agent("Middle");
        middle.agents = vec!["leaf".to_owned()];
        let leaf = agent("Leaf");
        let agents = vec![top, middle, leaf];
        let order = build_order(&agents).expect("a chain should order");
        assert_eq!(order, [2, 1, 0]);
        // the workflow as a whole validates too
        assert!(workflow(agents).validate().is_ok());
    }

    /// Bad delegation graphs are rejected with a useful error.
    #[test]
    fn bad_delegation_graphs_are_rejected() {
        // an unknown delegate name is a typo worth rejecting
        let mut config = agent("Top");
        config.agents = vec!["nobody".to_owned()];
        assert!(workflow(vec![config, agent("Leaf")]).validate().is_err());
        // an agent may not delegate to itself
        let mut config = agent("Top");
        config.agents = vec!["top".to_owned()];
        assert!(workflow(vec![config]).validate().is_err());
        // the same delegate twice is a typo worth rejecting
        let mut config = agent("Top");
        config.agents = vec!["leaf".to_owned(), "leaf".to_owned()];
        assert!(workflow(vec![config, agent("Leaf")]).validate().is_err());
        // a delegation cycle could never be built or terminate
        let mut a = agent("Alpha");
        a.agents = vec!["beta".to_owned()];
        let mut b = agent("Beta");
        b.agents = vec!["alpha".to_owned()];
        let error = workflow(vec![a, b]).validate().unwrap_err().to_string();
        assert!(error.contains("cycle"), "unexpected error: {error}");
        // a delegated-to agent must keep at least one nested slot
        let mut top = agent("Top");
        top.agents = vec!["leaf".to_owned()];
        let mut leaf = agent("Leaf");
        leaf.max_nested_alive = 0;
        assert!(workflow(vec![top, leaf]).validate().is_err());
    }

    /// The delegation fields parse from yaml and default sensibly.
    #[test]
    fn delegation_fields_parse_from_yaml() {
        // a workflow wiring one agent to another parses with the list intact
        let yaml = "agents:\n  - name: Dev\n    description: d\n    preamble: p\n    max_nested_alive: 3\n  - name: Re\n    description: d\n    preamble: p\n    agents: [dev]\n";
        let workflow = Workflow::from_yaml(yaml).expect("delegating workflow should parse");
        assert_eq!(workflow.agents[1].agents, ["dev"]);
        assert_eq!(workflow.agents[0].max_nested_alive, 3);
        // omitted fields fall back to their defaults
        assert!(workflow.agents[0].agents.is_empty());
        assert_eq!(workflow.agents[1].max_nested_alive, 6);
    }

    /// Script-running agents get the generated host-function preamble section.
    #[test]
    fn script_agents_get_bindings_preamble() {
        // an agent without run_script gets no generated section
        let mut config = agent("Dev");
        config.tools = vec!["transforms".to_owned()];
        let resolved = config.resolve_tools().expect("transforms should resolve");
        assert!(script_bindings_preamble(&resolved).is_none());
        // a script-running agent's section lists its enabled bindings
        config.tools = vec!["transforms".to_owned(), "scripts".to_owned()];
        let resolved = config.resolve_tools().expect("scripts should resolve");
        let section = script_bindings_preamble(&resolved).expect("section should render");
        assert!(section.contains("bytewise_xor"));
        // disabling a bound tool drops it from the generated section
        config.disabled_tools = vec!["bytewise_xor".to_owned()];
        let resolved = config.resolve_tools().expect("disable should resolve");
        let section = script_bindings_preamble(&resolved).expect("section should render");
        assert!(!section.contains("bytewise_xor"));
        // a script runner with no bindings at all says so explicitly
        assert!(section.contains("none"));
    }

    /// Memory-enabled agents get the generated recall-discipline section.
    #[test]
    fn memory_agents_get_memory_preamble() {
        // an agent without the memory tools gets no generated section
        let mut config = agent("Dev");
        config.tools = vec!["transforms".to_owned()];
        let resolved = config.resolve_tools().expect("transforms should resolve");
        assert!(memory_tools_preamble(&resolved).is_none());
        // a full memory agent (the curator) gets the save paragraph too
        config.tools = vec!["memory".to_owned()];
        let resolved = config.resolve_tools().expect("memory should resolve");
        let section = memory_tools_preamble(&resolved).expect("section should render");
        assert!(section.contains("search_memories"));
        assert!(section.contains("save_memory"));
        // a read-side agent (save_memory disabled) drops the save paragraph
        config.disabled_tools = vec!["save_memory".to_owned()];
        let resolved = config.resolve_tools().expect("disable should resolve");
        let section = memory_tools_preamble(&resolved).expect("section should render");
        assert!(section.contains("search_memories"));
        assert!(!section.contains("save_memory"));
    }

    /// Tool names are the snake_case form of the agent name.
    #[test]
    fn tool_name_is_snake_case() {
        assert_eq!(agent("ReverseEngineer").tool_name(), "reverse_engineer");
        assert_eq!(agent("Researcher").tool_name(), "researcher");
        assert_eq!(agent("lowercase").tool_name(), "lowercase");
    }

    /// Optional fields fall back to their defaults.
    #[test]
    fn optional_fields_default() {
        // parse an agent that omits the tool lists and max_turns
        let workflow =
            Workflow::from_yaml("agents:\n  - name: Tester\n    description: d\n    preamble: p\n")
                .expect("minimal workflow should parse");
        assert!(workflow.agents[0].tools.is_empty());
        assert!(workflow.agents[0].disabled_tools.is_empty());
        assert!(!workflow.agents[0].read_only);
        assert_eq!(workflow.agents[0].max_turns, 10);
        // the swarm limits fall back to their defaults too
        assert_eq!(workflow.agents[0].max_swarm_size, 4);
        assert!(workflow.agents[0].swarmable());
        assert_eq!(workflow.max_alive_sub_agents, 8);
        // the forum-context fields default to linked mode at the default depth
        assert_eq!(workflow.forum_context, ForumContextMode::Linked);
        assert_eq!(workflow.forum_link_depth, DEFAULT_FORUM_LINK_DEPTH);
    }

    /// The turn-budget knobs derive from max_turns when a yaml omits them.
    #[test]
    fn turn_budget_defaults_derive_from_max_turns() {
        // a yaml without the new fields still parses (the serde-default guard)
        let workflow = Workflow::from_yaml(
            "agents:\n  - name: Tester\n    description: d\n    preamble: p\n    max_turns: 40\n",
        )
        .expect("a workflow without budget fields should parse");
        let config = &workflow.agents[0];
        assert_eq!(config.max_extension_turns, None);
        assert_eq!(config.warn_turns, None);
        // the derived defaults are a quarter and a fifth of the turn cap
        assert_eq!(config.extension_allowance(), 10);
        assert_eq!(config.warn_window(), 5);
        // the derivations clamp at both ends of the turn-cap range
        let mut tiny = agent("Tiny");
        tiny.max_turns = 1;
        assert_eq!(tiny.extension_allowance(), 2);
        assert_eq!(tiny.warn_window(), 1);
        let mut huge = agent("Huge");
        huge.max_turns = 200;
        assert_eq!(huge.extension_allowance(), 25);
        assert_eq!(huge.warn_window(), 5);
        // an explicit zero allowance disables self-service extensions
        let mut off = agent("Off");
        off.max_extension_turns = Some(0);
        assert_eq!(off.extension_allowance(), 0);
        // the bundled budget carries the resolved values plus the window
        let budget = workflow.agents[0].turn_budget(200_000);
        assert_eq!(budget.max_turns, 40);
        assert_eq!(budget.max_extension_turns, 10);
        assert_eq!(budget.warn_turns, 5);
        assert_eq!(budget.context_window, 200_000);
    }

    /// Explicit forum-context fields parse, and a bad mode is rejected.
    #[test]
    fn forum_context_parses_from_yaml() {
        // an explicit full mode and custom depth override the defaults
        let yaml = "forum_context: full\nforum_link_depth: 4\nagents:\n  - name: Dev\n    description: d\n    preamble: p\n";
        let workflow = Workflow::from_yaml(yaml).expect("explicit workflow should parse");
        assert_eq!(workflow.forum_context, ForumContextMode::Full);
        assert_eq!(workflow.forum_link_depth, 4);
        // an unknown mode string is rejected at parse time
        let bogus =
            "forum_context: bogus\nagents:\n  - name: Dev\n    description: d\n    preamble: p\n";
        assert!(Workflow::from_yaml(bogus).is_err());
    }

    /// The generated forum-reading guidance matches the context mode.
    #[test]
    fn forum_reading_preamble_matches_mode() {
        // linked mode points the agent at its preloaded context section
        let linked = forum_reading_preamble(ForumContextMode::Linked);
        assert!(linked.contains("# Forum context"));
        assert!(linked.contains("supersedes"));
        // full mode mandates the whole-forum download instead
        let full = forum_reading_preamble(ForumContextMode::Full);
        assert!(full.contains("Always start by calling list_posts"));
        assert!(!full.contains("# Forum context"));
        // both modes point the agent at search_forum for targeted discovery
        assert!(linked.contains("search_forum"));
        assert!(full.contains("search_forum"));
    }

    /// The builtin preambles rely on the generated reading guidance instead of
    /// hardcoding a list_posts-first instruction.
    #[test]
    fn builtin_preambles_omit_list_posts_first() {
        let workflow = Workflow::builtin().expect("builtin workflow.yaml should be valid");
        // no builtin agent may hardcode the list_posts-first instruction
        for agent in &workflow.agents {
            assert!(
                !agent.preamble.contains("start by calling"),
                "agent {} still mandates list_posts-first in its preamble",
                agent.name
            );
        }
    }

    /// A max swarm size of zero opts an agent out of swarms.
    #[test]
    fn zero_swarm_size_is_not_swarmable() {
        // parse an agent that explicitly opts out of swarms
        let workflow = Workflow::from_yaml(
            "agents:\n  - name: Tester\n    description: d\n    preamble: p\n    max_swarm_size: 0\n",
        )
        .expect("opted out workflow should parse");
        assert!(!workflow.agents[0].swarmable());
    }

    /// A group entry resolves to every tool in that group.
    #[test]
    fn groups_resolve_to_their_tools() {
        // enable the whole ghidra group on an agent
        let mut config = agent("ReverseEngineer");
        config.tools = vec!["ghidra".to_owned()];
        // the resolved toolset is exactly the ghidra group, sorted by name
        let resolved = config.resolve_tools().expect("ghidra group should resolve");
        let names: Vec<&str> = resolved.iter().map(|entry| entry.name).collect();
        assert_eq!(
            names,
            [
                "callees",
                "callers",
                "decomp_function",
                "disassemble_function",
                "emulate_function",
                "find_function",
                "get_function",
                "import_child",
                "list_comments",
                "list_exports",
                "list_funcs",
                "list_imports",
                "list_programs",
                "list_strings",
                "memory_map",
                "read",
                "rename_function",
                "rename_symbol",
                "search_memory",
                "set_comment",
                "set_return_type",
                "set_signature",
                "string_refs",
                "xrefs_from",
                "xrefs_to",
            ]
        );
    }

    /// A read-only agent drops every mutating tool after group expansion.
    #[test]
    fn read_only_strips_mutating_tools() {
        // enable the whole ghidra group on a read-only agent
        let mut config = agent("ReverseEngineer");
        config.tools = vec!["ghidra".to_owned()];
        config.read_only = true;
        // the resolved toolset must keep the read tools but drop the write ones
        let resolved = config.resolve_tools().expect("ghidra group should resolve");
        let names: Vec<&str> = resolved.iter().map(|entry| entry.name).collect();
        assert!(names.contains(&"decomp_function"));
        assert!(names.contains(&"list_strings"));
        for write_tool in [
            "rename_function",
            "rename_symbol",
            "set_comment",
            "set_return_type",
            "set_signature",
        ] {
            assert!(!names.contains(&write_tool));
        }
        // the same agent keeps the write tools once read_only is lifted
        config.read_only = false;
        let resolved = config.resolve_tools().expect("ghidra group should resolve");
        let names: Vec<&str> = resolved.iter().map(|entry| entry.name).collect();
        assert!(names.contains(&"rename_function"));
    }

    /// The read_only flag parses from yaml and defaults to false.
    #[test]
    fn read_only_parses_from_yaml() {
        // an agent that declares itself read-only parses with the flag set
        let yaml = "agents:\n  - name: Tester\n    description: d\n    preamble: p\n    tools: [ghidra]\n    read_only: true\n";
        let workflow = Workflow::from_yaml(yaml).expect("read-only workflow should parse");
        assert!(workflow.agents[0].read_only);
    }

    /// Individual tools can be enabled by name alongside groups.
    #[test]
    fn individual_tools_resolve_by_name() {
        // enable a single tool by its name
        let mut config = agent("Transformer");
        config.tools = vec!["bytewise_xor".to_owned()];
        // only that one tool should resolve
        let resolved = config.resolve_tools().expect("tool name should resolve");
        let names: Vec<&str> = resolved.iter().map(|entry| entry.name).collect();
        assert_eq!(names, ["bytewise_xor"]);
    }

    /// Disabled tools are removed after group expansion.
    #[test]
    fn disabled_tools_override_groups() {
        // enable the ghidra group but drop the raw memory tools from it
        let mut config = agent("ReverseEngineer");
        config.tools = vec!["ghidra".to_owned()];
        config.disabled_tools = vec!["read".to_owned(), "memory_map".to_owned()];
        // the disabled tools must not be in the resolved set
        let resolved = config.resolve_tools().expect("group should still resolve");
        let names: Vec<&str> = resolved.iter().map(|entry| entry.name).collect();
        assert!(!names.contains(&"read"));
        assert!(!names.contains(&"memory_map"));
        assert!(names.contains(&"decomp_function"));
    }

    /// The tool lists parse from yaml and are validated at load time.
    #[test]
    fn tool_lists_parse_and_validate_from_yaml() {
        // a workflow enabling a group and disabling one of its tools parses
        let yaml = "agents:\n  - name: Tester\n    description: d\n    preamble: p\n    tools: [ghidra]\n    disabled_tools: [read]\n";
        let workflow = Workflow::from_yaml(yaml).expect("tool lists should parse");
        assert_eq!(workflow.agents[0].tools, ["ghidra"]);
        assert_eq!(workflow.agents[0].disabled_tools, ["read"]);
        // a typo'd group name is rejected when the workflow is loaded
        let bad =
            "agents:\n  - name: Tester\n    description: d\n    preamble: p\n    tools: [ghydra]\n";
        assert!(Workflow::from_yaml(bad).is_err());
    }

    /// Unknown tool or group names are rejected at validation time.
    #[test]
    fn unknown_tool_names_are_rejected() {
        // a typo'd group name must fail resolution and validation
        let mut config = agent("ReverseEngineer");
        config.tools = vec!["ghydra".to_owned()];
        assert!(config.resolve_tools().is_err());
        assert!(workflow(vec![config]).validate().is_err());
        // an unknown disabled tool is a typo worth rejecting too
        let mut config = agent("ReverseEngineer");
        config.tools = vec!["ghidra".to_owned()];
        config.disabled_tools = vec!["decompile".to_owned()];
        assert!(config.resolve_tools().is_err());
        assert!(workflow(vec![config]).validate().is_err());
    }

    /// Invalid workflows are rejected.
    #[test]
    fn invalid_workflows_are_rejected() {
        // an empty workflow has no agents to delegate to
        assert!(workflow(vec![]).validate().is_err());
        // names must be ascii alphanumeric
        assert!(workflow(vec![agent("Bad Name")]).validate().is_err());
        assert!(workflow(vec![agent("")]).validate().is_err());
        // two agents may not share a tool name
        let dupes = vec![agent("Researcher"), agent("Researcher")];
        assert!(workflow(dupes).validate().is_err());
        // a run must allow at least one live sub-agent
        let mut no_slots = workflow(vec![agent("Researcher")]);
        no_slots.max_alive_sub_agents = 0;
        assert!(no_slots.validate().is_err());
    }
}
