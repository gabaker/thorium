# Tools

ThorPal agents do their work by calling tools: small, well-defined operations like
"decompile this function" or "post this finding to the forum". This page lists every tool
bundled into Thorctl AI and how tools are assigned to agents.

Tools matter for cost as well as capability. Every tool an agent has enabled is described to
the model (name, description, and argument schema) on **every single request** that agent
makes, and each call's result is added to the conversation. A lean, relevant toolset is
therefore both cheaper and more focused than a kitchen-sink one, which is why tools are
grouped and opt-in per agent.

## Tool Groups
---

Tools belong to named groups, and each agent in the workflow opts into the groups (or
individual tools) it needs:

- `containers` — static container listing/extraction (ZIP, CAB, OLE/CFB) and raw-stream
  decompression; no external requirements, and nothing is ever executed
- `decode` — native byte decoding, transforming, searching, and hashing (base64/RC4/XOR
  pipelines, key brute-forcing, API-hash matching, keyword grep, range strings, digests);
  pure static computation with no external requirements
- `file` — raw access to the original sample file (or an extracted file) on disk; no
  external requirements, and works even when Ghidra cannot import the sample
- `ghidra` — the Ghidra reverse engineering tools; requires a configured Ghidra install
- `transforms` — pure data-transformation tools with no external requirements
- `scripts` — the Rhai analysis-script tools; see [Scripting](./scripting.md)
- `memory` — the cross-run memory tools over the persistent library of durable
  generic lessons; see [Memories](./memories.md)

