# anti_instrumentation

Part of the [telemetry review](../detectors.md#what-the-telemetry-says) of the [preseed detectors](../detectors.md).

**Status (2026-10-31):** the fixes below shipped in the fleet-wide detector overhaul (see the main page's overhaul status note). No deviations from the proposals below.

anti_instrumentation is not earning its forum mass: most of its leads are ignored and most engaged threads are disputed. The damage concentrates in one heuristic: the 0xCC "breakpoint-byte scan" arm substring-matches "0xcc" anywhere in a space-stripped CMP/TEST operand string, so stack displacements like [RSP+0xcc8] fire — that one bug produces most of the disputes, most of all leads, and most of the ignore flood, and its ControlFlowGated tag is dead weight because the ported engine has no validation pass (no crate::validate module exists; the only kind consumer, seed.rs gate_questions, also requires a HostIdentifier artifact these hits never carry). The string catalog has two precision bugs: the "idaq" needle matches the IDAQAB base64 tail of every RSA-65537 SPKI key (and of many CA certificates, always via IDAQAB), and tool-name needles fire on VERSIONINFO masquerade metadata (CompanyName=Sysinternals decoys) and Go runtime symbols (procExpandEnvironmentStringsW contains procexp). The referenced tool-name arm is the part paying rent — real process-blocklist/OllyDbg/Wireshark findings. Surgical static fixes (exact-immediate parsing, letter-boundary needle matching, a .rsrc metadata gate, per-function collapse, toolchain-independent rollup) would remove nearly all of the false mass without touching the real tool-name findings.

## Where the false positives come from
---

### 1. Cc substring displacement match

The 0xCC arm flags any CMP/TEST whose flattened, space-stripped operand text contains the substring "0xcc". That matches hex tokens that merely start with 0xcc (0xcc8, 0xccc, 0xcccccccd) and tokens inside memory-operand displacements that are not the compared value at all — so a drop-guard null check `CMP qword ptr [RSP+0xcc8],0x0` (immediate 0x0, displacement 0xcc8) is reported as an INT3 self-scan. Large Rust/C++ binaries have such displacements everywhere, and the hit's EvidenceKind::ControlFlowGated is dead weight: no validation pass exists in the ported engine (the docstring's `crate::validate` module was never ported; the only consumer of `kind` is seed.rs gate_questions:1374-1421, which also requires a HostIdentifier artifact), so nothing ever checks for an actual code-reading loop.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/anti_instrumentation.rs:165 (operand flattening at :124; per-instruction emission via detections.rs:1286-1310)`

### 2. Tool name in versioninfo decoy

TOOL_STRINGS needles ("sysinternals", "procexp", ...) match passive VERSIONINFO resource metadata of crypters masquerading as the tool (CompanyName=Sysinternals, FileDescription=Sysinternals PsFile in .rsrc). The string is decoy data with zero code references and the stub has no process/window enumeration, yet the fixed reasoning template asserts the sample "searches running processes or windows for these and refuses to detonate". The detector even prints "(string not referenced by code in this analysis)" on these leads and still makes the scanning claim.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/anti_instrumentation.rs:42-55,105-117 (unreferenced-string branch: detections.rs:1415-1436)`

### 3. Idaq needle matches rsa base64

The needle "idaq" is four base64-alphabet letters, and "IDAQAB" is the universal base64 rendering of the RSA public exponent 65537 — every bare RSA SPKI public key ends with it (a fresh openssl RSA-2048 SPKI PEM ends "...IDAQAB"), and many real certificates contain it contiguously, depending on the base64 phase alignment of the embedded SPKI; no other TOOL_STRINGS needle matches a CA certificate. Case-insensitive substring matching (hay.contains at detections.rs:1402) therefore flags embedded PEM public keys and certificates as "contains the name of a debugging/analysis/monitoring tool", producing both disputed and ignored referenced-certificate leads.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/anti_instrumentation.rs:48 (match at detections.rs:1402)`

### 4. Template overclaims usage

The single fixed reasoning string asserts one specific story (process/window scanning that gates detonation) for every TOOL_STRINGS match. When the real usage differs — an "OLLYDBG" string sitting next to the OllyDbg %s%s format-string crash exploit is an active anti-debugger attack rather than a detection check — the agent grades the lead disputed even though tool-aware anti-analysis behavior genuinely exists at the site. The lead's substance is right; the claimed mechanism is wrong.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/anti_instrumentation.rs:112-113`

## Why leads get ignored
---

- **Per instruction cc flood** — scan_instructions emits one hit per matching instruction with no per-function collapse (unlike api_call_hits and referenced_literal_hit, which group by function), and the 0xCC substring match multiplies in large binaries. Most ignores are 0xCC leads, both finding-shaped comments and rollup lines, and a single session can carry hundreds of them; a forum carrying hundreds of identical untrusted one-liners trains agents to skip the whole detector class.
- **Untrusted grade triage** — Most ignored leads carry confidence "untrusted" / suspicion 15 printed in the lead body (the 0xCC arm and all unreferenced strings are hardcoded to low() = (Untrusted, 15)). Agents triage threads by the printed grade and skip untrusted leads wholesale, so the detector's own labeling correctly tells agents these leads are not worth a comment, making the finding-shaped presentation pure overhead.
- **Benign value visible in lead** — For the string arms the lead body quotes the matched value, so when it is visibly a PEM certificate, a Go runtime symbol, or a Sysinternals registry path, agents can see it is benign without spending a disputing comment — the leads die silently instead of being disputed. This is the referenced-string ignore mass plus most of the unreferenced ones.
- **Rollup suppressed by design** — Many ignored leads are rollup lines inside the single runtime-noise post — zero engagement is that shape's intended presentation, and it works. The problem is the inverse: suppression only triggers when a toolchain was fingerprinted (partition_suppressed returns everything unsuppressed when toolchain is None), so un-fingerprinted binaries dump the same untrusted 0xCC noise as per-function finding comments instead.

## What would reduce the noise
---

Static-side changes, ordered as proposed; each was checked against the true positives so the fix does not suppress what the detector gets right.

### Parse the 0xCC compare: exact immediate + byte-width operand

In the 0xCC arm of AntiInstrumentation::run (anti_instrumentation.rs:165), stop substring-testing the whole operand string. Split `ops` on ',', require one operand to be an immediate that parse_imm()s (detections.rs:1317) to exactly 0xCC, and require the other operand to be byte-wide — note `ops` has spaces stripped at :124, so test for "byteptr" (or match on the unflattened operand string) or an 8-bit register al/bl/cl/dl/ah..dh/sil/dil/spl/bpl/r8b-r15b. Optionally raise surviving matches from low() to medium() so genuine INT3 scans stay out of rollup suppression.

*Effort:* small \
*True-positive risk:* Genuine INT3 scans survive — a real INT3 scan is by nature a byte compare against immediate 0xCC (a non-canonical genuine scan, e.g. a dword compare of packed 0xCCCCCCCC, would be lost). Displacement-byte matches like `CMP qword ptr [RSP+0xcc8],0x0` are correctly suppressed.

Addresses: cc substring displacement match, per instruction cc flood, untrusted grade triage.

### Collapse 0xCC hits to one per function with a sites list

In run(), have the scan_instructions closure accumulate matching 0xCC sites into a captured `BTreeMap<u64, Vec<u64>>` keyed by c.func_entry (returning None), then after the scan emit one DetectionHit per function anchored at the first site with .sites(all) — the same collapse api_call_hits (detections.rs:1117-1146) and referenced_literal_hit (detections.rs:1497-1525) already perform. Apply the same to ICEBP/trap-flag arms for consistency.

*Effort:* small \
*True-positive risk:* None: no true positive depends on duplicate per-site leads, and the full site list rides along on the collapsed hit (the seeder already renders 'Sites (n):').

Addresses: per instruction cc flood.

### Letter-boundary needle matching + key-material veto for TOOL_STRINGS

For the TOOL_STRINGS group, replace bare hay.contains(needle) (detections.rs:1402) with a bounded matcher (new helper or a flag on string_artifact_hits): veto a match whose adjacent character on either side is an ASCII letter, and skip haystacks that are key material (contain "-----BEGIN " or are >=100 chars of >=95% base64 alphabet). This kills "idaqAB" in every RSA key/cert (the 'q' is always followed by 'a') and "procEXPand..." in Go symbol tables while keeping "procexp64.exe" (digit boundary) and "idaq.exe" (dot boundary).

*Effort:* small \
*True-positive risk:* Real tool-name strings such as "procexp" (exact string, bounded), "OLLYDBG" (bounded), "WIRESHARK" (bounded), and "Sysinternals PsFile"-style values (space boundary) all still match; real tool names do not sit inside base64 blobs.

Addresses: idaq needle matches rsa base64, benign value visible in lead.

### Resource-metadata gate: demote unreferenced tool names in .rsrc to a masquerade note

In the TOOL_STRINGS path, for matches with no code xrefs resolve the string's section via ctx.sections() (context.rs:538) + section_at (context.rs:56); when it lands in a resource section (.rsrc / non-executable resource block), stop asserting T1622 scanning — either drop the hit or emit it with reasoning naming what it is ("version-info metadata naming `<tool>`; consistent with T1036 masquerading as the tool, not necessarily tool scanning") at low severity. The Sysinternals-decoy dispute class disappears while the masquerade IOC is surfaced instead.

*Effort:* medium \
*True-positive risk:* An unreferenced tool-name string in a data section rather than .rsrc (for example an unreferenced string in a 0x400000-based PE's data) survives; a real blocklist stored in .rsrc is demoted/reworded rather than silently lost, so the worst case is a weaker but still-present lead.

Addresses: tool name in versioninfo decoy.

### Reword the tool-name reasoning to stop asserting one usage story

Change the fixed TOOL_STRINGS reasoning (anti_instrumentation.rs:112-113) from the unconditional "malware searches running processes or windows for these and refuses to detonate" to a claim that matches what was actually observed: the string names an analysis tool, which malware uses to scan for analyst tooling, to masquerade as the tool, or to attack the tool itself — verify which applies at the referencing site. Keeps agents from grading a correct lead disputed because the template's story was wrong.

*Effort:* small \
*True-positive risk:* Text-only; tool-name leads (procexp, OLLYDBG, WIRESHARK) keep their technique, severity, and sites unchanged.

Addresses: template overclaims usage, tool name in versioninfo decoy.

### Seeder: allow rollup of all-untrusted instruction-tell buckets without a toolchain fingerprint

In seed.rs partition_suppressed (seed.rs:452-497), relax the toolchain.is_some() precondition for buckets whose every hit is instruction-scan noise expressible in DetectionHit fields: confidence == Confidence::Untrusted && artifact.is_none() && kind == EvidenceKind::ControlFlowGated, optionally scoped to the instruction-scan detectors via the hit.detection producer stamp added by 5bfd2ab5 (detections.rs stamp_detection:858). Route those buckets to the suppressed rollup and give seed_suppressed_rollup (seed.rs:511) a no-toolchain title variant. Do not phrase the predicate in CallContext terms: the enum (provenance_types.rs:13-24) has only Runtime/SeedsRng/PacesLoop/GatesBranch variants (no Native), and these leads carry no call context.

*Effort:* medium \
*True-positive risk:* partition_suppressed never suppresses file-level buckets (function.is_some() required at seed.rs:489), so file-level tool-name leads are structurally safe, and the ControlFlowGated+Untrusted+no-artifact criterion cannot touch the Unsure referenced-string leads. Rollup presentation retains engagement when warranted (rollup 0xCC and IsDebuggerPresent/NtGlobalFlag lines do get engaged); suppressed hits stay itemized in the rollup body up to the 40-line cap (MAX_ROLLUP_LINES, seed.rs:500) with an explicit remainder count. Defense in depth behind fixes 1-2.

Addresses: per instruction cc flood, untrusted grade triage, rollup suppressed by design.

## Verification notes
---

- Verified every code anchor by reading the files: the 0xCC substring arm is exactly anti_instrumentation.rs:165, TOOL_STRINGS at :42-55 with 'idaq' at :48, the reasoning template at :112-113, hay.contains at detections.rs:1402, the unreferenced-string branch at detections.rs:1415-1436 with hardcoded low() at :1432, scan_instructions per-instruction emission at detections.rs:1286-1310, api_call_hits per-function grouping at detections.rs:1117-1146, partition_suppressed's toolchain gate at seed.rs:457-459. Operand space-stripping is anti_instrumentation.rs:124.
- The idaq/IDAQAB claim holds against the system CA bundle (/etc/ssl/certs/ca-certificates.crt): every certificate matching 'idaq' case-insensitively does so via the IDAQAB exponent tail, no other TOOL_STRINGS needle matches any cert, and a freshly generated openssl RSA-2048 SPKI PEM contains the trailing IDAQAB.
- Verified the 'no validation pass' claim: the thorpal crate has no validate module (the EvidenceKind docstring's crate::validate was never ported), DetectionHit::strengthen has zero callers outside detections.rs, and the only functional consumer of hit.kind is seed.rs gate_questions:1374-1421, which requires ControlFlowGated AND a HostIdentifier artifact — 0xCC hits carry no artifact, so ControlFlowGated is dead weight.
- Checked already_fixed via git log: anti_instrumentation.rs last changed in 9e4a6f1f (2026-10-26), so every mechanism is 'active'. 5bfd2ab5 (2026-10-30) touched detections.rs/seed.rs but its diff only adds run_all_with_roster/stamp_detection producer stamping and telemetry fields — no change to contains-matching, the 0xCC arm, or partition_suppressed's toolchain precondition.
- Fix 6's predicate is stated in actual DetectionHit fields (kind == ControlFlowGated && artifact.is_none() && confidence == Untrusted, optionally scoped by the hit.detection stamp 5bfd2ab5 added) because an 'Untrusted AND Native instruction-scan evidence' predicate is not implementable — CallContext (provenance_types.rs:13-24) has no Native variant. Its structural protections are verified in code (file-level buckets never suppressed at seed.rs:489; the MAX_ROLLUP_LINES=40 cap bounds itemization).
- Fix 1 implementation trap: the detector strips spaces from operands at :124, so a 'byte ptr' width test must match 'byteptr' post-flattening (or run on the unflattened operand string); parse_imm exists at detections.rs:1317, and exact-immediate parsing rejects `CMP qword ptr [RSP+0xcc8],0x0` while keeping canonical `cmp byte ptr [reg], 0xCC` scans.
- Verified severity mapping: low()=(Untrusted,15) at detections.rs:230 (0xCC arm, unreferenced strings) and medium()=(Unsure,40) at :223 (referenced strings), and string_artifact_hits ignores the passed severity for unreferenced strings.
- Completeness check: the shared GetThreadContext/NtQuerySystemInformation/OutputDebugString disputes are emitted by debugger_evasion.rs (grep-verified), so excluding them from this page is correct.
- Driver 4's description of partition_suppressed matches seed.rs:457-459 in behavior.
