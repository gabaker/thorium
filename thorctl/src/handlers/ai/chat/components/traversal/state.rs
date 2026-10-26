//! The traversal graph's events, state, and pure reducer
//!
//! [`GraphState::apply`] is the single fold both the live event drain and the
//! transcript replay run their [`TraversalEvent`]s through, so a replayed
//! session rebuilds exactly the graph the live run produced. Target
//! normalization mirrors `thorpal`'s coverage ledger
//! (`thorpal/src/thorpal/coverage.rs`), with the same caveat: a function only
//! ever referenced by name stays a name-keyed node until a tool result reveals
//! its address, at which point the nodes merge.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use thorpal::{ThorPalEvent, TranscriptItem, TranscriptLine, registry};

/// How many recent non-function-scoped tool calls the side strip remembers
const GLOBAL_TOOL_CAP: usize = 50;

/// How many discovered-only neighbor nodes a single tool result may add
const NEIGHBOR_CAP: usize = 50;

/// One normalized traversal event, derivable from live events and transcript
/// lines alike
#[derive(Debug, Clone, PartialEq)]
pub enum TraversalEvent {
    /// An agent (sub-agent or swarm member) came into existence
    AgentStarted {
        /// The unique per-call display name of the agent
        agent: String,
    },
    /// A tool call started
    ToolCall {
        /// The display name of the agent making the call
        agent: String,
        /// The call id correlating this call with its result
        call_id: String,
        /// The name of the tool being called
        tool: String,
        /// The raw JSON argument string the tool was called with
        args: String,
        /// Milliseconds since the Unix epoch when the call was captured
        ts: Option<u64>,
    },
    /// A tool call finished
    ToolResult {
        /// The display name of the agent that made the call
        agent: String,
        /// The call id correlating this result with its call
        call_id: String,
        /// The name of the tool that ran
        tool: String,
        /// The model-visible rendered output of the tool call
        result: String,
        /// Whether the tool call succeeded
        success: bool,
        /// Milliseconds since the Unix epoch when the result was captured
        ts: Option<u64>,
    },
}

impl TraversalEvent {
    /// Maps a live event onto a traversal event, if it is one
    ///
    /// Live events carry no capture time of their own, so they are stamped at
    /// receive time -- close enough to the persisted stamp for replay pacing.
    ///
    /// # Arguments
    ///
    /// * `event` - The live event to map
    pub fn from_event(event: &ThorPalEvent) -> Option<TraversalEvent> {
        // stamp live events as they arrive so replay can pace proportionally
        let ts = now_ms();
        match event {
            // a sub-agent coming alive gets a cursor even before its first call
            ThorPalEvent::SubAgentStarted { name, .. } => Some(TraversalEvent::AgentStarted {
                agent: name.clone(),
            }),
            // a tool call and its correlated result drive the whole graph
            ThorPalEvent::ToolCallStarted {
                agent,
                call_id,
                tool,
                args,
            } => Some(TraversalEvent::ToolCall {
                agent: agent.clone(),
                call_id: call_id.clone(),
                tool: tool.clone(),
                args: args.clone(),
                ts,
            }),
            ThorPalEvent::ToolCallFinished {
                agent,
                call_id,
                tool,
                result,
                success,
            } => Some(TraversalEvent::ToolResult {
                agent: agent.clone(),
                call_id: call_id.clone(),
                tool: tool.clone(),
                result: result.clone(),
                success: *success,
                ts,
            }),
            // nothing else moves agents between functions
            _ => None,
        }
    }

    /// Maps a persisted transcript line onto a traversal event, if it is one
    ///
    /// # Arguments
    ///
    /// * `line` - The persisted transcript line to map
    pub fn from_transcript(line: &TranscriptLine) -> Option<TraversalEvent> {
        match &line.item {
            // a sub-agent coming alive gets a cursor even before its first call
            TranscriptItem::SubAgentStarted { name, .. } => Some(TraversalEvent::AgentStarted {
                agent: name.clone(),
            }),
            // a tool call and its correlated result drive the whole graph
            TranscriptItem::ToolCall {
                agent,
                call_id,
                tool,
                args,
            } => Some(TraversalEvent::ToolCall {
                agent: agent.clone(),
                call_id: call_id.clone(),
                tool: tool.clone(),
                args: args.clone(),
                ts: line.ts,
            }),
            TranscriptItem::ToolResult {
                agent,
                call_id,
                tool,
                result,
                success,
            } => Some(TraversalEvent::ToolResult {
                agent: agent.clone(),
                call_id: call_id.clone(),
                tool: tool.clone(),
                result: result.clone(),
                success: *success,
                ts: line.ts,
            }),
            // nothing else moves agents between functions
            _ => None,
        }
    }

    /// The capture time of this event in milliseconds since the epoch, if known
    pub fn ts(&self) -> Option<u64> {
        match self {
            // agent starts are synthesized breadcrumbs with no stamp of their own
            TraversalEvent::AgentStarted { .. } => None,
            TraversalEvent::ToolCall { ts, .. } | TraversalEvent::ToolResult { ts, .. } => *ts,
        }
    }
}

/// The current wall clock as milliseconds since the Unix epoch
fn now_ms() -> Option<u64> {
    // a pre-epoch clock yields none rather than a bogus stamp
    u64::try_from(chrono::Utc::now().timestamp_millis()).ok()
}

