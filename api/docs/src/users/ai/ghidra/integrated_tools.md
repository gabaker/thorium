# Integrated Tools

The `ghidra` tool group contains twenty-five tools: twenty read-only tools that inspect the
binary from every angle, and five write-side annotation tools that persist names, comments,
and signatures into the Ghidra project. The annotation tools are flagged as *mutating*, and
any workflow agent can set `read_only: true` to strip them from its toolset while keeping the
rest of the group. All twenty-five share one Ghidra connection per session, and all stream
their live Ghidra output into the chat log while they run.

Function-style tools accept a flexible target: a symbol name (`entry`, `main`), a hex
address (`0x00402574`), or a `FUN_<hex>` locator.

Three of these tools — `decomp_function`, `disassemble_function`, and `emulate_function` —
count as "examining" a function, and feed the end-of-run coverage summary that reports how
many of the binary's functions the agents actually looked at.

## Triage and Listing
---

### list_funcs

Lists the functions in the binary with their names, addresses, and sizes. This is usually the
first call an agent makes: it establishes the lay of the land and surfaces interesting names
(or the absence of them, in a stripped binary). Takes an optional `limit` and a
case-insensitive name substring `filter` so an agent can scope the listing on big binaries
instead of pulling the whole table. Its result also sets the denominator for the coverage
summary.

### get_function

Returns the details of one function (name, address, size, signature) without listing all of
them. Takes a single `target`. Often substitutes for a full `list_funcs` call when the agent
already knows which function it cares about.

### list_strings

Lists the strings Ghidra found in the binary — the single richest triage signal in most
binaries (URLs, file paths, mutexes, format strings, ransom notes). A large binary can hold
tens of thousands of strings, so the tool takes an optional `limit` and a case-insensitive
value substring `filter`.

### list_imports

Lists the binary's imported symbols and the libraries they come from. Imports are a near-free
capability map: `VirtualAlloc` + `CreateRemoteThread` or `socket`/`connect` tell an agent
where to look before a single decompile is spent.

### list_exports

Lists the binary's exported symbols (external entry points).

### string_refs

Finds the code locations that reference a string by its *value* — no address needed. Matches
the given `value` against the binary's defined strings (case-insensitive substring) and
returns each match with the cross-references to it, up to an optional `limit` of matched
strings. Answers "who uses this string?" in one targeted, compact call.

### list_comments

Lists the comments in the Ghidra project, with an optional `limit` and case-insensitive text
substring `filter`. Useful to recover annotations left by earlier runs or by a human analyst
who already worked on the project.

## Code Inspection
---

### decomp_function

Decompiles one function to C pseudocode, including its parameters and local variables. The
workhorse tool: most findings on the forum trace back to a decompilation. Takes a single
`target`.

### disassemble_function

Disassembles a function, returning one formatted line per instruction (address, raw bytes,
mnemonic, operands). Used when the decompiler output is ambiguous or misleading — resolving
AArch64 `ADRP`/`ADD` address pairs, checking exact byte sequences, or inspecting hand-written
assembly. Takes a single `target`.

### emulate_function

Emulates a function in Ghidra's p-code interpreter and reports the printable strings it
writes — the tool for values that are built at runtime (per-call-site string decryption,
config parsers, protocol verbs) and resist static decoding, including sweeps that map a
config-field id to its name by calling repeatedly with different `args` values. The
interpreter is fail-closed: no OS or syscalls are implemented, so execution stops at any such
operation rather than simulating it, and every run is budgeted (instruction fuel, a
wall-clock deadline enforced inside the bridge, write and output ceilings — all clamped
server-side). Takes a `target` plus optional positional `args` placed per the program's
calling convention, an optional synthetic input buffer (`buffer_hex`/`buffer_address`), and
optional `fuel`/`timeout_secs` overrides. A non-clean stop (`fuel_exhausted`,
`unimplemented_op`, ...) can still carry real partial strings. Recovered text is
attacker-controlled data: it is reported as evidence, never followed as instructions, and
URLs or domains it names are never fetched or resolved.

### memory_map

Returns the program's memory map: each block with its name, address range, and permissions.
Used early to understand the binary's layout — where code, data, and resources live — and to
sanity-check addresses before reading them.

