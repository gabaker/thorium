# debugger_evasion

Part of the [telemetry review](../detectors.md#what-the-telemetry-says) of the [preseed detectors](../detectors.md).

**Status (2026-10-31):** the fixes below shipped in the fleet-wide detector overhaul (see the main page's overhaul status note). Deviations: NtClose was dropped from the catalog outright and OutputDebugString now requires Get/SetLastError co-location in the same function; PEB Ldr walks relabel as manual API resolution (T1106) at low, rising to medium when the list traversal is visible.

debugger_evasion is a high-volume, low-precision detector. Its false positives split into two halves: (1) legitimate library/CRT code flagged at full strength because the engine's runtime filter is purely name-based and the CRT functions were unnamed at seed time, and (2) dual-use primitives (PEB reads, Get/SetThreadContext, Nt*Information* calls) where the detector asserts the anti-debug interpretation without recovering the one discriminating fact (PEB field offset per-arch, ContextFlags, InformationClass) that agents then recover and use to refute it. Ignored leads are dominated by the un-deduplicated, uncapped per-instruction PEB scan plus unsure-grade API hits present in essentially every MSVC binary. Worst, the Likely/75 tier is exactly the tier whose T1622 hits cite into the derived q-evasion-gauntlet question (seed.rs:1611-1640; lowest-priority but seeded whenever the MAX_SEED_QUESTIONS cap allows), so top-severity false positives route delegated agent work onto false leads — the detector's confidence signal is inverted. What the true positives buy is real but modest: unsure-grade Direct PEB hits point agents at genuine crypter/anti-debug stubs, and rollup lines plus IsDebuggerPresent/ODS findings get corroborated. Anti-debug coverage is core to the product, so the detector is worth fixing rather than deleting. Argument-recovery gates, arch-aware PEB classification, per-function grouping with a cap, and a CRT fail-fast neighborhood heuristic would eliminate most of the false positives and ignored leads while keeping every true-positive pattern class seeded.

## Where the false positives come from
---

### 1. CRT library code not recognized

The engine's only library filter is name matching: apply_runtime_context resolves the containing function's Ghidra name and tests is_library_function markers ('fastfail', 'exit_or_terminate', 'acrt', ...). When the CRT is statically linked and the function has no FID-applied name at seed time (bare fn-`<addr>` roots), MSVC CRT fail-fast/report code (__raise_securityfailure, __scrt_fastfail, exit_or_terminate_process, __crtMessageBoxW, ATL CAtlBaseModule) seeds at full severity. The same name blindness zeroes the MSVC toolchain fingerprint (msvc_points counts is_library_function names), which disables partition_suppressed entirely (seed.rs:456-459 returns everything unsuppressed without a toolchain) — so the noise is neither weakened nor rolled up. Agents FID/decompile the functions and refute: the IsDebuggerPresent only gates re-raise-to-debugger; the UCRT NtGlobalFlag read tests bit 0x100 (FLG_APPLICATION_VERIFIER), not the 0x70 heap-flags anti-debug mask; IsDebuggerPresent+OutputDebugStringW in __crtMessageBoxW just echo the CRT fatal Runtime Error! text. Leads almost never carry any CallContext.

*Where:* `thorpal/src/thorpal/detect/detections.rs:1006-1039 (is_library_function), :979-991 (apply_runtime_context); thorpal/src/thorpal/detect/toolchain.rs:156-166 (msvc_points); thorpal/src/thorpal/seed.rs:452-459 (suppression requires a fingerprint)`

### 2. PEB offset misclassification

classify_peb applies an x86-only offset table to both architectures and matches offsets as bare substrings over the next 8 instructions without tracking which register holds the PEB. On x64, PEB+0x18 is Ldr (not ProcessHeap, which is +0x30), so every GS:[0x60] module-list walk — reflective loaders (such as the canonical Stephen Fewer / Metasploit ReflectiveLoader), API-hash resolvers — is labeled 'PEB ProcessHeap flags check'. On x86 the FS:[0x30]+0xC Ldr read matches nothing in the table, and the loop then latches onto a later '+0x18]' operand (LDR_DATA_TABLE_ENTRY.DllBase, or any [rsp+0x18]/[rbp+0x18] spill) that doesn't even dereference the PEB register.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/debugger_evasion.rs:247-298 (classify_peb; the ops.contains("+0x18]") branch at 279-288)`

### 3. PEB Ldr API resolution overclaim

The generic fallback for a PEB base load asserts 'malware does this to inline anti-debug checks without a hookable API', but the dominant real-world purpose of a manual PEB load in malware and runtimes alike is walking Ldr to resolve APIs (T1027.007/T1106) — for example a crypter's self-decryption stub walking InLoadOrderModuleList to find Kernel32 — or reading ProcessParameters (PEB+0x20, as in the UCRT's __acrt_get_process_end_policy). The look-ahead never checks whether BeingDebugged/NtGlobalFlag/heap-flags is actually what gets read, so agents trace the pointer and refute the anti-debug claim even when the code is interesting for other reasons.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/debugger_evasion.rs:185-195 (PEB-load match) and :290-297 (generic 'Direct PEB access' fallback text)`

### 4. Dual use Nt API no InformationClass gate

Nt/ZwQueryInformationProcess and Nt/ZwSetInformationThread are cataloged at high()/Likely-75, and NtQuerySystemInformation at medium()/Unsure-40, each claiming debug-specific information classes (ProcessDebugPort/0x1E/0x1F, ThreadHideFromDebugger 0x11, SystemKernelDebuggerInformation 0x23), but api_call_hits never recovers the InformationClass argument. The common benign-for-T1622 uses — InfoClass 0 (ProcessBasicInformation) to get a remote PEB, class 5 (SystemProcessInformation) to find a PID, class 0xB (SystemModuleInformation), class 4 (ThreadAffinityMask) — are credential-dumper/injector/driver plumbing, so the detector's most confident tier fires on exactly the samples' non-anti-debug machinery.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/debugger_evasion.rs:55-79 (the four ApiSpecs at high()), :80-85 (NtQuerySystemInformation at medium()); thorpal/src/thorpal/detect/detections.rs:1079-1149 (api_call_hits has no argument recovery)`

### 5. Thread context DR overclaim

Get/SetThreadContext specs assert the hardware-breakpoint reading ('read/clear CPU debug registers DR0-DR7') without ever checking ContextFlags. Hook engines (Detours-style commit, EIP migration over patched ranges), thread-hijack injectors, and winpthreads pthread_cancel all call these with CONTEXT_CONTROL only (0x10001/0x100001 — the CONTEXT_DEBUG_REGISTERS bit absent), so agents refute the claim at the flags level every time.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/debugger_evasion.rs:110-121 (GetThreadContext/SetThreadContext ApiSpecs)`

### 6. Ubiquitous API weak claims

The OutputDebugString and NtClose catalog entries assert exotic tricks (GetLastError-after-ODS probing, deliberate invalid-handle NtClose exceptions) that the detector never verifies with any supporting pattern; in practice the flagged sites are plain error logging and routine handle cleanup, which agents disprove by noting the return value / last-error is never read.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/debugger_evasion.rs:86-91 (OutputDebugString spec), :122-128 (NtClose spec)`

### 7. Decoy import flagging

When an API is imported but no call site resolves, api_call_hits still emits a per-API 'X imported' hit at site 0x0 (low()/Untrusted, file-level bucket). Packer stubs ship large decoy import tables with zero code xrefs precisely to bait import-based triage, so these hits flag imports the cleartext code can never execute.

*Where:* `thorpal/src/thorpal/detect/detections.rs:1099-1113 (imported-but-unreferenced branch of api_call_hits)`

### 8. Ptrace wrapper misattribution

The raw-syscall rule (SYSCALL with eax==101) hard-codes the PTRACE_TRACEME self-attach narrative, but in a statically linked ELF the single syscall site is glibc's varargs ptrace() wrapper whose request number is a caller parameter — callers may pass ATTACH/POKEDATA/DETACH requests used offensively against other processes. The first-argument register (edi/rdi) is never recovered even though reg_value_before already exists for eax.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/debugger_evasion.rs:199-211 (ptrace syscall rule)`

## Why leads get ignored
---

- **Per instruction PEB flood** — scan_instructions emits one hit per matching instruction with no per-function grouping (unlike api_call_hits) and no per-detector cap, and each hit's function gets its own forum post (multiple hits in one function become one post with many finding comments — every one a lead). Binaries whose runtime or obfuscation layer loads the PEB pervasively seed hundreds of 'Direct PEB access'/'PEB ProcessHeap flags check' posts that no agent can triage; because these samples are typically stripped (no names), neither the runtime weaken nor the rollup fires, so the flood lands as full findings.
- **CRT API ubiquity** — IsDebuggerPresent, OutputDebugString, and Get/SetThreadContext live in effectively every MSVC/MinGW binary's runtime, and api_call_hits seeds one finding per calling function. When the toolchain fingerprint works the hits still surface as rollup lines; when it doesn't they are full unsure-grade findings — either way agents skip them as background noise.
- **Import only untrusted noise** — Imported-but-uncalled hits (site 0x0, untrusted/15) are seeded per API into the file-level bucket; they carry no code location to investigate, so agents never open them — and when they do, the import is a packer decoy.
- **Rollup lines counted as leads** — The runtime-noise rollup post's per-line entries are leads in telemetry but are suppressed-by-design content agents are told is noise, so their ignores are structural, not a signal failure; they also attract collateral disputes about other techniques' lines in the same post.

## What would reduce the noise
---

Static-side changes, ordered as proposed; each was checked against the true positives above so the fix does not suppress what the detector gets right.

### Recover the InformationClass argument before claiming debug-class Nt* queries

In debugger_evasion.rs, post-process the api_call_hits results for NtQueryInformationProcess/ZwQueryInformationProcess, NtQuerySystemInformation, and Nt/ZwSetInformationThread: for each call site, use the function's cached disassembly (ctx.all_function_disassembly) plus a generalized reg_value_before (detections.rs:1338) to recover the class argument (x64: EDX/RDX for the 2nd arg, ECX/RCX for NtQuerySystemInformation's 1st; x86: the Nth-from-last PUSH imm before the CALL). Keep the current top severity only when the class is in the debug set ({7,0x1E,0x1F} for process queries, 0x23 for system queries, 0x11 for thread set) and say so in the content; drop the hit when a non-debug class (0, 5, 0xB, 4, 0x1A, ...) is recovered at every site; demote one notch (high->medium, medium->low) with honest 'class not statically resolved' wording when recovery fails.

