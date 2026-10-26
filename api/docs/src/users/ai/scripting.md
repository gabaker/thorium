# Scripting

ThorPal's agents can write and run [Rhai](https://rhai.rs) scripts to automate
analysis work that would be unreliable by hand: decoding obfuscated data,
xor/rolling decoders, or scanning functions for complex byte and code patterns.
The scripting capability is owned by the **Developer** sub-agent in the builtin
workflow, which the main agent, the ReverseEngineer, and the Researcher can all
delegate to.

## The Developer Agent
---

The builtin workflow's `Developer` agent carries all three tool groups
(`ghidra`, `transforms`, and `scripts`) and a preamble containing a condensed
Rhai cheat-sheet plus an auto-generated list of the host functions its scripts
can call. The ReverseEngineer and Researcher list it in their `agents:` field,
so they can hand it scripted work mid-task — e.g. "decode the 64 bytes at
`0x402000` with key `0x5a`" — without going back through the main agent.

## Script Lifecycle
---

Scripts live in the session store under `~/.thorium/sessions/<NAME>/scripts/`
as `<name>.rhai` files, so they survive crashes and resumes and you can read
or reuse them after a run.

1. `create_script` saves a new script. The source is compile-checked first and
   rejected (nothing saved) on a syntax error, so every script on disk parses.
2. `run_script` executes a saved script. An optional `args` JSON value is
   exposed to the script as the `args` constant, and the script's final
   expression comes back as the tool result.
3. `edit_script` fixes a script in place by exact find/replace; the edited
   source is compile-checked and a breaking edit is rejected with the old
   version kept.
4. `read_script`, `list_scripts`, and `search_scripts` let the agent find and
   reuse earlier work instead of rewriting it.
5. `promote_script` publishes a proven session script into the persistent
   cross-session library (see below) with the docs future runs search.
6. `rhai_docs` returns the full Rhai language reference plus the live list of
   host functions the agent's scripts can call, with each function's arguments
   and return shape.

Note that compiling only catches syntax errors — Rhai resolves functions at
call time, so a call to an unknown (or not enabled) function surfaces when the
script runs, as a `Function not found` error the agent can react to.

## The Persistent Script Library
---

Session scripts die with their session, so scripts that prove themselves can be
**promoted** into a persistent library every analysis run shares — by default
`~/.thorium/scripts/`, overridable with the `ai` config's `scripts_dir` key.
Promotion is primarily the agent's own decision: the Developer's preamble
teaches the criteria (the script worked, and every binary-specific input flows
through `args` rather than being hardcoded), and the `finish` tool nudges the
main agent exactly once per run to review the session's scripts before ending.
An analyst can also promote manually from the chat TUI with
`/promote <script> <description…>`.

A promoted script is a single self-contained `.rhai` file whose leading `//!`
doc header carries its metadata:

```rhai
//! name: find-xor-stubs
//! detector: false
//! promoted-from: mwater-triage (2026-10-09)
//! description: Finds small functions that XOR a buffer with a
//!   single-byte key and returns candidate decoder stubs.
//! usage: run with args = #{ max_size: 128 };
//!   returns [#{ address, key, score }, ...]

let hits = [];
// ...
```

The `description` and `usage` fields are what future runs search: `list_scripts`
returns the library section with each script's description and detector flag,
and `search_scripts` matches library sources *and* their doc headers, so an
agent can find an applicable script by what it does. `run_script` and
`read_script` resolve names session-first with a library fallback — a session
script always shadows a same-named library script. Library scripts are edited
copy-on-write: `create_script` a session script with the same name (shadowing
the library version), iterate until it works, then `promote_script` it with
`overwrite: true` to publish. Operators can prune the library by deleting files
from the scripts directory; hand-dropped scripts without a header still list
and run, just without searchable docs.

Scripts promoted with `detector: true` run automatically at every session
start, alongside the native Rust detectors — see the
[Preseed Detectors](./ghidra/detectors.md) page for the contract and limits.

## Live Output
---

A running script's `print()` and `debug()` lines stream into the chat's log
window under the `run_script` tool call while it runs — the same live-output
treatment the Ghidra tools get — and are also returned to the agent alongside
the result, so a failed run still shows how far it got.

## Host Functions Follow the Workflow
---

Scripts are not limited to pure computation: read-side tools also register a
host function of the same name into the script engine (`read` returns a blob
of raw bytes, `decomp_function` a map with the decompiled C, `list_strings`
an array of string maps, and so on — plus `bytewise_xor` from the transforms
group). The write-side annotation tools are never exposed to scripts.

Which host functions exist is decided **at runtime from the running agent's
resolved toolset**: enable or disable a tool on an agent in your workflow yaml
(via `tools`, `disabled_tools`, or `read_only`) and its script binding appears
or disappears with it. The generated preamble section and `rhai_docs` both
reflect the live set, so the model is never told about a function it cannot
call.

## Limits
---

Every run is bounded by an engine operation cap and a wall-clock deadline
(default 300 seconds; the `timeout_secs` argument raises it to at most 600).
A script that exceeds either is terminated with a clear error rather than
hanging the run.