/// The canonical identity of a function node
///
/// Address-keyed when the address is known, name-keyed otherwise; a name-keyed
/// node merges into an address-keyed one as soon as a tool result reveals the
/// pairing. `Ord` keeps `BTreeMap` iteration (and thus layout and rendering)
/// deterministic.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NodeId {
    /// A normalized entry-point address (lowercase hex, no `0x`, no leading zeros)
    Addr(String),
    /// A function name no address is known for yet
    Name(String),
}

impl NodeId {
    /// Builds an address-keyed node id from a raw address string
    ///
    /// # Arguments
    ///
    /// * `raw` - The raw address in any hex form (`0x`-prefixed or bare)
    pub fn addr(raw: &str) -> NodeId {
        NodeId::Addr(normalize_addr(raw))
    }

    /// The target string a Ghidra query resolves this node by
    pub fn target_string(&self) -> String {
        match self {
            // addresses query in their explicit 0x form
            NodeId::Addr(addr) => format!("0x{addr}"),
            // names query verbatim
            NodeId::Name(name) => name.clone(),
        }
    }
}

/// One function node in the traversal graph
#[derive(Debug, Clone, PartialEq)]
pub struct FnNode {
    /// The canonical identity of this node
    pub id: NodeId,
    /// The best display name known (symbol name when learned, address otherwise)
    pub label: String,
    /// The normalized entry-point address, when known
    pub address: Option<String>,
    /// The per-agent set of tools used on this function
    pub agents: BTreeMap<String, BTreeSet<String>>,
    /// How many times each tool was called on this function
    pub tool_counts: BTreeMap<String, u32>,
    /// How many function-scoped tool calls targeted this node
    pub visits: u32,
    /// An `examining` tool (decompile/disassemble) ran on this function
    pub examined: bool,
    /// A `mutating` tool (set_signature/set_return_type/...) ran on this function
    pub mutated: bool,
    /// The node's only visit failed, so the target is likely bogus
    pub unresolved: bool,
    /// How many discovered-only neighbors were dropped by the per-result cap
    pub hidden_neighbors: u32,
    /// This function calls itself (self-loops are annotated, not drawn)
    pub recursive: bool,
    /// The monotonic discovery index driving stable layout
    pub seq: u32,
}

impl FnNode {
    /// Creates a new, untouched node
    ///
    /// # Arguments
    ///
    /// * `id` - The canonical identity of the node
    /// * `label` - The best display name known at creation
    /// * `seq` - The monotonic discovery index assigned to this node
    fn new(id: NodeId, label: String, seq: u32) -> Self {
        // an address-keyed node knows its address from its own identity
        let address = match &id {
            NodeId::Addr(addr) => Some(addr.clone()),
            NodeId::Name(_) => None,
        };
        FnNode {
            id,
            label,
            address,
            agents: BTreeMap::new(),
            tool_counts: BTreeMap::new(),
            visits: 0,
            examined: false,
            mutated: false,
            unresolved: false,
            hidden_neighbors: 0,
            recursive: false,
            seq,
        }
    }
}

/// The per-agent live cursor: where an agent is and what it is doing
#[derive(Debug, Clone, PartialEq)]
pub struct AgentCursor {
    /// The display name of this agent
    pub agent: String,
    /// The stable palette slot assigned in first-seen order
    pub color_slot: usize,
    /// The function this agent most recently targeted
    pub at: Option<NodeId>,
    /// The tool this agent is running or last ran
    pub tool: Option<String>,
    /// A tool call is currently in flight for this agent
    pub busy: bool,
    /// Whether this agent's last finished tool call succeeded
    pub last_ok: Option<bool>,
}

/// A recent non-function-scoped tool call shown in the side strip
#[derive(Debug, Clone, PartialEq)]
pub struct GlobalToolCall {
    /// The call id correlating this call with its result
    pub call_id: String,
    /// The display name of the agent that made the call
    pub agent: String,
    /// The name of the tool that was called
    pub tool: String,
    /// Whether the call succeeded, once its result has arrived
    pub success: Option<bool>,
}

/// Aggregate call counts for one tool across the whole session
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolStats {
    /// How many times the tool was called
    pub calls: u32,
    /// How many of those calls finished successfully
    pub ok: u32,
    /// How many of those calls finished in failure
    pub err: u32,
}

/// An in-flight tool call awaiting its result
#[derive(Debug, Clone, PartialEq)]
struct PendingCall {
    /// The display name of the agent that made the call
    agent: String,
    /// The function node the call targeted, for function-scoped tools
    target: Option<NodeId>,
}

/// The whole reduced traversal graph (a pure value, no locks)
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GraphState {
    /// Every function node, keyed by canonical identity
    pub nodes: BTreeMap<NodeId, FnNode>,
    /// Every deduplicated call edge as a `(caller, callee)` pair
    pub edges: BTreeSet<(NodeId, NodeId)>,
    /// The learned `name -> normalized address` alias map
    pub aliases: HashMap<String, String>,
    /// The per-agent cursors, keyed by agent display name
    pub agents: BTreeMap<String, AgentCursor>,
    /// The in-flight tool calls awaiting results, keyed by call id
    pending: HashMap<String, PendingCall>,
    /// The recent non-function-scoped tool calls, newest last
    pub global_tools: VecDeque<GlobalToolCall>,
    /// The per-tool aggregate call counts, scoped and global tools alike
    pub tool_counts: BTreeMap<String, ToolStats>,
    /// The next monotonic discovery index to assign
    next_seq: u32,
}