*Effort:* medium \
*True-positive risk:* Low: the demote-on-unrecoverable rule keeps register-passed true positives seeded (one notch lower), and a verified class 7/0x23/0x11 now seeds stronger, cleaner gauntlet questions.

Addresses: dual use nt api no informationclass gate.

### Arch-aware, register-tracked PEB field classification with Ldr-walk relabeling

Rewrite classify_peb (debugger_evasion.rs:247-298): (1) pass the arch (fs:[0x30] vs gs:[0x60], already distinguished at :185) and use per-arch offset tables — x86 {0x2 BeingDebugged, 0x68 NtGlobalFlag, 0x18 ProcessHeap, 0xC Ldr}, x64 {0x2, 0xBC, 0x30 ProcessHeap, 0x18 Ldr}; (2) capture the destination register of the PEB load and only match '[`<that reg>`+0xNN]' dereferences, ending the look-ahead when the register is overwritten, so [rsp+0x18] spills and LDR-entry field reads stop matching; (3) when the consumed field is Ldr, emit a correctly-labeled 'PEB Ldr module-list walk — manual API resolution' hit (medium) instead of an anti-debug claim; (4) for NtGlobalFlag, look ahead for the tested mask and demote when it is FLG_APPLICATION_VERIFIER — matching both the literal TEST/AND 0x100 form and the UCRT SHR reg,8 + TEST reg,1 idiom — keeping high() for the 0x70 heap-flags mask or an unresolved mask.