An agent's `tools` list in the workflow names groups and/or individual tools, and
`disabled_tools` removes individual tools after the groups are expanded — for example,
enabling the whole `ghidra` group but dropping `read`. See
[Customizing the Workflow](#customizing-the-workflow) below.

## The containers Group
---

Malware routinely ships inside containers — an MSI carrying a CAB carrying the PE, a ZIP
with the "infected" password, a gzip-wrapped payload. These tools unpack that nesting
natively and statically: bytes are parsed and copied, nothing is executed.

| Tool | What it does |
|------|--------------|
| `list_container` | List the entries of a ZIP, CAB, or OLE/CFB compound file (MSI stream names are shown decoded); single-stream formats (gzip/xz) come back with a note pointing at `decompress` |
| `extract_file` | Extract one entry into the session's extracted workspace; supports ZIP passwords (the "infected" convention) and an `offset` for containers embedded mid-file |
| `extract_macros` | Statically extract Office macro content from an OLE compound file (legacy `.doc`/`.xls`/`.ppt`, or an extracted OOXML `vbaProject.bin`): every VBA module's source is MS-OVBA-decompressed into the extracted workspace, and the `Workbook`/`Book` stream is scanned for Excel 4.0 (XLM) macro sheets with their hidden state |
| `decompress` | Decompress a raw compressed window (gzip, zlib, deflate, lzma — headered or headerless via `props` — xz, lznt1) into the extracted workspace |

Extracted files land in the session's `extracted/` folder and are addressable by every
file and container tool through the `target` argument, so nested containers (a CAB inside
an MSI) list and extract all the way down. Extraction is capped at 256 MiB per entry and
1 GiB per session; a cap breach extracts nothing rather than truncating silently.

## The decode Group
---

Malware hides its strings and payloads behind a small set of standardized transforms, and
these tools replay them natively. The dividing line with [Scripting](./scripting.md) is
deliberate: **standardized algorithms (base64, RC4, byte loops, hashing, API-hash brute
forcing) run through these native tools — never hand-written in Rhai** — while Rhai
scripts handle the binary-specific glue around them (config layouts, per-sample
constants). Every tool is pure static computation with no external requirements.

| Tool | What it does |
|------|--------------|
| `decode_bytes` | Run a pipeline (up to 8 steps) of standardized decode steps: base64 (standard, URL-safe, custom or rotated alphabets), RC4, repeating-key XOR, single-byte ADD/SUB, `sub_from_key` (`(K - b) & 0xff`), and `self_key_xor` (a buffer carrying its own key up front — the LummaC2 shape) |
| `byte_transform` | Apply one byte transform (xor/add/sub/sub_from_key/rol), or brute-force its key space (0–255, 1–7 for rol), scoring each key by the readable text it recovers |
| `pairwise_sub` | Decode the pairwise-SUB stack-string scheme (`out[i] = in[2i] - in[2i+1]`), surfacing the readable runs with their source offsets |
| `api_hash_match` | Resolve API-name hashes (FNV-1a, multiply-rolling, ROR13, CRC32, DJB2, byte-sum) against a curated dictionary of well-known export/DLL/process names, trying case, UTF-16LE, NUL-terminated, `Nt`/`Zw`-twin, extension-stripped, and salted variants |
| `grep_bytes` | Search raw bytes for up to 32 keywords in ASCII and UTF-16LE at once, case-insensitive by default, with a context hexdump per match |
| `strings_range` | Extract strings from an explicit byte range — the range-bounded twin of `strings_file` for scanning exactly one region |
| `digest` | MD5/SHA-1/SHA-256 over the whole file or explicit `[offset, length]` ranges (concatenated or per-range), streamed so size never matters |

Inputs come inline (hex/base64/text `data`) or from a file window (`target`/`offset`/
`length` — 16 MiB per call for the decode/transform tools, while `grep_bytes` and
`strings_range` scan one 8 MiB window like `strings_file`); decoded output can be saved
into the extracted workspace via `name`, where every file tool can address it. Caps: 8
pipeline steps, 1 MiB brute-force input with the top 16 candidates, 32 keywords of 128
bytes with 500 matches per call, 64 digest ranges, and 256 hash targets per call.

The API-hash dictionary is **curated from well-known Windows exports, DLL names, and
process names** (roughly 2,400 entries) rather than generated from any system's DLLs, so
an unmatched hash may simply be an uncommon export — `extra_names` extends the dictionary
at call time.

## The file Group
---

Raw access to the bytes of the original sample file on disk — independent of Ghidra's
loaded view, so these work even when the import fails ("No load spec found") and see bytes
Ghidra does not map (format headers, overlays, appended data). Each takes an optional
`target` naming an extracted file instead of the original sample.

| Tool | What it does |
|------|--------------|
| `read_file` | Read raw bytes as a hexdump (default 256, max 4096 per call) |
| `read_text` | Read decoded, line-numbered text (BOM/heuristic encoding detection: UTF-8, UTF-16LE/BE, lossy fallback), paged by line with a substring filter — the text twin of `read_file` for scripts, configs, and anything Ghidra cannot import |
| `strings_file` | Extract printable-ASCII and UTF-16LE strings from an 8 MiB window, with min-length, limit, and substring filter |
| `file_info` | The file's identity: size, MD5/SHA1/SHA256, whole-file Shannon entropy, and the format identified from its byte signature (refined with text/script-language heuristics), plus a `ghidra_import` note when the format has no Ghidra loader |

## The ghidra Group
---

These drive a headless Ghidra instance against the target binary. They are documented in
detail in the [Ghidra](./ghidra/ghidra.md) section; in brief:

| Tool | What it does |
|------|--------------|
| `list_funcs` | List the functions in the binary |
| `decomp_function` | Decompile a function to C pseudocode |
| `disassemble_function` | Disassemble a function |
| `emulate_function` | Emulate a function in a budgeted, fail-closed p-code interpreter and recover the strings it builds at runtime |
| `memory_map` | Get the program's memory map |
| `read` | Read raw bytes at an address |
| `xrefs_to` | Find every reference to an address or function |
| `xrefs_from` | Find every reference a function or address makes |
| `callers` | Walk the call graph upward from a function |
| `callees` | Walk the call graph downward from a function |

Agents with these tools fail to start when no Ghidra install is configured.

## The transforms Group
---

| Tool | What it does |
|------|--------------|
| `bytewise_xor` | XOR two equal-length hex strings, commonly used to decode obfuscated data found in the binary |

`bytewise_xor` accepts spaced (`de ad be ef`) or unspaced (`deadbeef`) hex and matches the
input format in its output.

## The scripts Group
---

These let an agent write and run [Rhai](https://rhai.rs) analysis scripts — decoding data,
scanning functions for complex patterns — stored in the session's `scripts/` folder. See
[Scripting](./scripting.md) for the full story; in brief:

| Tool | What it does |
|------|--------------|
| `create_script` | Save a new script, compile-checked first (nothing saved on a syntax error) |
| `edit_script` | Fix a script by exact find/replace, compile-checked before applying |
| `read_script` | Read a saved script's source |
| `list_scripts` | List the session's saved scripts |
| `search_scripts` | Search every saved script for a case-insensitive substring |
| `run_script` | Run a script with an optional `args` JSON value, streaming its print/debug output live |
| `rhai_docs` | The Rhai language reference plus the live list of host functions scripts can call |

Scripts can call host functions mirroring the agent's enabled read-side tools (reading
bytes, decompiling, listing strings, xor, ...). The set is derived from the agent's
resolved toolset at runtime, so disabling a tool in the workflow also disables its script
binding.

## The memory Group
---

These give an agent the persistent cross-run memory library of durable GENERIC lessons
(techniques, workflow rules, verified family intel, pitfalls) saved by past analysis
runs. See [Memories](./memories.md) for the full story; in brief:

| Tool | What it does |
|------|--------------|
| `list_memories` | List every memory (builtins included) with its name, kind, and description |
| `search_memories` | Search every memory (headers included) for a case-insensitive substring |
| `read_memory` | Read a memory's full source by name |
| `save_memory` | Save a durable generic lesson into the library (collision-checked, content-guarded) |

In the builtin workflow every sub-agent reads memories, but only the Researcher holds
`save_memory` (the others disable it via `disabled_tools`), so the library stays curated.
Memory-enabled agents also get a per-spawn memory index in their task prompts and a
generated recall-discipline preamble section.

## Forum Tools
---

Every agent — the main agent and each sub-agent — always carries the forum tools. The forum
is how agents share findings with each other and with you, and it is the same forum shown in
the chat's sidebar. The [Forum](./forum/forum.md) section covers how it works in depth.

| Tool | What it does |
|------|--------------|
| `create_post` | Create a top-level post raising a single question, claim, or finding; a question may attach an optional `tasks` checklist |
| `create_comment` | Comment on a post or reply to a comment, with a stance of supports, disproves, or neutral |
| `add_tasks` | Attach completion-gate checklist tasks to an existing question (at most 20 per call) |
| `complete_task` | Complete one task, citing the evidence comment (`!123`) that satisfies it |
| `list_posts` | List all posts to browse existing discussion before posting |
| `search_forum` | BM25 relevance search over post names/titles/bodies and comment bodies; returns references, titles, and snippets to follow up with `follow_link` |
| `follow_link` | Resolve a `#post-name` or `!123` reference to the post or comment and its full reply thread |
| `set_status` | Update a question's status: unsolved, solved, needs_evidence, or blocked; solved is refused while the question has incomplete tasks |
| `list_questions` | List questions filtered by status to find what still needs answering |
| `finish` | Declare the analysis finished; main agent only, and it refuses while any question is still open |

Posts and comments are typed as statements (facts, claims, findings) or questions, and every
question carries a resolution status. The `finish` gate is what keeps a run honest: the main
agent cannot end the analysis while unsolved or untrusted questions remain on the forum.

## Analyst Interaction Tools
---

Two tools let the run reach the human analyst instead of silently guessing. They are on by
default and gated together by the workflow's `analyst_asks: false` (or a one-off
`--no-analyst-asks` on `thorctl analyze chat`), which keeps every agent's tool list and
preamble byte-identical to pre-feature runs for strict evaluation A/B comparisons.

