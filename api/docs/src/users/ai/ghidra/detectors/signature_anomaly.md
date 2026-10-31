# signature_anomaly

Part of the [telemetry review](../detectors.md#what-the-telemetry-says) of the [preseed detectors](../detectors.md).

**Status (2026-10-31):** the fixes below shipped in the fleet-wide detector overhaul (see the main page's overhaul status note). No deviations from the proposals below.

signature_anomaly emits exactly one hit max per binary (run() returns vec![hit]), and its strong leads are grafted/stolen-key Authenticode findings (a signer that contradicts the VersionInfo company name) anchoring T1553.002. Its false-positive mechanism is a claim-precision bug, not a concept bug: authenticode_digest is spec-faithful and hashes bytes appended after the declared certificate table, so a file whose signature verifies over the signed scope but carries a post-signing appendix (such as a second PKCS#7 fragment) is reported with the maximal 'digest does NOT match — modified after signing' wording, which an agent can disprove by recomputing the scope-limited digest even though the file genuinely was modified after signing. Ignored leads are an engagement-plumbing gap, not flood: derive_questions covers packing, gates, config, crypto, C2, ransomware, and evasion but has no T1553 theme, so every signature lead lands as a statement comment under the always-kept file-level post, which the question-driven delegation loop never has to touch, and the body names no concrete byte range to pursue. Splitting the overlay case into an accurately-worded arm and deriving a question from the strong arm addresses the false-positive mechanism and much of the ignore problem without touching the grafted-signature pattern.

## Where the false positives come from
---

### 1. Overlay after cert table counted as digest mismatch

authenticode_digest hashes every byte outside the skip ranges, including the span AFTER the declared certificate table ([table_end, EOF)). When data was appended beyond the declared table — for example a second PKCS#7 fragment placed after the table's end — the computed digest includes the appendix and mismatches the stored messageDigest, and the detector's single Some(false) arm asserts the maximal claim 'the file's Authenticode digest does NOT match the signed digest — the binary was modified after signing', implying a tampered host or grafted cert. The scope-limited digest (stopping at the table start) actually matches: the signature is genuine over the signed region and the real anomaly is a post-signing appendix. Note the hashing itself is spec-faithful (real verifiers also flag these files), so this is purely a wording/claim bug: the code already computes a distinguishing fact (table_is_tail = end == raw.len(), false here) but only uses it to append a sentence in the tail case, and never re-verifies with the trailing span excluded or softens the claim. table_is_tail alone cannot separate overlays from grafts, which is why the fix re-verifies the digest instead of keying on layout.

*Where:* `thorpal/src/thorpal/detect/authenticode.rs:160-168 (trailing [table_end,EOF) fed to hashers) + detections/defense_evasion/signature_anomaly.rs:75-109 (single strong-wording arm for Some(false))`

## Why leads get ignored
---

- **No question derivation for t1553** — The seeder's derive_questions covers unpacking (T1027.002), host-identity gates, decoded config, crypto keys, C2, ransomware (T1486), and the T1622/T1497 evasion rollup — exactly seven themes, none matching T1553 — so every signature_anomaly hit is seeded only as a finding comment under the always-kept file-level statement post (partition_suppressed never suppresses the file-level bucket, but also nothing promotes it). The main agent's delegation loop is driven by open questions (seed.rs:47 'Every open question forces the main agent to delegate and resolve it'); statement comments get engaged only when an agent voluntarily reads and picks them up.
- **Verdict without a pursue target** — The strong arm's body states a conclusion and exhorts 'pursue what was added' but names no byte range beyond the table itself, no delta, and no artifact — there is nothing concrete to delegate. Engagement appears to track a built-in contradiction worth chasing (signer vs VersionInfo company): when the cert is an obvious self-signed decoy with no VersionInfo, the lead merely corroborates what the session already believes about an overtly malicious file and buys no next step.
- **Untrusted floor low arm** — The unparseable-blob arm emits confidence untrusted / suspicion 15 (low()), the grade the runtime-noise suppression policy itself characterizes as 'the ubiquitous-API grade that also fires on benign code'; being file-level it is always seeded (never rolled up), but it sits at the bottom of any triage ordering and is never picked up. It is noise-free in volume but structurally unengageable.

## What would reduce the noise
---

Static-side changes, ordered as proposed; each was checked against the true positives above so the fix does not suppress what the detector gets right.

### Split 'data appended after the certificate table' into its own accurately-worded arm

In authenticode::inspect (authenticode.rs), when the full-scope digest mismatches and end < raw.len(), recompute the four digests with the trailing span [table_end, raw.len()) also excluded (one extra skips entry into authenticode_digest, which already takes a sorted skip list) and surface two new fields on AuthenticodeInfo: trailing_bytes: u64 and signed_scope_matches: `Option<bool>`. In signature_anomaly.rs add a third match arm for digest_matches==Some(false) && signed_scope_matches==Some(true): keep technique T1553.002 and high() confidence but word it correctly — 'signature verifies over the signed scope, but N bytes were appended after the declared certificate table (@0x..-0x..); carve and identify the appendix (secondary signature blob, implant, or payload)' — and drop the 'digest does NOT match' / 'grafted certificate table' claims from that arm.

*Effort:* medium \
*True-positive risk:* None against the grafted-signature pattern: a cert table grafted from another binary matches neither the full-scope nor the scope-limited digest (and tail grafts have no trailing span, so signed_scope_matches equals digest_matches and they keep the strong arm); a host tampered inside the signed region keeps mismatching both scopes. Grafted-signature leads therefore still take the strong arm. A file with a post-signing appendix still seeds a likely-grade lead — the appendix can be real actor tradecraft — just with a claim an agent's recomputation corroborates instead of disproves.

Addresses: overlay after cert table counted as digest mismatch, verdict without a pursue target.

### Derive a q-signature question from strong signature anomalies

Add a signature_question theme to derive_questions in seed.rs (alongside the existing seven): fire when any T1553.002 hit has confidence_rank >= confidence_rank(Confidence::Likely), cite the file-level bucket via evidence_line (same unscoped-citation convention the other themes use), with a title that stays accurate for both strong arms once fix 1 lands — e.g. 'The code signature does not cover the file as it exists on disk — what was added or changed after signing?' — and one completion-gate task ('Identify the bytes the signed digest fails to cover — tampered region, grafted table, or appended data — and post byte evidence of what they contain'). Place it late in the fixed priority order (after ransomware_question at seed.rs:1203, before evasion_question at :1205) so it never displaces unpacking/gate questions under MAX_SEED_QUESTIONS=6.

*Effort:* medium \
*True-positive risk:* No suppression risk — it only raises engagement on the grafted-signature pattern. Cost is bounded: the detector emits at most one hit per binary (run() returns a single-element vec), and the low (untrusted) arm never clears the Likely bar, so the question queue grows by at most one per signed-and-anomalous binary.

Addresses: no question derivation for t1553.

### Carry the concrete anomaly shape in the finding body

In signature_anomaly.rs content formatting, add statically-computed specifics so an unengaged lead still delivers its conclusion and an engaging agent has a target: the trailing range and byte count when !table_is_tail (from fix 1's trailing_bytes), and a chain-shape note when cert_names.len() <= 1 (wording must cover both the single-CN and '`<no CN recovered>`' cases: 'at most one CN, no issuer chain recovered — consistent with a self-signed decoy certificate rather than a stolen/grafted identity'). Keep confidences and suspicions unchanged.

*Effort:* small \
*True-positive risk:* Purely additive text on the same hits; no lead is demoted or dropped, so no true-positive pattern can be suppressed. Only guard: the decoy-cert phrasing must stay subordinate to the mismatch claim so it cannot be read as benign-ing the finding on true grafts with short recovered chains (cert_names is a bounded heuristic byte scan, MAX_CERT_NAMES=8, and may under-recover on real chains).

Addresses: verdict without a pursue target, untrusted floor low arm.

## Verification notes
---

- Mechanism 1's code anchors: authenticode.rs:160-168 is the cursor loop whose final branch (lines 167-168) hashes [table_end, EOF) into all four digests, and signature_anomaly.rs:75-109 is the single Some(false) arm carrying the 'does NOT match / modified after signing' wording; table_is_tail (authenticode.rs:72) only ever appends a sentence (signature_anomaly.rs:69-74).
- The digest computation is Authenticode-spec-faithful — real verifiers report tampering on these files — so mechanism 1 is a claim-wording/arm-splitting bug, not an incorrect digest.
- Ignore driver 1 end to end: derive_questions (seed.rs:1189-1208) calls exactly seven theme functions whose filters are T1027.002 (1257), T1486 (1582), T1622/T1497* (1617), plus kind/artifact-gated gate/config/key/C2 themes — no T1553 path; file-level buckets (function=None, which both detector arms emit) are never suppressed by partition_suppressed (seed.rs:489 requires function.is_some()) and seed as a Kind::Statement post ('file-level', scoped 's2-file-level') with one Statement comment per finding (seed_bucket, seed.rs:591-653), while only Kind::Question(Unsolved) posts drive delegation (seed.rs:47, thorpal.rs:137).
- Ignore driver 3: low() = (Confidence::Untrusted, 15) at detections.rs:230-232, and the quoted 'also fires on benign code' characterization is real (seed.rs partition_suppressed doc, lines 440-441, and low()'s own docstring); the file-level untrusted lead is always seeded (never rolled up) — its problem is triage floor, not suppression.
- All three fixes are purely static and implementable at the named seams: authenticode_digest already takes a sorted skip list (one extra entry suffices); confidence_rank exists (detections.rs:713) for fix 2's Likely bar; SeedQuestion carries tasks seeded as completion gates (seed_question_posts, MAX_SEED_TASKS_PER_QUESTION=4) and the insertion point between ransomware_question (seed.rs:1203) and evasion_question (:1205) is real with MAX_SEED_QUESTIONS=6. Fix 1's tp_risk logic holds: a grafted table mismatches both full-scope and scope-limited digests, and tail grafts have an empty trailing span, so no grafted-signature lead can flip to the new arm.
- Fix 2's proposed question title stays accurate once fix 1 lands (the overlay arm's signature verifies over the signed scope, so 'does not verify' would be wrong for it), and it follows the existing unscoped evidence_line citation convention ('#file-level' even for scoped samples — a pre-existing quirk shared by all themes). Fix 3's chain-shape wording covers the zero-CN case ('`<no CN recovered>`').
- signature_anomaly is registered (defense_evasion.rs:139) but missing from the detectors.md user-docs table — a docs gap only.
