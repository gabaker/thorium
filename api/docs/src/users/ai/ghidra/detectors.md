# Preseed Detectors

Before the agent loop starts, ThorPal runs a suite of static detectors (ported from the
`ghidra-flag` proof of concept) over the target binary and seeds the forum with what they find.
The agents open against a forum full of concrete points of interest — a function, the evidence
found there, and why it matters — instead of a cold Ghidra database.

The economics are the point. Every fact a detector recovers statically — a decoded string, a
packer family, the constant a sandbox check compares against — is a fact the agents do not have
to spend tool calls and tokens rediscovering. Every targeted question the seeder derives focuses
a delegation on something the evidence already justifies, instead of paying for open-ended
exploration. Seeding runs once, costs no LLM tokens at all, and the agent loop starts with the
cheap wins already banked.

Seeding is on by default and controlled by two knobs: `thorctl analyze … --disable-detectors` skips
the whole pass, and the `SessionConfig::seed_questions` flag (library callers; on by default)
controls whether the derived questions are seeded alongside the statement posts.

## How the Engine Works
---

Every detector implements one trait, `Detection`, with a `run` method that receives a shared
`AnalysisContext` and returns a list of `DetectionHit`s. The registry
(`detect/detections.rs::all()`) collects the detectors by ATT&CK tactic and the driver
(`run_all`) runs them in sequence — a detector that fails is logged and skipped, so one bad
pass never sinks the report.

The `AnalysisContext` is what keeps the pass cheap: it is built once from a handful of fast
`ghidra-cli` queries (functions, imports, defined strings, sections, program info) and lazily
memoizes the heavy material — whole-program disassembly, cross-references, resolved call sites
through import thunks, section bytes, and a reverse data-reference index — so thirteen detectors
share one round of Ghidra work.

Each `DetectionHit` carries:

- **An ATT&CK technique** (e.g. `T1497.003 Time Based Evasion`) and the evidence address plus
  its containing function.
- **An evidence kind** — how the finding should be validated: `ApiResultCheck` (the API's
  return value should gate behavior), `Timing`, `ControlFlowGated` (the result demonstrably
  steers a branch), or `ContextOnly` (strings, imports, sleeps).
- **A confidence and suspicion score** — detectors rate findings `high` (likely, 75),
  `medium` (unsure, 40), or `low` (untrusted, 15), and deliberately under-claim: a ubiquitous
  API or byte pattern alone never scores high.
- **An optional recovered artifact** — a typed, pivotable *value*: a network endpoint, a
  symmetric key, a decoded config record, a host identifier, a packer section inventory.
- **An optional provenance note** — what the surrounding code actually does with the evidence
  (seeds a PRNG, paces a loop, gates a branch), when the ported provenance slice classified it.