impl GraphState {
    /// Folds one event into the state (the reducer; pure and infallible)
    ///
    /// # Arguments
    ///
    /// * `event` - The traversal event to fold in
    pub fn apply(&mut self, event: &TraversalEvent) {
        match event {
            // an agent came alive: give it a cursor and a palette slot
            TraversalEvent::AgentStarted { agent } => {
                self.cursor(agent);
            }
            // a tool call started: move the agent and upsert the target node
            TraversalEvent::ToolCall {
                agent,
                call_id,
                tool,
                args,
                ..
            } => self.apply_call(agent, call_id, tool, args),
            // a tool call finished: settle the cursor and mine the result
            TraversalEvent::ToolResult {
                agent,
                call_id,
                tool,
                result,
                success,
                ..
            } => self.apply_result(agent, call_id, tool, result, *success),
        }
    }

    /// The highest visit count across all nodes, for heat scaling
    pub fn max_visits(&self) -> u32 {
        // discovered-only nodes sit at zero and never win
        self.nodes
            .values()
            .map(|node| node.visits)
            .max()
            .unwrap_or(0)
    }

    /// Folds a started tool call into the state
    ///
    /// # Arguments
    ///
    /// * `agent` - The display name of the agent making the call
    /// * `call_id` - The call id correlating this call with its result
    /// * `tool` - The name of the tool being called
    /// * `args` - The raw JSON argument string the tool was called with
    fn apply_call(&mut self, agent: &str, call_id: &str, tool: &str, args: &str) {
        // every call counts toward the per-tool aggregates, scoped or not
        self.tool_counts.entry(tool.to_owned()).or_default().calls += 1;
        // the calling agent gets a cursor on its first activity (this also
        // covers "main" and swarm members never announced via AgentStarted)
        let cursor = self.cursor(agent);
        cursor.tool = Some(tool.to_owned());
        cursor.busy = true;
        // a function-scoped call with a string target visits a function node
        let target = registry::is_function_scoped(tool)
            .then(|| extract_target(args))
            .flatten();
        if let Some(target) = target {
            // resolve the target to its canonical node and record the visit
            let id = self.canonical_id(&target);
            let node = self.node_mut(id.clone(), display_label(&target));
            node.visits += 1;
            // a visit always outranks an earlier lone failure
            node.unresolved = false;
            node.agents
                .entry(agent.to_owned())
                .or_default()
                .insert(tool.to_owned());
            // count this tool's call against the node's own stats
            *node.tool_counts.entry(tool.to_owned()).or_default() += 1;
            // flag the node when the tool reads or mutates the function
            if registry::is_examining(tool) {
                node.examined = true;
            }
            if registry::is_mutating(tool) {
                node.mutated = true;
            }
            // move the agent's cursor onto the node and track the call
            if let Some(cursor) = self.agents.get_mut(agent) {
                cursor.at = Some(id.clone());
            }
            self.pending.insert(
                call_id.to_owned(),
                PendingCall {
                    agent: agent.to_owned(),
                    target: Some(id),
                },
            );
        } else {
            // a non-function call lands in the side strip's global feed
            self.global_tools.push_back(GlobalToolCall {
                call_id: call_id.to_owned(),
                agent: agent.to_owned(),
                tool: tool.to_owned(),
                success: None,
            });
            // keep the feed bounded, dropping the oldest entries
            while self.global_tools.len() > GLOBAL_TOOL_CAP {
                self.global_tools.pop_front();
            }
            self.pending.insert(
                call_id.to_owned(),
                PendingCall {
                    agent: agent.to_owned(),
                    target: None,
                },
            );
        }
    }

    /// Folds a finished tool call into the state
    ///
    /// # Arguments
    ///
    /// * `agent` - The display name of the agent that made the call
    /// * `call_id` - The call id correlating this result with its call
    /// * `tool` - The name of the tool that ran
    /// * `result` - The model-visible rendered output of the call
    /// * `success` - Whether the call succeeded
    fn apply_result(
        &mut self,
        agent: &str,
        call_id: &str,
        tool: &str,
        result: &str,
        success: bool,
    ) {
        // settle the agent's cursor regardless of what the call targeted
        let cursor = self.cursor(agent);
        cursor.busy = false;
        cursor.last_ok = Some(success);
        // settle the per-tool aggregate, counting orphan results too
        let stats = self.tool_counts.entry(tool.to_owned()).or_default();
        if success {
            stats.ok += 1;
        } else {
            stats.err += 1;
        }
        // settle the matching global feed entry, when the call was global
        if let Some(entry) = self
            .global_tools
            .iter_mut()
            .find(|entry| entry.call_id == call_id)
        {
            entry.success = Some(success);
        }
        // pop the in-flight record; an orphan result (torn call) is ignored
        let Some(pending) = self.pending.remove(call_id) else {
            return;
        };
        if let Some(target) = pending.target {
            if success {
                // mine the result for aliases, neighbor nodes, and call edges
                self.mine_result(&target, tool, result);
            } else if let Some(node) = self.nodes.get_mut(&target) {
                // a failed lone visit marks the target as likely bogus
                if node.visits == 1 {
                    node.unresolved = true;
                }
            }
        }
    }