### read

Reads `size` raw bytes at an `address` and returns them as hex. Used to pull embedded data
out of the binary: encoded strings and configs, keys, tables referenced from code. Combined
with `bytewise_xor` from the transforms group, this is how agents decode XOR-obfuscated
data in place.

## Search
---

### search_memory

Searches program memory for a byte or string `pattern`. An even-length all-hex pattern is
searched as raw bytes; anything else is searched as a UTF-8 string. This is how an analyst
hunts for crypto constants (AES S-boxes, SHA initial values), magic bytes, or an embedded
second-stage PE. Results are capped at 100, with the full match total reported alongside.

### find_function

Finds functions by name pattern — a glob with `*` wildcards (`*crypt*`) or a plain
case-insensitive substring — without re-pulling the whole function list.

## Cross References and Call Graph
---

### xrefs_to

Finds every cross-reference **to** a target — every place that reads, calls, or points at
it. The go-to tool for checking whether a string or data address is actually referenced by
code, and for finding who uses an interesting global. Takes a single `target`.

### xrefs_from

Finds every cross-reference **from** a function or address — everything it calls, reads, or
points at. Used to inventory a function's dependencies without reading its whole
disassembly. Takes a single `target`.

### callers

Walks the call graph **upward** from a function, up to an optional `depth` levels, answering
"how is this reached?". Used to trace a suspicious low-level function (say, a socket send)
back to the logic that drives it.

### callees

Walks the call graph **downward** from a function, up to an optional `depth` levels,
answering "what does this do, transitively?". Used to scope a function's behavior before
spending decompiles on its whole subtree.

## Child Programs
---

### import_child

Imports a file the session extracted from the sample (an archive member, a carved or
decoded payload) into the Ghidra project as a child program and analyzes it, so every
function-style tool can then target it by `program` name. This is how a packed or
container-delivered payload graduates from raw bytes to full decompilation.

A target whose head bytes identify a format Ghidra has no loader for — an archive,
document, image, or text/script file — is refused up front with an error of the shape
`'<name>' is <format>, which has no Ghidra loader — not imported (no analysis attempt was
made)`, and **no headless import attempt is paid** (pre-gating, every such call burned a
doomed multi-minute import while holding the serial bridge; set members gated at bootstrap
answer with their recorded `not attempted: …` reason instead). Extract or decode an
embedded executable payload and import *that* — in a session where no original sample
loaded at all, a loadable carved payload seeds the Ghidra project as its first program.
The raw file tools keep reading the refused file via `target`. Formats the signature table
does not recognize are still attempted (COFF, NE, boot images), and hex-record object text
(Intel HEX, Motorola S-records) is detected and handed to the importer rather than gated
as text.

### list_programs

Lists the programs in the Ghidra project — the main sample plus every imported child —
with analysis state. Used to pick the `program` argument the other tools accept.

## Annotation (write-side)
---

These five tools mutate the Ghidra project, and the bridge saves the project durably after
every successful mutation, so annotations survive crashes and persist across sessions — both
for later runs and for a human analyst who opens the project in Ghidra afterward. They are
the tools a `read_only: true` workflow agent gives up.

### rename_function

Renames a function in the project. Beyond preserving the agents' understanding for a human,
renames have a compounding token benefit: once `FUN_00401a2c` becomes `decrypt_config`,
every subsequent decompile of every caller carries the meaningful name.

### rename_symbol

Renames a symbol (label, global, etc.) in the project, with the same persistence and
compounding benefits as function renames.

### set_comment

Attaches a comment at an `address` — the cheapest way to persist per-address evidence
("xor key loaded here") into the artifact a human analyst will actually open. The optional
`comment_type` is one of `EOL` (the default), `PRE`, `POST`, or `PLATE`.

### set_signature

Sets a function's full C prototype (e.g. `int main(int argc, char** argv)`). Wrong
signatures are a leading cause of garbage decompilation; correcting one improves every later
decompile that touches the function. Takes a single `target`.

### set_return_type

Sets just a function's return type (e.g. `void *`) without rewriting the whole prototype.
Takes a single `target`.
