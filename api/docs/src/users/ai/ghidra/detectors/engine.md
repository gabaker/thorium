# Cross-cutting engine measures

Part of the [telemetry review](../detectors.md#what-the-telemetry-says) of the [preseed detectors](../detectors.md). The sixteen per-detector pages repeatedly trace their false positives and ignore floods to the same shared machinery — the API/string catalog helpers, the instruction scanner, the seeder's bucketing and rollup rules, and the library-recognition pass. This page collects the engine-level measures that fix those causes centrally.

**Status (2026-10-31):** the measures below shipped in the fleet-wide detector overhaul, with three partial items: the PacesLoop/SeedsRng provenance wiring (measure 6's last clause) is still unwired; the MSVC fail-fast structural recognizer (measure 9) is live but reads only catalogued-API call maps, so it engages fully once a richer per-function import map is supplied; and MoveFileEx validation recovers dwFlags but not yet a NULL destination. The admission policy's budgets shipped as DETECTOR_FINDING_BUDGET=10 (encoded_strings/network_comms at 15), with verified-use and validated-artifact leads exempt.

ThorPal's seeding engine floods the forum because its shared mechanics admit everything a detector emits: api_call_hits (detections.rs:1079-1149) posts presence-only dual-use API sightings at catalog-asserted severity with zero argument or result-use recovery (the EvidenceKind::ApiResultCheck/ControlFlowGated validation contract at detections.rs:243-255 was never given its validator — no validation pass exists in the ported engine; the kind's only runtime consumer is the seeder's host-gate question filter at seed.rs:1378, and CallContext::GatesBranch is constructed only in a test at detections.rs:1838); scan_instructions (detections.rs:1286-1310) collects one hit per instruction with no grouping or caps; the seeder (seed.rs:204-245) creates one post per function bucket and one comment per hit with no per-detector or per-session budget; suppression (partition_suppressed, seed.rs:452-497) is disabled entirely without a toolchain fingerprint, exempts the shared file-level bucket, and is vetoed by any pivot-class artifact — including the fabricated carved MACs and junk 'recovered' values that drive the artifact disputes; and library recognition is name-only (is_library_function detections.rs:1006-1039 is MSVC/CRT markers; the fingerprinted-toolchain markers at toolchain.rs:210-283 cover Go/Rust/MFC/Delphi but require a two-point fingerprint, and runtime_classification bails without a function name at detections.rs:946), so stripped binaries get no downgrade at all. Most leads are never engaged, and engaged leads are mostly disputed; confidence is inverted (the Likely and Fact tiers are disputed more often than Unsure leads) yet the q-evasion-gauntlet question keys on >=Likely (seed.rs:1619); artifact-carrying leads are disputed more often than leads without. The decisive asymmetry is shape: finding-shape leads are disputed far more often than confirmed, while rollup lines are confirmed more often than disputed — so the safe cross-cutting lever is demote-to-rollup, never silent drop, combined with grouping, budgets, validation-gated artifacts, and recalibrated severity, with full posting volume reserved for the detectors and evidence classes that measurably earn engagement.

## The measures
---

### 1. Group instruction-scan tells per function and cap generic tells per program

Add a shared post-pass over scan_instructions output (detections.rs:1286-1310) that merges same-detection, same-content-class hits within one function into a single hit carrying the full sites list (the engine already does exactly this for APIs in api_call_hits:1117-1146, literals in referenced_literal_hit:1497-1525, and byte sigs in build_signature_hit), plus a per-program cap on unclassified tells (generic PEB reads, 0xCC compares, bare RDTSC/SYSCALL) with the overflow summarized in one line. Apply it centrally in run_all_with_roster (detections.rs:798) so every per-instruction detector inherits it.

*Why:* debugger_evasion's per-instruction PEB flood dominates its ignores; anti_instrumentation's 0xCC displacement-byte flood is its main FP mechanism and per-instruction-cc-flood ignore driver; native_api's duplicate-syscall-flood seeded byte-identical VMProtect-gadget SYSCALL leads every session a VMP sample appeared. Useful leads are overwhelmingly single-site-per-function; the engine's own doc says ten calls is one behavior repeated (detections.rs:1062, 1114-1116).

*Effort:* medium \
*True-positive risk:* Near zero: per-site true positives (classified PEB reads, Hell's Gate stubs) stay one-per-function with sites listed. Residual: the cap must exempt classified/argument-verified tells so a flooded session still seeds its few strong reads individually.

### 2. Per-detector per-sample admission budget with ranked overflow into a census rollup

In seed_forum_from_detectors (seed.rs:199-245), after apply_runtime_context (:201) and before bucketing (:204), rank each detector's hits (confidence_rank desc then suspicion desc — exactly the per-bucket sort that already exists at seed.rs:210-216 — extended with validated-artifact-first) and keep only the top K (~10-15 for low-tier detectors) as finding-shape comments; route the remainder into a per-detector census post reusing the seed_suppressed_rollup shape (seed.rs:511-578, itemized to MAX_ROLLUP_LINES=40, provenance to 500). Today there is no cap of any kind on buckets, posts, or comments.

*Why:* encoded_strings, network_comms, and debugger_evasion seed hundreds of leads in their worst sessions, and most leads are never engaged. Rollup-shape lines earn confirms far more reliably than finding-shape leads (in sandbox_time_evasion the rollup was the detector's best shape) — so overflow-to-census preserves the engagement channel. Confirmed threads per fired session for the flood-prone detectors sit far under any sane K.

*Effort:* medium \
*True-positive risk:* Low: true positives concentrate at the top of exactly the ranking used for admission; everything demoted stays itemized and re-raisable. Risk case is a true positive ranked low by a miscalibrated catalog — mitigated by landing the severity recalibration first and by artifact-validated hits bypassing the budget.

### 3. Let all-Untrusted, artifact-less buckets roll up without a toolchain fingerprint

In partition_suppressed (seed.rs:452-497), split the two suppression criteria: keep the Unsure+Runtime-context criterion fingerprint-gated (it attributes noise to a specific runtime), but suppress buckets whose every hit is Untrusted and artifact-less regardless of toolchain (seed.rs:457-459 currently returns early when toolchain.is_none(), so stripped binaries — where name-based recognition is blind and floods are worst — get zero suppression). The rollup call site goes with it: seed_forum_from_detectors unwraps ctx.toolchain with an expect at seed.rs:224 and seed_suppressed_rollup takes a mandatory Toolchain, so both must accept the no-fingerprint case, and the rollup body must then stop asserting a recognized runtime for that class (see the wording measure).

*Why:* Untrusted finding-shape leads are disputed far more often than confirmed. The toolchain-fingerprint gate recurs across the per-detector reviews: 'unsure-grade-exempt-from-rollup-in-stripped-binaries' (sandbox_user_activity) and 'rollup-gated-on-toolchain-fingerprint' (crypto_capability) are named ignore drivers, anti_instrumentation's 'rollup-suppressed-by-design' driver states the same gate, and ransomware_behavior's stripped-Delphi mechanism shows a binary that never fingerprints and so never suppresses. File-level buckets stay exempt (seed.rs:488-489), so file-level string leads such as anti_instrumentation's WIRESHARK match are structurally safe.

*Effort:* small \
*True-positive risk:* Low: suppression is demotion into an itemized rollup, not deletion; rollup lines demonstrably still earn confirms. The pivot-artifact veto and file-level exemption are unchanged, so recoveries and file-scoped findings stay loud.

### 4. Fix the rollup post's false 'statically links' premise

Rewrite seed_suppressed_rollup's title/body (seed.rs:525-557): the body asserts 'This binary statically links a recognized X runtime', which is false for dynamically-linked MFC and becomes meaningless once fingerprint-independent suppression lands. The counts are already split (runtime_located vs low_grade at seed.rs:519-523); word each class for what it is ('N sit inside recognized runtime code; M are ubiquitous-API/pattern matches benign code also contains').

*Why:* The 'statically links' overclaim is a named FP mechanism in sandbox_time_evasion (rollup-collateral-and-framing), packing (rollup-static-link-overclaim), and ransomware_behavior (rollup-collateral-and-static-link-overclaim); agents spend threads disproving it against binaries that dynamically link MFC42.DLL. Rollups are graded as leads in telemetry, so the wording alone buys disputes.

*Effort:* small \
*True-positive risk:* None — wording only; suppression routing unchanged.

### 5. Stop seeding per-import 'imported, no call site' findings; emit one census line per detector

In api_call_hits (detections.rs:1099-1113), replace the per-import low() hit at address 0 with one combined per-detector line ('N catalog APIs imported but never referenced from code: X, Y, Z — possible decoy IAT') routed to the census/rollup, with a per-detector opt-out (crypto_capability's uncalled CryptEncrypt is a different prior and may keep the fallback).

*Why:* Import-only leads have near-zero confirmed value; decoy/uncalled-import mechanisms and ignore drivers recur in debugger_evasion (decoy-import-flagging), sandbox_time_evasion (decoy-import-fallback), ransomware_behavior (decoy-or-dead-import-seeding, unreferenced-import-leads), and self_deletion (uncalled-import-hits, uncalled-import-filler).

*Effort:* small \
*True-positive risk:* Near zero: a dual-use import such as GetTickCount is still named in the merged census line, which preserves the decoy-IAT signal agents currently derive by hand. Packed binaries where the IAT is the only evidence are covered by the packing detector.

### 6. Implement the promised result-use/argument-recovery validation pass (demote-not-drop)

Build the validator the EvidenceKind contract reserves (detections.rs:243-255; today the kind's only runtime consumer is the seeder's host-gate question filter at seed.rs:1378 — no pass checks the evidence): after run_all, for ApiResultCheck/Timing/ControlFlowGated hits, check cheaply (cached disassembly + reg_value_before at detections.rs:1338) whether the result feeds a compare/branch — if yes set CallContext::GatesBranch (today constructed only in a test, detections.rs:1838) and keep/strengthen; if provably discarded, weaken one step toward the rollup. Recover the one discriminating argument for the known dual-use catalogs (NtQueryInformationProcess class, GetSystemMetrics index, SetWindowsHookEx idHook, MoveFileEx flags, CPUID leaf) and demote unrecoverable claims one notch instead of asserting them. Wire PacesLoop/SeedsRng (provenance_types.rs:18-21, currently dead) from co-located network/PRNG detector output.

*Why:* Dual-use presence-only overclaim is the dominant FP mechanism across the low tier: ransomware_behavior, sandbox_system_checks, self_deletion (GetModuleFileName), sandbox_time_evasion, sandbox_user_activity, and debugger_evasion (Nt*/ThreadContext); these detectors lose several engagements per win. Genuine gates (GetCursorPos, GetForegroundWindow results that feed a branch) pass a branch-on-result test.

*Effort:* large \
*True-positive risk:* Managed: demote only on provable discard, never on absence of a local branch (interprocedural gates survive); demotion lands in the itemized rollup, so true positives either pass the gate or survive demoted-but-visible.

### 7. Recalibrate severity: presence-only API hits can never be Likely, and Fact requires self-validating evidence

Clamp api_call_hits-produced hits (build_api_hit, detections.rs:1168-1209, which renders spec.severity verbatim at :1183) to at most medium()/Unsure unless the validation pass upgraded them (GatesBranch/recovered argument); audit the catalogs' high() entries (severity helpers detections.rs:216-232) accordingly. Reserve Fact for evidence that validates itself (a decode that reproduces, a recomputed digest), not byte-pattern recoveries — the crypto detector's verbatim-PEM recovery path (crypto_capability.rs:1282) is the only Fact producer and its leads were never confirmed.

*Why:* Confidence is inverted: the Likely and Fact tiers are disputed more often than Unsure — the confident tiers lean on template overclaim. debugger_evasion's Likely tier is disputed while being exactly what feeds q-evasion-gauntlet. Marker-forced high() (packing's named-family arm, UPX/VMProtect) and signature_anomaly's Likely arm are the exceptions and must keep their tier — the clamp scopes to API-presence hits only.

*Effort:* small \
*True-positive risk:* Low: few true-positive Likely leads are API-presence call hits (e.g. NtMapViewOfSection, URLDownloadToFile, URLDownloadToCacheFile) — under the clamp they seed at Unsure and stay fully visible, and unsure leads already earn most confirms; structural/marker/replay paths are exempt. Side effect is intended: ranking and question derivation stop amplifying the worst tier.

### 8. Render sub-Likely claims as dual-use hypotheses with a verification cue, not asserted intent

Add a shared rendering rule in comment_body (seed.rs:708-756) / build_api_hit: hits below Likely (or without GatesBranch context) get hypothesis framing appended ('dual-use API; the evasion claim is unverified — confirm the result gates a branch before treating it as evidence'), instead of the catalogs' asserted reasoning strings standing alone as Kind::Statement facts.

*Why:* sandbox_system_checks' 'template-asserts-unverified-intent' is its dominant FP mechanism — agents dispute the asserted story while the observation itself is correct; the same wrong-intent pattern appears in sandbox_user_activity (real-malware-wrong-intent-claim), self_deletion MoveFileEx, native_api's NtSetInformationFile template (info-class-template-overclaim), and crypto_capability's ransomware template.

*Effort:* small \
*True-positive risk:* None: text-only; an agent confirming a real check still confirms, and agents stop having to grade correct observations as Disproved because the attached story was wrong.

### 9. Extend library/runtime recognition beyond named MSVC markers (structural, name-free recognizers)

Extend is_library_function (detections.rs:1006-1039, MSVC/CRT names only) with marker families absent from the project entirely (OpenSSL/musl/mingw/libstdc++/ConcRT — none appear in toolchain.rs or detections.rs; Delphi SysUtils markers already exist at toolchain.rs:80 but only apply after a two-point Delphi fingerprint, which stripped Delphi never reaches), and add structural recognizers that work on stripped code where runtime_classification's name requirement (detections.rs:946) makes it blind: the MSVC fail-fast/report cluster (co-located IsDebuggerPresent + RtlCaptureContext + SetUnhandledExceptionFilter + terminate) and the /GS cookie-init cluster (GetTickCount+QPC+PID+TID XORed into one global). Classified hits take the existing weaken+Runtime-note path (apply_runtime_context, detections.rs:979-991).

*Why:* Unrecognized library/CRT code is a recurring FP mechanism: debugger_evasion (its largest), sandbox_user_activity (stripped GUI frameworks), sandbox_time_evasion, ransomware_behavior (stripped Delphi never fingerprints: DELPHI_FUNCTION_MARKERS like '@system@' never match FUN_xxx names, so the string class caps at 1 of the required 2 points), self_deletion, host_binding — all citing detections.rs:1006-1039/:921-960 as the anchor.

*Effort:* medium \
*True-positive risk:* Low-moderate: action is weaken one step plus an honest note, never drop; a sample's own SEH anti-debug could match the fail-fast cluster, but genuine IsDebuggerPresent/ODS checks rarely co-locate the full cluster signature, and anything misclassified remains visible in the rollup.

### 10. Validation-gate the pivot-artifact privileges (suppression veto, runtime-noise immunity, question derivation)

Both artifact vetoes — partition_suppressed's has_protected (seed.rs:467-482) and runtime_classification's artifact exemption (detections.rs:931-944) — currently shield any hit carrying any pivot-class artifact, including fabricated ones. Gate the privilege on a per-class validity check applied where the artifact is attached: MAC/HostIdentifier values must pass degeneracy/text-byte tests and be user-code-referenced (the text-scan path already has is_degenerate_mac_text at indicators.rs:1214; the binary-carving path that drives host_binding's main FP mechanism bypasses it; keep Guid protected — untrusted GUID literals are among host_binding's true positives the veto saves today); recovered-plaintext/key artifacts must pass the shared wordiness/table-run gates (and apply those same gates to emulation-recovered strings); endpoints need an xref or >=medium grade with the digest as fallback. Unvalidated artifacts lose the veto and the artifact rendering, not the hit.

*Why:* Artifact-carrying leads are disputed more often than leads without; the fabrication mechanisms are the top FP mechanisms in host_binding (binary-mac-carving), crypto_capability (wrong-key XOR fabrications), and encoded_strings (blob-sweep fabrication, emulated table bytes) — and the veto plus the 'artifactless-loop-key flood' ignore driver keep that mass loud.

*Effort:* medium \
*True-positive risk:* Moderate if done bluntly — the veto today protects real GUID/config/key recoveries, so the gate must be per-class and err toward keeping (real GUIDs, wordy decodes, and referenced endpoints pass the proposed per-class tests). Demote-not-drop bounds the downside.

### 11. Split the shared file-level bucket per detector

Bucketing keys only on hit.function (seed.rs:204-207), so every detector's file-level hits share one None bucket and the single fixed 'file-level' post (bucket_name, seed.rs:660-666). Key file-level buckets by (None, detection) and name them file-`<detection>`, so one detector's dispute (or a pile of icon comments) cannot sweep another's correct lead, and thread-level grading stops smearing stances across detectors. Function buckets stay shared (cross-detector co-location in one function is genuine corroboration).

*Why:* Mechanisms: yara 'shared-thread-dispute-bleed' (its family rules sit under other detectors' disputes), packing 'shared-file-level-thread-misgrading', embedded_content 'mono-thread-flood-sweep' (one dispute sweeps a whole batch, up to 16 per-member/icon leads in the shared post); embedded_content's fix is pure topology with zero content change.

*Effort:* small \
*True-positive risk:* None — thread granularity only; costs a few more posts per session bounded by detector count, and true positives become easier to engage.

### 12. Filter resolve_call_sites to CALL/JUMP/THUNK-typed xrefs

resolve_call_sites (context.rs:833-878) keeps any xref whose source lies in executable memory (is_executable_addr at :843-846) with no ref_type check, so constant-propagation data references inside executable sections (VMProtect .vmp slots, ELF PLT quirks) fabricate call sites that every API catalog then reports as calls. Filter on the xref's ref_type (CALL/JUMP/thunk classes), smoke-testing the exact ref_type strings against a real binary first; data-only imports then correctly fall into the imported-but-unreferenced census path.

*Why:* sandbox_time_evasion's 'import-slot-as-call-site' mechanism: a 'Sleep call' lead can actually be a VMP import-table slot (data, not code) — a false confirm this fix removes; self_deletion's ELF PLT misresolution feeds its uncalled-import filler.

*Effort:* small \
*True-positive risk:* Near zero: genuine calls carry CALL-typed refs, thunked calls hop via callers() which is already call-filtered (context.rs:773 keeps only ref_type containing CALL), JUMP keeps tail-call stubs.

### 13. Word-boundary matching and context vetoes for the shared string catalogs

string_artifact_hits matches needles by bare substring (hay.contains at detections.rs:1402). Add per-catalog matching modes: letter-boundary matching for tool/VM names (so 'idaq' stops firing inside RSA base64 and 'WIRESHARK' still matches bounded), shape requirements for structured needles ('.onion' requires a plausible 16/56-char base32 label before the TLD), key-material/base64-run vetoes, and extension-shape vetoes for VM_STRINGS — all as options on the shared helper so every catalog detector inherits them.

*Why:* Mechanisms: anti_instrumentation idaq-in-base64 and tool-name decoys, ransomware_behavior onion-substring in c-ares guards, sandbox_system_checks benign-string-coincidence. Real string leads (procexp, OLLYDBG, WIRESHARK, vbox/vmware registry paths, full .onion addresses) survive the proposed rules.

*Effort:* small \
*True-positive risk:* Low; residual near-misses are bounded by demote-not-drop.

### 14. Re-route question derivation toward verified-engagement evidence classes

derive_questions (seed.rs:1189-1209) spends its 6 slots partly on the worst tier: evasion_question gates on >=Likely T1622/T1497 (seed.rs:1619) — the most-disputed tier — while the three detectors with the best engaged economics derive no question at all. Gate q-evasion-gauntlet on GatesBranch/argument-verified hits (after the validation pass) instead of raw Likely; add a q-signature question from a strong Authenticode mismatch (signature_anomaly emits at most one hit/binary), a family-identification question from a corroborated YARA alias cluster, and route the native_api section-mapping pair — all bounded by the existing MAX_SEED_QUESTIONS=6 cap and merge order (seed.rs:1164-1174).

*Why:* debugger_evasion's Likely tier is exactly what cites into q-evasion-gauntlet, actively routing delegations onto false leads; signature_anomaly has 'no-question-derivation' as an ignore driver, yara has 'no-engagement-path', and native_api's 'no-question-routing' leaves NtCreateSection+NtMapViewOfSection section-pair leads mostly ignored.

*Effort:* medium \
*True-positive risk:* None suppressive — it only re-targets forced engagement toward the lead classes that produce true positives; worst case a lower-priority derived question is displaced under the fixed cap.

## A forum-admission policy
---

Putting the measures together, a concrete policy for which leads earn a statement post, which get rolled up, and which are dropped:

Three admission tiers per hit, decided after apply_runtime_context and the validation pass, with per-detector budgets tied to measured engagement.

**Tier 1 — full statement-post lead** (post + finding comment, question-eligible): (a) hits with verified use — GatesBranch/argument-recovered or ControlFlowGated evidence (the only class whose confident claims survive scrutiny; today's unverified Likely tier is heavily disputed); (b) hits carrying a class-validated artifact (referenced endpoint, key that provably decoded wordy text, parsed config, Guid/host-id passing degeneracy gates, packer marker, Authenticode mismatch, corroborated YARA family cluster) — validated-artifact leads are the useful anchors (killswitch URLs, ransom notes, configs) even though unvalidated artifacts drive the disputes; (c) >=Unsure user-code hits within the detector's finding budget K, ranked confidence-then-suspicion (unsure leads are the engine's honest workhorse tier). Budgets: detectors earning engagement keep full volume — signature_anomaly, native_api, yara, and packing/embedded_content's structural-marker leads post everything that clears Tier 1c; network_comms and encoded_strings keep full volume only for validated-artifact leads and get K≈15 for presence/API leads (their worst sessions are where the ignore mass lives); the nine low-tier detectors (host_binding, sandbox_user_activity, sandbox_system_checks, crypto_capability's API/weak-sig arms, debugger_evasion, sandbox_time_evasion, anti_instrumentation, ransomware_behavior, self_deletion's unpaired-GMF arm) get K≈10 and their presence-only API hits default to Tier 2 until upgraded by the validation pass — these nine rarely convert engagement into confirms, so the budgets lose essentially no true positives.

**Tier 2 — rolled up** (itemized census: per-detector census posts plus the runtime-noise rollup, every entry listed with technique/site/first-line and full provenance): all Untrusted presence-only hits regardless of toolchain fingerprint (as findings they are mostly disputed, while rollup lines — untrusted, or unsure-in-runtime, by construction — are mostly confirmed when engaged, the single strongest shape result in the data), Unsure hits in classified runtime code, imported-but-unreferenced APIs (one combined line), per-function grouped-tell overflow, survey-API observations (GetSystemInfo/GlobalMemoryStatusEx class), and Tier-1 budget overflow. Rollups stay re-raisable ('re-raise any entry as a question'), keeping the record honest without buying threads.

**Tier 3 — dropped** (recorded in detector provenance/telemetry only, never seeded): hits failing validity gates that fabricate values — degenerate/text-run MAC carvings, decode 'recoveries' failing wordiness/table-run tests, PEM blocks with mismatched labels or non-base64 bodies, fragment URLs and folded char-store ports, and exact per-site duplicates of an already-grouped hit. Question derivation reads only Tier-1 leads, with q-evasion-gauntlet additionally requiring verified-use evidence.

Finally, close the loop: the per-detector verdict telemetry already distinguishes shapes, so budgets are revisited per release — a detector that converts engagement (confirm:dispute at thread level above ~1:2) earns its K raised; one that stays below 1:5 has its presence-only output moved wholly to Tier 2.

## Verification notes
---

Each anchor, behavior claim, and feasibility assessment was re-checked against the code:

- Code anchors verified (thorpal/src/thorpal/...): api_call_hits detections.rs:1079-1149 with the per-import low() hit at address 0 at :1099-1113 and per-function grouping at :1117-1146; build_api_hit :1168-1209 rendering spec.severity verbatim at :1183; scan_instructions :1286-1310 (one hit per instruction, no grouping/caps); severity helpers :216-232; EvidenceKind :242-255; run_all_with_roster :798; runtime_classification :921-960 with the artifact exemption at :931-944 and the name bail (fn_name?) exactly at :946; apply_runtime_context :979-991; is_library_function :1006-1039 (MSVC/CRT markers only); reg_value_before :1338; hay.contains :1402; referenced_literal_hit :1497-1525; build_signature_hit :1688-1725. seed.rs: bucketing on hit.function :204-207; per-bucket sort :210-216 (confidence_rank desc then suspicion desc only — the artifact-first term in measure 2 is an addition); partition_suppressed :452-497 with the toolchain early return :457-459, has_protected :467-482, file-level exemption :488-489; MAX_ROLLUP_LINES=40 :500; seed_suppressed_rollup :511-578 with runtime_located/low_grade split :519-523, 'statically links' body :548-557, provenance take(500) :572; bucket_name :660-666; comment_body :708-756; MAX_SEED_QUESTIONS=6 :51; merge_questions :1164-1174; derive_questions :1189-1209; evasion_question's >=Likely gate exactly at :1619. context.rs: resolve_call_sites :833-878 keeps any executable-memory xref with no ref_type check (is_executable_addr test at :843-846); callers() is CALL-filtered at :773. provenance_types.rs:18-21 holds SeedsRng/PacesLoop, both unconstructed anywhere.
- CORRECTED (summary, measure 6): the claim that the EvidenceKind contract 'has no consumer' was too strong. No validation pass exists (no validate module was ported; host_binding.rs:42 references a validate.rs that is absent), but EvidenceKind::ControlFlowGated IS consumed at runtime by the seeder's gate_questions filter (seed.rs:1378, paired with a HostIdentifier artifact) to derive q-host-gate questions, and several detectors set ControlFlowGated themselves (host_binding.rs:1834, debugger_evasion, anti_instrumentation, native_api, sandbox_system_checks). CallContext::GatesBranch is constructed only in a test (detections.rs:1838; non-test grep hits none). Wording corrected to 'never given its validator'.
- CORRECTED (measure 7): the Fact producer is the crypto detector's verbatim plaintext embedded-PEM path (crypto_capability.rs:1282: encoding None => Confidence::Fact), not a 'cert path' narrowly. Some true-positive Likely leads ARE API-presence call hits; under the clamp they demote to Unsure but remain seeded.
- CORRECTED (measure 9): 'Delphi SysUtils' is not a missing marker family — DELPHI_FUNCTION_MARKERS ('@system@', 'system::', 'sysutils::', '@sysutils@') exist at toolchain.rs:80, gated on a two-point Delphi fingerprint plus surviving names (which stripped Delphi never satisfies). OpenSSL/musl/mingw/libstdc++/ConcRT confirmed absent from toolchain.rs and detections.rs. Recognition is not purely MSVC-centric: is_toolchain_runtime (toolchain.rs:210-283) covers Go/Rust/MFC/Delphi when fingerprinted; the summary wording was adjusted accordingly.
- CORRECTED (measure 10): a MAC degeneracy gate already exists on the text-scan path (is_degenerate_mac_text, indicators.rs:1214); the binary-carving path driving host_binding's main mechanism bypasses it — the measure says extend, not invent. The HostIdentifier artifacts worth protecting are GUID literals, not carved MACs.
- CORRECTED (measure 3): feasibility detail added — fingerprint-independent suppression must also change seed_forum_from_detectors' ctx.toolchain.expect('suppression requires a toolchain') at seed.rs:224 and seed_suppressed_rollup's mandatory Toolchain parameter/title.
- Feasibility of every change confirmed at its anchor: run_all_with_roster post-pass (hits + stamped detection names available), seed-time budget between apply_runtime_context (:201) and bucketing (:204), partition_suppressed criterion split, rollup rewording, api_call_hits import-census mode, validation pass (cached disassembly via all_function_disassembly, sites lists, weaken()/strengthen(), GatesBranch note rendering already wired in comment_body), build_api_hit severity clamp, comment_body hypothesis framing, is_library_function extension + structural recognizers, per-class artifact gates, (None, detection) bucket keying, XRef.ref_type field exists for the resolve_call_sites filter, string_artifact_hits options, and derive_questions re-routing under MAX_SEED_QUESTIONS=6. No measure was found unimplementable.