    /// Mines a successful edge-bearing tool result for aliases, nodes, and edges
    ///
    /// Only `get_function`, `callers`, `callees`, and the xref tools carry
    /// relationship data. Results are compact JSON in practice; anything that
    /// fails to parse simply yields no edges (the visit itself was already
    /// recorded from the call's args).
    ///
    /// # Arguments
    ///
    /// * `target` - The node the finished call targeted
    /// * `tool` - The name of the tool that ran
    /// * `result` - The rendered output text to mine
    fn mine_result(&mut self, target: &NodeId, tool: &str, result: &str) {
        // only the relationship tools carry anything worth mining
        if !matches!(
            tool,
            "get_function" | "callers" | "callees" | "xrefs_to" | "xrefs_from"
        ) {
            return;
        }
        // best-effort parse; non-JSON or truncated output mines nothing
        let Ok(value) = serde_json::from_str::<serde_json::Value>(result) else {
            return;
        };
        // the target may merge into an address-keyed node as aliases land, so
        // re-resolve it after every mining step through the alias map
        match tool {
            // a function's details pair its name with its address
            "get_function" => {
                if let (Some(name), Some(addr)) =
                    (field_str(&value, "name"), field_str(&value, "address"))
                {
                    self.learn_alias(name, addr);
                }
            }
            // direct callers/callees pair neighbor names with addresses and
            // give us an edge to/from the target; deeper entries carry no
            // parent linkage, so only depth-0 entries become edges
            "callers" => self.mine_call_list(target, &value, "callers", true),
            "callees" => self.mine_call_list(target, &value, "callees", false),
            // xrefs only yield call edges; the containing function is a name
            "xrefs_to" => self.mine_xrefs(target, &value, true),
            "xrefs_from" => self.mine_xrefs(target, &value, false),
            _ => (),
        }
    }

    /// Mines a `callers`/`callees` payload for neighbor nodes and edges
    ///
    /// # Arguments
    ///
    /// * `target` - The node the finished call targeted
    /// * `value` - The parsed result payload
    /// * `key` - The payload key holding the neighbor list
    /// * `inbound` - Whether the neighbors call the target (vs are called by it)
    fn mine_call_list(
        &mut self,
        target: &NodeId,
        value: &serde_json::Value,
        key: &str,
        inbound: bool,
    ) {
        // the neighbor list is an array of {name, address, call_site, depth}
        let Some(list) = value.get(key).and_then(serde_json::Value::as_array) else {
            return;
        };
        // count how many direct neighbors the per-result cap drops
        let mut added = 0u32;
        let mut dropped = 0u32;
        for entry in list {
            // a neighbor needs at least a name or an address to be a node
            let name = field_str(entry, "name");
            let addr = field_str(entry, "address");
            // pair the name with its address so earlier name nodes merge; even
            // deeper entries teach a pairing worth remembering
            if let (Some(name), Some(addr)) = (name, addr) {
                self.learn_alias(name, addr);
            }
            // deeper entries carry no parent linkage, so only direct ones count
            let depth = entry
                .get("depth")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            if depth != 0 {
                continue;
            }
            // enforce the per-result cap on discovered-only neighbors
            if added >= NEIGHBOR_CAP as u32 {
                dropped += 1;
                continue;
            }
            // resolve the neighbor to a canonical node, preferring the address
            let neighbor = match (addr, name) {
                (Some(addr), _) => self.canonical_id(addr),
                (None, Some(name)) => self.canonical_id(name),
                (None, None) => continue,
            };
            let label = name.unwrap_or_else(|| addr.unwrap_or_default()).to_owned();
            self.node_mut(neighbor.clone(), label);
            added += 1;
            // the target itself may have merged while aliases landed
            let target = self.resolve(target);
            // a self-call is annotated on the node instead of drawn as a loop
            if neighbor == target {
                if let Some(node) = self.nodes.get_mut(&target) {
                    node.recursive = true;
                }
                continue;
            }
            // orient the edge: callers point at the target, callees away from it
            let edge = if inbound {
                (neighbor, target)
            } else {
                (target, neighbor)
            };
            self.edges.insert(edge);
        }
        // remember how many neighbors the cap hid, for a "+N more" badge
        if dropped > 0 {
            let target = self.resolve(target);
            if let Some(node) = self.nodes.get_mut(&target) {
                node.hidden_neighbors += dropped;
            }
        }
    }

    /// Mines an xref list for call edges touching the target
    ///
    /// # Arguments
    ///
    /// * `target` - The node the finished call targeted
    /// * `value` - The parsed result payload (an array of xrefs)
    /// * `inbound` - Whether the xrefs point to the target (vs away from it)
    fn mine_xrefs(&mut self, target: &NodeId, value: &serde_json::Value, inbound: bool) {
        // the payload is a plain array of XRef objects
        let Some(list) = value.as_array() else {
            return;
        };
        for entry in list {
            // only call-type references become call edges
            let is_call = field_str(entry, "ref_type")
                .map(|kind| kind.contains("CALL"))
                .unwrap_or(false);
            if !is_call {
                continue;
            }
            // the far end is the function containing the other side of the
            // reference; only its name is known, so the node is name-keyed
            // (or alias-resolved) rather than falsely bound to the raw address
            let far = if inbound {
                field_str(entry, "from_function")
            } else {
                field_str(entry, "to_function")
            };
            let Some(far) = far else {
                continue;
            };
            let neighbor = self.canonical_id(far);
            self.node_mut(neighbor.clone(), display_label(far));
            // the target itself may have merged while nodes were added
            let target = self.resolve(target);
            // a self-call is annotated on the node instead of drawn as a loop
            if neighbor == target {
                if let Some(node) = self.nodes.get_mut(&target) {
                    node.recursive = true;
                }
                continue;
            }
            // orient the edge: inbound xrefs call the target, outbound leave it
            let edge = if inbound {
                (neighbor, target)
            } else {
                (target, neighbor)
            };
            self.edges.insert(edge);
        }
    }

