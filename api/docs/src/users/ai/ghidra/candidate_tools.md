# Candidate Tools

The `ghidra-cli` bridge behind ThorPal supports far more than the
[integrated tools](./integrated_tools.md): a full type editor, binary patching, program
diffing, export, and scripting. This page examines that surface and weighs each capability
as a candidate harness tool. The **Add** verdicts below (triage/listing, search, and the
annotation group) have since been implemented and are documented with the integrated tools;
their analyses are kept here as the rationale for the additions.

## Weighing a New Tool
---

Tools are not free. Every tool enabled for an agent is described to the model — name,
description, and argument schema — on **every request that agent makes**, for the whole run,
whether or not it is ever called. Each call then adds its result to the conversation, where
it stays for every subsequent turn. A large toolset also dilutes the model's attention,
making it likelier to pick a poor tool for the job. So each candidate is judged on:

- **Value density** — how much analysis signal a typical call returns per token of output
- **Overlap** — whether an existing tool already answers the same question acceptably
- **Output size** — whether a call can return unboundedly large results without limit or
  filter arguments to contain it
- **Risk** — whether the tool mutates the project or the binary, or executes code

Verdicts at a glance:

| Verdict | Candidates |
|---------|------------|
| **Add** *(implemented)* | `list_strings`, `list_imports`, `list_exports`, `string_refs`, `get_function`, `search_memory`, `find_function` |
| **Add, write-flagged** *(implemented)* | `rename_function`, `rename_symbol`, `set_comment`, `set_signature`, `set_return_type` |
| **Maybe** | `list_comments` *(implemented with the annotation tools)*, type editing, `summary`/`stats`, `diff_functions`, `disassemble_at` |
| **Skip** | memory writes and patching, program/bridge lifecycle, scripting, whole-program call graph, equates, reference and entry-point editing |

## Add: Triage and Listing
---

These are the highest value-per-token gaps in the current harness. They are read-only, their
outputs are compact or boundable, and together they cover the canonical first steps of any
reverse engineering session.

**`list_strings`** — lists the strings Ghidra found in the binary. Strings are the single
richest triage signal in most binaries (URLs, file paths, mutexes, format strings, ransom
notes), and today the agents simply cannot see them: the static detectors use the string
listing internally during the pre-seed pass, but the agents cannot pull strings themselves
when a question arises mid-run. The one caveat is output size — a large binary can hold tens
of thousands of strings — so the tool should take limit/filter arguments (ghidra-cli already
supports both) rather than dumping everything. **Add; the most valuable missing tool.**

**`list_imports` / `list_exports`** — list the binary's imported and exported symbols.
Imports are a near-free capability map: `VirtualAlloc` + `CreateRemoteThread` or
`socket`/`connect` tell an agent where to look before a single decompile is spent. Output is
small and the information is not reachable through any current tool. **Add.**

**`string_refs`** — finds the code locations that reference a given string. Today an agent
that spots an interesting string (in `read` output or detector evidence) has to guess its
address before `xrefs_to` helps; this answers "who uses this string?" in one targeted,
compact call. **Add.**

**`get_function`** — returns the details of one function (address, size, signature) without
listing all of them. `list_funcs` returns the entire function table even when the agent wants
one entry, which is pure token waste on big binaries. **Add; it will often substitute for
`list_funcs` calls and reduce net token spend.**

## Add: Search
---

**`search_memory` (byte/pattern search)** — searches program memory for a byte or string
pattern. This is how an analyst hunts for crypto constants (AES S-boxes, SHA initial
values), magic bytes, or an embedded second-stage PE — questions the current tools can only
answer by reading memory block by block, which is dramatically more expensive. **Add.**

**`find_function`** — finds functions by glob-style name pattern (`*crypt*`). Avoids
re-pulling the whole function list every time the agent wants "all the functions whose name
mentions ssl". Cheap, bounded, and directly replaces repeated `list_funcs` calls. **Add.**

A note on `find string`: it overlaps almost entirely with `list_strings` once that tool has a
filter argument, so one of the two suffices — prefer `list_strings` with filtering.

## Add as Write-Flagged Tools: Annotation
---

These are the first *write-side* tools worth having, and workflows need an explicit way to
opt out of mutation. Rather than a separate tool group, they shipped inside the `ghidra`
group with a `mutating` flag on each tool, and workflow agents gained a `read_only` field
(default false) that strips every mutating tool from an agent's resolved toolset — so a
cautious workflow keeps the whole read-side group with one line:

