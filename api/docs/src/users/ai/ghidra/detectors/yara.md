# yara

Part of the [telemetry review](../detectors.md#what-the-telemetry-says) of the [preseed detectors](../detectors.md).

**Status (2026-10-31):** the fixes below shipped in the fleet-wide detector overhaul (see the main page's overhaul status note). Deviation: `_Auto` rules stay medium per this page's TP-risk note (only SUSP_/_Unknown_/Generic_Threat/packer classes demote).

The yara detector is a low-per-session-volume, high-ignore identification feeder whose engaged leads buy the highest-value triage anchor — instant family identification (family signatures, in-memory shellcode rules, a known-malicious signing certificate) that agents then verify behaviorally. Its false positives involve self-declared low-precision rule classes (a MALPEDIA _Auto n-gram rule, a SIGNATURE_BASE _Unknown_ cluster rule) whose matches land in embedded benign content (a legitimate embedded utility PE; VERSIONINFO version strings of an embedded .NET loader), seeded at the uniform unsure/40 with bare raw-file offsets and no structural attribution — and some disputes graded against YARA leads are bleed-over from disputes aimed at a sibling detector's finding in the shared file-level thread. The ignored leads are dominated by self-inflicted redundancy (multiple packs shipping aliases of the same family, each seeding an identical-looking comment), generic SUSP_/Generic_/packer rules that duplicate native detectors, and the structural fact that YARA leads never enter the question workflow (derive_questions keys on ATT&CK ids the "YARA" pseudo-technique never matches). Clustering aliases into corroborated leads, attributing offsets to file structure with the in-tree PE/ELF parser, quoting matched bytes, and deriving one identification question would cut the flood and pre-empt both false-positive classes without suppressing any true-positive pattern.

## Where the false positives come from
---

### 1. Embedded benign content match

Rules match bytes inside embedded foreign content, and the detector reports only bare raw-file offsets — no section, resource, overlay, or embedded-PE mapping (the rendering loop emits offsets straight from pattern.matches() with no file-layout lookup) — so the lead asserts a family identity from bytes that are not the sample's own code. An auto-generated n-gram rule can fingerprint a legitimate tool the sample embeds (every match offset falling inside the embedded PE), making the family claim unsupported; a cluster rule can match generic '1.0.0.0' version metadata inside the UTF-16 VERSIONINFO block of an embedded .NET loader. The lead gives agents no way to see any of this, so they burn delegation work disproving it.

*Where:* `thorpal/src/thorpal/detect/yara.rs:139-188`

### 2. Weak rule class at uniform severity

Overlapping lens on the same false positives: scan() assigns every matching rule the identical medium() severity (Confidence::Unsure, suspicion 40 — detections.rs:223-225) and the identical canned reasoning paragraph regardless of rule precision class. Low-precision rules self-declare it in their identifiers — MALPEDIA `_Auto` (machine-generated n-grams) and SIGNATURE_BASE `*_Unknown_*` (unattributed cluster rule) — yet they seed indistinguishably from handcrafted family signatures. Nothing in the lead tells the agent, or the bucket sort (confidence then suspicion, a total tie across YARA hits), that these hits deserve more skepticism.

*Where:* `thorpal/src/thorpal/detect/yara.rs:159`

### 3. Shared thread dispute bleed

*(verification left this one uncertain)*

In the main seeding pass all detectors' file-level hits share one None bucket, so YARA hits are seeded as comments under the same never-suppressed `file-level` post as every other file-level detector's findings; a disputing comment aimed at a sibling detector's finding in that thread (a string/network-indicator finding, or debugger_evasion's T1622 findings) gets charged to the YARA lead by grading, even when the YARA lead itself has no replies and is implicated only because some of its offsets overlap the same region.

*Where:* `thorpal/src/thorpal/seed.rs:202-216 (shared None bucket in the main pass) + thorpal/src/thorpal/seed.rs:660-666 (bucket_name)`

## Why leads get ignored
---

- **Family alias flood** — The operator pack aggregates many sources (MALPEDIA, SIGNATURE_BASE, DITEKSHEN, CAPE, SEKOIA, ARKBIRD, R3C0NST, RUSSIANPANDA, elastic), which ship near-duplicate rules for the same family; every alias seeds its own comment-lead with an identical body shape. An agent at most engages one of the cluster and the rest idle. MAX_YARA_HITS=32 per scan permits the flood, and multi-sample sessions (per-sample s2..s7 scopes each run their own scan) multiply it.
- **Uniform boilerplate no triage signal** — All leads are byte-identical except rule name and offsets: same unsure/40 severity, same canned reasoning sentence, no matched-string excerpt, no rule metadata, no next action. Agents cannot rank a precise family signature above Generic_Threat noise from the lead text, so they treat the whole block as background. The sort inside the file-level bucket (confidence then suspicion, seed.rs:210-215) is a total tie across YARA hits, so ordering is arbitrary.
- **Generic technique rules duplicate native detectors** — SUSP_/Generic_/packer-class rules carry no identification value and restate what native detectors already seed (packing has its own T1027.002 detector feeding the unpack question; stack strings and byte encoders have decode-detector coverage), so they are pure noise in the YARA channel.
- **No engagement path** — YARA hits never enter the engaged workflow: derive_questions (seed.rs:1189-1209) emits only unpack/gate/decoded-config/crypto-key/c2/ransomware/evasion questions keyed on ATT&CK technique ids (e.g. the T1027.002 filter at seed.rs:1257), and the "YARA" pseudo-technique matches none, so no question is ever derived from a signature match; the hits sit as passive statement comments under the shared file-level post (which partition_suppressed exempts from the runtime-noise rollup via its function.is_some() requirement, seed.rs:487-489). Nothing forces a delegated agent to resolve them, so even strong family leads usually idle.

## What would reduce the noise
---

Static-side changes, ordered as proposed; each was checked against the true positives above so the fix does not suppress what the detector gets right.

### Cluster same-family alias rules into one corroborated lead

In yara.rs scan() post-process the matching rules before building DetectionHits: derive a family token from each rule identifier — strip source/author prefixes (MALPEDIA_, SIGNATURE_BASE_, DITEKSHEN_, CAPE_, SEKOIA_, ARKBIRD_, R3C0NST_, RUSSIANPANDA_, CRAIU_, elastic Windows_Trojan_/Linux_Trojan_/Windows_Generic_) and generic tokens (Win/Lnx/Mac, MAL/MALWARE/HKTL/Implant/Backdoor, trailing _Auto/_`<8hex>`/date/version tokens like _Feb24_1/_V2), lowercase the salient remainder — group hits by token, and emit one merged DetectionHit per family listing every member rule (namespace:identifier) with its own offsets. When >=2 distinct namespaces (packs) or >=2 distinct source prefixes agree on a family, upgrade the merged lead from medium() to high() (Likely/75) with an 'N independent signatures agree' reasoning line. Token derivation works on real identifier shapes (R3C0NST_Prolock_Malware / DITEKSHEN_MALWARE_Win_Prolock / SIGNATURE_BASE_MAL_Prolock_Malware all reduce to 'prolock').

*Effort:* medium \
*True-positive risk:* A multi-source family match (for example a MALPEDIA _Auto rule, a handcrafted family rule, and an elastic Windows_Trojan_ rule for the same family) merges into one stronger Likely lead — still seeded and more prominent, not suppressed. Solo family rules have no alias to merge with and seed unchanged. Uncorroborated weak-class rules have no sibling, so they get no corroboration upgrade.

Addresses: family alias flood, uniform boilerplate no triage signal, weak rule class at uniform severity.

### Attribute match offsets to file structure and flag embedded/benign regions

After collecting sites in yara.rs scan()/scan_file(), map each offset against the file layout using the in-tree parser the crate already has: fileinfo::parse_header(raw, format) returns FormatHeader with per-section raw_offset/raw_size and cert_dir_entry_offset for both PE and ELF, and authenticode.rs already demonstrates locating the .rsrc on-disk extent and its VERSIONINFO block (company_name/read_company_value). Add an MZ/PE-header sweep of the raw buffer to tag embedded foreign-PE ranges. Append a location line to the lead body ('all N matches inside embedded PE @0x..-0x..' / 'matches in .rsrc VERSIONINFO'). Demote the hit to low() (Untrusted/15) ONLY when every offset falls inside version-resource metadata; for embedded-PE and overlay regions keep medium() but add a reasoning sentence directing the agent to verify the embedded object rather than the host code. No new dependency needed.

*Effort:* medium \
*True-positive risk:* Malicious-certificate rules (Windows_Generic_MalCert_*) match the certificate overlay by design — the overlay path is annotate-only, never demoted. Droppers whose embedded PE is the actual payload keep a medium lead with an annotation inviting carving. The narrow all-offsets-in-VERSIONINFO demotion does not fire when only some offsets sit in the version block, so its effect there is annotation-only; family signatures do not match only version-resource metadata, so the single demotion rule leaves them alone.

Addresses: embedded benign content match, weak rule class at uniform severity, uniform boilerplate no triage signal.

### Quote matched pattern excerpts in the lead body

In the yara.rs hit-rendering loop (which already iterates pattern.matches() for ranges at lines 142-152), use the match data yara-x exposes (found.range() over the raw buffer, or found.data()) to render per-site a capped, sanitized excerpt: printable ASCII/UTF-16 runs as quoted strings (with a utf16 note), other bytes as a short hex snippet, e.g. '0x...: u"1.0.0.0"'. Cap at ~48 bytes per site and 4 excerpts per rule.

*Effort:* small \
*True-positive risk:* None — purely additive. True-positive leads gain self-evidencing content; a cluster-rule lead showing u"1.0.0.0" version strings is self-triaging instead of needing a delegated disproof. Matched bytes are content-derived evidence exactly like the rule name (the module doc already takes this position), so blinded-run compatibility is unchanged.

Addresses: uniform boilerplate no triage signal, embedded benign content match.

### Tier severity by rule-class from the identifier

Add a rule-class heuristic in yara.rs before the severity assignment at line 159: identifiers matching SUSP_, Generic_Threat, _Unknown_, or Packer_/_Packer/UPX classes get low() (Untrusted/15) plus a 'heuristic/cluster rule — weak family evidence' reasoning note; keep _Auto and MalCert classes at medium() (both have produced true positives); handcrafted family rules keep medium() until the corroboration upgrade from the clustering fix lifts them.

*Effort:* small \
*True-positive risk:* Family, in-memory-shellcode, and malicious-certificate identifiers do not match the demote list — Windows_Generic_MalCert_* is 'Generic_MalCert' not 'Generic_Threat' and MalCert is explicitly excluded; an `_Unidentified_NNN_Auto` identifier does not contain '_Unknown_'; _Auto stays medium because _Auto family rules produce true positives. SIGNATURE_BASE `*_Unknown_*` cluster rules do match '_Unknown_' and rank down. Demoted hits still seed: YARA hits are function=None and partition_suppressed requires function.is_some() (seed.rs:487-489), so the file-level bucket is never rollup-suppressed — they just sort below family rules.

Addresses: weak rule class at uniform severity, generic technique rules duplicate native detectors.

### Derive one family-identification question from a corroborated YARA cluster

Add a yara_identity_question() to derive_questions in seed.rs (seed.rs:1189-1209; keyed on HitSource::Yara or technique id "YARA", which currently matches no deriver): when a merged cluster is Likely (>=2 independent sources per the clustering fix) seed one question per sample — 'Signatures identify this as `<family>`; verify the implied behaviors against the binary' — citing #file-level via the existing evidence_line helper, with gate tasks to confirm or refute the family claim with code-level evidence. Place it after the unpack/gate questions in the fixed priority order so MAX_SEED_QUESTIONS (=6) still favors blockers. Note this applies to the main seeding pass only — seed_yara_only (the failed-Ghidra fallback) derives no questions by design.

*Effort:* medium \
*True-positive risk:* None suppressive — it adds a forced engagement path for exactly the lead class that produces true positives (family rules). Bounded to one question per sample and capped by MAX_SEED_QUESTIONS=6, so it can displace at most one lower-priority derived question (the evasion rollup is the lowest).

Addresses: no engagement path, family alias flood.

### Cap rendered YARA leads per sample with a remainder rollup comment

After clustering and tiering in yara.rs scan(), emit the top ~10 clusters strongest-first as individual DetectionHits and fold the tail into one summary DetectionHit ('N more rules matched: name@first-offset, ...') instead of letting MAX_YARA_HITS=32 individual comments land; keep the scan cap at 32 so the rollup line still names everything. Emitting the remainder as a single hit keeps the change local to yara.rs — seed.rs needs no change.

*Effort:* small \
*True-positive risk:* With family clusters upgraded and generic classes demoted, true-positive patterns (family rules + MalCert + in-memory shellcode rules) rank far above a 10-cut; the folded tail (SUSP_/Generic_/packer singles) stays discoverable by name and offset in the rollup comment.

Addresses: family alias flood, generic technique rules duplicate native detectors.

## Verification notes
---

- Anchors: yara.rs:139-188 is the hit-rendering loop emitting bare raw-file offsets with no file-layout lookup; yara.rs:159 assigns the uniform medium() severity, and detections.rs:223-225 confirms medium() = (Confidence::Unsure, 40); seed.rs:660-666 is bucket_name with the fixed 'file-level' name; seed.rs:1189-1209 is derive_questions with no YARA/HitSource::Yara branch (derivers filter on ATT&CK ids, e.g. the T1027.002 filter at seed.rs:1257).
- Mechanism 3's code anchor: seed.rs:344-347 is the None bucket inside seed_yara_only, the Ghidra-less fallback where only YARA hits exist — no sibling detector can bleed disputes there. Disputes that bleed over come from threads containing sibling detectors' findings, which are seeded by the main pass whose shared None bucket is seed.rs:202-216.
- Fix 2 is concretely in-tree: fileinfo.rs already exposes parse_header(raw, format) returning FormatHeader with per-section raw_offset/raw_size and cert_dir_entry_offset for both PE and ELF, and authenticode.rs already locates the .rsrc on-disk extent and reads its VERSIONINFO block — so file-structure attribution needs no new dependency (which would otherwise require user approval under the project's dependency rules).
- Fix 2's all-offsets-in-VERSIONINFO demotion branch only fires when every offset is in the version block; cluster-rule false positives with offsets outside it are pre-empted by the annotation plus fix 4's _Unknown_ demotion. This makes the demotion rule even safer, not less.
- Fix 4's never-suppressed claim: partition_suppressed requires function.is_some() (seed.rs:487-489), and YARA hits are always function=None, so demoted hits still seed.
- Fix 5's prerequisites: HitSource::Yara exists (detections.rs:276), MAX_SEED_QUESTIONS=6 (seed.rs:51), evidence_line/bucket_name helpers exist, and seed_yara_only intentionally derives no questions, so the new deriver applies to the main pass only.
- Raw-offset-vs-VA ambiguity (YARA sites are file offsets while other detectors seed VAs) is a sub-facet of mechanism 1's bare-offset presentation, not an independent mechanism.
