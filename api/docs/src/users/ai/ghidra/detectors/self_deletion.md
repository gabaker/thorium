# self_deletion

Part of the [telemetry review](../detectors.md#what-the-telemetry-says) of the [preseed detectors](../detectors.md).

**Status (2026-10-31):** the fixes below shipped in the fleet-wide detector overhaul (see the main page's overhaul status note). Deviation: exec sinks join the GetModuleFileName pairing only through same-function co-location (gate b) or a delete-command string anywhere in the binary (gate a), per fix 1's sink-set definition; a classic shell-mediated self-delete routine survives through its ShellExecute hit.

self_deletion is a volume detector whose API sub-pass flags seven ubiquitous Win32/libc APIs (GetModuleFileName, DeleteFile, MoveFileEx, ShellExecute/Ex, WinExec, system) once per calling function with no pairing, argument, or data-flow checks, producing a large volume of leads, most of them ignored and many of the engaged ones disputed. The dominant failure is claiming T1070.004 from a single dual-use API: GetModuleFileName (self-update, injection, RTL path helpers, config-path building), DeleteFile on other files, unvalidated MoveFileEx flags, and bare exec sinks; the seeder's runtime-noise rollup adds disputes of its own by labeling real app-code hits (including genuine %ComSpec% self-deletes) as benign-grade noise, and part of the disputed count is grading smear from co-thread disputes of other detectors' findings. The genuinely valuable part is the string sub-pass (embedded del /f /q / fsutil / rm -f one-liners, near-dispositive) plus API hits that land in actually-malicious functions. The proposed fixes — pairing/argument gates, uncalled-import collapse, flood caps, suppression-premise tightening — are all implementable with existing engine machinery (disasm_fn, reg_value_before, partition_suppressed) and would cut most of the disputed and ignored mass while keeping string leads and keeping paired-function leads visible.

## Where the false positives come from
---

### 1. Gmf unpaired dual use

GetModuleFileName is cataloged as a standalone self-deletion indicator, so every function that resolves its own module path gets a T1070.004 'File Deletion' lead. The API is radically dual-use — self-update, RunPE/injection (writing own DLL path into a target), host-process gating, building `<own dir>`\config.ini paths, anti-emulation self-open gates — and the detector requires no deletion sink anywhere: a binary whose import table contains no DeleteFile/MoveFileEx at all is still flagged. The reasoning template asserts the self-delete chain ('the value a self-delete routine feeds to DeleteFile/MoveFileEx') as if observed, which is what agents formally dispute.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/self_deletion.rs:50-55 (GetModuleFileName ApiSpec; run() at 96-133 does no cross-API correlation; per-function emission in detections.rs:1079-1149)`

### 2. Deletefile any target

DeleteFile is flagged under T1070.004 for every calling function, but malware (and installers) deletes many files that are not itself: temp output files after exfil, dropper artifacts, C2-commanded plugin uninstalls, NSIS exehead stale-temp cleanup. No static link from a GetModuleFileName result (or any own-path source) to the DeleteFile argument is ever checked, so the conditional reasoning ('when the target is the malware's own path...') fires on arbitrary file deletion.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/self_deletion.rs:44-49 (DeleteFile ApiSpec, low())`

### 3. Movefileex unvalidated args

The MoveFileEx hit (medium() = Unsure/40, the detector's loudest API) asserts the MOVEFILE_DELAY_UNTIL_REBOOT + NULL-destination self-removal trick without ever recovering the dwFlags or lpNewFileName arguments, both of which are usually static immediates (PUSH 0x1 / PUSH 0x4, xor reg,reg) the engine's existing disassembly helpers (disasm_fn, reg_value_before, parse_imm) could read. Renames with MOVEFILE_REPLACE_EXISTING and C2-driven uninstall fallbacks get the same claim.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/self_deletion.rs:37-43 (MoveFileEx ApiSpec; no call-site argument recovery anywhere in run())`

### 4. Exec sink speculation

ShellExecute, ShellExecuteEx, WinExec, and system are each flagged as 'launches the shell one-liner that deletes the malware's own file' purely because they can launch programs. No delete-command string (del, rm, fsutil, %comspec%), no GetModuleFileName pairing, and no argument shape is required, so download-and-execute handlers, UAC runas elevation loops, remote run-file C2 commands, and an esxcli service-stop pipeline all receive the T1070.004 label.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/self_deletion.rs:56-79 (four exec-sink ApiSpecs)`

### 5. Rollup mislabels app code

The seeder's runtime-noise rollup suppresses any per-function bucket whose hits are all untrusted-grade once any toolchain is fingerprinted — the hits need not actually sit in runtime code (partition_suppressed's is_noise accepts Untrusted outright) — and the post renders them under a 'recognized `<toolchain>` binary' banner as 'untrusted-grade matches on APIs and patterns that benign code also contains'. Agents dispute both directions: the fingerprint premise is sometimes wrong (MFC imported by ordinal, or no CRT linked at all), and genuinely malicious functions (including a real %ComSpec% /c del self-delete, the one thing this detector exists to find) get buried as benign-grade noise lines.

*Where:* `thorpal/src/thorpal/seed.rs:452-497 (partition_suppressed: untrusted grade alone suffices) and 511-578 (rollup title/wording)`

### 6. Dispute smearing from co thread findings

A measurement mechanism, not a detector defect: the telemetry grader attributes an agent comment's stance to the nearest detector node with ids, falling back to the root post's full technique-id set, so a post-level reply disproving one finding (a port misread, MAC-address FPs, Sleep/GetTickCount entries) marks every technique in that post disputed — including this detector's T1070.004 leads the comment never addresses. The same fallback also smears supports, inflating rollup confirms.

*Where:* `thorctl/src/handlers/ai/telemetry/fold.rs:1307-1340 (nearest-ancestor-with-ids attribution with root-post/thread-union fallback)`

### 7. Uncalled import hits

When no call site resolves, api_call_hits still emits a per-API file-level '`<API>` imported' hit. Crypter stubs carry large decoy import tables precisely to bait this (zero code xrefs), and on ELF the resolver can miss a real PLT-mediated call because references from the GOT data slot are dropped by the executable-memory filter, so the hit is wrong in both directions — claiming capability from a dead decoy, or claiming 'no call site' when one exists.

*Where:* `thorpal/src/thorpal/detect/detections.rs:1099-1113 (empty-sites fallback in api_call_hits); context.rs:833-878 (resolve_call_sites, PE-thunk-oriented; data refs dropped at 843-846)`

### 8. Stripped static library code unfiltered

The runtime-noise filter is name-based (MSVC CRT markers plus per-toolchain name markers like '@system@' for Delphi), so statically linked RTL whose functions decompile as bare FUN_xxxxxxxx — Delphi RTL ParamStr/locale/exception helpers, NSIS exehead — is never classified as library code (and the stripped names also deny the toolchain fingerprint its function-marker point, so no rollup suppression either), and its GetModuleFileName/DeleteFile calls seed full-strength per-function leads.

*Where:* `thorpal/src/thorpal/detect/detections.rs:1006-1039 + 921-960 (name-only is_library_function / runtime_classification); toolchain.rs:80 (DELPHI_FUNCTION_MARKERS match names stripped binaries do not have)`

### 9. String dual emit and xref carving

Two stacked issues on the string sub-pass: a string containing both an overwrite needle ('setzerodata') and a delete needle ('/f /q') is emitted twice, once per pass with different reasoning; and the hit's 'referenced @' sites trust Ghidra constant-propagation xrefs, which can be NUL-overrun copy-loop artifacts of adjacent strings rather than real uses of the delete command.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/self_deletion.rs:100-127 (two independent string_artifact_hits passes over overlapping catalogs); detections.rs:1385-1452 (xrefs taken as-is)`

## Why leads get ignored
---

- **Per function ubiquitous API flood** — One lead per calling function for seven of the most common APIs in Windows software means up to dozens of near-identical untrusted/15 leads per sample (multi-sample sessions re-run the whole catalog per sibling). GetModuleFileName and DeleteFile together account for most of the ignored leads. Agents cannot and do not chase every 'this function calls DeleteFile' comment.
- **Rollup line mass** — A large share of the ignored volume is lines inside the single runtime-noise rollup post — suppression working as designed (the flood is contained in one post) but each itemized line still counts as a lead, and unrated rollup lines are engaged almost never. The detector's untrusted-grade output is exactly what the rollup swallows, so most of its hits are born pre-buried whenever a toolchain fingerprints.
- **Untrusted grade deprioritization** — Most finding-shaped untrusted (susp 15) leads are ignored: the comment itself tells agents the signal grade is the ubiquitous also-in-benign-code floor, and the detector emits almost everything there (every API spec except MoveFileEx is low() in self_deletion.rs:36-80). The grade differential is real but modest, so this is a secondary driver on top of the flood: medium-grade (unsure/40) leads are ignored only slightly less often.
- **Uncalled import filler** — Per-API 'imported (no call site resolved)' file-level hits carry no address, no function, and no action for an agent — pure filler in the file-level thread, inflated further by decoy import tables.

## What would reduce the noise
---

Static-side changes, ordered as proposed; each was checked against the true positives so the fix does not suppress what the detector gets right.

### Demote GetModuleFileName to a pairing corroborator (two-tier gate)

In SelfDeletion::run (self_deletion.rs:96-133), post-process the api_call_hits output: build the set of functions holding a deletion-sink hit (DeleteFile/MoveFileEx call, or a DELETE_CMD/OVERWRITE string xref — all available in the same run() from the three sub-passes; API hits are distinguishable by running api_call_hits per spec or by the content prefix). Keep a GetModuleFileName hit at low() only when its function is in that set, appending '— paired with `<sink>` in this function'; when the binary imports a deletion sink but no same-function pair exists, collapse all unpaired GMF hits into one combined file-level line (or route them to the rollup via the fix-5 extension of partition_suppressed); when the binary imports neither DeleteFile/MoveFileEx nor any exec sink, emit nothing for GMF; with exec sinks only, keep one combined weak line (shell-mediated self-delete needs no deletion import). Reword the GMF reasoning to conditional ('own-path lookup; self-deletion only if fed to a deletion sink').

*Effort:* medium \
*True-positive risk:* Classic self-delete routines pair GetModuleFileName with a ShellExecute or deletion sink in the same function, so they survive tier 1; unpaired-but-sink-imported true positives degrade to a combined/rollup line rather than vanish (rollup-shaped GMF leads do get engaged, though partly through post-level-attribution smears, see notes). Lost outright only when no deletion sink and no exec sink is imported, where only hash-resolved deletion remains — invisible to this import-driven detector either way.

Addresses: gmf unpaired dual use, per function ubiquitous api flood, stripped static library code unfiltered.

### Statically validate MoveFileEx flags and destination at each call site

For each MoveFileEx call site, read the cached containing-function disassembly (AnalysisContext::disasm_fn, context.rs:922; windowing like InsnCtx::preceding, detections.rs:1252) and recover dwFlags (x86 stdcall: 3rd PUSH imm walking back from the CALL — the PUSH 0x1/PUSH EDX/PUSH EAX/CALL idiom; x64: last r8d write via reg_value_before, detections.rs:1338, plus parse_imm) and lpNewFileName (PUSH 0 / xor reg,reg). Keep medium() only when flags & MOVEFILE_DELAY_UNTIL_REBOOT (0x4) and the destination is NULL; when both arguments are recoverable and do not match, drop the hit (it is a rename); when not statically recoverable, emit at low() with hedged wording ('flags not statically recoverable').

*Effort:* medium \
*True-positive risk:* True delay-until-reboot sites keep medium (flags are immediates in that idiom); unresolvable sites degrade to low but are not dropped. Self-update routines that delay-delete their own renamed previous version stay flagged (flags=4, NULL dest). A C2 plugin-uninstall fallback can also use flags=4+NULL on another file, so this fix validates the trick but not self-targeting — reword the claim to 'schedules a file for reboot deletion' to stop asserting self-removal.

Addresses: movefileex unvalidated args.

### Require co-evidence before flagging exec sinks under T1070.004

In SelfDeletion::run, keep ShellExecute/ShellExecuteEx/WinExec/system hits only when (a) a DELETE_CMD_STRINGS/OVERWRITE_STRINGS hit exists in the binary, or (b) the calling function also calls GetModuleFileName; otherwise drop them from this detector. Note the drop is a deliberate scope cut: no other registered detector flags these APIs today (execution/ holds only native_api.rs), which is acceptable because a bare exec API is not deletion evidence. When only binary-level evidence (a) holds, keep but note 'delete command string present elsewhere in the binary'; additionally accept GMF-anywhere + exec-sink at combined/rollup grade to cover stack-built delete commands.

*Effort:* medium \
*True-positive risk:* A %ComSpec% stack-string self-delete survives gate (b) when its function pairs GetModuleFileName with ShellExecute. Residual risk is a stack-built delete command launched from a function with no GMF (path received as argument) in a binary with no embedded delete string — mitigated by the GMF-anywhere rollup-grade acceptance; WinExec/ShellExecute true positives in binaries with GMF and DeleteFile activity satisfy the gates.

Addresses: exec sink speculation, per function ubiquitous api flood.

### Collapse imported-but-uncalled APIs into one combined line and fix ELF PLT resolution

In api_call_hits (detections.rs:1099-1113) add a mode (or filter in SelfDeletion::run on the address==0 hits) that accumulates uncalled matches and emits a single file-level hit 'self-deletion-related APIs imported but never called: GetModuleFileName, DeleteFile, …' at low(), explicitly naming decoy-import tables as the common benign cause. Separately, extend resolve_call_sites (context.rs:833-878): when an import's references all come from data (the ELF GOT slot, currently dropped by the executable-memory filter at 843-846), follow xrefs to that slot from executable code and treat the referencing .plt-resident function like a PE thunk (hop to its callers via the existing is_thunk/callers machinery, without requiring the single-JMP body shape standard PLT entries lack).

*Effort:* medium \
*True-positive risk:* None: 'imported' hits are not where true positives come from. The combined line preserves the information at a fraction of the volume, and PLT resolution strictly improves accuracy (an ELF system() call becomes a located call-site hit, which fix 3 then correctly drops from this detector).

Addresses: uncalled import hits, uncalled import filler.

### Cap per-binary finding-shaped untrusted leads for this detector

After the pairing gates above, cap SelfDeletion's remaining low()/untrusted per-function hits at N (e.g. 6) per binary, ranked by suspicion then site count, and route the overflow into the existing runtime-noise/low-signal rollup path by extending partition_suppressed (seed.rs:452-497) to accept detector-tagged overflow buckets even when no toolchain fingerprinted (hits carry detection names via stamp_detection, detections.rs:858), rendered under a neutral 'low-signal detector findings collapsed' title when toolchain is None.

*Effort:* medium \
*True-positive risk:* Some finding-shaped untrusted true positives would seed as rollup lines instead of per-function comments; rollup lines still get engaged (though partly through post-level attribution smears, so the cushion is thinner than it looks). The paired, highest-suspicion hits where true positives concentrate stay as full threads.

Addresses: per function ubiquitous api flood, untrusted grade deprioritization, rollup line mass.

### Dedupe dual-catalog string hits and verify direct string references

In SelfDeletion::run, run the OVERWRITE_STRINGS pass first, record each matched string's hit address/site set, and skip those strings in the DELETE_CMD_STRINGS pass (one hit per string, overwrite reasoning wins) — trivially done by deduping the two passes' hits on primary address. Additionally, for matched strings, disassemble the referencing sites (cached disasm_fn + op_str) and check whether any referencing instruction encodes the string's base address as an immediate; when every reference is computed/indexed off a different base (the NUL-overrun copy idiom, where a MOV off an adjacent string's base never encodes the flagged string's address), weaken() the hit and append '(indirect/computed references only)'.

*Effort:* medium \
*True-positive risk:* None for dedupe. The reference check keeps real string leads ('/c timeout /t 5 & del /f /q', vssadmin, 'rm -f' one-liners) since real uses load the string base directly; the weakened-not-dropped policy preserves the LockBit-style case where the command string genuinely exists even though the recorded xrefs are constant-propagation artifacts.

Addresses: string dual emit and xref carving.

### Tighten the runtime-noise suppression premise and wording

In seed.rs partition_suppressed (452-497) / seed_suppressed_rollup (511-578): only enable toolchain-based suppression when at least one suppressed-candidate hit was actually classified CallContext::Runtime (proof the fingerprint maps to real runtime functions — blocks the MFC-by-ordinal and no-CRT false premises), and reword the body so buckets suppressed purely for untrusted grade are labeled 'weak-signal, unverified' instead of sitting under the 'recognized `<toolchain>` binary' noise banner. Keep self_deletion buckets containing a same-function GMF+sink pair out of suppression entirely (checkable in seed.rs from hit.detection plus content prefixes within one bucket).

*Effort:* small \
*True-positive risk:* None — suppression only moves where hits render, and the gate makes suppression rarer. A real self-delete whose bucket pairs GetModuleFileName with ShellExecute in the same function stays a full thread.

Addresses: rollup mislabels app code.

### Attribute post-level stances only to findings the comment addresses (telemetry)

In thorctl/src/handlers/ai/telemetry/fold.rs:1307-1340 (stance attribution): when a stance's target comes from the root-post Techniques-line or thread-union fallback rather than a nearest detector finding comment, record disproves/supports only for technique ids (or finding API tokens) the comment body actually names, downgrading the rest to engagement-only (neutral). Purely a measurement fix in engine telemetry code — it changes grading, not detection.

*Effort:* small \
*True-positive risk:* Symmetric by design: it also deflates smeared supports (rollup lines today benefit from one post-level supporting comment confirming every same-technique line); no detector true positive is suppressed because no seeding behavior changes.

Addresses: dispute smearing from co thread findings.

## Verification notes
---

- All code anchors verified line-exact at HEAD: self_deletion.rs ApiSpecs 37-79 / run() 96-133 / string passes 100-127; detections.rs api_call_hits 1079-1149 (empty-sites fallback 1099-1113), string_artifact_hits 1385-1452, is_library_function 1006-1039, runtime_classification 921-960, low()/medium() = (Untrusted,15)/(Unsure,40) at 216-232; seed.rs partition_suppressed 452-497 (Untrusted grade alone suffices once any toolchain fingerprints) and rollup wording 511-578; context.rs resolve_call_sites 833-878 (drops data refs at 843-846, PE-thunk-oriented); toolchain.rs:80 DELPHI_FUNCTION_MARKERS.
- No mechanism is already_fixed: the detector file has a single commit (9e4a6f1f, 2026-10-26); the only later commit touching detections.rs/seed.rs (5bfd2ab5, 2026-10-30) adds telemetry provenance recording only — diff shows no behavioral change to suppression or api_call_hits.
- Dispute-smearing-from-co-thread-findings is anchored at the production grader (thorctl/src/handlers/ai/telemetry/fold.rs:1307-1340), which the analysis extraction mirrors; its telemetry fix has a symmetric effect on rollup confirms (one post-level support confirms every same-technique rollup line).
- Untrusted-grade deprioritization is a secondary driver (the floor grade plausibly deprioritizes); the flood is the dominant driver.
- Fix 1 adjusted: 'self-deletion via imports is impossible anyway' was wrong — shell-mediated self-delete (exec sink + runtime-built del command) needs no DeleteFile/MoveFileEx import; the no-sink tier emits a combined weak line when exec sinks are imported. Routing GMF into the rollup requires fix 5's partition_suppressed extension (today it needs a fingerprinted toolchain). Stripped-static-library-code-unfiltered is addressed by fix 1: Delphi-RTL hits are unpaired GMF hits the gate removes.
- Fix 3 corrected: '(generic execution is other detectors' territory)' is false — no other registered detector flags ShellExecute/WinExec/system (execution/ contains only native_api.rs); reworded as a deliberate scope cut.
- Fix 2's claimed machinery verified to exist: reg_value_before (detections.rs:1338, handles mov-imm and xor-zero), parse_imm (1317), InsnCtx::preceding (1252), scan_instructions (1286), cached disasm_fn (context.rs:922). Fix 6's immediate-encoding check implementable via disasm of referencing sites + op_str; fix 5/7's detector-tagged routing possible because stamp_detection (detections.rs:858) writes hit.detection.
- All fixes retained (each names the function/catalog to touch); the degradations they cause are stated in their true-positive risks (unpaired GMF to combined/rollup lines, unresolvable MoveFileEx to hedged low).