| Tool | Who has it | What it does |
|------|------------|--------------|
| `ask_analyst` | main agent | Poses one structured multiple-choice question (2-4 labeled options plus a designated recommended default) and returns the analyst's answer |
| `flag_for_review` | every agent | Marks a finding, conclusion, or area as needing analyst review: a short title, the reason human eyes are needed, and optionally the `#post-name`/`!123` it concerns |

In the interactive chat TUI an `ask_analyst` call opens a question modal and the turn waits
for your answer — pick an option (`1`-`4`, or `↑`/`↓` and `Enter`), type your own answer
(`o`), or press `Esc` to take the recommended default. With no TUI attached — headless
`--prompt` runs, batch runs, blinded evaluations — the tool never waits: it immediately
returns the recommended default, records the question, and auto-files a `system` review
flag so the fork stays on your queue. Declining a question behaves exactly the same way,
and quitting the TUI mid-question declines everything still pending.

Review flags are non-blocking bookkeeping: they never pause the run and never gate
`finish`. Recorded asks and flags persist in the session's `forum.json`, render on the
message board (anchored flags inside their post's card, everything else in an
"Analyst asks & review flags" tail card), and land in the report appendix as
**Analyst decisions** and **Needs analyst review** — placed ahead of the finding dumps so
they survive the appendix size budget first. Caps: 10 asks and 40 flags per run.

