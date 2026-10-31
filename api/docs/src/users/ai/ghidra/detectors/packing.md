# packing

Part of the [telemetry review](../detectors.md#what-the-telemetry-says) of the [preseed detectors](../detectors.md).

**Status (2026-10-31):** the fixes below shipped in the fleet-wide detector overhaul (see the main page's overhaul status note). No deviations from the proposals below.

Packing seeds roughly one lead per sample and is the backbone of the high-leverage q-unpack question, but most of its threads are ignored and many engaged threads are disputed. The disputed leads split into distinct clusters rather than one misread: the overlay leg of file_entropy_signal firing on small appended data that is actually an Authenticode certificate table (never checked against the PE security directory); entropy double-counting plus the 'executable code is stored compressed or encrypted' template on payload carriers and flattened/VB6 images; a sharp marker bug (an empty header section name satisfies wanted.starts_with("") for every marker, and UPX is first in the table); and section-naming convention gaps (Ghidra-synthesized segment_N names and Go runtime sections read as 'rewritten section table'). The entry-point attribution ('packer stub' role planted on the recognized entry regardless of signals) is the disproof vector agents actually use — they decompile the stock CRT entry. The marker-named and multi-signal leads are genuinely valuable (UPX and VMProtect identifications buy correct unpack routing; PECompact/RLPack leads are question-siphoned into ignores), so the fixes should split the payload-carrier shape from the packed-code claim, exclude declared cert tables, and harden marker/name hygiene rather than mute the detector.

## Where the false positives come from
---

### 1. Payload entropy as packed code

A dominant high-entropy NON-executable section (.rsrc resource, embedded payload in .data) is double-counted: high_entropy_section_signal fires on it, and file_entropy_signal fires again on the whole file that section dominates, yielding '2 structural signal(s)' -> medium (Unsure/40) and the q-unpack question; a variant double-counts unpack_target_signal's source section with high_entropy_section_signal on the same section (an ELF .bss paired with a small .rodata counted twice). The claim template then asserts 'its executable code is stored compressed or encrypted and reconstituted at runtime' even though the code sections are ordinary and the import table is complete. The same template also ships on 1-signal shapes where only the resource section or only the whole-file figure (control-flow-flattened images whose individual sections all read lower than the file as a whole, VB6, .NET-with-cargo) cleared a bar — but those entropy-only shapes are a minority of the Untrusted tail (most of it is the cert-overlay and naming mechanisms below).

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/packing.rs:471-493 (high_entropy_section_signal), 627-658 (file_entropy_signal whole-file leg), 408-443 (unpack_target_signal source overlap), 697-707 (claim wording in build_hit)`

### 2. Cert overlay as packed code

file_entropy_signal's overlay leg fires on ANY >=4096-byte region past image_end() at entropy >=7.2 and the template words it as packed executable code, but it never consults the PE security data directory — so the Authenticode certificate table (DER certs/signatures routinely read 7.2-7.6) of a signed binary becomes a 1-signal Untrusted 'Packed or obfuscated image' lead. The module's own doc comment lists 'signed binaries carry an appended certificate table' as a known mundane cause, yet no code excludes it. Most of the compressed-file-body untrusted disputes are this small-overlay shape, with disputing agents byte-proving WIN_CERTIFICATE/PKCS#7 structures; it is the single largest dispute cluster.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/packing.rs:641-653 (overlay leg); thorpal/src/thorpal/detect/fileinfo.rs (parses the import data directory at ~399-408 but never the security directory, entry 4)`

### 3. Entry stub misattribution

attribute() always plants the hit on the recognized entry function (ENTRY_NAMES includes Ghidra's generic 'entry') with the role 'entry point — the code that reconstitutes the packed body before it runs', regardless of whether any stub-shaped signal fired. On unpacked binaries with carrier/overlay entropy, the entry is a stock CRT/CLR startup, so the lead reads as 'this CRT function is a packer stub' — the exact claim agents decompile and disprove (it is the disproof vector in most finding-shape disputes, co-occurring with the two entropy mechanisms). It also keys the thread name (fn-`<entry>`) to CRT code, which routes 1-signal hits into the runtime-noise rollup via partition_suppressed.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/packing.rs:753-800 (attribute, ENTRY_NAMES at 760-769, role phrase at 777-784)`

### 4. Empty section name matches upx marker

match_marker's section test accepts either direction of prefix match: names.iter().any(|name| name.starts_with(wanted) || wanted.starts_with(name)). fileinfo.rs's section_name() trims a PE name field at the first NUL, so a NUL-led garbage name (ConfuserEx/.NET protectors, memory dumps) becomes the empty string; section_names() filters empty names only on the Ghidra-blocks side, not header-derived names. The empty name then satisfies wanted.starts_with("") for EVERY marker's section list — and UPX is first in PACKER_MARKERS, so the finding confidently names UPX (high() = Likely/75) with no UPX0/UPX1 sections and no UPX! magic. The code-level 'likely' disputes carry an empty `""` name in their unconventional-names list (a remaining likely dispute is thread pollution — see shared-file-level-thread-misgrading).

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/packing.rs:863-868 (reversed prefix leg), 884-903 (section_names keeps raw header names; empty filter only at 901 on the Ghidra side), 222-227 (UPX first in PACKER_MARKERS); thorpal/src/thorpal/detect/fileinfo.rs:917-921 (NUL-trim producing empty names)`

### 5. Section name convention gaps

section_naming_signal reads any name outside CONVENTIONAL_SECTIONS as 'the section table was rewritten after the program was built', but two legitimate name sources are missing: (a) Ghidra synthesizes segment_0..segment_N block names for images whose header declares no section table (stripped ELFs, raw-ish loads), producing leads like '1 of 1 sections carry names no linker emits (`segment_0`)'; (b) Go linker sections (.typelink, .gopclntab, .noptrdata, ...) are absent from the ELF list, so Go binaries read as mostly-rewritten. These seed 1-signal Untrusted leads asserting packed code on unmodified images.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/packing.rs:583-619 (section_naming_signal), 140-196 (CONVENTIONAL_SECTIONS: no Go names, no segment_N handling)`

### 6. Rollup static link overclaim

1-signal packing hits grade low() = Untrusted, so partition_suppressed sweeps their entry-function bucket into the runtime-noise rollup whenever a toolchain was fingerprinted. The rollup body asserts 'This binary statically links a recognized {X} runtime', but toolchain::detect() counts function/import names and strings — MFC_STRING_MARKERS ('mfc42' etc.) and MFC_FUNCTION_MARKERS ('afx', 'cwinapp') fire on Ghidra-resolved dynamic imports of MFC42.DLL too — so the premise is provably wrong on DLL-importing binaries, and the suppression buries structural signals that were real (a high-entropy custom resource), which agents re-raise as a correction.

*Where:* `thorpal/src/thorpal/seed.rs:452-497 (partition_suppressed), 548-557 (rollup body wording); thorpal/src/thorpal/detect/toolchain.rs:64-68 (MFC string/function markers), 86-105 (detect() chains imports into the name pool)`

### 7. Shared file level thread misgrading

Seeding buckets hits by containing function with None = one shared 'file-level' post for ALL detectors' file-level findings, so a disputing reply aimed at a sibling detector's comment grades every lead in that thread disputed — including correct packing leads. Not a packing-heuristic defect, but it inflates this detector's disputed count on file-level-anchored leads, and it can mark a genuine UPX identification disputed.

*Where:* `thorpal/src/thorpal/seed.rs:202-207 (BTreeMap<Option<u64>,...> bucketing; None bucket shared across detectors), 660-666 (bucket_name: one fixed 'file-level' post name)`

## Why leads get ignored
---

- **Per sample ubiquity of weak structural leads** — The detector emits a lead on virtually every binary that has any >=4KB section at entropy >=7.2 (compressed icons/PNGs in .rsrc, embedded assets) or any >=4KB appended region (cert tables), so a boilerplate 'Packed or obfuscated image' note appears in almost every session. Its own text disclaims action ('compressed payloads are also entirely normal in installers ... not that the program is malicious'), so agents read it as context and move on; most ignores are this one generic pattern.
- **Rollup line mass** — Each suppressed 1-signal packing hit becomes a '- T1027.002 @ `<a>`: ...' line inside the runtime-noise rollup post, and telemetry counts each line as a lead. The rollup exists precisely so agents need not engage, so these lines are ignored by design.
- **Question siphon of strong leads** — Every >=Unsure packing finding also derives the q-unpack question (seed.rs:1252-1264 gates on confidence_rank >= Unsure and cites the statement posts), and the actual unpacking work lands in the question thread, not on the fn-/file-level statement post — so even marker-named Likely/75 leads (UPX, VMProtect, PECompact, RLPack) often show zero replies. PECompact and RLPack leads are ignored this way, so those marker families get no verdict despite being exactly the routing the question consumed.
- **Multi sample scope duplication** — Multi-sample sessions seed a separate packing lead per sibling program, multiplying near-identical structural notes within one session; the siblings' leads are rarely engaged individually.

## What would reduce the noise
---

Static-side changes, ordered as proposed; each was checked against the true positives so the fix does not suppress what the detector gets right.

### Split the payload-carrier shape from the packed-code claim and stop double-counting entropy

In packing.rs collect_signals/build_hit: (a) drop file_entropy_signal's whole-file part when the max-entropy section is NON-executable and its size covers >=~60% of raw.len() (same measurement as high_entropy_section_signal, not an independent signal; the non-executable condition protects packed-.text cases where 2 signals buy the medium grade); (b) when unpack_target_signal's source section is the same section high_entropy_section_signal picked, count them as one signal (the .bss/.rodata shape); (c) when every fired signal is entropy-only (signals 3/7), the max-entropy section is non-executable, the image has >= THIN_IMPORT_TABLE imports, and an executable section reads < ~7.0, replace the 'executable code is stored compressed or encrypted' template with 'large high-entropy embedded data (resource/overlay payload carrier); the visible code appears to be the code that runs; carve and identify the payload' — keeping T1027.002, the Packer artifact, and medium (Unsure) severity when the carrier is >=64KB or >=~25% of the file (so q-unpack still derives, seed.rs:1258 gates on >=Unsure), low otherwise.

*Effort:* medium \
*True-positive risk:* Packed-.text true positives keep the packed-code claim via the executable-section condition; marker leads force high() regardless. True-positive NON-executable carriers (large .data or .rsrc payloads) exceed the 64KB carrier floor, so they keep medium severity and the q-unpack question; only their wording changes to the carve-the-payload claim, which matches what agents verify on them. Residual risk: a carrier whose 'visible code' is itself the stub (fully packed but with a fat import table) gets the softer wording.

Addresses: payload entropy as packed code, per sample ubiquity of weak structural leads.

### Exclude the declared Authenticode certificate table from the overlay entropy leg

In fileinfo.rs, parse PE data directory entry 4 (the security directory: file offset + size — the directory walk already exists for the import directory at ~399-408) into a new FormatHeader field. In packing.rs file_entropy_signal (641-653), subtract a declared cert range that overlaps the overlay before measuring: if the remainder is < MIN_ENTROPY_LEN, drop the overlay part; otherwise measure only the non-cert remainder. When a cert range was excluded, say so in the detail ('past the Authenticode certificate table').

*Effort:* small \
*True-positive risk:* Keyed on the header-declared security-directory range, not on overlay size, so small overlays that are real appended cargo rather than declared cert tables keep firing; dropper cargo alongside a grafted cert also stays (only the cert sub-range is excluded). A sample that lies in its security directory to hide an overlay suppresses the signal — but a cert directory covering a non-cert payload is itself checkable by other detectors.

Addresses: cert overlay as packed code, per sample ubiquity of weak structural leads.

### Harden match_marker against empty/garbage section names

In packing.rs match_marker (863-868): before matching, trim each candidate name and discard names that are empty, non-printable, or shorter than 3 chars; keep the forward test name.starts_with(wanted) and restrict the reversed leg wanted.starts_with(name) to names of length >= 3 (or drop the reversed leg entirely). Apply the same empty/printable filter to header-derived names in section_names (884-892), which currently only filters the Ghidra-blocks side (901).

*Effort:* small \
*True-positive risk:* None expected for real marker leads: UPX (upx0/upx1 forward match or UPX! magic) and VMProtect (.vmp0 forward match) match forward; PECompact/RLPack leads match via 'pec'/'PEC2' magic and '.rlpack', all >= 3 printable chars. The reversed leg only legitimately serves truncated names >= 3 chars (e.g. 'upx'), which the length floor preserves.

Addresses: empty section name matches upx marker.

### Gate the entry-point 'packer stub' attribution on stub-shaped evidence

In packing.rs attribute() (pass it the fired signals and family from build_hit): return the entry function + the 'entry point — the code that reconstitutes the packed body' role only when a marker matched OR a stub-shaped signal fired (reserved unpack target, entry point in a writable section, writable executable section, loader-shaped import table) OR the max-entropy section is executable; otherwise anchor the hit file-level (function=None, address = the high-entropy section base or overlay start) with the existing 'compressed body' unattributed detail. This removes the decompile-the-CRT-entry dispute bait and keeps 1-signal entropy hits out of per-function buckets, hence out of partition_suppressed's runtime-noise rollup (seed.rs:488-489: file-level buckets are never suppressed).

*Effort:* medium \
*True-positive risk:* Marker leads (UPX/VMProtect) carry stub-shaped signals so they keep entry attribution; packed-.text true positives keep it via the executable max-entropy section. File-level anchoring increases exposure to shared-thread misgrading — that is why the per-detector file-level bucket fix below ships with it.

Addresses: entry stub misattribution, rollup static link overclaim, rollup line mass.

### Split the shared file-level bucket per detector

(Mechanism 5 otherwise has no dedicated fix; the attribution fix above also routes more packing hits file-level.) In seed.rs (202-207 bucketing + 660-666 bucket_name): for function=None hits only, key buckets by (None, technique-or-detector) and name the posts 'file-level-`<technique-id>`' (keep the single 'file-level' name when only one detector lands there, for forum continuity); per-function buckets unchanged.

*Effort:* small \
*True-positive risk:* No packing true-positive is suppressed — this only changes thread granularity. Cost: a few more file-level posts per session (bounded by detector count), and historical thread-name continuity breaks for telemetry diffing; both acceptable against mis-grading correct leads such as a real UPX identification.

Addresses: shared file level thread misgrading.

### Fix the runtime-noise rollup's 'statically links' premise

In seed.rs seed_suppressed_rollup (548-557): reword 'This binary statically links a recognized {X} runtime' to fingerprint-accurate language, e.g. 'This binary carries the vocabulary of a recognized {X} toolchain/runtime (statically linked or imported)', since toolchain::detect() counts import names and strings (toolchain.rs:64-68 MFC markers like mfc42/afx/cwinapp) that dynamic MFC42.DLL imports also satisfy. Shared-engine change benefiting every detector's rollup lines.

*Effort:* small \
*True-positive risk:* None — wording only; suppression routing is unchanged.

Addresses: rollup static link overclaim.

### Raise the floor for the high-entropy-section signal

In packing.rs high_entropy_section_signal (471-493; thread raw.len() through from collect_signals): require the candidate section to be executable OR >= 64KB OR >= ~20% of the raw file before it counts as a packing signal (keep MIN_ENTROPY_LEN=4096 only as the absolute floor). This stops icon/asset-sized compressed resources and small .rodata tables from seeding a boilerplate lead on nearly every GUI/Rust binary.

*Effort:* small \
*True-positive risk:* True-positive high-entropy-section leads are >= 64KB or executable (.text cases); UPX1/VMProtect/RLPack bodies are hundreds of KB to MB. Small-section false positives (small .rsrc and .rodata tables) are exactly what it removes; a real resource below 64KB in a small file stays via the ~20%-of-file branch. Residual: a tiny non-executable encrypted blob in a large benign-looking file loses its one weak signal — acceptable, that shape belongs to the weak Untrusted tail.

Addresses: per sample ubiquity of weak structural leads, payload entropy as packed code.

### Close the section-name convention gaps

In packing.rs section_naming_signal (583-619): skip names matching ^segment_\d+$ when the parsed header declares no sections (header.map_or(true, |h| h.sections.is_empty()) — they are Ghidra loader inventions, not a rewritten table; keep is_synthetic_block exact-match semantics untouched for marker logic). In CONVENTIONAL_SECTIONS (140-196), extend the elf list with the Go linker sections (.typelink, .itablink, .gosymtab, .gopclntab, .noptrdata, .noptrbss, .zdebug, .go.buildinfo).

*Effort:* small \
*True-positive risk:* True-positive naming signals come from UPX0/UPX1/.vmp/garbage names, all still unconventional, not from segment_N or Go names. A packer that names its sections segment_0-style AND strips the header section table would slip signal 6 — but in that header-less case the detector's other signals (w+x, entropy, unpack target) carry the detection.

Addresses: section name convention gaps, per sample ubiquity of weak structural leads.

## Verification notes
---

- All code anchors verified against HEAD and corrected where off: packing.rs anchors exact (high_entropy_section_signal 471-493, file_entropy_signal 627-658, match_marker reversed leg 863-868, PACKER_MARKERS UPX-first 222-227, attribute 753-800); toolchain.rs MFC markers are at 64-68; build_hit claim wording is 697-707; seed.rs bucketing 202-207, partition_suppressed 452-497 (file-level never suppressed at 488-489), rollup wording 548-557.
- already_fixed check: packing.rs is byte-identical since commit 9e4a6f1f (2026-10-26); the only later commit touching this area (5bfd2ab5, 2026-10-30) adds telemetry provenance/shape labels to seed.rs with no behavioral change to bucketing, suppression, or wording — all mechanisms correctly marked active.
- empty-section-name-matches-upx-marker is proven from the code: fileinfo.rs:917-921 NUL-trims a NUL-led PE name to "", packing.rs keeps header names unfiltered, and "".starts_with checks match every marker with UPX winning by table order.
- The payload-entropy mechanism includes the unpack-target/source double-count variant (.bss/.rodata).
- cert-overlay-as-packed-code: the module doc names signed-binary cert tables as a known mundane cause but no code checks data directory 4.
- section-name-convention-gaps: segment_N loader-synthesized names and Go runtime sections read as 'section table was rewritten'. rwx-.text single-signal disputes remain as acknowledged residual weak-signal cost (no fix proposed; the signal is legitimate and already graded low).
- Fix 1 amended: whole-file drop conditioned on a non-executable dominator, and the reworded carrier claim keeps medium severity for >=64KB / >=25%-of-file carriers, so true-positive non-executable carriers keep their q-unpack derivation (seed.rs:1258 gates on >=Unsure).
- Since no other fix addressed shared-file-level-thread-misgrading while the attribution fix increases file-level routing, the per-detector file-level bucket fix was added.
- rollup-static-link-overclaim: partition_suppressed sweeps all-Untrusted function buckets only when a toolchain fingerprinted; toolchain::detect() chains imports into the name pool so ordinal-imported MFC42 vocabulary + 'mfc42' strings score the required two marker classes, making 'statically links' provably wrong.
- Entry-stub-misattribution overlaps the entropy mechanisms: it is the disproof vector inside their threads (agents attack the CRT-entry attribution), not an independent lead population.