**`rename_function` / `rename_symbol`** — rename a function or symbol in the project. Beyond
preserving the agents' understanding for a human who opens the project in Ghidra afterward,
renames have a compounding token *benefit*: once `FUN_00401a2c` becomes `decrypt_config`,
every subsequent decompile of every caller carries the meaningful name, so later agents stop
re-deriving (and re-asking about) the same function. **Add.**

**`set_comment`** — attach a comment at an address. The cheapest way to persist per-address
evidence ("xor key loaded here") into the artifact a human analyst will actually open. **Add.**

**`set_signature` / `set_return_type`** — fix up a function's prototype. Wrong signatures are
a leading cause of garbage decompilation; letting the agent correct one improves every later
decompile that touches it. Slightly riskier (a wrong signature makes decompilation worse),
hence the opt-in group. **Add, behind the same opt-in group.**

The risk with all of these is that a wrong annotation persists in the project across
sessions. That is the same risk a human analyst carries, the operations are reversible in
Ghidra, and the `read_only` boundary means a cautious workflow can simply not enable them.

## Maybe: Situational Value
---

**`list_comments` / `get_comment`** — read existing annotations. Low value on a fresh binary,
but valuable the moment annotation tools exist or when a session reuses a project a human has
already worked on. **Worth adding together with the annotation tools, not before** —
`list_comments` landed alongside them.

**Type editing (`create_type`, `add_field`, `apply_type`, enums, typedefs, ...)** — building
and applying struct definitions is the biggest decompilation-fidelity lever Ghidra has, and
for config-structure-heavy malware it would be transformative. But it is a *large* surface —
eight-plus tools, each with schema overhead on every request — and the model must get
offsets and sizes exactly right for it to help rather than hurt. **Hold until there is
evidence agents are blocked on unresolved structs; then add as its own opt-in group.**

**`summary` / `stats`** — a compact program overview (architecture, counts, entry points).
Cheap, but the detector pre-seed and `memory_map`/`list_funcs` already supply most of it.
**Marginal; add only if the seed pass is ever made optional-by-default.**

**`diff_functions` / `diff_programs`** — compare two functions or programs. Genuinely
valuable for variant analysis and patch diffing, but a chat session targets a single binary
today, so the tool would mostly be dead schema weight. **Revisit if multi-binary sessions
land.**

**`disassemble_at` / `clear`** — force disassembly at an arbitrary address (and undefine code
units first if needed). Niche, but it is the only way to look at code auto-analysis missed —
shellcode blobs, unpacked regions, mid-instruction tricks. **A reasonable later addition for
packed/evasive samples; skip for now.**

Separately from new tools, the existing listing tools should grow **filter/limit arguments**:
ghidra-cli's list commands already support filtering, limits, offsets, and field selection,
and `list_funcs` originally ignored all of it and returned everything. That is a token
optimization of tools we already pay for, and it was done alongside the new listing tools —
`list_funcs` now takes the same optional `limit`/`filter` the new tools do.

## Skip
---

**`write_memory`, `patch_bytes`, `patch_nop`, `patch_export`** — patching modifies the
program bytes. ThorPal is an *analysis* harness: nothing in the workflow needs a modified
binary, a hallucinated patch silently corrupts the project's view of the sample, and a
patched-binary export is an artifact we do not want produced as a tool side effect. The
schema cost buys only risk. **Skip.**

**Program and bridge lifecycle (`program open/close/save/delete/export`, bridge
start/stop/restart, `batch`)** — the harness already manages the project, program, analysis,
and bridge lifecycle itself, and `program delete` is destructive. Exposing these invites the
agent to fight its own session management. **Skip.**

**Scripting (`script run`, `script python`, `script java`)** — running GhidraScripts is
arbitrary code execution inside the analysis environment, which is a trust boundary the
harness should not hand to a model (the inline python/java variants are additionally
non-functional in ghidra-cli). **Skip.**

**Whole-program call graph (`graph calls`, `graph export`)** — the full call graph of a real
binary is enormous, and token-explosive output is the exact failure mode tool design should
avoid. The integrated `callers`/`callees` with a depth argument answer the same questions
incrementally. **Skip.**

**Equates, `ref add/delete`, entry-point editing** — manual-curation features for human
Ghidra workflows (naming constants, fixing up references the analyzer missed). Each is
narrow, rarely needed, and mutating; none has shown up as a gap in actual runs. **Skip
unless a concrete need appears.**