## Orchestration Tools
---

A few more tools are always available for coordinating the run itself:

- `think` — a scratchpad the model uses to reason through a problem without taking an action
- `rename_session` — renames the session to a short, descriptive name based on the findings;
  only attached when the session started under an auto-generated name
- **Sub-agent tools** — each agent defined in the workflow is exposed to the main agent as a
  tool named after it, taking a goal and a prompt. The builtin workflow provides
  `researcher` (domain-knowledge help, like resolving AArch64 `ADRP`/`ADD` address pairs),
  `reverse_engineer` (an expert that drives the Ghidra and transforms tools), and
  `developer` (an expert that writes and runs Rhai analysis scripts; see
  [Scripting](./scripting.md)). Sub-agents can also call each other: an agent's `agents:`
  list in the workflow attaches other agents to it as the same kind of tool — in the
  builtin workflow both the researcher and the reverse engineer can call the developer
  directly. Nested invocations draw from the callee's own `max_nested_alive` pool rather
  than the global cap, so nested delegation can never deadlock a run
- `spawn_swarm` — fans several independent tasks out to sub-agents concurrently. Each agent
  caps how many copies of it one swarm may include (`max_swarm_size`), and the whole run
  caps concurrently live sub-agents (`max_alive_sub_agents`, default 8)

### Turn budgets, extensions, and death handoffs

Every sub-agent invocation runs under its agent's `max_turns` model-call budget. Once only
`warn_turns` calls remain, each request carries a countdown warning telling the agent to
post unposted findings to the forum immediately, make its final message a proper handoff
(status, findings with evidence, remaining work), and — only if genuinely justified — call
`request_more_turns` with a count and a one-sentence justification. Extensions are granted
automatically but clamped to the agent's per-task `max_extension_turns` allowance; every
request (granted or refused) is recorded with its justification in the invocation's
`agents/<name>.json` record and surfaced live in the TUI and headless output
(with its trigger: `requested`, `reserved`, `reservation_applied`, or `auto`).

Extensions are reachable from anywhere in the run: an in-window request grants
immediately, while a request made **before** the window opens is recorded as a **standing
reservation** that converts automatically (clamped to the allowance) if the agent actually
hits its cap — asking early counts instead of being refused. An agent that never asked at
all still gets **one automatic extension** of `min(warn_turns, remaining allowance)` at
the cap, buying exactly one wrap-up horizon. Once nothing can extend the run — the
allowance is spent and the automatic extension is used — the true final call has **tool
calls disabled** (`tool_choice: none`), so the reply is structurally the final handoff
rather than another doomed tool call; a tool call a model emits anyway is skipped with
corrective feedback instead of killing the run outright.