    /// Learns that a function name pairs with an address, merging nodes
    ///
    /// # Arguments
    ///
    /// * `name` - The function's symbol name
    /// * `addr` - The function's entry-point address (any hex form)
    fn learn_alias(&mut self, name: &str, addr: &str) {
        // normalize the address onto its canonical form
        let addr = normalize_addr(addr);
        // an address-shaped "name" teaches nothing worth aliasing
        if looks_like_addr(name) {
            return;
        }
        self.aliases.insert(name.to_owned(), addr.clone());
        // a pre-existing name-keyed node now merges into the address node
        let name_id = NodeId::Name(name.to_owned());
        let addr_id = NodeId::Addr(addr);
        if let Some(orphan) = self.nodes.remove(&name_id) {
            // union the orphan into the address node, creating it if needed
            let node = self.node_mut(addr_id.clone(), orphan.label.clone());
            node.visits += orphan.visits;
            node.examined |= orphan.examined;
            node.mutated |= orphan.mutated;
            node.unresolved &= orphan.unresolved;
            node.recursive |= orphan.recursive;
            node.hidden_neighbors += orphan.hidden_neighbors;
            // keep the earliest discovery index so layout stays stable
            node.seq = node.seq.min(orphan.seq);
            // the symbol name is always the better label
            node.label = orphan.label;
            for (agent, tools) in orphan.agents {
                node.agents.entry(agent).or_default().extend(tools);
            }
            // the orphan's per-tool counts fold into the merged node
            for (tool, count) in orphan.tool_counts {
                *node.tool_counts.entry(tool).or_default() += count;
            }
            // re-point every edge, cursor, and pending call at the merged node
            self.repoint(&name_id, &addr_id);
        } else if let Some(node) = self.nodes.get_mut(&addr_id) {
            // no orphan, but an existing address node learns its real name
            if looks_like_addr(&node.label) {
                node.label = name.to_owned();
            }
        }
    }

    /// Re-points edges, cursors, and pending calls from one node id to another
    ///
    /// # Arguments
    ///
    /// * `from` - The merged-away node id
    /// * `to` - The canonical node id everything now points at
    fn repoint(&mut self, from: &NodeId, to: &NodeId) {
        // rebuild the edge set, mapping both endpoints through the merge and
        // dropping any self-loop the merge may have created
        let edges = std::mem::take(&mut self.edges);
        for (a, b) in edges {
            let a = if &a == from { to.clone() } else { a };
            let b = if &b == from { to.clone() } else { b };
            if a == b {
                // a merge-created self-loop means the function is recursive
                if let Some(node) = self.nodes.get_mut(&a) {
                    node.recursive = true;
                }
                continue;
            }
            self.edges.insert((a, b));
        }
        // move every cursor standing on the merged-away node
        for cursor in self.agents.values_mut() {
            if cursor.at.as_ref() == Some(from) {
                cursor.at = Some(to.clone());
            }
        }
        // keep in-flight calls pointed at the canonical node too
        for pending in self.pending.values_mut() {
            if pending.target.as_ref() == Some(from) {
                pending.target = Some(to.clone());
            }
        }
    }

    /// Resolves a node id through the alias map onto its canonical form
    ///
    /// # Arguments
    ///
    /// * `id` - The node id to resolve
    fn resolve(&self, id: &NodeId) -> NodeId {
        // only name-keyed nodes can have learned a better identity
        match id {
            NodeId::Name(name) => match self.aliases.get(name) {
                Some(addr) => NodeId::Addr(addr.clone()),
                None => id.clone(),
            },
            NodeId::Addr(_) => id.clone(),
        }
    }

    /// The canonical node id for a raw target or function reference
    ///
    /// # Arguments
    ///
    /// * `target` - The raw target string (a name, `0x` address, or bare hex)
    fn canonical_id(&self, target: &str) -> NodeId {
        // address-shaped targets key by normalized address
        let trimmed = target.trim();
        if looks_like_addr(trimmed) {
            return NodeId::Addr(normalize_addr(trimmed));
        }
        // names resolve through the alias map when a pairing is known
        match self.aliases.get(trimmed) {
            Some(addr) => NodeId::Addr(addr.clone()),
            None => NodeId::Name(trimmed.to_owned()),
        }
    }

    /// Gets or creates the node for an id, assigning its discovery index
    ///
    /// # Arguments
    ///
    /// * `id` - The canonical identity of the node
    /// * `label` - The display label to use when the node is new
    fn node_mut(&mut self, id: NodeId, label: String) -> &mut FnNode {
        // hand out the next discovery index only when the node is new
        let next_seq = &mut self.next_seq;
        self.nodes.entry(id.clone()).or_insert_with(|| {
            let seq = *next_seq;
            *next_seq += 1;
            FnNode::new(id, label, seq)
        })
    }