The seeder (`seed.rs`) groups hits by containing function and creates one statement post per
function (named `fn-<addr>`, or `file-level`), authored by `detector`, with one comment per
finding sorted strongest-first. On top of those, the strongest findings derive a small, capped
set of open **questions** — see [Question Seeding](#question-seeding) below.

## The Detectors
---

| Detector | Technique(s) | What it finds |
|----------|--------------|---------------|
| `debugger_evasion` | T1622 | Anti-debug APIs, PEB reads, `INT 0x2D`, raw ptrace syscalls |
| `anti_instrumentation` | T1622 | Frida/preload/tool-name strings, trap flag, `0xCC` self-scans |
| `sandbox_system_checks` | T1497.001 | CPUID hypervisor probes, VM artifacts, fingerprint APIs |
| `sandbox_time_evasion` | T1497.003 | RDTSC, timing/sleep APIs, long-sleep constants |
| `sandbox_user_activity` | T1497.002 | Cursor/keystroke/input-idle probing APIs |
| `self_deletion` | T1070.004 | Overwrite-then-delete strings and self-delete API chains |
| `encoded_strings` | T1027/T1140 | Decoder loops, trivial-scheme blobs, layered decodes — with recovered plaintext |
| `packing` | T1027.002 | Structural packing signals, entropy, packer family markers |
| `host_binding` | T1480.001/T1082 | Hardcoded MACs/GUIDs/serials/names the sample is keyed to |
| `native_api` | T1106 | Raw ntdll `Nt*`/`Zw*` use, direct `SYSCALL`/`INT 0x2E` (PE only) |
| `network_comms` | T1071/T1095/T1571/T1573 | Protocol API families, string IOCs, carved URLs, hardcoded ports |
| `crypto_capability` | T1486 | Crypto byte signatures, PEM/DER keys, CryptoAPI imports, static decrypt replay |
| `ransomware_behavior` | T1486 | Drive/file enumeration + rename APIs, ransom-note phrases |

The per-detector improvement notes below are tagged by what they buy:

- **(value)** — recovers a concrete value statically, directly saving agent tool calls.
- **(evidence)** — upgrades a hit from "present" to "demonstrably gates control flow", so
  delegations land on things that matter.
- **(coverage)** — closes a blind spot the agents would otherwise have to find manually.

## Defense Evasion
---

### debugger_evasion

Detects debugger checks three ways: a fifteen-entry anti-debug API catalog
(`IsDebuggerPresent`, `NtQueryInformationProcess`, `GetThreadContext`, `ptrace`, …) matched
against import call sites; an instruction scan for `INT 0x2D`, manual PEB loads via
`FS:[0x30]`/`GS:[0x60]` with the touched field classified by offset (`BeingDebugged`,
`NtGlobalFlag`, heap flags), and raw `SYSCALL` with `eax` recovered as the Linux `ptrace`
number for stripped static ELFs; plus `/proc/self/status` + `TracerPid` strings.

**Improvements**

- **(coverage)** `NtQuerySystemInformation(SystemKernelDebuggerInformation)`, the
  CloseHandle-invalid-handle exception trick, `SetUnhandledExceptionFilter` abuse, and
  `OutputDebugString` probing are common checks the API catalog misses today.
- **(coverage)** Linux: `prctl(PR_SET_DUMPABLE)` and `/proc/self/stat` parsing alongside the
  existing `TracerPid` string check.
- **(evidence)** Promote hits whose API result demonstrably feeds a branch to
  `ControlFlowGated`, reusing the decompiler-backed comparison walk `host_binding` already has
  — an anti-debug call whose result gates execution is a far stronger lead than one that is
  merely present.

### anti_instrumentation

Flags tooling-awareness: string catalogs for Frida (`frida`, `gum-js-loop`), preload hijack
checks (`LD_PRELOAD`, `/proc/self/maps`), and analysis-tool names (OllyDbg, x64dbg, IDA,
Wireshark, …); instruction tells for `ICEBP`, EFLAGS trap-flag tests near `PUSHF`/`POPF`, and
`0xCC` breakpoint-byte self-scans.

**Improvements**

- **(coverage)** Code self-checksumming (a CRC/hash loop whose input range covers the text
  section) — the standard integrity-check idiom that defeats patching.
- **(coverage)** Inline-hook detection idioms: reading the first bytes of API prologues and
  comparing against `0xE9`/`0xCC`, plus fresh-`ntdll` remapping strings
  (`\KnownDlls\ntdll.dll`) that signal unhooking.
- **(coverage)** EDR-blinding tells: `EtwEventWrite` and `AmsiScanBuffer` references with
  write-xrefs into their pages.

### sandbox_system_checks

Finds sandbox/VM fingerprinting: CPUID hypervisor probes (vendor leaves, or leaf 1 only when
the hypervisor bit is actually tested afterwards — the leaf is recovered from the register
state); VM artifact strings, registry keys, hypervisor MAC OUI prefixes, and Linux DMI paths;
and a fingerprinting API catalog (`GetSystemInfo`, `GlobalMemoryStatusEx`, `GetAdaptersInfo`,
Toolhelp, `FindWindow`, …).

**Improvements**

- **(value)** Recover the compared constant in resource-threshold gates — "fewer than 2 cores",
  "less than 4 GiB of RAM", "disk smaller than 60 GB" — the way the long-sleep heuristic
  already recovers sleep durations. Handing the agent the actual threshold turns "calls
  `GetSystemInfo`" into an actionable sandbox-escape note.
- **(coverage)** Uptime checks (`GetTickCount` compared against a small constant), monitor
  count via `GetSystemMetrics`, and recent-document counts — all common Cuckoo-era heuristics.
- **(coverage)** Correlate Toolhelp enumeration with an adjacent pool of AV/EDR/VM-tool process
  names so a process-scan loop is reported as one strong finding rather than one API hit.

### sandbox_time_evasion

Finds timing-based evasion: `RDTSC`/`RDTSCP` opcodes; Linux time syscalls by number; timing and
sleep API catalogs; and a long-sleep heuristic that recovers the millisecond constant fed to
`Sleep`/`SleepEx` and flags waits of a minute or more.

**Improvements**

- **(value)** Paired-`RDTSC` detection: two reads with a subtraction and a compare, recovering
  the threshold — the canonical single-step detector, and the recovered delta tells the agent
  exactly what the sample times.
- **(value)** Extend the sleep-constant recovery beyond `Sleep`/`SleepEx` to `NtDelayExecution`
  and large `WaitForSingleObject` timeouts.
- **(coverage)** Busy-wait stalling (large-iteration empty loops, repeated cheap computation)
  that burns sandbox time without any sleep API at all.

### sandbox_user_activity

A pure API-catalog detector for human-presence probing: `GetCursorPos`, `GetAsyncKeyState`,
`GetLastInputInfo`, `SetWindowsHookEx`, `GetForegroundWindow`, and friends.

**Improvements**

- **(evidence)** Detect the comparison idiom — two cursor reads compared for equality in a loop
  — and promote it to `ControlFlowGated`; the bare APIs are dual-use and noisy.
- **(coverage)** Distinguish keylogging *intent* from sandbox checking: a keyboard hook or
  key-state poll adjacent to file/network writes is Collection, not evasion, and belongs in the
  proposed collection detector below.

### self_deletion

Finds anti-forensic self-removal: overwrite-then-delete strings (`shred -`,
`dd if=/dev/urandom`), shell delete fragments (`del /f`, `rm -f`), and the self-delete API set
(`MoveFileEx` delay-until-reboot, `DeleteFile`, `GetModuleFileName`, `ShellExecute`, …).

**Improvements**

- **(coverage)** The modern delete-on-close idiom: `SetFileInformationByHandle` with
  `FILE_DISPOSITION_INFO`, including the alternate-data-stream rename variant that deletes a
  running executable.
- **(coverage)** Batch-script shapes that pair a `ping`/`timeout` stall with a `del` of the
  sample's own path.

### encoded_strings

The deepest defense-evasion pass: it does not just flag obfuscation, it statically recovers the
plaintext and the key. Three passes: decoder-loop recovery (a function with byte-wise
`XOR`/`SUB`/`ADD` transforms in a loop has its key lifted from the immediate and applied to the
data it references), a bounded brute-force of trivial schemes over data sections, and a chained
decode that carves base64-style runs (custom alphabets included), runs a layered-decode search,
and parses recovered blobs into classified configuration records.

**Improvements**

- **(value)** Pure stack-string assembly — sequences of byte-immediate stores building a string
  with *no* transform loop. The transform-loop idiom is covered; plain assembly, which modern
  compilers emit for short literals and malware authors use deliberately, is not.
- **(value)** Wide-character (UTF-16) variants of the XOR/SUB/ADD schemes, and rolling
  multi-byte keys recovered from the loop body rather than a single immediate.
- **(coverage)** Compressed blobs: `RtlDecompressBuffer`/LZNT1 imports and aPLib magic
  adjacent to high-entropy data are a strong "packed config" tell the decode chain could feed.
- **(value)** Spend the brute-force caps smarter: order candidate buffers by reference count so
  the bounded pass works the data the code actually touches first.

### packing

Scores seven structural signals: an unpack-target section (mapped far larger than its disk
size next to a high-entropy source), writable+executable sections, section entropy, a thin
import table dominated by resolver APIs, an entry point in a writable non-first section,
unconventional section names, and whole-file/overlay entropy. Scoring is conservative — one
signal is `low`, several are `medium` — and a packer *family* is only asserted on known
section-name/stub markers, which promote to `high`. Emits a `Packer` artifact carrying the
section inventory.

**Improvements**

- **(coverage)** High-entropy **resource-section payloads** — the single most common packer
  idiom not currently signaled: a benign-looking stub whose real payload sits in `.rsrc`.
- **(coverage)** TLS-callback presence (pre-entry execution), overlay structure parsing
  (SFX stubs, appended archives), and .NET detection so managed samples get routed to the
  right analysis path instead of scored as "thin imports".
- **(value)** Import-hash lookup against a small known-packer database, turning "probably
  packed" into "UPX 4.x" without any agent work.
- **(coverage)** Mach-O support in the shared header parser (`detect/fileinfo.rs` is PE/ELF
  only), which currently blinds this detector — and the file-level passes generally — to macOS
  samples.

### host_binding

Finds machine-identity values the sample is keyed to (environmental keying guardrails) from
four angles: shaped literals (MAC/GUID/volume-serial renderings), name candidates gated on the
function calling a name-enumeration API, binary identifiers scanned only in buffers that
enumerating functions reference (with aggressive false-positive rejection), and literals carved
from pooled data blocks. Scoring is enumerator-gated, and a decompiler-observed comparison that
feeds a branch promotes the hit to `ControlFlowGated` — the strongest evidence the engine
produces.

**Improvements**

- **(value)** **Locale and geography keying** — `GetKeyboardLayout`,
  `GetUserDefaultUILanguage`, `GetSystemDefaultLangID` with the compared LANGID constant
  recovered. The "skip CIS keyboards" gate is among the most common execution guardrails in
  crimeware and nothing in the engine covers it today; the recovered language list is also
  attribution-relevant.
- **(coverage)** Domain-membership checks (`NetGetJoinInformation`, `USERDNSDOMAIN`) and
  timezone checks (`GetTimeZoneInformation` plus a compared bias) — the "corporate victim
  only" and regional variants of the same guardrail.

## Execution
---

### native_api

Flags programs that drop below the Win32 layer: a twenty-entry ntdll catalog (`Nt*`/`Zw*`/
`Ldr*`) with the injection primitives (`NtWriteVirtualMemory`, `NtMapViewOfSection`,
`NtCreateThreadEx`, `NtQueueApcThread`) rated high, plus an instruction scan for direct
`SYSCALL`/`INT 0x2E` in PE files, where no legitimate compiler emits them.

**Improvements**

- **(value)** Direct-syscall stubs with the syscall *number* recovered and mapped back to its
  `Nt*` name — the register-recovery helper already exists (it resolves `eax` for the Linux
  `ptrace` case), and naming the syscall turns "manual syscall present" into "manually invokes
  `NtAllocateVirtualMemory`".
- **(coverage)** Heaven's-gate detection (far jump/call to selector `0x33` in a 32-bit image)
  and the manual PEB→`Ldr` export-walk idiom, which also feeds the proposed API-hashing
  detector.

## Command and Control
---

### network_comms

One detection, four angles: per-protocol API catalogs (HTTP, DNS, raw sockets, QUIC, TLS, with
`ws2_32` ordinal imports resolved to names); runtime symbols for static Go/Rust binaries;
string IOCs — URLs parsed into scheme/host/port/path, endpoints, `.onion` addresses, IP
literals, and HTTP "request fabric" (user agents, header templates, URI paths) — plus raw-byte
URL carving from data sections; and hardcoded ports recovered from `htons()` arguments or
compiler-folded `sin_port` stores, with non-standard ports flagged separately. A sub-pass
groups recovered IOCs into pools by data adjacency or shared referencing function and reports
the selection idiom (PRNG, scaled-index load, loop) seen in that function.

**Improvements**

- **(value)** Beacon-cadence recovery: a recovered sleep constant adjacent to a network loop is
  the sample's beacon interval — a value analysts and detection engineers want and the agent
  would need several tool calls to derive.
- **(coverage)** Tunneling channels: `IcmpSendEcho`, raw-socket protocol constants, and DNS
  tunneling shapes (long/high-entropy subdomain construction feeding `DnsQuery`).
- **(coverage)** A DGA idiom built on the existing pool/idiom machinery: a PRNG call feeding a
  character-append loop that terminates in a resolver call.
- **(value)** Correlate pinned certificates: when `crypto_capability` recovers a DER
  certificate and this detector sees a TLS stack, one combined finding ("TLS with an embedded
  pinned cert") is worth more than the two halves.

## Impact
---

### crypto_capability

Findcrypt-style byte signatures in three strength tiers (ChaCha/AES tables strong; SHA/MD5
constants medium; CRC tables and base64 alphabets weak); PEM boundary detection with
key-consumer attribution; raw base64-armor carving and decode; DER-encoded key/certificate
recovery re-armored to PEM; a CryptoAPI/CNG import catalog; and a static decrypt replay that
collects embedded key and ciphertext candidates and actually runs RC4/RC6/XOR over them,
emitting recovered plaintext.

**Improvements**

- **(coverage)** An **AES-NI/SHA-NI instruction scan** (`AESENC`, `AESKEYGENASSIST`,
  `SHA256RNDS2`, `PCLMULQDQ`) — cheap, signature-free, and immune to the table-elimination
  that defeats byte signatures when hardware crypto is used.
- **(coverage)** Missing table/constant families: Salsa20's `expand 16-byte k`, Blowfish
  P-array, Twofish/Serpent/Camellia/SM4 tables, and Curve25519/Ed25519 constants (modern
  ransomware's asymmetric half).
- **(value)** Cross-detector key sharing: feed the loop keys `encoded_strings` recovers into
  the decrypt replay's key candidates, so a key found in one pass unlocks ciphertext found in
  the other.

### ransomware_behavior

A deliberately weak clustering detector: drive/file enumeration and rename APIs
(`GetLogicalDrives`, `FindFirstFile`, `MoveFile`, `SetFileAttributes`, …) and ransom-note
phrase strings. It exists to corroborate `crypto_capability`, not to stand alone.

**Improvements**

- **(coverage)** Recovery-inhibition commands in *defined* strings — `vssadmin delete
  shadows`, `bcdedit`, `wbadmin`. The decode engine already classifies these when they surface
  in decoded blobs, but a sample carrying them in the clear is never flagged.
- **(coverage)** Target-extension pools (a data-adjacent run of many short `.ext` strings),
  ransom-note filename shapes (`README`, `HOW_TO_DECRYPT`, `RESTORE_FILES`), and wallpaper
  replacement via `SystemParametersInfo`.
- **(evidence)** A cross-detector correlation rule: enumeration here plus strong crypto
  signatures in `crypto_capability` should raise one combined, higher-confidence finding —
  today the halves stay separate and the question seeder has to join them.

## Gaps — Proposed New Detectors
---

Ordered by expected payoff against the goal: recover statically what the agents would
otherwise spend tokens deriving.

1. **API hashing / dynamic resolution** (T1027.007) — a catalog of known hash constants
   (ROR13, CRC32 polynomial, FNV offset/prime, djb2's 5381) plus the PEB-walk and
   `GetProcAddress`-loop idioms. With precomputed hash tables for the common export names,
   hashed imports can be *resolved statically* — turning an opaque import-less binary into one
   with named capabilities before the agent's first tool call. The single biggest token saver
   on this list, and `native_api`'s PEB-walk work feeds it.
2. **Process injection, Win32 layer** (T1055) — `VirtualAllocEx` + `WriteProcessMemory` +
   `CreateRemoteThread`/`QueueUserAPC`/`SetThreadContext`, scored on the *chain* co-occurring
   in one function. `native_api` covers only the ntdll spellings; classic Win32 injection is
   currently invisible.
3. **Persistence** (TA0003) — Run-key and service registry paths, `CreateService`/
   `StartServiceCtrlDispatcher`, scheduled-task strings (`schtasks`, `ITaskScheduler`),
   startup-folder paths; cron/systemd/launchd strings for Linux and macOS. Pure catalogs plus
   string-shape work — cheap and high-signal.
4. **Credential access** (TA0006) — DPAPI (`CryptUnprotectData`), `CredEnumerate`, and
   browser/credential-store path strings (`Login Data`, `cookies.sqlite`, keychain paths).
   The path strings double as recovered artifacts.
5. **Collection** (T1056/T1113) — keylogging intent (hook or key-state poll adjacent to a
   file/network write), screenshots (`BitBlt`/`GetDIBits`), clipboard reads, with any staging
   path recovered. Takes the dual-use APIs `sandbox_user_activity` sees and resolves what they
   are *for*.
6. **Mutex infection markers** — `CreateMutex` with the name literal recovered. One API, one
   string — and the mutex name is a campaign-grade pivot artifact.
7. **Command execution** (T1059) — `CreateProcess`/`ShellExecute`/`system` with the command
   line recovered; `powershell -enc` payloads handed straight to the existing base64 decode
   chain.

## Question Seeding
---

Statement posts record evidence; they do not direct work. The seeder therefore also derives a
small set of open questions (`Question · Unsolved` posts, author `detector`) from the strongest
findings, which the main agent's question loop picks up and delegates exactly like questions it
raised itself. Each question cites the backing statement posts (`#fn-<addr>`, `#file-level`) so
the delegated agent jumps straight to the evidence.

Derivation is deliberately picky — a weak signal seeds no question, and the queue is capped at
six so seeding never floods the delegation loop with mandatory work (which would spend the
tokens the static pass is meant to save). The themes, strongest first:

| Question | Seeded when |
|----------|-------------|
| Is this binary packed, and what does the payload do? | Packing evidence at medium confidence or better |
| What does the host-identity check at `<addr>` gate? | A `ControlFlowGated` comparison against a recovered host identifier (up to two) |
| How is the statically decoded data used at runtime? | Any recovered plaintext or parsed config record |
| What data do the recovered keys protect? | Recovered symmetric or private key material |
| Characterize the C2 channel behind the recovered endpoints | Any recovered network endpoint |
| Confirm or refute ransomware behavior | Impact findings corroborated across two or more functions |
| Map the anti-analysis gauntlet | High-confidence anti-debug/anti-sandbox checks |

The derived gate, unpack, decoded-config, and container questions also carry concrete
**check tasks** — capped at four per question — naming the specific addresses, functions, or
values the answer must cover. Tasks gate solving: the question cannot be marked `solved`
until each is completed with `complete_task` citing an evidence comment (see
[Tasks](../forum/how_it_works.md#tasks)).

Questions are enabled by default alongside detector seeding; library callers can turn them off
with `SessionConfig::seed_questions` while keeping the statement posts.

When detector scripts (below) emit their own questions, those are seeded first — a promoted
detector asked for them explicitly — then the derived themes fill the remaining slots, with
duplicates by name dropped and the combined queue still capped at six.

## Script Detectors
---

Beyond the native Rust detections, any script in the persistent library
(`~/.thorium/scripts/`, see the [Scripting](../scripting.md) page) whose doc header carries
`detector: true` runs automatically during this same seed pass. This is how a scan that one
run's Developer agent wrote and promoted keeps paying off on every future sample — without
anyone porting it to Rust.

Each detector script runs exactly like a `run_script` call: sandboxed in the embedded Rhai
engine with **every** registered read-only host binding (the write-side annotation tools never
carry script bindings), the shared operation cap, and a 300-second wall-clock deadline. Scripts
receive `args = #{ mode: "detector" }` so one script can serve both interactive and detector
use. At most eight detector scripts run per session, sequentially, and a script that fails,
times out, or returns a value outside the contract is logged and skipped — the same per-detector
failure isolation the native driver has. Malformed individual entries are warned about and
dropped without discarding the script's other results.

A detector script's final expression must be either a bare array of hit maps, or a map carrying
separate `hits` and `questions` arrays:

```rhai
#{
  hits: [#{
    address: 0x401000,          // required; int or hex string (what the bindings return)
    content: "...",             // required: the observed characteristic
    reasoning: "...",           // required: why it matters
    confidence: "medium",       // optional: "low" | "medium" | "high" (default low)
    technique: "T1027",         // optional ATT&CK id; technique_name required with it
    technique_name: "...",
    tactic: "defense evasion",  // optional; parsed by the ATT&CK tactic name parser
    function: 0x400f00,         // optional; else resolved from the address
    sites: [0x401000, 0x401020],
  }],
  questions: [#{                // optional, at most two per script
    name: `q-xor-${addr}`,      // space-free post name
    title: `What does the stub at ${addr} decode?`,
    body: `... evidence in #fn-${addr}.`,
    tasks: [`check the decode loop at ${addr}`], // optional completion-gate checks
  }],
}
```

Hits are tagged `source: Script`, suffixed with the producing script's name in their reasoning,
and flow through the exact same bucketing, statement posts, comments, and derived-question
pipeline as native hits; a hit that names no technique falls back to the generic `SCRIPT`
technique. Questions are seeded as `Question · Unsolved` posts ahead of the derived themes
(Rhai's string interpolation is the templating — the script builds its titles and bodies from
its own findings). A question's optional `tasks` array holds short check descriptions that
become completion-gate tasks on the seeded question — at most four are kept per question — so
the question cannot be marked `solved` until each is completed with `complete_task`.

`--disable-detectors` skips script detectors along with the native ones. A note on trust: the
library directory is operator-owned state that auto-executes at every session start. The engine
is sandboxed (no filesystem, network, or process access; read-only Ghidra bindings only) and
capped, but you should treat `~/.thorium/scripts/` like any other configuration you own — review
what lands there and prune scripts you no longer want running.