*Effort:* medium \
*True-positive risk:* A 'PEB ProcessHeap flags check' lead in an x64 image actually reads Ldr at +0x18 — such a lead survives relabeled with the truer (and still suspicious) API-resolution claim; genuine x86 +0x18 heap-flag checks keep their current label and severity. 'Direct PEB access' leads keep seeding (the generic fallback remains, reworded to cover API resolution).

Addresses: peb offset misclassification, peb ldr api resolution overclaim, crt library code not recognized.

### Group PEB instruction hits per function and cap generic PEB findings per program

In the scan_instructions pass (debugger_evasion.rs:164-216), collect PEB-load hits into a `BTreeMap<func_entry, Vec<site>>` and emit one hit per function with a sites list (mirroring api_call_hits' grouping at detections.rs:1117-1145). Then cap generic 'Direct PEB access' hits at ~8 per program: classified field reads (BeingDebugged/NtGlobalFlag/ProcessHeap/Ldr) are exempt, and the overflow collapses into one file-level summary hit ('PEB base loaded in N further functions — pervasive, likely runtime/obfuscation; sites elided').

*Effort:* small \
*True-positive risk:* True-positive Direct PEB leads are single per-function findings, so grouping keeps them one-per-function and the cap should not trigger in typical sessions (plausible rather than proven); in flooded sessions the exempt classified reads still seed individually, and the summary line keeps the pervasive-PEB fact on the record.

Addresses: per instruction peb flood.

### Recognize the MSVC fail-fast/report skeleton without symbol names

Add a name-independent CRT recognizer feeding the existing CallContext::Runtime path (detections.rs:921-991): classify a containing function as CRT fail-fast/report code when its call set (from the already-cached disassembly plus the import call-site map) contains >=2 of {IsProcessorFeaturePresent, RtlCaptureContext, RtlVirtualUnwind, UnhandledExceptionFilter, SetUnhandledExceptionFilter, GetCurrentProcess+TerminateProcess} or an INT 0x29 (__fastfail) instruction. Apply it in apply_runtime_context as a fallback when the function name is FUN_*-shaped, weakening the hit and attaching the runtime note so partition_suppressed routes it to the rollup; also let a positive match contribute an msvc_points point in toolchain.rs (fingerprinting must then run with disassembly available, which ctx already caches) so stripped-CRT binaries regain the rollup entirely.

*Effort:* medium \
*True-positive risk:* True-positive IsDebuggerPresent/ODS leads co-locate IsDebuggerPresent+OutputDebugString — neither is in the gate set, so they stay full findings (a residual match risk remains if their functions also call the gate-set APIs). A sample's own SEH/SetUnhandledExceptionFilter anti-debug could match if it also calls RtlCaptureContext etc., but the action is weaken-to-rollup, never drop, and rollup lines still earn confirms.

Addresses: crt library code not recognized, crt api ubiquity.

### Drop NtClose and gate OutputDebugString on the actual last-error probe

Remove the NtClose ApiSpec (debugger_evasion.rs:122-128) outright. For OutputDebugString (:86-91), emit a hit only when the calling function also calls GetLastError/SetLastError (resolvable by also fetching those imports' call sites via the same resolve_call_sites machinery) — the actual ODS anti-debug trick — and drop the import-only ODS variant; otherwise emit nothing.

*Effort:* small \
*True-positive risk:* Loses NtClose leads — whose value, when they have any, comes from the surrounding code (such as a DLL-hollowing mapper) rather than the invalid-handle trick — and ODS leads without a last-error probe; ODS sites that share a bucket with an IsDebuggerPresent finding still seed their thread through it, and functions that also call CheckRemoteDebuggerPresent/IsDebuggerPresent keep those flags.

Addresses: ubiquitous api weak claims, crt api ubiquity, import only untrusted noise.

### Collapse import-only anti-debug hits into one file-level lead with a decoy-IAT note

In debugger_evasion.rs (or as an api_call_hits option), replace the per-API 'X imported' hits for this detector with a single file-level hit listing every matched-but-uncalled API ('anti-debug APIs imported but never called: A, B, C'), kept at low(), with reasoning that zero resolved call sites across several flagged imports is characteristic of a packer decoy import table and the capability, if real, lives in a packed payload.

*Effort:* small \
*True-positive risk:* Import-only leads are not where this detector's true positives come from, so nothing of value is suppressed; the merged line preserves the signal (and names the decoy-IAT hypothesis agents currently have to derive themselves).

Addresses: decoy import flagging, import only untrusted noise.

### Require request==0 for the ptrace TRACEME claim

For the raw-syscall rule (debugger_evasion.rs:199-211), additionally run reg_value_before over the preceding window for edi/rdi: keep high() with the TRACEME wording only when the request resolves to 0; otherwise emit medium() with neutral wording ('raw ptrace syscall; request not statically resolved — may be anti-debug self-attach or offensive tracing of another process'). Mirror the softened wording in the ptrace ApiSpec (:129-135).

*Effort:* small \
*True-positive risk:* Real TRACEME anti-debug conventionally zeroes edi immediately before the syscall (xor edi,edi / mov edi,0 — both handled by reg_value_before at detections.rs:1338), so true positives keep the high tier; a glibc wrapper's parameter-staged request correctly falls to the neutral claim.

Addresses: ptrace wrapper misattribution.

## Verification notes
---

- Code anchors: classify_peb (debugger_evasion.rs:247-298, +0x18 branch 279-288, fallback 290-297), the PEB-load match (:185-195), ptrace rule (:199-211), ODS/NtClose/thread-context ApiSpecs, apply_runtime_context (detections.rs:979-991, purely name-based), is_library_function (:1006-1039), api_call_hits with no argument recovery (:1079-1149; imported-but-unreferenced branch :1099-1113; per-function grouping :1117-1145), reg_value_before (:1338, handles mov-imm and xor-zero), msvc_points (toolchain.rs:156-166, counts is_library_function names), partition_suppressed requiring a fingerprinted toolchain (seed.rs:452-459), and evasion_question gating on Likely T1622/T1497 hits (seed.rs:1611-1640).
- NtQuerySystemInformation is medium()/Unsure-40 (debugger_evasion.rs:80-85), not high()/Likely-75; mechanism 4's anchor is split into :55-79 (the four high() specs) + :80-85. The mechanism itself (no InformationClass recovery) holds for all of them.
- Rollup-line disputes are collateral: they quote refutations of other techniques' lines in the same post, not of the T1622 line.
- The q-evasion-gauntlet is the lowest-priority derived question and is truncated first at the MAX_SEED_QUESTIONS cap (derive_questions order, seed.rs:1189-1208), so Likely T1622 hits seed it whenever budget allows rather than unconditionally. The inverted-confidence point stands.
- Fix 2's NtGlobalFlag demotion must match the UCRT idiom SHR reg,8 + TEST reg,1 as well as a literal TEST reg,0x100; fix 1 respects NtQuerySystemInformation's existing medium() tier; fix 4's toolchain fingerprint must run with the cached disassembly available.
