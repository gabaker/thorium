# native_api

Part of the [telemetry review](../detectors.md#what-the-telemetry-says) of the [preseed detectors](../detectors.md).

**Status (2026-10-31):** the fixes below shipped in the fleet-wide detector overhaul (see the main page's overhaul status note). Deviations: the bare-SYSCALL evidence gate keeps the page's 8-instruction look-back window (not function-wide), and the ungated per-site SYSCALL hit shape was retired in favor of gated/demoted/rolled-up forms.

native_api is two detectors in one: a 20-entry ntdll import catalog (15 medium, 5 high) that is basically sound, and a bare-opcode SYSCALL/INT-0x2E scanner that supplies most of the leads, nearly all of the disputes, and most of the ignores. The scanner matches the mnemonic with no SSN-setup, reachability, or stub-shape gate, so VMProtect-packed samples wallpaper sessions with several identical unsure/40 leads apiece, carved from overlapping exception-directory pseudo-functions; the same sites read as executed VMP VM-exit syscall gadgets to some agents and as unreachable filler to others — contested, protector-attributable noise either way, since every reading agrees there is no static SSN setup. The scanner's cleanest win sits on a different shape: a genuine Hell's Gate stub (mov r10,rcx; mov eax,[g_ssn]; syscall; ret) — and that is the reason any SSN gate must accept memory-load EAX writes rather than only immediates. The remaining false positive is a template over-claim: the NtSetInformationFile catalog entry asserts "rename/delete-on-close" without recovering FileInformationClass (a class of 4 means timestomping; the T1106 angle itself still holds). Ignores come from per-function duplication of the identical SYSCALL lead, uniform unsure/40 rendering with no artifacts, and T1106 having no consumer in derive_questions, so likely/75 injection natives are mostly ignored. An SSN-setup gate with an any-EAX-write fallback, per-function grouping with a cross-function rollup, the template fix, and question routing would cut the VMP wallpaper without touching the true-positive catalog hits or the Hell's Gate stub.

## Where the false positives come from
---

### 1. Ungated syscall opcode in vmp code

The SYSCALL branch matches the bare mnemonic with no gate: no syscall-number (EAX/RAX) setup check, no reachability check, no stub-shape check. scan_instructions feeds it every decoded instruction of every Ghidra-defined function range, and in VMProtect-packed binaries Ghidra carves overlapping pseudo-functions (zero callers, carved from the relocated exception directory / VM dispatch table) over `0f05` bytes sitting between `push imm32; call vm_entry` trampolines — each carved range yields its own unsure/40 lead. Ground truth on such sites is contested, not settled filler: they can be unreachable filler between non-returning VM-entry stubs, or executed VMP VM-exit→SYSCALL→VM-reenter gadgets with VM-staged SSNs (where the VM-entry CALL's pushed return address lands exactly on the 0f05). Every reading agrees on the operative facts: no static EAX/SSN setup exists at any site, the 'functions' are Ghidra artifacts, and the behavior belongs to the protector, not the payload author — so the ungated per-site lead over-claims and flip-flops between confirmed/disputed/ignored. The sibling detector already gates the same opcode on reg_value_before(c.preceding(8),"eax","rax")==Some(101) (debugger_evasion.rs:199), proving a gate is implementable; native_api never adopted one.

*Where:* `thorpal/src/thorpal/detect/detections/execution/native_api.rs:197-217 (ungated SYSCALL match; INT 0x2E branch at 219-229; scan_instructions driver at thorpal/src/thorpal/detect/detections.rs:1286-1310)`

### 2. Info class template overclaim

The NtSetInformationFile ApiSpec reasoning asserts a specific FileInformationClass behavior — "(rename/delete-on-close)" — as fact, but api_call_hits never recovers the class argument; build_api_hit ships spec.reasoning verbatim on every call site. When the class is 4 (FileBasicInformation) the function is timestomping (T1070.006), so the lead's interpretation is wrong even though the native-API claim itself is right. The template claims more than the heuristic checks.

*Where:* `thorpal/src/thorpal/detect/detections/execution/native_api.rs:65-70 (NtSetInformationFile ApiSpec, reasoning text at 68-69; rendered verbatim via build_api_hit at thorpal/src/thorpal/detect/detections.rs:1188-1204)`

## Why leads get ignored
---

- **Duplicate syscall flood** — One hit per SYSCALL instruction, bucketed per containing function (seed.rs:204-207), means each VMP-packed sample seeds a fixed block of near-identical "direct SYSCALL" posts per session (one per overlapping pseudo-function), all unsure/40 with byte-identical reasoning. Agents engage at most one thread and settle the siblings inside it, so the sibling leads grade ignored; sessions where no agent picks the sample up at all contribute the whole block as ignores.
- **No question routing** — derive_questions has no T1106 consumer — it derives questions only from T1027.002 unpacking, host gates, decoded config, keys, C2, T1486, and T1622/T1497 (seed.rs:1189-1209) — so every native_api lead is a statement post plus comment that nothing tasks an agent with. Even the high-severity injection natives (likely/75 NtMapViewOfSection) are never pulled into the delegation loop.
- **Uniform unsure wallpaper** — 15 of 20 catalog entries are medium(), so nearly every lead renders the same "confidence: unsure | suspicion: 40" line (only the 5 high() injection natives render likely/75); a session with several Nt*/Ldr* imports gets a spread of per-API, per-function restatements (file I/O, registry, loader) that mostly duplicate the injection/persistence story other detectors also tell, with no artifact or distinguishing signal to triage by — agents engage one native-API thread per sample and leave the rest.

## What would reduce the noise
---

Static-side changes, ordered as proposed; each was checked against the true positives above so the fix does not suppress what the detector gets right.

### Gate SYSCALL/INT 0x2E on syscall-number setup (with a mandatory any-EAX-write fallback)

In NativeApi::run's scan_instructions closure (native_api.rs:199-231), require evidence that the register the kernel reads is actually loaded. Step 1: call reg_value_before(c.preceding(8), "eax", "rax") (same helper and window debugger_evasion.rs:199 already uses); when it recovers an immediate SSN, put it in the content (e.g. "direct SYSCALL (EAX=0x3c)") so the lead is self-corroborating. Step 2 (MANDATORY, not optional): when it returns None, scan c.preceding(8) for any instruction writing eax/rax (MOV/XOR/LEA/ADD/OR with an eax/rax first operand) — this is load-bearing because a Hell's Gate stub typically loads its SSN via `MOV EAX,[g_ssn]`, a bracketed memory operand reg_value_before's parse_imm cannot parse (detections.rs:1317-1325), so an immediate-only gate would kill the detector's best true-positive shape. Step 3: when no eax/rax write exists in the window, do NOT emit a per-site unsure/40 hit — downgrade to low() and feed the cross-function rollup of the next fix (preferred over outright skipping, since the VMP-gadget capability can be worth recording). Apply the identical gate to the INT 0x2E branch (219-229), which also takes its SSN in EAX.

*Effort:* small \
*True-positive risk:* Honest accounting: this suppresses (or downgrades to one low() rollup) per-site leads on VMProtect's own VM-staged-SSN gadgets — but no static EAX setup exists there and the behavior belongs to the protector, so per-site unsure/40 leads there are coin-flips, not reliable TPs; the low() rollup keeps the capability signal. Genuine Hell's Gate stubs (`mov r10,rcx; mov eax,[g_ssn]; syscall; ret`) pass via the step-2 memory-load fallback — verify that fallback with a unit test on exactly that operand shape. The catalog patterns (NtCreateSection/NtMapViewOfSection) go through api_call_hits and are untouched. Residual risk: a stub whose SSN load sits more than 8 instructions back; widening the fallback window to the function scope covers it.

Addresses: ungated syscall opcode in vmp code, duplicate syscall flood.

### Group SYSCALL hits per function and roll up cross-function repeats

Stop emitting one hit per SYSCALL instruction: collect the scanner's sites into a captured Vec (the FnMut closure returns None), dedupe by address (overlapping Ghidra function ranges, where one pseudo-function starts inside another, can decode the same byte twice), group per containing function into one hit carrying .sites(...) exactly as api_call_hits groups API calls (detections.rs:1117-1146; render_sites cap at 1538), and when more than ~2 functions carry only bare/ungated SYSCALL hits emit a single file-level hit ("direct SYSCALL opcode in N functions — sites @ ...") instead of N posts. The seeder then produces one thread instead of several per session for VMP samples.

*Effort:* medium \
*True-positive risk:* None for the catalog patterns (different code path) and none for a single Hell's Gate stub (one function, one site — unchanged). It does collapse VMP gadget leads to ~1 per session — a volume reduction, not a signal loss, matching the engine's own api_call_hits philosophy (one behavior repeated is one finding). For real syscall-stub farms (one stub per API across dozens of functions, all passing the SSN gate) the rollup produces one stronger, site-listed lead rather than losing signal.

Addresses: duplicate syscall flood, ungated syscall opcode in vmp code.

### Stop asserting the NtSetInformationFile information class; recover it when possible

Rewrite the ApiSpec reasoning at native_api.rs:65-70 to be class-agnostic and directive: "Calls native NtSetInformationFile instead of the Win32 file APIs — depending on the FileInformationClass argument this renames (10), sets delete-on-close (13), or rewrites basic info/timestamps (4, timestomping); recover the class to tell which." Optionally (M): these hits come from api_call_hits (import xrefs), not the instruction scanner, so class recovery needs a small follow-up pass — for each resolved call site, fetch the containing function's cached disassembly (ctx.all_function_disassembly), locate the site index, and walk the preceding instructions for the 5th-argument constant (stack push on x86, the stack slot after r9 on x64); specialize the content when a known class constant (4/10/13/65) is found.

*Effort:* small \
*True-positive risk:* None: text-precision change only; the T1106 claim is retained, and a correctly-recovered class 4 would hand the agent the timestomping find directly.

Addresses: info class template overclaim.

### Promote the section-mapping pair and route high-severity natives into a derived question

Two parts. (a) In NativeApi::run after api_call_hits returns (DetectionHit's confidence/suspicion fields are pub and directly mutable), when both NtCreateSection and NtMapViewOfSection resolve call sites in the same program, raise the NtCreateSection hits to high() (NtMapViewOfSection already is high()) and note the co-occurrence in both reasonings; same for NtWriteVirtualMemory/NtCreateThreadEx/NtQueueApcThread co-occurrence. (b) In seed.rs derive_questions (1189-1209), add an injection-chain question in the style of ransomware_question (seed.rs:1577-1604) that fires when the pair co-occurrence from (a) exists — i.e. any bucket set carrying both section natives, or any T1106 hit at likely-or-better — citing those buckets via evidence_line. Do NOT gate it on ">=2 distinct functions at likely-or-better": real section-mapping pairs can sit in one function with one hit at unsure, or span two functions with only one at likely, and that trigger would miss both.

*Effort:* medium \
*True-positive risk:* None — this only promotes and routes the NtCreateSection+NtMapViewOfSection pair pattern; it suppresses nothing. The MAX_SEED_QUESTIONS=6 cap (seed.rs:51, truncate at 1207) bounds any added question-volume cost.

Addresses: no question routing, uniform unsure wallpaper.

## Verification notes
---

An independent pass re-checked every mechanism, anchor, and fix against the code:

- CORRECTED: mechanism renamed unreachable-filler-syscall -> ungated-syscall-opcode-in-vmp-code. 'Unreachable filler' is one side of a contested adjudication: the same sites can also be executed VMP VM-exit syscall gadgets. The code-level mechanism (no SSN/reachability/stub-shape gate; verified ungated at native_api.rs:203-217) holds either way.
- CORRECTED fix 1: made the any-EAX-write fallback mandatory rather than optional, and preferred low()-rollup over skip. Verified against detections.rs:1338-1359 + 1317-1325 that reg_value_before early-returns None on a `MOV EAX,[mem]` load (bracketed memory operand fails parse_imm), so an immediate-only gate would suppress Hell's Gate stubs. Also made the tp_risk honest: the gate does suppress/downgrade per-site VMP gadget leads.
- CORRECTED fix 4(b)'s trigger: the original '>=2 distinct functions carry T1106 hits at likely-or-better' misses section-mapping pairs in one function or with only one likely hit. Re-triggered on the pair co-occurrence that part (a) promotes. Also noted NtMapViewOfSection is already high(); (a) promotes NtCreateSection on co-occurrence. Verified DetectionHit's pub confidence/suspicion fields make the promotion implementable (detections.rs:468-503).
- CORRECTED fix 3 wording: NtSetInformationFile hits come from api_call_hits (import xrefs), not the scan_instructions closure, so there is no InsnCtx `c` at those sites; class recovery needs a follow-up walk over the containing function's cached disassembly. Still purely static; efforts unchanged. Also fixed the addresses reference ('info-class-over-claim' named a nonexistent mechanism).
- CORRECTED uniform-unsure-wallpaper: 15 of 20 catalog entries are medium(), not 18 (5 are high(): NtProtectVirtualMemory, NtWriteVirtualMemory, NtMapViewOfSection, NtCreateThreadEx, NtQueueApcThread — counted in native_api.rs:33-157).
- VERIFIED anchors: native_api.rs:197-217 ungated SYSCALL match and 219-229 INT 0x2E branch; scan_instructions at detections.rs:1286-1310; api_call_hits at 1079-1149 with per-function grouping at 1117-1146; build_api_hit renders spec.reasoning verbatim at 1188-1204 (NtSetInformationFile ApiSpec at native_api.rs:65-70); seed.rs:204-207 per-function bucketing; derive_questions at seed.rs:1189-1209 with exactly the 7 claimed consumers and no T1106; ransomware_question at 1577-1604; MAX_SEED_QUESTIONS=6 (seed.rs:51); debugger_evasion gate at detect/detections/defense_evasion/debugger_evasion.rs:199 (analysis omitted the defense_evasion path segment); medium()=(Unsure,40)/high()=(Likely,75) at detections.rs:216-225; render_sites at 1538.
- VERIFIED no already_fixed: native_api.rs touched only by 9e4a6f1f (2026-10-26); seed.rs/detections.rs additionally by 5bfd2ab5 (2026-10-30, telemetry/memory work); working tree equals HEAD and still contains the ungated match and no T1106 question consumer. Both mechanisms active.