    /// Gets or creates the cursor for an agent, assigning its palette slot
    ///
    /// # Arguments
    ///
    /// * `agent` - The display name of the agent
    fn cursor(&mut self, agent: &str) -> &mut AgentCursor {
        // palette slots are handed out in first-seen order and never change
        let slot = self.agents.len();
        self.agents
            .entry(agent.to_owned())
            .or_insert_with(|| AgentCursor {
                agent: agent.to_owned(),
                color_slot: slot,
                at: None,
                tool: None,
                busy: false,
                last_ok: None,
            })
    }
}

/// Extracts the `target` string from a tool call's raw JSON arguments
///
/// # Arguments
///
/// * `args` - The raw JSON argument string the tool was called with
fn extract_target(args: &str) -> Option<String> {
    // the args must parse to an object carrying a string `target`
    match serde_json::from_str::<serde_json::Value>(args) {
        Ok(serde_json::Value::Object(map)) => map
            .get("target")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        _ => None,
    }
}

/// Reads a string field off a JSON object, if present
///
/// # Arguments
///
/// * `value` - The JSON value to read from
/// * `key` - The field name to read
fn field_str<'v>(value: &'v serde_json::Value, key: &str) -> Option<&'v str> {
    value.get(key).and_then(serde_json::Value::as_str)
}

/// Whether a reference looks like a hex address rather than a symbol name
///
/// # Arguments
///
/// * `target` - The trimmed reference to classify
fn looks_like_addr(target: &str) -> bool {
    // an explicit 0x prefix is always an address
    let bare = target.strip_prefix("0x").unwrap_or(target);
    // otherwise it must be non-empty, all hex, and at least four digits so
    // short all-hex names (e.g. "add") stay names
    !bare.is_empty() && bare.len() >= 4 && bare.chars().all(|c| c.is_ascii_hexdigit())
}

/// Normalizes an address onto its canonical form
///
/// Mirrors `thorpal`'s coverage normalization (trim + strip `0x`) and
/// additionally lowercases and strips leading zeros, so `0x00402574`,
/// `00402574`, and `402574` all land on one key.
///
/// # Arguments
///
/// * `addr` - The raw address string to normalize
fn normalize_addr(addr: &str) -> String {
    // drop surrounding whitespace and a single 0x prefix
    let trimmed = addr.trim();
    let bare = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    // lowercase and strip leading zeros, keeping at least one digit
    let lowered = bare.to_ascii_lowercase();
    let stripped = lowered.trim_start_matches('0');
    if stripped.is_empty() {
        "0".to_owned()
    } else {
        stripped.to_owned()
    }
}

