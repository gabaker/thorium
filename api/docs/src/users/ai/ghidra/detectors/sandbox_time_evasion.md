# sandbox_time_evasion

Part of the [telemetry review](../detectors.md#what-the-telemetry-says) of the [preseed detectors](../detectors.md).

**Status (2026-10-31):** the fixes below shipped in the fleet-wide detector overhaul (see the main page's overhaul status note). Deviation: the bare-tell always-noise flip was left off — the per-program cap plus the seeder admission budget absorb that volume instead (both at once would double-fold). Refinement: in the import-slot false-call fix, non-call-typed references count as call evidence only when the referencing site sits inside a function, which keeps real indirect import calls while the slot shape still drops.

sandbox_time_evasion is a very high-volume detector and one of the least precise. It flags pure presence of the most dual-use APIs on Windows (Sleep, GetTickCount, QPC, GetSystemTimeAsFileTime) with no use-analysis — the EvidenceKind::Timing "delta correlation" validation its own comments promise was never ported (no crate::validate module exists), and the CallContext::SeedsRng/PacesLoop variants written for exactly these false positives are never constructed outside a unit test — so beacon intervals, C2 retry backoff, PRNG seeds, SCM timeouts, CRT cookie init, static OpenSSL/musl library code, VMProtect junk RDTSC, and VMP import-table slots all seed as T1497.003 claims of sandbox stalling. The pattern also generates false confirms: a byte-proven-dead VMP RDTSC site, a VMP import slot reported as a 'Sleep call', and an exploit's inter-packet pacing function can all be accepted before being disproved. The genuine wins are long-sleep stalls (e.g. a 24-hour, 86,400,000 ms sleep), rollup lines agents re-raise in suspect hosts, and Sleep/timer site inventories agents use as a census skeleton to map C2 cadence. Because its max severity is medium()/Unsure it can never trigger the q-evasion-gauntlet question (seed.rs:1619 requires Likely, and nothing in the tree ever calls strengthen()), so all this volume buys only statement posts and rollup lines — no delegation leverage. It is not harmful (the rollup already contains most noise), but as shipped it is a large noise source for a small per-lead return.

## Where the false positives come from
---

### 1. Sleep API presence overclaim

Every resolved Sleep/SleepEx call site (one hit per calling function) is seeded as T1497.003 with template text asserting intent ('long or repeated sleeps stall past a sandbox's analysis window'). Sleep is the most dual-use API in Windows code: agents disprove these as beacon intervals, C2 reconnect/backoff loops (e.g. polling InternetGetConnectedState until online, then backing off between failed connection attempts), registry-availability polling, keylogger/shell poll pacing, malloc-retry waits, UAC-prompt retry delays, and an alertable-infinite APC park (SleepEx(INFINITE,TRUE), no finite delay at all). The claim of analysis-window stalling is wrong even when the host function is malicious (wrong-technique attribution on real C2 loops). Part of the disputes are collateral grading: a dispute aimed at a sibling detector's claim in the same per-function thread (such as a Data Encrypted for Impact finding, or a network-port finding) marks the Sleep lead disputed too.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/sandbox_time_evasion.rs:76-122 (SLEEP_APIS) + detections.rs:1079-1149 (api_call_hits presence-only emission)`

### 2. Timer read presence no use analysis

TIMING_APIS hits are emitted on any call of GetTickCount/QPC/QPF/timeGetTime/GetSystemTimeAsFileTime/time-syscalls with no check that the value is ever compared to a second reading. The EvidenceKind::Timing contract ('validated by delta correlation', sandbox_time_evasion.rs:207-209; 'post-detection pass (crate::validate)', detections.rs:238) is vestigial — no crate::validate module exists in the ported engine, and CallContext::SeedsRng (provenance_types.rs:18) is never constructed anywhere. Disproved uses: srand(GetTickCount) seeding (e.g. for a ZipCrypto encryption header), GetTickCount as a sprintf beacon nonce, SCM stop-wait 5s timeouts, exploit inter-packet pacing, keylogger 500ms rate-limits, media-queue dequeue timeouts, CPU-MHz estimation, and outright dead reads (CALL EDI with EAX never stored/compared; GetTickCount in an unreachable builder-junk block).

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/sandbox_time_evasion.rs:30-73 (TIMING_APIS) + :207-209 (unported validation claim); provenance_types.rs:18-21 (dead SeedsRng/PacesLoop variants, constructed nowhere outside a unit test)`

### 3. RDTSC mutation junk

scan_instructions emits a medium/40 hit per RDTSC instruction with no def-use requirement: no check that EAX/EDX are consumed before being overwritten, and no requirement of a second RDTSC or a delta subtraction/compare. VMProtect's instruction-mutation layer laces functions with bare dead `rdtsc` whose outputs are immediately clobbered (both result registers overwritten before any use, no second RDTSC, no delta comparison), so a single protected sample yields dozens of junk hits. The detector docs' own improvement list (detectors.md 'Paired-RDTSC detection') already names the missing gate.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/sandbox_time_evasion.rs:150-174 (per-instruction RDTSC/RDTSCP arms, no operand/pair gate)`

### 4. Unrecognized library code

Runtime classification is name-only (is_library_function marker substrings + toolchain runtime markers, applied once over merged hits by apply_runtime_context), so library code escapes the weaken/rollup path whenever names are missing or unlisted: stripped MSVC binaries put __security_init_cookie's canonical GSTAFT+QPC+GetTickCount cluster in FUN_-named functions (the 'security_init_cookie' marker never matches); statically-linked OpenSSL RAND_poll (GetTickCount walk-time bounds), musl clock_gettime (hit twice — syscall 228 plus its ENOSYS 96 fallback in the same wrapper), ConcRT ContextBase::DetachStealers Sleep(1) spin-backoff (FID-named but matching no marker), and media-library timeGetTime timeouts all seed as sample behavior.

*Where:* `thorpal/src/thorpal/detect/detections.rs:1006-1039 (is_library_function name markers) + :921-960 (runtime_classification requires a resolvable name, line 946); sandbox_time_evasion.rs:176-201 (syscall arm — relies on the same name-only pass, which cannot fire on stripped static code)`

### 5. Import slot as call site

resolve_call_sites drops xrefs only when the referencing address sits in non-executable memory (context.rs:844); it never checks the xref's ref_type (the sibling callers() helper does filter on CALL at context.rs:773). VMProtect relocates the import table into an executable .vmp section, so each 8-byte import lookup slot passes is_executable_addr and is reported as '`<API>` call @ `<slot addr>`' — a data address no code executes, attributed to no function, landing in the never-suppressed file-level bucket.

*Where:* `thorpal/src/thorpal/detect/context.rs:838-846 (executability-only xref filter in resolve_call_sites; no ref_type check)`

### 6. Long sleep context blind

long_sleep_hits upgrades any recovered constant >= 60,000 ms to a medium/40 'long stall used to outlast a sandbox's analysis window' with no loop/retry context: 2-minute C2 reconnect backoffs, 5-minute beacon-round pauses, and 8-minute retry delays all qualify, and each long-sleep hit duplicates the bare Sleep-call hit already seeded for the same site in the same bucket (two leads per site).

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/sandbox_time_evasion.rs:125 (LONG_SLEEP_MS=60_000) + :230-300 (long_sleep_hits, no loop/network context)`

### 7. Rollup collateral and framing

Rollup-shape disputes are mostly not about the T1497 entries at all: one disputing comment on the runtime-noise rollup post (e.g. about wrongly-suppressed T1070.004 self-delete entries) marks every graded T1497.003 line in it disputed. The rollup body itself invites a framing dispute: seed_suppressed_rollup always says 'This binary statically links a recognized {toolchain} runtime', which is factually wrong for MFC — the fingerprint fires on afx/mfc42 import names and strings (toolchain.rs:66, detect_from_names), not on static linking — so an import table pulling MFC42.DLL disproves the sentence.

*Where:* `thorpal/src/thorpal/seed.rs:548-556 (rollup body 'statically links' wording) + :511-578 (one post fronting all suppressed lines)`

### 8. Decoy import fallback

When an import has no resolvable call site, api_call_hits emits a file-level '`<API>` imported' hit (untrusted/15) at address 0 (DetectionHit::at resolves no containing function). Crypter/loader stubs ship decoy import tables whose entries are never called (e.g. IAT slots reached only by FF25 thunks that have zero callers), so these hits describe the decoy, not behavior — 'import-only findings on crypter/loader stubs' is a systematic FP class. The file-level bucket is never suppressed (partition_suppressed requires function.is_some(), seed.rs:489), so these always seed as findings.

*Where:* `thorpal/src/thorpal/detect/detections.rs:1099-1112 (imported-no-call-site fallback) + seed.rs:487-489 (file-level bucket exempt from suppression)`

## Why leads get ignored
---

- **Rollup line mass** — Much of the ignored output is rollup lines: every suppressed per-function entry in the runtime-noise rollup post is graded as its own lead (seed_suppressed_rollup records up to 500 per post as structured provenance), and the rollup exists precisely so agents don't engage each line. The suppression is working as designed, but telemetry counts each line, so this detector's ubiquitous-API catalogs (Sleep, GSTAFT, GetTickCount, QPC) make it a dominant contributor to rollup bulk.
- **Per function fanout of ubiquitous APIs** — api_call_hits emits one finding per (API, calling function) across a 15-entry catalog (7 TIMING_APIS + 8 SLEEP_APIS) of APIs that appear in essentially every Windows binary, and when no toolchain is fingerprinted (plain dynamically-linked MSVC binaries) nothing is suppressed (partition_suppressed returns everything kept when toolchain is None, seed.rs:457) — every function that sleeps or reads a clock gets its own untrusted finding comment, far beyond what agents triage.
- **Per instruction and per site multiplication** — The instruction scan emits one hit per RDTSC opcode (VMP-mutated binaries contain dozens of junk ones) and one per time/nanosleep syscall site (a single static binary can seed many 'nanosleep syscall' leads; musl's clock_gettime wrapper is hit twice for its two fallback syscalls). On top of that, long-sleep hits duplicate the bare Sleep hit for the same site, and the sibling timing APIs triple-report the same CRT cookie function (GSTAFT+QPC+GetTickCount as three leads in one bucket).
- **Untrusted floor grade and no question leverage** — Nearly the whole output sits at the confidence floor (untrusted/suspicion 15), and agents triage by confidence, so untrusted leads are mostly ignored. Because the detector's maximum severity is medium()/Unsure, it can never clear the Confidence::Likely bar in evasion_question (seed.rs:1619) — and nothing in the current tree ever calls strengthen(), so no post-detection pass can lift it — meaning no T1497.003 hit ever seeds a question; the leads have no delegation pull, only passive statement posts competing with stronger detectors' threads.

## What would reduce the noise
---

Static-side changes, ordered as proposed; each was checked against the true positives above so the fix does not suppress what the detector gets right.

### Filter resolve_call_sites to CALL/JUMP/THUNK-typed xrefs

In AnalysisContext::resolve_call_sites (context.rs:838-846), after the executability check, drop xrefs whose ref_type (upper-cased) contains none of CALL, JUMP, or THUNK, mirroring the filter callers() already applies at context.rs:773. The bridge fills ref_type from Ghidra's getReferenceType().toString() (GhidraCliBridge.java:1236), so call instructions arrive as *_CALL, thunk JMPs as *_JUMP/THUNK, and import-table pointer slots as DATA/READ. Imports referenced only by data slots then fall through to the existing 'imported, no call site' path instead of fabricating file-level call findings at data addresses.

*Effort:* small \
*True-positive risk:* Near zero: every genuine call site carries a CALL-typed ref (thunked calls hop via callers(), which is already CALL-filtered), JUMP keeps tail-call stubs, and THUNK keeps Ghidra's thunk-relationship refs. The only leads this removes are VMP import-slot 'calls', which are data, not code. Smoke-test the ref_type strings against one real binary before relying on the exact names.

Addresses: import slot as call site.

### Gate RDTSC hits on propagated output or a paired read

In the RDTSC/RDTSCP arm (sandbox_time_evasion.rs:150-174), inspect c.following(~8) (InsnCtx::following exists, detections.rs:1263): treat the TSC value as consumed only when EAX/EDX is PROPAGATED — stored to memory, pushed, moved to another register, used as a source operand of arithmetic/compare whose destination is a different location, or shifted/or-ed into a 64-bit combine. In-place mutation (SAR/ROL/XOR-imm on the register itself, as VMP junk does) must NOT count as a use. If both outputs are overwritten without propagation, skip the hit as mutation junk. Keep medium() only when the function contains a second RDTSC or a sub/cmp consuming the result; otherwise demote to low(). Also dedupe to one hit per function with a sites list (reuse the api_call_hits grouping idiom) so VMP-laced functions stop emitting dozens of leads.

*Effort:* medium \
*True-positive risk:* Genuine consumed-RDTSC checks survive: a real bracketing check (two reads, delta compared) stays medium, and a lone propagated RDTSC survives at low — only propagation-free junk is dropped. Residual risk: a heavily-mutated genuine timing check whose propagation falls outside the ~8-instruction window would be skipped.

Addresses: rdtsc mutation junk, per instruction and per site multiplication.

### Wire PacesLoop/SeedsRng context from co-located detector output

Add a sibling pass to apply_runtime_context (detections.rs:979) run over the seeder's per-function buckets: when a bucket holds both a T1497.003 sleep/timing hit and a command-and-control-tactic hit (or the function's resolved call set — computed via resolve_call_sites over connect/send/recv/InternetOpen/WinHttp*/InternetGetConnectedState imports — includes network I/O), set the dormant CallContext::PacesLoop (provenance_types.rs:21) on the sleep/timer hit and weaken() it; when a timer read's value feeds srand (srand import called in the same function near the read), set CallContext::SeedsRng (provenance_types.rs:18). The prose notes for both variants already exist and render via comment_body (seed.rs:751-754). Note the real effects precisely: weaken() no-ops on already-Untrusted bare Sleep hits (their payoff is the rendered note), it demotes Unsure long-sleep hits to Untrusted, and rollup eligibility still requires a fingerprinted toolchain plus an all-noise bucket (a bucket holding the C2 hit stays kept regardless) — extend partition_suppressed's is_noise to accept PacesLoop alongside Runtime if suppression of these is also wanted.

*Effort:* medium \
*True-positive risk:* Moderate but managed: a true pre-detonation stall inside a network-capable function gets the PacesLoop note and one confidence step down, not deletion — long-sleep (>=60s) hits keep their own lead (demoted, still seeded). Sleep/GetTickCount leads that map C2 cadence remain visible (rollup lines are the detector's best-performing shape), and the note hands the agent the correct framing ('beacon interval') that disputing agents currently write by hand.

Addresses: sleep api presence overclaim, timer read presence no use analysis.

### Recognize the /GS cookie-init cluster without symbol names

In the detector (or a shared helper beside is_library_function), before emitting TIMING_APIS hits, test each candidate function: small (<= ~50 instructions) and calling at least three of {GetSystemTimeAsFileTime, QueryPerformanceCounter, GetCurrentProcessId, GetCurrentThreadId, GetTickCount} with an XOR chain — the canonical __security_init_cookie shape. Classify every timing hit in such a function as CallContext::Runtime("__security_init_cookie (shape-matched)") so it weakens and rolls up even in stripped binaries where the name markers (detections.rs:1009-1037) cannot fire.

*Effort:* medium \
*True-positive risk:* Low: requiring the PID/TID companions makes the signature specific — a genuine timing gauntlet reads clocks but has no reason to mix in GetCurrentProcessId/ThreadId and XOR into one global. True-positive GetTickCount/QPC leads sit in C2 loops and service code, not this shape. Covers only the cookie-init slice of the mechanism (OpenSSL/musl/ConcRT need marker additions or FID handling separately).

Addresses: unrecognized library code.

### Collapse uncorroborated untrusted sleep/timer findings into a census

Stop seeding one per-function finding per bare untrusted sleep/timer hit when nothing corroborates it: either (a) extend partition_suppressed (seed.rs:452-497) so this detector's Untrusted T1497.003 hits (both ContextOnly sleeps and Timing-kind reads) are rollup-eligible even without a toolchain fingerprint — which also means changing seed_suppressed_rollup's mandatory `toolchain: Toolchain` parameter to an Option and giving the no-fingerprint census its own accurate body wording — keeping a bucket as findings when it also holds any >=Unsure hit from any detector; or (b) have the detector aggregate per API — one hit per API listing the calling functions in sites — the way api_call_hits already aggregates per function. Either way the per-session lead count drops from tens to a handful.

*Effort:* medium \
*True-positive risk:* Untrusted-grade true positives (bare-Sleep and GetTickCount findings) move to census/rollup form. Rollup entries still get engaged and confirmed at a far better ratio than finding-shape presence hits, and the >=Unsure-co-location escape hatch keeps buckets like a day-long-sleep loader as full threads. Long-sleep, RDTSC, and NtDelayExecution hits are medium and unaffected.

Addresses: per function fanout of ubiquitous apis, sleep api presence overclaim, rollup line mass, untrusted floor grade and no question leverage.

### Send 'imported, no call site' sleep/timer hits to the census/rollup

For this detector's catalogs, divert the api_call_hits imported-fallback hits (detections.rs:1099-1112) out of the never-suppressed file-level bucket: in the seeder, pull Untrusted import-only hits (address==0, reasoning carries the 'no direct call site was resolved' marker or a dedicated flag on the hit) out of the file-level bucket into the suppressed set before partition_suppressed runs, and record them in the rollup/census post with an explicit 'import present, zero resolved callers — possible decoy IAT' line. Do NOT express this as CallContext::Runtime — its note prose asserts 'this is compiler-generated runtime code', which is false for decoy imports; add a new CallContext variant or census-only wording instead. Because decoy-IAT crypter stubs rarely fingerprint a toolchain, this depends on the no-fingerprint census post from the previous fix (or ship its own small 'uncalled imports' post).

*Effort:* medium \
*True-positive risk:* Low: import-only hits are not where this detector's true positives come from, and the hit's own text already concedes no call site was resolved — statically unprovable behavior. The record is kept for re-raising.

Addresses: decoy import fallback.

### Dedupe long-sleep vs bare-Sleep leads and split the threshold

In SandboxTimeEvasion::run, when long_sleep_hits covered a site, remove that site from (or skip) the corresponding SLEEP_APIS call hit for the same function so each site seeds once; in long_sleep_hits keep medium() only for ms >= ~600_000 (10 min), and emit 60s–10min recoveries at low() with reasoning that names both readings ('sandbox stall or retry/beacon cadence').

*Effort:* small \
*True-positive risk:* Day-scale stalls (e.g. 86,400,000 ms) stay medium; 2–8 min delays (120000/300000/480000 ms C2 backoffs) drop to low but still seed. Any genuine stall of 1–10 min would rank lower in its bucket but remain seeded, not lost.

Addresses: long sleep context blind, per instruction and per site multiplication.

### Fix the rollup post's 'statically links' wording

In seed_suppressed_rollup (seed.rs:548-556), replace 'This binary statically links a recognized {label} runtime' with fingerprint-accurate phrasing, e.g. 'This binary was built with a recognized {label} toolchain/runtime (statically linked or imported)' — or thread the fingerprint source through Toolchain so MFC (fingerprinted from afx/mfc42 import names and strings, toolchain.rs:66) renders 'dynamically links'.

*Effort:* small \
*True-positive risk:* None — wording only; suppression behavior unchanged, and it removes a factual error agents otherwise spend a thread rebutting.

Addresses: rollup collateral and framing.

## Verification notes
---

- Code anchors: SLEEP_APIS at sandbox_time_evasion.rs:76-122, TIMING_APIS :30-73, RDTSC arm :150-174, syscall arm :176-201, LONG_SLEEP_MS :125 + long_sleep_hits :230-300; api_call_hits detections.rs:1079-1149 with imported-fallback :1099-1112; is_library_function :1006-1039; runtime_classification :921-960 (name required at :946); resolve_call_sites context.rs:833-877 with executability-only filter at :844 and the CALL-filtered callers() at :773; partition_suppressed seed.rs:452-497 (file-level exemption :489, toolchain gate :457); seed_suppressed_rollup :511-578 ('statically links' wording :548-556); comment_body context note :751-754; evasion_question Likely bar :1619.
- Dead-code claims: CallContext::SeedsRng/PacesLoop are constructed nowhere (GatesBranch only inside a unit test at detections.rs:1838, inside mod tests at :1728); no crate::validate module exists anywhere in thorpal/src (the only validate functions are unrelated: mcp, store, workflow), so the EvidenceKind::Timing 'delta correlation' contract (detections.rs:238-248, detector comment at :207-209) is vestigial. strengthen()/strengthen_to() have zero production callers, so the 'can never seed the question' claim is unconditional.
- SeedsRng is provenance_types.rs:18, PacesLoop :21; the 'validated by delta correlation' comment sits at sandbox_time_evasion.rs:207-208 (call at :209). Mechanism 4's syscall-arm hits do pass through the shared apply_runtime_context (seed.rs:201) like all others, but that pass is name-only and cannot fire on stripped static code, which is the actual gap.
- Fix 1 is implementable end-to-end: XRef.ref_type exists (the sibling ghidra-cli crate, models.rs:293-299), filled from Ghidra getReferenceType().toString() (GhidraCliBridge.java:1236), with the callers() CALL-filter precedent in-tree; it also keeps THUNK-typed refs, and the exact RefType strings should be smoke-tested. Fix 2 relies on InsnCtx::following (detections.rs:1263); VMP junk mutates EAX in place, which an 'arithmetic/compare with another operand' rule would wrongly count as a use — consumption must mean value propagation.
- Fix 3: weaken() saturates at the floor so it no-ops on already-Untrusted bare Sleep hits (the real payoff is the rendered note), suppression still requires a fingerprinted toolchain and an all-noise bucket (a bucket holding the C2 hit stays kept), and partition_suppressed's is_noise only honors CallContext::Runtime today. Fix 5: 'Untrusted ContextOnly' alone would miss the Timing-kind timer reads, and option (a) requires changing seed_suppressed_rollup's mandatory Toolchain parameter plus no-fingerprint census wording. Fix 6: decoy-IAT stubs rarely fingerprint a toolchain so the suppression path as-is would never fire for them, and reusing CallContext::Runtime would render the false sentence 'this is compiler-generated runtime code'. Fix 7 is a threshold split only (loop detection has no existing helper).
- Supporting facts: detectors.md's improvement list names 'Paired-RDTSC detection'; MFC fingerprinting is import-name/string based (MFC_STRING_MARKERS 'mfc42' etc. at toolchain.rs:66, two-point detect_from_names), confirming the rollup wording error; imported-fallback hits land file-level because DetectionHit::at(ctx, …, 0, …) resolves no containing function; the rollup records up to 500 suppressed findings as provenance (seed.rs:570-574), which is why telemetry grades each rollup line as a lead; runtime classification almost never fires for this detector.
- Collateral grading (disputes aimed at sibling detectors or at other rollup entries marking T1497 leads disputed) is covered inside sleep-api-presence-overclaim and rollup-collateral-and-framing; it also hits non-Sleep finding-shape leads such as long-sleep hits.
