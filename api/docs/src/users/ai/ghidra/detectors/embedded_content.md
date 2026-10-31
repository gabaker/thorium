# embedded_content

Part of the [telemetry review](../detectors.md#what-the-telemetry-says) of the [preseed detectors](../detectors.md).

**Status (2026-10-31):** the fixes below shipped in the fleet-wide detector overhaul (see the main page's overhaul status note). No deviations from the proposals below.

embedded_content is a binwalk-style raw magic scan whose true positives are exactly its design target — real ZIP payload bundles, PE droppers in .data/.rsrc/.rdata/overlay, and PNGs grafted into .text — but its precision collapses on any binary that *handles* those formats, because a 4-byte magic (PK\x03\x04, \x7fELF, MSCF) with zero header-field validation fires on comparison immediates in code and format-recognition constants in data. Its PNG path emits a hit on the bare 8-byte signature even when the stream dies on the very first chunk, and reports that failure ("truncated chunk data") at medium severity when it is evidence against a PNG. The ignore mass is self-inflicted volume: one lead per ZIP member LFH (re-seeded every run the sample recurs), one lead per resource icon, capped at 16 per sample (the cap is routinely hit), all dumped as comments into the single shared "file-level" post where one disputed sibling condemns the whole batch — including leads the disputing comment itself affirms. Structural header validation plus per-artifact span measurement would remove most of the junk without touching a true positive, since every real catch is a structurally real file.

## Where the false positives come from
---

### 1. Magic immediate in code

scan_embedded reports a hit on a bare 4-byte magic with no validation of the header fields that must follow, so magic values appearing as instruction immediates in format-handling code (cmp/ldc.i4 against the magic) or as format-recognition constants/templates in data are reported as embedded artifacts. Any binary that parses ZIP/ELF/CAB necessarily contains those magics in its code; the CAB branch even keeps the hit when its declared size overruns the file (it only drops the size, :180-182), and the ELF branch checks nothing past the 4 magic bytes — it can flag an 'ELF executable' in a .rodata far too small to hold one.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/embedded_content.rs:150-229 (ZIP :170-176, CAB :177-188, ELF :199-205)`

### 2. Mono thread flood sweep

seed.rs buckets every hit by containing function only, so all file-level hits from every detector land in the one None bucket → a single 'file-level' post per sample scope, one comment per finding; embedded_content pours up to 16 individual leads into it (one per ZIP member, one per icon). Telemetry grades leads by thread resolution, so one disputing comment about any sibling lead marks the whole batch disputed — including leads the comment never addresses, leads it explicitly affirms, and real artifacts. This is partly a grading artifact, but the detector's per-hit flooding of the shared thread is what enables it and buries its own true positives.

*Where:* `thorpal/src/thorpal/seed.rs:202-207 (buckets keyed only on hit.function) + :660-665 (single 'file-level' post name); embedded_content.rs:23,63-68 (up to 16 individual hits)`

### 3. Png signature constant escalation

check_png emits a hit for the bare 8-byte PNG signature even when parsing dies on the very first chunk, and build_hit's severity policy (low() only for anomaly-free images inside .rsrc, :88-92) reports that failed parse at medium/suspicion 40 — treating 'truncated chunk data' or garbage chunk types as an anomalous image rather than as proof of a non-image. Graphics libraries (Delphi image units, codec detection tables) store the full 8-byte signature as a recognition constant followed by struct fields, which always 'parses' as truncated. A non-ASCII chunk type is likewise proof of a non-PNG, yet still seeds at medium. First-chunk parse failure is evidence against a PNG, not for a payload.

*Where:* `embedded_content.rs:260-327 (check_png emits unconditionally) + :88-92,110-121 (anomalous or non-.rsrc ⇒ medium())`

### 4. Weak magic ciphertext coincidence

The gzip branch needs only 3 magic bytes plus FLG ≤ 0x1f (~2^-27 per offset), which still fires by chance inside encrypted/high-entropy blobs; the lead then asserts a bogus format even when the blob is a genuinely interesting encrypted payload, handing the disputer an easy factual kill.

*Where:* `embedded_content.rs:189-198`

## Why leads get ignored
---

- **Zip member multiplication** — ZIP hits carry size: None, so the scanner resumes 1 byte past the magic (:218) and flags every member's local-file-header as a separate 'ZIP archive' — an archive emits one lead per member, re-emitted in every run the sample appears in. Unsized ELF/PE/gzip hits similarly let the scan walk into a real embedded artifact's interior and flag its nested magics.
- **Cap filling per sample flood** — One lead per magic hit up to MAX_EMBEDDED_HITS = 16 per sample scope, multiplied by per-sample scopes in multi-sample sessions, floods the shared file-level thread far past what any agent triages; agents answer the thread once (if at all) and the remaining leads count as ignored.
- **Cross run corpus repetition** — The corpus reuses samples across runs and seeding has no memory, so an artifact engaged once is re-seeded verbatim in every later run and never engaged again — pure dead volume in the telemetry.
- **Linux corpus weak magic junk** — A Linux/Go-heavy corpus multiplies the bare-magic FP modes (ELF magic-compare constants in static binaries, gzip coincidences, ZIP immediates), and agents visibly learned to skip the class without posting disputes — junk recognized on sight buys zero engagement.
- **Low severity png tail** — Clean in-.rsrc PNGs seed at untrusted/suspicion 15 (the low() branch) — correctly ranked, but still one lead per icon, so they pile up at the bottom of the pre-ranked thread and are structurally never read.

## What would reduce the noise
---

Static-side changes, ordered as proposed; each was checked against the detector's true positives so the fix does not suppress what the detector gets right.

### Validate structural headers before reporting ZIP/ELF/CAB/gzip/PE hits

In scan_embedded (embedded_content.rs:150-229): ZIP (:170-176) — parse the local-file-header at the match (version-needed u16@4 ≤ 100, method u16@8 ∈ {0,8,9,12,14,93..=99}, name-len u16@26 in 1..=512 with printable name bytes, header+name within file); ELF (:199-205) — validate e_ident (EI_CLASS@4 ∈ {1,2}, EI_DATA@5 ∈ {1,2}, EI_VERSION@6 == 1, pad bytes 9..16 zero — leave EI_OSABI/EI_ABIVERSION unconstrained, real Linux ELFs carry OSABI 3), plus e_type u16@16 ∈ 1..=4, e_version u32@20 == 1, e_ehsize matching the class, and ≥52/64 bytes of room; CAB (:177-188) — require reserved1 u32@4 == 0, version bytes @24/@25 == 3/1, cbCabinet in 36..=remaining, coffFiles < cbCabinet, and drop the hit entirely on failure (today :180-182 only drops the size); gzip (:189-198) — additionally require XFL byte@8 ∈ {0,2,4} and OS byte@9 ∈ 0..=13 ∪ {255}; PE — in embedded_pe_at (:237-246) also require e_lfanew ≤ 0x10000, a known COFF machine u16 at pe+4, and optional-header magic ∈ {0x10b,0x20b}.

*Effort:* medium \
*True-positive risk:* Low: real embedded ZIPs have genuine LFHs, real PE droppers have real COFF headers, and a real embedded ELF has a full header, so all pass. Immediates fail on the fields after the magic: a ldc.i4 ZIP-magic compare reads an out-of-range version-needed, a MSCF cmp immediate reads a nonzero reserved1, and ld.so's 12-byte e_ident templates pass the e_ident checks but fail e_type (reads 0x457f from the adjacent constant).

Addresses: magic immediate in code, weak magic ciphertext coincidence, linux corpus weak magic junk.

### Gate PNG hits on a plausible first chunk; stop reporting instant parse failure as an anomalous image

In check_png (:260-327): require the first chunk to look like an IHDR (length == 13, type bytes == "IHDR", width/height in 1..2^24, bit depth ∈ {1,2,4,8,16}, color type ∈ {0,2,3,4,6}) before emitting any Embedded — return None otherwise (change the call site at :168-169 to flat_map). In build_hit (:88-92), keep medium() for anomalous streams only when the stream had a valid IHDR and walked ≥2 ASCII-typed chunks (the IDAT-steganography shape the module doc targets); a walk that dies on chunk 1 or hits non-ASCII chunk types never reaches build_hit under the gate.

*Effort:* small \
*True-positive risk:* Low: real PNG payloads (grafted into .text, carried in .rsrc, or the motivating payload-in-PNG graft) are real streams that reach IEND with measured sizes, so they have valid IHDRs. An attacker corrupting IHDR *fields* (not just CRC) to dodge the gate also breaks every real PNG loader, defeating the disguise; CRC-only tampering still passes (field checks, not CRC, gate emission).

Addresses: png signature constant escalation, magic immediate in code.

### Measure archive spans and emit one lead per artifact, not per member

After a validated ZIP LFH, walk the member chain (or locate the EOCD) and set Embedded.size to the whole archive span so the existing skip logic (:155-165) jumps it — one lead per archive instead of one per member. For validated embedded PEs, estimate the raw extent from the section table (max of PointerToRawData+SizeOfRawData, clamped to remaining file — SizeOfImage is virtual and overshoots raw spans, risking skipping a following artifact); for ELF use e_shoff + e_shnum*e_shentsize when sane. Include the member count in the lead text ('ZIP archive, N members').

*Effort:* medium \
*True-positive risk:* A real multi-member ZIP collapses to one lead carrying the same offset and a member count — the thread is still seeded and the extraction pointer preserved; nested-magic true positives inside a measured span (a PE hiding a second PE in its own body) would be subsumed into the outer lead. Streaming ZIP entries (bit-3 data descriptors) break the member walk early — fall back to the EOCD span so members past the break are still covered.

Addresses: zip member multiplication, cap filling per sample flood, mono thread flood sweep, cross run corpus repetition.

### Roll up small clean resource PNGs into one lead

In EmbeddedContent::run / build_hit: when >2 clean (anomaly-free) in-.rsrc PNGs are found, emit a single low() lead listing count, size range, and each offset/size pair (`N PNG resources, <min>–<max> bytes, @<offsets>… — verify consumed as images`), keeping individual leads only for PNGs ≥16KB or structurally anomalous (per fix 2's plausible-stream definition). Exempt the rollup from the 16-hit cap accounting so it never displaces executable/archive hits.

*Effort:* small \
*True-positive risk:* A large resource PNG stays an individual lead under the ≥16KB carve-out, and the payload-in-resource-PNG shape is inherently large/anomalous. A small real payload PNG would move into the rollup, which is why the rollup body must list every offset/size so the pivot stays extractable.

Addresses: low severity png tail, mono thread flood sweep, cap filling per sample flood.

### Skip ELF matches inside .gnu.prelink_undo

In EmbeddedContent::run/build_hit, when containing_section (:335-342) returns .gnu.prelink_undo for an ELF hit, drop it (prelink stores a legitimate copy of the pre-link ELF image there by design; build_hit becomes fallible/Option-returning); optionally note it once at low().

*Effort:* small \
*True-positive risk:* Low: a payload hidden in a section named .gnu.prelink_undo on a non-prelinked binary would still surface via the entropy/section detectors; could additionally require real prelink markers before skipping.

Addresses: linux corpus weak magic junk.

### Split the file-level bucket per technique so one dispute cannot sweep unrelated leads

In the seed.rs run loop (:202-207) and bucket_name (:660-665): key None-function hits by (None, technique id) — post names like 'file-level-t1027-009' — so embedded-content leads get their own thread instead of sharing one post (and one thread-level grade) with network-indicator, debugger-evasion, and every other file-level detector. The bucket-map key change ripples into partition_suppressed and derive_questions signatures (same file). Combined with fixes 3/4 the thread holds a handful of validated leads instead of 16 mixed ones.

*Effort:* medium \
*True-positive risk:* None — pure forum topology; no lead content or severity changes, and true positives become easier to engage because they no longer sit under icon comments and other detectors' disputes.

Addresses: mono thread flood sweep.

## Verification notes
---

Each mechanism, anchor, and fix was re-checked against the code:

- All four mechanism code anchors verified against source: scan_embedded spans :150-229 with ZIP :170-176, CAB :177-188 (size-only filter at :180-182), gzip :189-198 (FLG ≤ 0x1f at :190), ELF :199-205; check_png :260-327 emits an Embedded unconditionally; build_hit severity at :88-92 (low() only for clean .rsrc images, else medium() = Unsure/40 per detections.rs:223-231); MAX_EMBEDDED_HITS=16 at :23 with the take() at :63-68; seed.rs buckets keyed only on hit.function at :202-207 and bucket_name returns the fixed 'file-level' at :660-665.
- Refined png-signature-constant-escalation: build_hit's medium() comes from anomalies OR non-.rsrc location (a signature constant in .data is medium even anomaly-free); the core defect is check_png emitting a hit for a bare signature whose first chunk fails to parse. Fix 2 unchanged in substance.
- Fix 1 corrected: the ELF e_ident 'zero pad' must not cover EI_OSABI/EI_ABIVERSION (bytes 7-8) — real Linux ELFs and ld.so templates carry OSABI 3, and requiring zero there would mis-specify real ELF validation; pad check moved to bytes 9..16. All six fixes are purely static and name real functions/lines.
- Fix 3 sharpened: embedded-PE span must come from section-table raw extents (max PointerToRawData+SizeOfRawData), not SizeOfImage, which is virtual-aligned and overshoots the raw span — an overshoot could skip a genuinely separate artifact right after the embedded PE. Added the streaming-ZIP (bit-3 data descriptor) caveat with the EOCD fallback.
- Fix 4: the rollup body must list each offset/size so a rolled-up true positive remains extractable.
- The thread-level grading attribution is a telemetry artifact rather than a detector heuristic and is folded into mono-thread-flood-sweep; fix 6 (per-technique file-level buckets) plus fixes 3/4 attack it from the detector side.