Separately from turns, each sub-agent's **context window** is tracked against the
workflow's `context_window_tokens` (default 200000; `0` disarms): at ~80% a context
warning orders an immediate forum flush in several small posts (extra turns cannot add
memory, so `request_more_turns` is never suggested there), and at ~90% the request's tools
are clamped to the forum/communication set so bulk tool output stops inflating the
conversation. Pressure levels are reported once each (`[context]` lines, durable
`context_pressure` transcript markers).

When a sub-agent still dies — it ran out of turns and allowance, hit its context window,
or a response was truncated — the run salvages instead of discarding: the partial
transcript is persisted in the agent record, the parent receives a partial handoff marked
`SUB-AGENT TERMINATED EARLY` carrying the agent's own last substantial message (or, when
it never wrote one, a one-shot post-mortem summary of the partial transcript) plus any
unmet extension request or never-applied reservation, and the same handoff is **posted to
the forum** as a first-class finding authored by the dead agent (`#salvage-<agent>`), so
the findings reach the report and re-delegations can cite them. Each death also leaves a
durable `sub_agent_terminated` transcript marker with its class (`max_turns`, `context`,
`cancelled`, or `other`) and the salvage post name. An output-truncated turn
(`finish_reason=Length`) is also retried once per invocation with corrective feedback
while a budgeted call remains to dispatch the retry — on the final call the truncated text
is finalized as-is instead, which still reaches the parent.

## Customizing the Workflow
---

The set of agents and the tools each one gets live in a workflow YAML. The builtin workflow
is used by default; replace it with `--workflow <path>` on a run, or persistently with:

```bash
$ thorctl config --ai-workflow /path/to/workflow.yaml
```

A minimal workflow granting one agent the Ghidra tools minus raw memory reads, plus a
script developer it can delegate to:

```yaml
max_alive_sub_agents: 8
agents:
  - name: ReverseEngineer
    description: An expert reverse engineer that drives Ghidra to answer questions.
    tools:
      - ghidra
      - transforms
    disabled_tools:
      - read
    max_turns: 100
    max_extension_turns: 25
    warn_turns: 5
    max_swarm_size: 6
    agents:
      - developer
    preamble: |
      You are an expert reverse engineer...
  - name: Developer
    description: An expert developer that writes and runs Rhai analysis scripts.
    tools:
      - scripts
      - transforms
    max_turns: 50
    max_nested_alive: 6
    preamble: |
      You are an expert developer...
```

