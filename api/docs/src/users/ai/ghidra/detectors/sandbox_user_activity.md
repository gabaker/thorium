# sandbox_user_activity

Part of the [telemetry review](../detectors.md#what-the-telemetry-says) of the [preseed detectors](../detectors.md).

**Status (2026-10-31):** the fixes below shipped in the fleet-wide detector overhaul (see the main page's overhaul status note). Deviations: the SetWindowsHookEx relevant set tightened to {WH_KEYBOARD, WH_KEYBOARD_LL, WH_MOUSE_LL}; an unrecovered idHook stays at the central medium clamp rather than demoting to low.

sandbox_user_activity is a bare import-catalog matcher over nine ubiquitous Win32 user-interaction APIs (GetCursorPos, mouse_event, GetAsyncKeyState, GetKeyState, GetLastInputInfo, SetWindowsHookEx, GetForegroundWindow, GetSystemMetrics, EnumDisplaySettings) with no argument recovery and no result-use check. Every resolved call site is stamped EvidenceKind::ApiResultCheck, but the validator that kind routes to (crate::validate) was never ported — provenance_types.rs's header and apply_runtime_context's doc both say so — so every call-site hit seeds at full catalog severity and CallContext::GatesBranch is set nowhere outside tests. The false positives split between crypter junk-API salads whose results are provably discarded, statically-linked GUI framework code (VCL/MFC/NSIS/Inno) the name-based runtime filter cannot recognize in stripped binaries, argument-blind GetSystemMetrics/SetWindowsHookEx claims (SM_SERVERR2 and WH_MSGFILTER flagged as sandbox checks), and intent over-claiming on real malware that uses these APIs for other purposes (keylogging, forcing foreground, beacon telemetry). The ignore flood is structural: one hit per calling function of a GUI API at Unsure grade, which the runtime-noise rollup can only absorb when a toolchain was fingerprinted AND the containing function has a recognizable runtime name. The detector earns occasional true sandbox-gate catches, but in its current form it is a net drag on the forum.

## Where the false positives come from
---

### 1. Dead result junk API calls

Crypter/obfuscator stubs deliberately call (or merely import) batteries of benign-looking APIs with garbage args and discarded results; the detector flags every resolved call site at Unsure/40 because api_call_hits only resolves xrefs — it never checks whether the return value is read, compared, or branched on. EvidenceKind::ApiResultCheck documents exactly the needed validation ('the return is tested and feeds a conditional branch') but no validate pass exists in this port, and CallContext::GatesBranch is set only in a test. The same xref-only logic also emits zero-xref decoy-import hits (at Untrusted/15, default ContextOnly kind, file-level) and flags dead IAT-slot reads, which land in the same junk-API false positives.

*Where:* `thorpal/src/thorpal/detect/detections.rs:1079-1149 (api_call_hits; imported-no-call-site branch at 1099-1112), detections.rs:238-246 (ApiResultCheck doc pointing at the unported crate::validate; GatesBranch set only at detections.rs:1838 inside tests); sandbox_user_activity.rs:98-102`

### 2. Stripped gui framework library code

GetCursorPos/GetKeyState/GetSystemMetrics/SetWindowsHookEx/GetForegroundWindow are bread-and-butter GUI plumbing in statically-linked frameworks (Delphi VCL, MFC, NSIS ui.c, Inno engine) and hand-written WndProcs. The only live suppression is name-based: apply_runtime_context attaches CallContext::Runtime (and weakens one step) only when is_library_function matches MSVC/CRT markers or is_toolchain_runtime matches afx/cwnd::/Delphi-unit names — in stripped static binaries these functions are FUN_xxxxxxx, so no Runtime context attaches, the Unsure hits escape partition_suppressed's noise test, and each framework function seeds a standalone finding. toolchain.rs's own header admits 'MFC GUI apps import every user-activity API'.

*Where:* `thorpal/src/thorpal/detect/detections.rs:1006-1039 (is_library_function) + detections.rs:962-990 (apply_runtime_context, name-based) + detect/toolchain.rs:210-283 (is_toolchain_runtime) + seed.rs:452-497 (partition_suppressed: Unsure needs Runtime context to suppress)`

### 3. Real malware wrong intent claim

The API genuinely sits in malware code but serves a different capability than the reasoning template asserts: keylogger polling loops (collection T1056.001, not an unattended-sandbox gate), a force_foreground_window exploit helper (forces activity instead of checking it), GetLastInputInfo idle seconds formatted into a C2 beacon field with no branch (telemetry, not a gate), GetCursorPos as a bounded startup delay, GetSystemMetrics screen size used to place exploit windows off-screen. The template bakes a specific detonation-gating intent ('waits for genuine input before acting', 'malware delays or aborts') into every hit with zero corroboration, so agents must dispute the claim even when the function is hostile.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/sandbox_user_activity.rs:26-83 (reasoning templates; GetLastInputInfo at high()=Likely/75 lines 52-58)`

### 4. Getsystemmetrics index blind

The GetSystemMetrics claim ('reads screen resolution; small/default resolutions are characteristic of headless sandboxes') is asserted without recovering the statically-recoverable metric index argument. False-positive sites used SM_SERVERR2=89 (OS-version beacon field), SM_CYVSCROLL=0x15 (ListView column width), SM_MIDEASTENABLED=0x4A/SM_DBCSENABLED=0x2A (Delphi locale init), SM_CXICON/CYICON (MFC icon centering) — none resolution-related. The engine already recovers analogous immediates (reg_value_before for CPUID leaves in the sibling detector) but this detector never looks at the pushed index.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/sandbox_user_activity.rs:71-76 (GetSystemMetrics ApiSpec); contrast sandbox_system_checks.rs:185-186 (reg_value_before leaf recovery)`

### 5. Setwindowshookex hooktype blind

SetWindowsHookEx is flagged as an input-monitoring wait regardless of the idHook argument or thread scope. False-positive sites included a WH_MSGFILTER(-1) MFC CWinThread message-filter hook on the calling thread (cannot observe user input at all; the same Borland VCL hint-window WH_MSGFILTER idiom also appears in VCL GUI code), WH_MOUSE/WH_KEYBOARD hooks on the hooker's own thread feeding a disk-encryption tool's entropy pool, and a WH_GETMESSAGE remote-thread hook that is actually DLL injection (T1055.011, a different and more serious technique). Only WH_KEYBOARD(_LL)/WH_MOUSE(_LL)/journal hooks can support the user-activity story, and the idHook is the last immediate pushed before the call — statically recoverable.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/sandbox_user_activity.rs:59-64 (SetWindowsHookEx ApiSpec)`

## Why leads get ignored
---

- **Per calling function fanout of gui APIs** — api_call_hits emits one hit per (API, calling function) with no per-API or per-detector cap, so a statically-linked GUI binary turns every mouse/keyboard/metrics-touching framework function into its own lead. A single Delphi binary can contribute dozens of thunked GetCursorPos and GetSystemMetrics leads. No agent can open that many threads, so nearly all go unread.
- **Unsure grade exempt from rollup in stripped binaries** — partition_suppressed only rolls an Unsure hit into the runtime-noise post when its containing function name matched the runtime markers (and only when a toolchain was fingerprinted at all; Untrusted hits roll up on fingerprint alone); stripped VCL/MFC static code is FUN_-named, so the medium()-grade hits that dominate this detector (six of nine catalog APIs) always seed standalone per-function posts. Most ignored leads were full finding posts rather than rollup lines.
- **Weak untrusted and zero xref hits still seed visibly** — GetSystemMetrics hits are Untrusted/15 but still become per-function findings whenever no toolchain fingerprint exists (partition_suppressed returns everything unsuppressed on toolchain=None, seed.rs:457), and 'imported, no call site' hits land at address 0 with no containing function, i.e. in the file-level bucket, which is never suppressed by design — so the weakest possible evidence (an unreferenced decoy import) always occupies a visible file-level thread.
- **No question pull for medium leads** — Agent attention is routed by seeded questions, and evasion_question only cites T1497/T1622 hits at Likely or better — of this catalog only GetLastInputInfo is high()=Likely. The Unsure/Untrusted leads are never cited by any question, so engagement happens only when an agent independently lands in the same function.

## What would reduce the noise
---

Static-side changes, ordered as proposed; each was checked against the true positives above so the fix does not suppress what the detector gets right.

### Implement the promised result-use gate for ApiResultCheck hits

Add a post-pass in detections.rs (or inside api_call_hits) that, for each hit with EvidenceKind::ApiResultCheck, pulls the cached ctx.all_function_disassembly() (context.rs:661) for the containing function, locates each call site, and scans the following ~8 instructions: TEST/CMP of EAX/RAX followed by a Jcc sets CallContext::GatesBranch (keep severity); EAX overwritten (mov eax, xor eax,eax) or immediately-returned with no read demotes the hit to low() and attaches a new CallContext variant (e.g. ResultUnused) whose note says the return value is discarded (extend tag()/note() in provenance_types.rs and the exported provenance vocabulary from commit 65bbc473). Extend seed.rs partition_suppressed's is_noise to treat Unsure+ResultUnused like Unsure+Runtime so these roll up. For out-param APIs (GetCursorPos, GetLastInputInfo) gate on any compare+branch in the window instead of EAX specifically, since a real gate reads the output struct, not the BOOL.

*Effort:* large \
*True-positive risk:* True-positive GetCursorPos and GetForegroundWindow findings are genuine gates, so a branch-on-result test should pass them; the main risk is an interprocedural gate (helper returns the value for the caller to test) being demoted — mitigate by demoting only on provable overwrite/discard, never on mere absence of a local branch, and by demoting to rollup rather than dropping. A GetCursorPos bounded delay loop IS branched on, so this fix intentionally leaves that wrong-intent residual to the template fix.

Addresses: dead result junk api calls, real malware wrong intent claim.

### Recover the GetSystemMetrics index and gate the resolution claim on it

At each GetSystemMetrics call site, recover the index from the nearest preceding PUSH `<imm>` (x86) or MOV ECX, imm (x64 — reg_value_before(preceding, "ecx", "rcx")) via ctx.disasm_containing (context.rs:988), adding a small PUSH-immediate analog of reg_value_before using the existing parse_imm (detections.rs:1317) — the same pattern as sandbox_system_checks' CPUID leaf recovery (sandbox_system_checks.rs:185-186). Keep catalog severity only for sandbox-relevant indices (SM_CXSCREEN=0, SM_CYSCREEN=1, SM_MOUSEPRESENT=19, SM_CMONITORS=80, SM_REMOTESESSION=0x1000); rollup-demote (not drop) known-benign UI/locale/version indices (icon, scrollbar, DBCS/MidEast, SM_SERVERR2); for unrecoverable indices keep low() but reword content to name the uncertainty instead of asserting 'resolution'. Render the decoded SM_ name in the content so agents triage at a glance.

*Effort:* medium \
*True-positive risk:* True-positive GetSystemMetrics rollup lines are singles/adjacent pairs consistent with the classic CXSCREEN+CYSCREEN shape, but their recovered indices are unknown — so benign-index sites must be rollup-demoted rather than dropped (rollup lines still get engaged). Residual: even a real CXSCREEN/CYSCREEN pair can be off-screen window placement, which only the result-use fix can catch.

Addresses: getsystemmetrics index blind, stripped gui framework library code, weak untrusted and zero xref hits still seed visibly.

### Recover SetWindowsHookEx's idHook and keep only input-capture hook types

In stdcall the first argument is the last immediate pushed before the call, so recover idHook from the nearest preceding PUSH imm (MOV ECX, imm on x64), reusing fix 2's recovery helper. Keep the T1497.002 hit only for WH_KEYBOARD(2), WH_MOUSE(7), WH_KEYBOARD_LL(13), WH_MOUSE_LL(14), WH_JOURNALRECORD(0); drop WH_MSGFILTER(-1)/WH_GETMESSAGE(3)/WH_CALLWNDPROC(4)/WH_CBT(5) and other framework hook types from this detector (WH_GETMESSAGE-with-module belongs to an injection detector, not here); unrecovered idHook demotes to low().

*Effort:* small \
*True-positive risk:* No true-positive lead is a SetWindowsHookEx hit, so suppression risk is nil; own-thread WH_MOUSE/WH_KEYBOARD entropy hooks remain a residual FP that only thread-scope analysis could remove.

Addresses: setwindowshookex hooktype blind.

### Cap per-API fan-out: collapse ubiquitous usage into one summary hit

In api_call_hits (or a seed.rs pre-bucket pass keyed on hit.detection == "sandbox_user_activity", stamped by run_all_with_roster), when one catalog API resolves call sites in more than K distinct functions (K≈5), emit a single file-level summary hit at low() ('GetCursorPos called from N functions — ubiquitous GUI usage, not a targeted check') plus keep at most the few per-function hits that the result-use gate marked GatesBranch; route the rest into the suppressed rollup regardless of toolchain fingerprint. This kills the most-repeated ignored patterns at the source.

*Effort:* small \
*True-positive risk:* True-positive finding-shape leads are single-call-site hits, likely in low-fan-out binaries (an inference). A real targeted sandbox check concentrated in 1-3 functions is preserved; the GatesBranch carve-out that keeps a gating site loud inside a GUI-heavy binary only exists once fix 1 lands, so land fix 1 first or accept that until then a gate in a >K-fanout binary rolls up (recoverable: the rollup itemizes every entry).

Addresses: per calling function fanout of gui apis, stripped gui framework library code, unsure grade exempt from rollup in stripped binaries.

### Let weak buckets roll up without a toolchain fingerprint and move zero-xref imports into the rollup

In seed.rs partition_suppressed (452-497), allow Untrusted-only buckets (and ResultUnused-context Unsure buckets, after fix 1) to suppress even when toolchain is None, changing seed_suppressed_rollup's signature to take `Option<Toolchain>` (the expect at seed.rs:224 goes away) and retitling the rollup post generically ('N low-signal detector findings suppressed'); and in api_call_hits, tag 'imported, no call site' hits (address 0, function None, detections.rs:1099-1112) so the seeder routes them into the rollup instead of the never-suppressed file-level bucket — a zero-xref import (routinely a crypter decoy table) is the weakest evidence the detector produces.

*Effort:* small \
*True-positive risk:* Rollup lines still get engaged when the evidence matters; no true-positive lead is an 'imported' or Untrusted finding (true-positive findings are Unsure call-site hits, untouched by this change); the rollup body itemizes every entry, so nothing is lost — only de-amplified.

Addresses: weak untrusted and zero xref hits still seed visibly, unsure grade exempt from rollup in stripped binaries, dead result junk api calls.

### Re-tag per-function user-input API clusters as keylogging instead of sandbox evasion

After api_call_hits, when three or more distinct catalog/input APIs (GetForegroundWindow, GetCursorPos, GetAsyncKeyState/GetKeyState, SetWindowsHookEx keyboard/mouse types; optionally corroborated by GetKeyboardState/ToUnicode/MapVirtualKey/AttachThreadInput imports resolving into the same function) land in one function, replace the per-API T1497.002 hits for that function with one T1056.001 Input Capture: Keylogging hit (new technique const; AttackTactic::Collection already exists at detections.rs:80) listing all sites — the cluster is the classic poll-loop shape, and the single combined claim matches what agents actually conclude.

*Effort:* medium \
*True-positive risk:* A genuine user-activity gauntlet could also cluster these APIs in one function, but true-positive leads are single-API functions, and for cluster cases 'keylogging' is the more often correct claim; residual: a benign WndProc (e.g. a zoom-follow-cursor/hotkey handler) also clusters 3 APIs and would be re-tagged to a still-wrong keylogging claim — but as one lead instead of several, and the lead still points at the same function either way.

Addresses: real malware wrong intent claim, per calling function fanout of gui apis.

## Verification notes
---

An independent pass re-checked every mechanism, anchor, and fix against the code:

- All five fp_mechanisms verified against source; all anchors exist and behave as claimed: api_call_hits (detections.rs:1079-1149) emits one hit per (API, calling function) with no result-use check, crate::validate is absent (provenance_types.rs:1-6 header and apply_runtime_context's doc at detections.rs:962-973 both state the source project's validator/provenance walk was not ported), and CallContext::GatesBranch appears exactly once, at detections.rs:1838 inside #[cfg(test)].
- Anchor precision fixes: ApiResultCheck doc is detections.rs:238-246 (was 243-249); is_toolchain_runtime spans toolchain.rs:210-283; added detections.rs:962-990 (apply_runtime_context, the pass that attaches Runtime context and weakens) to mechanism 2's anchor; toolchain.rs:5 header quote 'MFC GUI apps import every user-activity API' verified verbatim.
- Corrected mechanism 1's severity detail: zero-xref 'imported' hits are emitted at low()=Untrusted/15 with default ContextOnly kind via DetectionHit::at (detections.rs:1099-1112), not Unsure/40 ApiResultCheck — only resolved call-site hits get Unsure/40 + ApiResultCheck. The summary's 'stamps every hit ApiResultCheck' narrowed to call-site hits.
- No already_fixed mechanisms: the later commits touching detections.rs/seed.rs (6d8f77f9, 2c30cdea, 65bbc473, 5bfd2ab5, dated 2026-10-29/30) add telemetry/provenance recording only — catalog, severities, api_call_hits, apply_runtime_context, and partition_suppressed are behaviorally unchanged; the detector file itself last changed in 9e4a6f1f (2026-10-26).
- Driver 2 amended to note Untrusted hits do roll up on fingerprint alone (only Unsure needs Runtime context).
- All six fixes verified purely static and concretely implementable: ctx.all_function_disassembly (context.rs:661, cached) and disasm_containing (context.rs:988) exist; parse_imm/reg_value_before (detections.rs:1317/1338) exist (a PUSH-imm scanner is a small new analog — reg_value_before only handles MOV/XOR today); AttackTactic::Collection already exists (detections.rs:80) so fix 6 needs only a new T1056_001 const; SM_*/WH_* constants in fixes 2-3 checked correct; fix 5's signature change at seed.rs:224 (toolchain expect) named explicitly.
- tp_risk hardening: fix 2 changed from 'drop' to 'rollup-demote' for benign indices because the true-positive rollup lines' metric indices are unknown (the CXSCREEN/CYSCREEN assumption is plausible but unverifiable); fix 4's 'low-fanout' claim marked as inference and its GatesBranch carve-out dependency on fix 1 made explicit; fix 6 gained the benign-WndProc residual (a benign 3-API WndProc would be re-tagged to a still-wrong keylogging claim, though as one lead instead of several).
- Completeness check: no clearly-missed major mechanism; the bounded-delay case is covered by mechanism 3's startup-delay clause and intentionally survives fix 1 (its result genuinely feeds branches).