/// The display label for a raw target reference
///
/// # Arguments
///
/// * `target` - The raw target string the agent used
fn display_label(target: &str) -> String {
    // addresses display in their canonical 0x form; names display verbatim
    let trimmed = target.trim();
    if looks_like_addr(trimmed) {
        format!("0x{}", normalize_addr(trimmed))
    } else {
        trimmed.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A function-scoped tool call event targeting a function
    fn call(agent: &str, call_id: &str, tool: &str, target: &str) -> TraversalEvent {
        TraversalEvent::ToolCall {
            agent: agent.to_owned(),
            call_id: call_id.to_owned(),
            tool: tool.to_owned(),
            args: format!(r#"{{"target":"{target}"}}"#),
            ts: None,
        }
    }

    /// A successful tool result event carrying a rendered payload
    fn result(agent: &str, call_id: &str, tool: &str, payload: &str) -> TraversalEvent {
        TraversalEvent::ToolResult {
            agent: agent.to_owned(),
            call_id: call_id.to_owned(),
            tool: tool.to_owned(),
            result: payload.to_owned(),
            success: true,
            ts: None,
        }
    }

    /// Address targets normalize onto one canonical node key
    #[test]
    fn addresses_normalize_onto_one_key() {
        let mut state = GraphState::default();
        // the same function visited via three spellings of its address
        state.apply(&call("main", "c1", "decomp_function", "0x00402574"));
        state.apply(&call("main", "c2", "disassemble_function", "00402574"));
        state.apply(&call("main", "c3", "get_function", "0x402574"));
        // all three collapse onto one address-keyed node with three visits
        assert_eq!(state.nodes.len(), 1);
        let node = state.nodes.values().next().expect("one node");
        assert_eq!(node.id, NodeId::Addr("402574".to_owned()));
        assert_eq!(node.visits, 3);
        assert!(node.examined);
    }

    /// A callers result adds dim neighbor nodes and inbound edges
    #[test]
    fn callers_result_adds_neighbors_and_edges() {
        let mut state = GraphState::default();
        // the agent walks the callers of `entry`
        state.apply(&call("main", "c1", "callers", "entry"));
        state.apply(&result(
            "main",
            "c1",
            "callers",
            r#"{"function":"entry","callers":[
                {"name":"start","address":"00401000","call_site":"00401010","depth":0},
                {"name":"outer","address":"00400f00","call_site":"00400f10","depth":1}
            ],"count":2}"#,
        ));
        // the direct caller becomes a discovered node with an inbound edge;
        // the depth-1 entry has no parent linkage so it adds nothing
        assert_eq!(state.nodes.len(), 2);
        let start = NodeId::Addr("401000".to_owned());
        assert!(
            state
                .edges
                .contains(&(start.clone(), NodeId::Name("entry".to_owned())))
        );
        // the discovered neighbor was never visited, only seen
        assert_eq!(state.nodes.get(&start).expect("start node").visits, 0);
        // the deeper entry still taught us its alias for later merging
        assert_eq!(state.aliases.get("outer"), Some(&"400f00".to_owned()));
    }

    /// Learning a name/address pairing merges the name node into the address node
    #[test]
    fn get_function_merges_name_into_address() {
        let mut state = GraphState::default();
        // visit by name first, then learn the address from get_function, then
        // visit again by address: everything lands on one merged node
        state.apply(&call("main", "c1", "decomp_function", "entry"));
        state.apply(&call("main", "c2", "get_function", "entry"));
        state.apply(&result(
            "main",
            "c2",
            "get_function",
            r#"{"name":"entry","address":"00402574","size":10,"entry_point":"00402574"}"#,
        ));
        state.apply(&call("main", "c3", "callees", "0x402574"));
        // one node remains, address-keyed, named, with every visit counted
        assert_eq!(state.nodes.len(), 1);
        let node = state
            .nodes
            .get(&NodeId::Addr("402574".to_owned()))
            .expect("merged node");
        assert_eq!(node.label, "entry");
        assert_eq!(node.visits, 3);
        assert!(node.examined);
        // the orphan's per-tool counts folded into the merged node
        assert_eq!(node.tool_counts.get("decomp_function"), Some(&1));
        assert_eq!(node.tool_counts.get("get_function"), Some(&1));
        assert_eq!(node.tool_counts.get("callees"), Some(&1));
        // the cursor moved onto the merged node too
        let cursor = state.agents.get("main").expect("cursor");
        assert_eq!(cursor.at, Some(NodeId::Addr("402574".to_owned())));
    }

    /// A merge re-points edges touching the merged-away node
    #[test]
    fn merge_repoints_edges() {
        let mut state = GraphState::default();
        // an xref walk links `helper` (by name) to the target first
        state.apply(&call("main", "c1", "xrefs_to", "entry"));
        state.apply(&result(
            "main",
            "c1",
            "xrefs_to",
            r#"[{"from":"00401234","to":"00402574","ref_type":"UNCONDITIONAL_CALL","from_function":"helper"}]"#,
        ));
        assert!(state.edges.contains(&(
            NodeId::Name("helper".to_owned()),
            NodeId::Name("entry".to_owned())
        )));
        // then a get_function on helper reveals its address
        state.apply(&call("main", "c2", "get_function", "helper"));
        state.apply(&result(
            "main",
            "c2",
            "get_function",
            r#"{"name":"helper","address":"00401200","size":5,"entry_point":"00401200"}"#,
        ));
        // the edge now starts at the address-keyed helper node
        assert!(state.edges.contains(&(
            NodeId::Addr("401200".to_owned()),
            NodeId::Name("entry".to_owned())
        )));
        assert!(
            !state
                .edges
                .iter()
                .any(|(a, _)| a == &NodeId::Name("helper".to_owned()))
        );
    }

    /// A failed lone visit marks the node unresolved; a later visit clears it
    #[test]
    fn failed_lone_call_marks_unresolved() {
        let mut state = GraphState::default();
        // the agent targets a function that does not exist
        state.apply(&call("main", "c1", "decomp_function", "no_such_func"));
        state.apply(&TraversalEvent::ToolResult {
            agent: "main".to_owned(),
            call_id: "c1".to_owned(),
            tool: "decomp_function".to_owned(),
            result: "no function named no_such_func".to_owned(),
            success: false,
            ts: None,
        });
        let id = NodeId::Name("no_such_func".to_owned());
        assert!(state.nodes.get(&id).expect("node").unresolved);
        // a later visit outranks the earlier lone failure
        state.apply(&call("main", "c2", "disassemble_function", "no_such_func"));
        assert!(!state.nodes.get(&id).expect("node").unresolved);
    }

    /// Non-JSON results mine nothing but the visit itself still counts
    #[test]
    fn non_json_result_adds_no_edges() {
        let mut state = GraphState::default();
        state.apply(&call("main", "c1", "callers", "entry"));
        state.apply(&result("main", "c1", "callers", "not json at all"));
        // the visit was recorded from the call args alone
        assert_eq!(state.nodes.len(), 1);
        assert!(state.edges.is_empty());
    }

    /// Non-function-scoped tools feed the global side-strip feed
    #[test]
    fn global_tools_feed_is_bounded() {
        let mut state = GraphState::default();
        // overflow the feed by a few entries
        for i in 0..(GLOBAL_TOOL_CAP + 3) {
            state.apply(&TraversalEvent::ToolCall {
                agent: "main".to_owned(),
                call_id: format!("c{i}"),
                tool: "list_strings".to_owned(),
                args: "{}".to_owned(),
                ts: None,
            });
        }
        // the feed stays bounded, keeping the newest entries, and no function
        // nodes were created for the global tool
        assert_eq!(state.global_tools.len(), GLOBAL_TOOL_CAP);
        assert!(state.nodes.is_empty());
        assert_eq!(
            state.global_tools.back().expect("newest").call_id,
            format!("c{}", GLOBAL_TOOL_CAP + 2)
        );
    }

    /// Tool calls aggregate per-tool counts, settling ok/err from results
    #[test]
    fn tool_counts_aggregate_calls_and_results() {
        let mut state = GraphState::default();
        // a scoped call that succeeds and a global call that fails
        state.apply(&call("main", "c1", "decomp_function", "entry"));
        state.apply(&result("main", "c1", "decomp_function", "ok"));
        state.apply(&TraversalEvent::ToolCall {
            agent: "main".to_owned(),
            call_id: "c2".to_owned(),
            tool: "list_strings".to_owned(),
            args: "{}".to_owned(),
            ts: None,
        });
        state.apply(&TraversalEvent::ToolResult {
            agent: "main".to_owned(),
            call_id: "c2".to_owned(),
            tool: "list_strings".to_owned(),
            result: "boom".to_owned(),
            success: false,
            ts: None,
        });
        // an orphan result (torn call) still settles its tool's aggregate
        state.apply(&TraversalEvent::ToolResult {
            agent: "main".to_owned(),
            call_id: "c3".to_owned(),
            tool: "list_strings".to_owned(),
            result: "late".to_owned(),
            success: true,
            ts: None,
        });
        // the scoped tool counted one call and one success
        let decomp = state.tool_counts.get("decomp_function").expect("decomp");
        assert_eq!((decomp.calls, decomp.ok, decomp.err), (1, 1, 0));
        // the global tool counted one call, one failure, and the orphan success
        let strings = state.tool_counts.get("list_strings").expect("strings");
        assert_eq!((strings.calls, strings.ok, strings.err), (1, 1, 1));
        // the scoped call also counted against its node's own stats
        let node = state.nodes.values().next().expect("node");
        assert_eq!(node.tool_counts.get("decomp_function"), Some(&1));
    }

    /// A self-call is annotated as recursion instead of drawn as a loop edge
    #[test]
    fn self_calls_annotate_recursion() {
        let mut state = GraphState::default();
        state.apply(&call("main", "c1", "callees", "0x401000"));
        state.apply(&result(
            "main",
            "c1",
            "callees",
            r#"{"function":"0x401000","callees":[
                {"name":"FUN_00401000","address":"00401000","call_site":"00401020","depth":0}
            ],"count":1}"#,
        ));
        // the self edge was skipped and the node flagged recursive
        assert!(state.edges.is_empty());
        let node = state
            .nodes
            .get(&NodeId::Addr("401000".to_owned()))
            .expect("node");
        assert!(node.recursive);
    }

    /// The same run folds identically from live events and transcript lines
    #[test]
    fn live_and_transcript_folds_are_equivalent() {
        // one run, expressed both as live events and persisted lines
        let live = vec![
            ThorPalEvent::SubAgentStarted {
                name: "ReverseEngineer(Europa)".to_owned(),
                role: "ReverseEngineer".to_owned(),
                goal: "triage".to_owned(),
                swarm: None,
                resumed: false,
            },
            ThorPalEvent::ToolCallStarted {
                agent: "ReverseEngineer(Europa)".to_owned(),
                call_id: "c1".to_owned(),
                tool: "decomp_function".to_owned(),
                args: r#"{"target":"entry"}"#.to_owned(),
            },
            ThorPalEvent::ToolCallFinished {
                agent: "ReverseEngineer(Europa)".to_owned(),
                call_id: "c1".to_owned(),
                tool: "decomp_function".to_owned(),
                result: "void entry(void) { ... }".to_owned(),
                success: true,
            },
        ];
        let persisted = vec![
            TranscriptLine {
                ts: Some(1),
                item: TranscriptItem::SubAgentStarted {
                    name: "ReverseEngineer(Europa)".to_owned(),
                    role: Some("ReverseEngineer".to_owned()),
                    goal: "triage".to_owned(),
                    swarm: None,
                    resumed: false,
                },
            },
            TranscriptLine {
                ts: Some(2),
                item: TranscriptItem::ToolCall {
                    agent: "ReverseEngineer(Europa)".to_owned(),
                    call_id: "c1".to_owned(),
                    tool: "decomp_function".to_owned(),
                    args: r#"{"target":"entry"}"#.to_owned(),
                },
            },
            TranscriptLine {
                ts: Some(3),
                item: TranscriptItem::ToolResult {
                    agent: "ReverseEngineer(Europa)".to_owned(),
                    call_id: "c1".to_owned(),
                    tool: "decomp_function".to_owned(),
                    result: "void entry(void) { ... }".to_owned(),
                    success: true,
                },
            },
        ];
        // fold both streams through the same reducer
        let mut from_live = GraphState::default();
        for event in &live {
            if let Some(event) = TraversalEvent::from_event(event) {
                from_live.apply(&event);
            }
        }
        let mut from_lines = GraphState::default();
        for line in &persisted {
            if let Some(event) = TraversalEvent::from_transcript(line) {
                from_lines.apply(&event);
            }
        }
        // the graphs are identical
        assert_eq!(from_live, from_lines);
        assert_eq!(from_live.nodes.len(), 1);
        assert!(from_live.nodes.values().next().expect("node").examined);
    }
}