Top-level fields: `max_alive_sub_agents` (default 8), `forum_context` (`linked` by
default, preloading each sub-agent's assigned forum thread and everything it references
into its starting prompt; `full` preloads nothing and tells agents to call `list_posts`
first — see [Agents and the Forum](./forum/agents.md)), `forum_link_depth` (how many
link hops linked-mode context follows beyond the assigned thread; default 2, 0 preloads
the assigned thread alone), `context_window_tokens` (the model context window the
sub-agent context warnings assume; default 200000 — the Claude-class window — raise it
for a bigger-window model or set 0 to disarm context tracking), and the MCP fields
`thorium_mcp` and `mcp_servers` (see [MCP Tools](#mcp-tools) below).

Per-agent fields: `name` (ASCII alphanumeric; its snake_case form becomes the tool name),
`description` (shown to the main agent), `tools`, `disabled_tools`, `read_only` (strips
the mutating tools after the lists resolve; default false), `max_turns` (default 10),
`max_extension_turns` (the extra turns the agent may grant itself per task through
`request_more_turns` — immediately, as a standing reservation, or as the one automatic
cap extension; default `max_turns/4` clamped to 2..=25, 0 disables the tool),
`warn_turns` (how many remaining turns arm the wrap-up warning, and the size of the
automatic cap extension; default `max_turns/5` clamped to 2..=5),
`max_swarm_size` (default 4; 0 opts the agent out of swarms), `agents` (the tool names of
other workflow agents this one may call; default none), `max_nested_alive` (the dedicated
pool for invocations started by other sub-agents; default 6), and `preamble` (the agent's
system prompt). Unknown tool or group names are rejected when the workflow loads, so a
typo fails fast instead of silently granting nothing — as are unknown `agents` references,
self-references, and delegation cycles.

Note the two concurrency pools: top-level delegations and swarms count against the global
`max_alive_sub_agents` cap, while sub-agent-to-sub-agent calls draw from the *callee's*
`max_nested_alive` pool. Keeping them separate is what makes nested delegation
deadlock-free, at the cost that the total number of live agents can briefly exceed the
global cap.

## MCP Tools
---

Beyond the compiled in tools, a workflow can source extra tools from
[MCP](https://modelcontextprotocol.io) (Model Context Protocol) servers — no recompiling
required. Servers are declared in a top-level `mcp_servers` map, and agents reference
their tools in the same `tools`/`disabled_tools` lists as everything else:

```yaml
thorium_mcp: true              # enable the built-in Thorium server (reserved name `thorium`)
mcp_servers:
  sandbox:                     # a stdio server: thorctl spawns the command and
    command: /usr/local/bin/my-mcp   # talks MCP over its stdin/stdout
    args: ["--mode", "analysis"]
    env:
      API_KEY: ${SANDBOX_KEY}  # ${VAR} is read from the environment at startup
    read_only: true            # every tool survives read_only agents
    optional: true             # a failed connect warns and skips instead of failing the run
  intel:                       # a streamable-HTTP server
    url: https://intel.example/mcp
    headers:
      Authorization: Bearer ${INTEL_TOKEN}
    timeout_secs: 120          # per-tool-call cap (default 300)

agents:
  - name: Researcher
    tools:
      - memory
      - "mcp:thorium"              # every tool the server lists
      - "mcp:sandbox:detonate"     # exactly one tool, by the name the server lists
    disabled_tools:
      - "mcp:thorium:start_tree"   # drop one tool after a whole-server enable
```

Each server sets exactly one transport: `command` (plus optional `args`/`env`) spawns a
local stdio server, or `url` (plus optional `headers`) connects to a streamable-HTTP
endpoint (HTTPS servers are verified against the system trust store; there are no
per-server TLS knobs). Server names must be lowercase snake_case without a double
underscore, and may not shadow a compiled in tool or group name. References to servers
the workflow never configures are rejected when it loads, like any other tool typo — as
is a `disabled_tools` entry for a server the agent never enables.

**The built-in Thorium server.** Thorium's API itself serves MCP tools (`get_sample`,
`get_sample_results`, `list_sample_result_file_paths`, `get_sample_result_file`,
`list_images`, `list_pipelines`, `start_tree`) at `{THORIUM_URL}/api/mcp`. Setting
`thorium_mcp: true` enables it under the reserved server name `thorium` with zero further
configuration: thorctl connects with its own configured credentials (an activated scoped
token, basic auth, or the keys' token — resolved only when an agent actually references
the server, so other runs stay fully offline), honoring the config's `invalid_certs`
setting. Defining your own `mcp_servers.thorium` entry is rejected.

**Secrets.** `${VAR}` references in `command`, `args`, `env` values, `url`, and header
values are expanded from the environment at connect time only — the workflow (and the
copy saved into every session snapshot) keeps the literal `${VAR}` text, so tokens never
land on disk. A reference to an unset variable fails the run (or skips the server when
`optional: true`). On resume the references are expanded again from the live environment.

**How the tools behave.** Referenced servers are connected once at session start and
their tool lists frozen for the run (a server adding tools mid-run is picked up on the
next session). Each tool is advertised to the model as `<server>__<tool>` — e.g.
`thorium__get_sample` — so same-named tools from different servers can never collide; the
server still receives its own tool name on every call. A `read_only: true` agent keeps an
MCP tool only when the server is declared `read_only: true` or the tool's own MCP
`readOnlyHint` annotation is true — MCP tools otherwise count as mutating. MCP tools are
only attachable to workflow sub-agents (the coordinator keeps its fixed orchestration
toolset), and they get no Rhai script bindings, so `run_script` cannot call them. Tool
calls, results, and token usage land in the transcript and usage ledger exactly like
compiled in tools.
