# sandbox_system_checks

Part of the [telemetry review](../detectors.md#what-the-telemetry-says) of the [preseed detectors](../detectors.md).

**Status (2026-10-31):** the fixes below shipped in the fleet-wide detector overhaul (see the main page's overhaul status note). Deviations: the four survey APIs were retired from per-function findings entirely (one file-level inventory line replaces them), and VM_STRINGS kept bare-substring matching with only the base64-run veto — letter-bounded matching would have stopped the camel-case artifacts (VBoxTrayToolWnd, vmtoolsdControlWndClass) from matching.

sandbox_system_checks is one of the fleet's worst precision offenders. The root cause is architectural and verified in code: the APIS catalog flags bare call sites of dual-use discovery APIs, and the EvidenceKind::ApiResultCheck contract ("result feeds a comparison/branch") has no consumer in this port (no validate pass exists; the only hit.kind reader is seed.rs gate_questions, ControlFlowGated only), so every lead asserts a compare-against-sandbox-values story that reverse engineers disprove — the actual uses are injection-target selection, victim-ID/C2-profile building, and recon exfil. The Toolhelp trio and the hostname/username/adapter/survey reads account for most of the false positives, and merging the trio per function removes a large share of the duplicate leads. The salvageable core is the string sub-pass — its true positives are genuine VM artifacts ("vbox", "vmware", "qemu", VBoxTray/VMware window classes, "SbieDll.dll", "VBoxService.exe") — while the leaf-gated CPUID pass emitted no false positives but also drew no engagement. A corroborating-tell gate, trio merge, survey-API retirement, string-shape vetoes, and hypothesis-form wording would remove most of the false-positive and ignored leads without suppressing any true-positive pattern.

## Where the false positives come from
---

### 1. Toolhelp walk assumed VM hunt

The catalog flags every CreateToolhelp32Snapshot/Process32First/Process32Next call site and asserts it 'searches for analysis processes (vmtoolsd, vboxservice, wireshark, procmon) and bails'. Process-list walking is the most dual-use malware idiom: in the false-positive cases the loop was injection-target selection (svchost/explorer/winlogon), C2 process-inventory (T1057), token theft/session hop, POS-scraper targeting, thread quiescing, or dead donor code. The detector never checks the discriminator agents use every time — whether any VM/analysis-tool process-name string exists in the binary — and no validator confirms a name comparison: EvidenceKind::ApiResultCheck is attached at sandbox_system_checks.rs:273 but nothing in this port reads it (the only hit.kind consumer is seed.rs:1378 gate_questions, which handles ControlFlowGated only; the 'crate::validate' pass the enum doc names does not exist here).

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/sandbox_system_checks.rs:88-105 (trio specs), detections.rs:1079-1148 (api_call_hits fires on bare call sites), detections.rs:243-255 (ApiResultCheck contract, consumer-less)`

### 2. Fingerprint read assumed sandbox compare

GetComputerName/GetUserName/GetAdaptersInfo/GetAdaptersAddresses (medium) and GetSystemInfo/GetNativeSystemInfo/GlobalMemoryStatusEx/GetDiskFreeSpaceEx (low) are flagged on call-site presence with reasoning asserting the result is compared against known sandbox values. In the false positives the values are read to BUILD something — CRC32/ROR13 victim IDs, MAC-derived bot IDs, 'eclient_`<MAC>`' C2 fields, ipconfig-style recon reports, subnet target lists, drive-survey command replies — and never compared or branched on. The engine has no dataflow/branch gate (no validate pass consumes ApiResultCheck), and apply_runtime_context (detections.rs:979-991) only reclassifies hits inside named CRT/toolchain library functions, never malware-body functions.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/sandbox_system_checks.rs:34-81 (API specs), detections.rs:1057-1148 + 243-255 (ApiResultCheck contract unimplemented), detections.rs:979-991 (apply_runtime_context library-only)`

### 3. Own handle and junk API uses

FindWindow and SetupDiGetClassDevs hits where the call targets the sample's own resources or is crypter junk-API padding: FindWindowA("ConsoleWindowClass") to hide/draw on its own console, SetupDiGetClassDevs on GUID_DEVINTERFACE_USBPRINT or a garbage stack GUID in machine-generated API-salad stubs whose results are all dead. The catalog cannot see the argument (window class / GUID) or that the result is discarded.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/sandbox_system_checks.rs:82-87, 106-111`

### 4. Benign string coincidence

The string sub-pass substring-matches VM_STRINGS/LINUX_VM_STRINGS against defined strings with empty excludes and no shape check (string_artifact_hits is called with excludes=&[] at sandbox_system_checks.rs:228/242/254/268): 'vbox' matches the ransomware encryption-target extension '.vbox' (listed beside .vhd/.vmdk/.vdi and dozens of document extensions), and '/proc/cpuinfo' matches statically-linked libc's get_nprocs() present in every static Linux binary.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/sandbox_system_checks.rs:121-142 (VM_STRINGS) and :158 (LINUX_VM_STRINGS), matched by detections.rs:1398-1407 (hay.contains(needle) at 1402, excludes empty)`

### 5. Template asserts unverified intent

Cross-cutting (overlaps the mechanisms above): the ApiSpec reasoning strings are declarative intent claims ('Reads the machine name ... to compare against known sandbox hostnames ... and evade if matched') rendered verbatim into the seeded comment (comment_body at seed.rs:708-756 emits 'Reasoning: {hit.reasoning}'). Because the lead claims the specific evasion story rather than posing the dual-use question, any agent who traces the dataflow can post 'Disproved' — converting correct observations (the API is called) into graded false positives.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/sandbox_system_checks.rs:33-118 (APIS reasoning templates) + thorpal/src/thorpal/seed.rs:708-756 (comment_body renders them as asserted statements)`

### 6. Per loop triple emission

Cross-cutting volume multiplier (overlaps toolhelp-walk-assumed-vm-hunt): api_call_hits emits one hit per (API, calling function) and additionally splits thunked vs direct call sites, so a single process-enumeration loop yields 2-6 near-identical leads on the same function that are each disputed (and ignored) separately.

*Where:* `thorpal/src/thorpal/detect/detections.rs:1114-1146 (per-API BTreeMap grouping, thunked/direct split; no cross-API merge of the Toolhelp trio)`

## Why leads get ignored
---

- **Universal API flood** — The catalog APIs (process walking, hostname/username, adapters, FindWindow) are near-universal in Windows malware for non-evasion reasons, so almost every session gets a stack of identical-boilerplate unsure/40 leads. Agents triage by novelty and skip the repeats, with the Toolhelp trio and GetComputerName/GetUserName leads making up most of what is ignored.
- **Import only stub leads** — When an import resolves no call site, api_call_hits (detections.rs:1099-1112) still emits an untrusted 'X imported' lead anchored at 0x0. These are pure padding.
- **Low grade survey leads** — The four low() survey APIs (GetSystemInfo/GetNativeSystemInfo/GlobalMemoryStatusEx/GetDiskFreeSpaceEx) emit untrusted/15 leads that agents correctly treat as noise. partition_suppressed (seed.rs:452-497) only rolls them up when a toolchain was fingerprinted and the whole per-function bucket is noise, so many still land as individual finding comments.
- **Rollup residue** — Runtime-noise rollup lines (shape=rollup) are counted as leads and almost all ignored — but that is the rollup doing its job (one cheap post per session recording suppressed CRT-located and untrusted-grade hits), not a defect. The occasional disputed rollup attacks the rollup's own framing (a toolchain misfingerprint, suppressed entries that were real), a tiny residual defect in suppression rather than in this detector.
- **Corpus repeat reseeding** — The same sample re-analyzed across sessions re-seeds byte-identical leads every time (no cross-session memory of past dispositions), so a once-disproved lead keeps reappearing and gets ignored or re-disputed. Multi-sample sessions multiply this within one session.

## What would reduce the noise
---

Static-side changes, ordered as proposed; each was checked against the true positives above so the fix does not suppress what the detector gets right.

### Corroborating-tell gate: demote medium API claims when the binary shows no VM/analysis-tool artifact

In SandboxSystemChecks::run (sandbox_system_checks.rs:174-276), run the string/CPUID sub-passes first and compute has_tell = any VM_STRINGS/VM_REGISTRY_STRINGS/MAC_PREFIX_STRINGS match, any CPUID hypervisor probe, or any analysis-tool name from anti_instrumentation's TOOL_STRINGS (anti_instrumentation.rs:42-55; export it or move it to indicators.rs) found in ctx.strings (a public field, context.rs:342). When has_tell is false, demote the medium() API specs (Toolhelp trio, GetComputerName, GetUserName, GetAdaptersInfo/Addresses, SetupDiGetClassDevs, FindWindow) to low() — via a parallel no-tell spec catalog or by post-processing api_call_hits output — and swap in dual-use reasoning noting 'no sandbox/VM/analysis-tool name strings exist in this binary; process/host reads here are more often target selection or victim fingerprinting'. Keep medium + current claim when a tell exists.

*Effort:* medium \
*True-positive risk:* The string true positives are themselves tells, so their leads keep medium. FindWindow and GetUserName true positives that co-occur with VM tell strings in the same binary (VBoxTrayToolWnd/vmtoolsdControlWndClass; SbieDll.dll/VBoxService.exe) are kept by the gate. At risk of demotion if their samples lack tells: CreateToolhelp32Snapshot, GetComputerName, Process32First/Next, and FindWindow leads (FindWindow window-scanners are likely kept via TOOL_STRINGS window names); demote-not-drop keeps all of these visible. Residual miss: samples comparing hashed/runtime-decrypted process names are demoted, not dropped.

Addresses: toolhelp walk assumed vm hunt, fingerprint read assumed sandbox compare, template asserts unverified intent.

### Merge the Toolhelp trio (and thunk/direct splits) into one process-walk lead per function

Post-process api_call_hits output in SandboxSystemChecks::run: group hits whose spec key is CreateToolhelp32Snapshot/Process32First/Process32Next by hit.function, merge each group into one DetectionHit ('process-list walk (CreateToolhelp32Snapshot + Process32First/Next) — N sites @ …') with the union of sites, keeping the max severity; alternatively add an optional merge_group field to ApiSpec (detections.rs:871-880) and do the grouping generically at detections.rs:1114-1146, also folding thunked and direct sites of one API in one function together.

*Effort:* small \
*True-positive risk:* None identified: no true-positive pattern depends on the trio being separate leads — a file-level Process32First + Process32Next pair (function=None) would merge into one still-confirmable lead, and snapshot leads on Thread32 walks have nothing else merging into them; sites are preserved for the markup pass.

Addresses: per loop triple emission, universal api flood.

### Retire the four survey APIs (GetSystemInfo, GetNativeSystemInfo, GlobalMemoryStatusEx, GetDiskFreeSpaceEx) from standalone leads

Remove the four low() entries from APIS (sandbox_system_checks.rs:34-57), or emit them only when the corroborating-tell gate passes (still at low()). The genuinely gating RAM/core checks resolve these APIs dynamically (invisible to the import catalog anyway).

*Effort:* small \
*True-positive risk:* Survey-API leads that looked like true positives either did not hold up under decompilation (the memory size written into an encrypted C2 packet, never branched on) or were rollup lines — suppressing these loses nothing demonstrated real. True RAM-threshold gates use dynamic resolution the catalog already misses.

Addresses: low grade survey leads, fingerprint read assumed sandbox compare.

### Validate string matches: extension-shape veto for VM_STRINGS and pair /proc/cpuinfo with a hypervisor tell

In SandboxSystemChecks::run's string sub-pass: (a) skip or re-tag (to a T1486 extension-target note at low()) any VM_STRINGS match whose whole defined string is a bare dot-prefixed extension (regex ^\.[a-z0-9]{1,6}$ — '.vbox', '.vmdk' style) — implement by pre-filtering ctx.strings or by adding a shape-veto parameter to string_artifact_hits (detections.rs:1385); (b) drop "/proc/cpuinfo" from LINUX_VM_STRINGS (sandbox_system_checks.rs:158) or emit it only when the binary also defines "hypervisor" or "/sys/class/dmi" — every statically-linked libc contains it via get_nprocs().

*Effort:* small \
*True-positive risk:* Verified against the true-positive string leads ('vbox', 'vmware', 'qemu', 'vmtoolsdControlWndClass', 'SOFTWARE\\VMware, Inc.', 'VMwareDragDetWndClass', 'HARDWARE\\ACPI\\DSDT\\VBOX__', 'VBoxTrayToolWnd(Class)', 'VMwareTrayIcon', 'SbieDll.dll', 'VBoxService.exe'): none is a bare dot-prefixed extension, so every string true positive survives the veto. A genuine /proc/cpuinfo hypervisor-flag check necessarily references the 'hypervisor' needle too, so the pairing keeps it.

Addresses: benign string coincidence.

### Reword APIS reasoning templates from asserted intent to dual-use hypothesis with a verification cue

Rewrite the reasoning strings in APIS (sandbox_system_checks.rs:33-118) to state the observation plus both interpretations and the discriminator, e.g. for GetComputerName: 'Reads the machine name. Dual-use: sandbox checks compare it against known sandbox hostnames; at least as often malware embeds it in a victim ID/C2 beacon. Treat as evasion only if the result feeds a comparison that gates control flow.' Same pattern for the Toolhelp trio (name-compare target selection vs analysis-process hunt) and the rest. Pure const-string change; comment_body (seed.rs:708-756) renders whatever the template says.

*Effort:* small \
*True-positive risk:* None: true-positive leads' claims remain valid under hypothesis wording (an agent confirming a real sandbox check still confirms); the change only stops agents from having to grade correct observations as 'Disproved' because the attached story was wrong.

Addresses: template asserts unverified intent, fingerprint read assumed sandbox compare, toolhelp walk assumed vm hunt.

### Stop emitting import-only ('X imported', site 0x0) leads for this detector

Gate the no-call-site fallback in api_call_hits (detections.rs:1099-1112) behind a new parameter or ApiSpec flag (emit_import_only), and set it false for SandboxSystemChecks — or filter |hit| hit.address == 0 out of this detector's api_call_hits result in run() (import-only stubs are the only address-0 hits this detector produces). If any signal is wanted, let the seeder fold them into the existing runtime-noise rollup instead of individual finding comments.

*Effort:* small \
*True-positive risk:* No true-positive lead has the 'imported' shape (file-level true positives have resolved sites, not 0x0); packed binaries where the IAT is the only visible evidence are covered by the separate packing detector, so dropping these never-engaged untrusted stubs costs nothing observed.

Addresses: import only stub leads, universal api flood.

## Verification notes
---

An independent pass re-checked every mechanism, anchor, and fix against the code:

- Confirmed the central architectural claim by code reading: EvidenceKind::ApiResultCheck (detections.rs:243-255, attached at sandbox_system_checks.rs:273) has no consumer in this port — no validate module exists in thorpal/src/thorpal/detect/, the enum doc's 'crate::validate' is a tombstone from the source project, and the only hit.kind reader is seed.rs:1378 (gate_questions, ControlFlowGated only); the two other ApiResultCheck references are unit tests.
- Mechanism 5's code anchor spans the whole APIS catalog at sandbox_system_checks.rs:33-118 (the wine_get_unix_file_name entry at 112-117 is also a declarative template). All other anchors verified exact: trio specs 88-105, survey specs 34-57, SetupDi/FindWindow 82-87/106-111, VM_STRINGS 121-142, LINUX_VM_STRINGS 158, run() 174-276, api_call_hits 1079-1148 with grouping at 1114-1146 and import fallback at 1099-1112, apply_runtime_context 979-991, string matching contains/excludes at detections.rs:1402/1405 with excludes=&[] passed at detector lines 228/242/254/268, comment_body seed.rs:708-756, partition_suppressed seed.rs:452-497, ApiSpec 871-880.
- already_fixed check: git log shows sandbox_system_checks.rs has a single commit (9e4a6f1f, 2026-10-26); the later 5bfd2ab5 (2026-10-30) only added telemetry provenance plumbing (HitSource/ArtifactClass keys, detection stamping) with no heuristic change — all mechanisms correctly marked active, none already_fixed.
- The rollup-residue disputes (a toolchain misfingerprint — MFC dynamically linked, not static; a suppression that hid real self-delete/UAC entries) are noted inside the rollup-residue driver rather than added as a mechanism since they are a seeder/toolchain issue, not a detector heuristic. Dead/unreachable-code flagging (donor code, zero-caller utilities) appears in some mechanism-1 false positives but is color on the same no-discriminator defect, not a separate major mechanism.
- No fixes were rejected: all six are purely static, name real functions/catalogs/lines, and are implementable where claimed (ctx.strings is a public AnalysisContext field at context.rs:342; TOOL_STRINGS is a private const at anti_instrumentation.rs:42-55 so fix 1's 'export or share via indicators.rs' is required and accurate).
