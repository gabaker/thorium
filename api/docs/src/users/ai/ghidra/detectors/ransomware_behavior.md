# ransomware_behavior

Part of the [telemetry review](../detectors.md#what-the-telemetry-says) of the [preseed detectors](../detectors.md).

**Status (2026-10-31):** the fixes below shipped in the fleet-wide detector overhaul (see the main page's overhaul status note). Deviations: the cross-detector crypto-corroboration gate and the T1083 re-homing need cross-module plumbing and did not ship; the catalog rewording, onion-shape gating, and per-function enumeration-surface merge did.

ransomware_behavior is an 8-API catalog (FindFirstFile/FindNextFile/GetDriveType/GetLogicalDrives/GetLogicalDriveStrings/GetVolumeInformation/MoveFile/SetFileAttributes) plus a ransom-note string pass, and the project's own docs (detectors.md:323) say it "exists to corroborate crypto_capability, not to stand alone" — yet it seeds every API call site as a standalone per-function T1486 claim with ransomware-intent reasoning. The dominant failure is dual-use filesystem/drive APIs in non-ransomware malware — RAT file-manager commands, recon dir listings for C2 exfil, volume-serial bot-ID generation, self-update/self-delete renames — which agents refute function by function and re-attribute to T1083/T1082. Ignored leads are flood by construction: one finding per (function x API x thunkness), zero-information "imported, no call site" file-level hits, and itemized rollup lines. The value is real but concentrated: ransom-note strings (e.g. "The only copy of the private key...") plus MoveFile and enumeration/rollup leads inside genuine ransomware, where they corroborate the encryption workflow; a corroboration gate plus per-function merging would keep essentially all of that while deleting most of the false positives and ignored leads.

## Where the false positives come from
---

### 1. Dual use fs API overclaim

The catalog flags ubiquitous filesystem/drive/rename APIs as ransomware target-discovery with reasoning that asserts encryption intent ("ransomware recurses the tree to find victim files to encrypt", "renames each encrypted file to append a marker extension"), with no gate requiring any co-located crypto evidence, downstream file write, or extension-rename pattern. In RATs, backdoors, and stealers these APIs implement file-manager C2 commands (such as a 'list drives' handler that opens no files), recursive directory-listing recon for exfil, volume-serial bot-ID generation, and self-update/self-delete (a rename of the malware's own module) — so agents refute the T1486 claim function by function and re-attribute to T1083/T1082/self-update.

*Where:* `thorpal/src/thorpal/detect/detections/impact/ransomware_behavior.rs:29-78 (APIS catalog; emitted ungated at ransomware_behavior.rs:111 via detections.rs:1079-1149 api_call_hits)`

### 2. Unnamed library code escapes filter

The library/runtime downgrade is name-based (is_library_function + per-toolchain function-name markers), and the toolchain fingerprint needs 2 points where each marker class (function names, strings) contributes at most 1 — DELPHI_FUNCTION_MARKERS like "@system@" never match stripped FUN_xxx names, so a stripped Delphi/Inno/SFX binary caps at 1 string point and never fingerprints. Without a fingerprint partition_suppressed never engages, so Delphi RTL helpers (such as the SysUtils long-path/canonicalization helper), the WinRAR SFX stub's setup-script command handlers, and the Inno Setup engine seed as per-function findings instead of rolling up — the detector flags stock installer/RTL code as ransomware.

*Where:* `thorpal/src/thorpal/detect/detections.rs:921-960,1006-1039 (name-only runtime_classification/is_library_function); thorpal/src/thorpal/detect/toolchain.rs:71-80,136-153,188-190 (Delphi markers, 1-point-per-class score, 2-point rule); seed.rs:452-459 (suppression requires a fingerprint)`

### 3. Rollup collateral and static link overclaim

Suppressed hits are itemized as rollup lines under one runtime-noise thread whose body claims "This binary statically links a recognized {toolchain} runtime". The fingerprint counts import names and strings (toolchain.rs detect() chains imports into the name list), so a dynamically-linked MFC42.DLL binary still fingerprints as MFC and the static-link claim is provably false. When an agent disproves that claim, or disputes any sibling entry on the thread, every T1486 line on that thread grades disputed — the detector's rollup lines absorb thread-level disputes they did not individually cause.

*Where:* `thorpal/src/thorpal/seed.rs:548-557 (rollup body wording); seed.rs:511-578 (one thread hosts all suppressed findings); toolchain.rs:94-107 (imports count toward the fingerprint)`

### 4. Decoy or dead import seeding

api_call_hits emits a file-level "X imported" hit when an import has zero resolved call sites, and emits call-site hits even when the call's result is provably dead. Crypter stubs carry decoy imports and machine-generated junk-API padding precisely to trigger this, so the detector seeds leads on imports that cannot execute the claimed behavior.

*Where:* `thorpal/src/thorpal/detect/detections.rs:1099-1113 (sites.is_empty() => "{norm} imported" hit at site 0x0)`

### 5. Onion substring coincidence

The NOTE_STRINGS needle ".onion" is a bare substring match over all defined strings, so library guard constants that merely contain the TLD (c-ares' RFC 7686 ".onion"/".onion." refusal strings, used to refuse resolving .onion names) are reported as "the victim-facing note a ransomware drops" — benign library data matched by an over-broad needle and over-claimed by the shared template reasoning.

*Where:* `thorpal/src/thorpal/detect/detections/impact/ransomware_behavior.rs:89 (".onion" needle) + detections.rs:1402 (hay.contains(n) substring match)`

## Why leads get ignored
---

- **Per function per API finding flood** — One finding is emitted per (calling function x catalog API x thunked/direct), across 8 ubiquitous APIs that nearly every Windows binary touching the filesystem calls. A single file-manager or installer yields many T1486 threads/comments at untrusted/15 whose reasoning itself concedes "Common, hence low confidence" — agents triage them out wholesale.
- **Rollup line mass** — Suppressed noise is itemized line-by-line under the runtime-noise post (body capped at 40 lines, but up to 500 per-finding provenance entries per post), and telemetry counts each provenance entry as a lead. These are suppressed-by-design, so near-total ignoring is expected.
- **Unreferenced import leads** — Imported-but-never-called catalog APIs seed file-level "X imported" hits anchored at Site 0x0 with nothing to decompile or dispute cheaply; for APIs this common (every Delphi/MSVC binary imports FindFirstFileA) the import's mere presence carries no information, so agents skip them.
- **Fleet duplicate sessions** — The same binaries recur across sessions (and in multi-sample sessions), re-seeding byte-identical leads that were already ignored or refuted elsewhere — a corpus/fleet artifact that multiplies this high-volume detector more than precise ones.

## What would reduce the noise
---

Static-side changes, ordered as proposed; each was checked against the true positives above so the fix does not suppress what the detector gets right.

### Gate per-function T1486 enumeration claims on in-binary crypto or ransom-note corroboration

In RansomwareBehavior::run (ransomware_behavior.rs:106-129), run the NOTE_STRINGS pass first and compute a corroboration flag: any note-string hit, OR crypto evidence in the same binary — share crypto_capability's catalogs via a pub(crate) helper in impact.rs (match ctx.imports against its APIS keys like CryptEncrypt/BCryptGenerateSymmetricKey — crypto_capability.rs:224/259 — using normalize(), and scan ctx.initialized_bytes() — memoized at context.rs:687-705 — for STRONG_SIGS/WORD_SIGS at crypto_capability.rs:42/76). When corroboration is absent, collapse all api_call_hits output into one file-level low() summary hit ("filesystem/drive enumeration surface: FindFirstFile, GetDriveType, ... — no co-located crypto evidence") instead of per-function findings. This implements the cross-detector correlation rule the detector's docs already call for (detectors.md:333-335), and keeps the q-ransomware join coherent on its own: ungated sessions collapse to one T1486 bucket so ransomware_question's >=2-bucket test stops firing on pure enumeration.

*Effort:* medium \
*True-positive risk:* Note-string true positives trivially pass the gate. True-positive API leads sit in true-ransomware binaries that should carry notes or crypto, but that cannot be guaranteed, so the honest residual is: a corroborated-elsewhere ransomware stage that only enumerates gets demoted to the retained one-line file-level summary rather than dropped.

Addresses: dual use fs api overclaim, per function per api finding flood.

### Merge the catalog's hits into one enumeration-surface finding per function

Post-process api_call_hits output in run(): group hits by hit.function, emit a single DetectionHit per function naming all matched APIs with the union of sites (severity = max of members via confidence_rank, keep the thunk note). Today one RAT file-manager function can draw up to 5 separate T1486 findings that agents must refute individually, and each is a separately-graded lead comment under the same bucket post.

*Effort:* small \
*True-positive risk:* None identified: no true-positive pattern depends on APIs being separate findings; the merged hit retains every site (DetectionHit::sites) for the validator and the markup pass, and true-positive functions still seed a (single) thread.

Addresses: per function per api finding flood, dual use fs api overclaim.

### Stop seeding imported-but-uncalled hits for this catalog

Skip the sites.is_empty() fallback (detections.rs:1099-1113) for ransomware_behavior — either add a skip_unreferenced flag to api_call_hits or filter address==0 "imported" hits in run() (they are the only hits DetectionHit::at builds with address 0). An unreferenced FindFirstFile/GetVolumeInformation import is base-rate noise in benign binaries and a deliberate decoy in crypters.

*Effort:* small \
*True-positive risk:* Near zero: an imported-but-uncalled enumeration API is not what makes ransomware leads true positives. Leave crypto_capability's own use of the fallback untouched — an uncalled CryptEncrypt is a different-prior signal.

Addresses: decoy or dead import seeding, unreferenced import leads.

### Require an onion-address shape instead of the bare .onion needle

Replace the ".onion" entry in NOTE_STRINGS (ransomware_behavior.rs:89) with a dedicated post-filter over ctx.strings in run(): match only when >=16 base32 chars ([a-z2-7]) immediately precede ".onion" (v2/v3 onion label), resolve xrefs and emit via referenced_literal_hit as string_artifact_hits does today. Optionally also drop the bare "your files" needle (ransomware_behavior.rs:83, fully shadowed by "all your files" plus the decrypt/recover phrases) while touching the list.

*Effort:* small \
*True-positive risk:* None expected: true-positive onion strings are full addresses (16-char base32 labels for v2, longer for v3), which pass the shape check; c-ares' ".onion"/".onion." guards fail it. Ransom-note strings still match other needles ("all your files", "have been encrypted", "decrypt your file").

Addresses: onion substring coincidence.

### Fix the rollup post's static-link claim and collapse rollup lines per API

In seed_suppressed_rollup (seed.rs:548-557) reword "This binary statically links a recognized {} runtime" to a claim the fingerprint actually supports ("was built with the recognized {} toolchain — its runtime/support code is identifiable, statically linked or via its import table"; the fingerprint counts import names, so dynamically-linked MFC fingerprints too), and group the itemized body lines by (technique, API/content-prefix) with counts ("- T1486: FindFirstFile x12 sites") instead of one line per finding. Keep per-finding provenance entries for telemetry continuity — note this means the rollup lead/ignore counts in telemetry will not drop; the fix removes the disproven factual claim and the body flood.

*Effort:* small \
*True-positive risk:* Low: true-positive rollup leads are thread-level confirmations in ransomware sessions; a per-API collapsed line still represents them and provenance still records each. The reworded claim removes the factual assertion agents disprove (dynamically-linked MFC42.DLL).

Addresses: rollup collateral and static link overclaim, rollup line mass.

### Re-home ungated enumeration hits to a discovery claim and keep the q-ransomware join working

For API hits that fail the fix-1 corroboration gate, emit them under T1083 File and Directory Discovery — a new AttackTechnique const in a new detections/discovery.rs module using the existing AttackTactic::Discovery variant (detections.rs:76; no T1083 const exists anywhere in detect/ today) — with recon-neutral reasoning instead of T1486 ransomware wording; this is exactly the re-attribution agents perform when they dispute these leads. Must land together with a seed.rs change: extend ransomware_question (seed.rs:1577-1604) to also fire on a T1486 (crypto/note) bucket co-occurring with a T1083 enumeration bucket, since today it requires >=2 distinct buckets carrying T1486 hits.

*Effort:* medium \
*True-positive risk:* Main risk is the question gate: without the paired ransomware_question update, a ransomware session whose note/crypto evidence lands in one bucket while enumeration became T1083 loses the q-ransomware seed (agents answer #q-ransomware, i.e. the question drives engagement). With the join updated, ransomware sessions keep both the question and their note/crypto T1486 leads; gated (corroborated) enumeration hits still claim T1486.

Addresses: dual use fs api overclaim.

### Fingerprint installer/SFX stubs and stripped Delphi so untrusted buckets roll up

Extend toolchain.rs: let two distinct strong Delphi string markers ("this program must be run under win32" + "borland"/"portions copyright" — the last is a new marker) count as the required 2 points without named RTL functions (today the score closure at toolchain.rs:136-153 caps each marker class at 1 point and DELPHI_FUNCTION_MARKERS never match stripped FUN_xxx names, so the 2-point rule at toolchain.rs:188-190 fails); add Inno Setup markers ("inno setup" — Delphi-built, can fold into Delphi) and a new Toolchain variant for the WinRAR SFX stub (MSVC-built with unnamed CRT functions; markers = the setup-script vocabulary "Presetup", "TempMode", "SavePath") with label and is_toolchain_runtime arm. With a fingerprint, partition_suppressed's is_noise policy (seed.rs:461-465) already rolls untrusted-grade buckets up without needing per-function name matches.

*Effort:* medium \
*True-positive risk:* Suppression demotes to the rollup rather than dropping (seed.rs records every suppressed finding, and agents do confirm rollup FindFirstFile/FindNextFile lines in real ransomware); unsure-grade hits (MoveFile, GetLogicalDriveStrings, note strings) are not noise unless runtime-located by name, so a Delphi-built true ransomware keeps loud leads. Two limits: file-level buckets are never suppressed (a file-level Inno hit still seeds), and a string-only fingerprint also rolls up the sample's own untrusted-grade user code — acceptable since that is precisely where the false positives concentrate.

Addresses: unnamed library code escapes filter, per function per api finding flood.

## Verification notes
---

- Code anchors: APIS at ransomware_behavior.rs:29-78, ungated emit at :111, api_call_hits at detections.rs:1079-1149, imported fallback at :1099-1113, substring match at :1402, ".onion" needle at ransomware_behavior.rs:89, runtime_classification/is_library_function at detections.rs:921-960/1006-1039, Delphi markers at toolchain.rs:71-80 and 2-point rule at :188-190 (with :136-153, the score closure that caps each marker class at 1 point — the actual reason stripped Delphi cannot reach 2), rollup wording at seed.rs:548-557, ransomware_question at seed.rs:1577-1604, is_noise at seed.rs:461-465.
- The fingerprint counts import names (toolchain.rs:94-107), so the static-link claim is genuinely wrong for import-linked MFC.
- Fix 1 is implementable: STRONG_SIGS/WORD_SIGS/CryptEncrypt/BCryptGenerateSymmetricKey exist in crypto_capability.rs (lines 42/76/224/259), AnalysisContext::initialized_bytes is memoized (context.rs:687-705), and detectors.md:333-335 calls for the cross-detector correlation rule. Fix 1 alone keeps the q-ransomware join coherent (ungated hits collapse to one bucket, so the >=2-bucket test stops firing on pure enumeration).
- Fix 6: no T1083 exists anywhere in detect/, AttackTactic::Discovery already exists (detections.rs:76) so the new const is additive, and ransomware_question (seed.rs:1577-1604) really does count distinct buckets carrying T1486 hits (crypto_capability also emits T1486, which is why the join works today) — the claimed coupling is real.
- Fix 7: once ANY toolchain is fingerprinted, partition_suppressed suppresses buckets whose hits are all untrusted-grade without per-function name matches, so a string-only Delphi fingerprint does roll the thunk hits up. File-level buckets are never suppressed, and WinRAR SFX needs a new Toolchain variant since the stub is MSVC-built with unnamed CRT functions. "portions copyright" is a new marker (not currently in DELPHI_STRING_MARKERS).
- Fix 5 keeps per-finding provenance entries, so the telemetry rollup lead count will not drop — it fixes the disproven static-link claim and the body flood only; acceptable because rollup ignores are by-design, but the "addresses rollup-line-mass" claim is partial.
- Fix 4's shape check: v2 onion labels are exactly 16 base32 chars, c-ares' bare ".onion"/".onion." has zero preceding label chars; dropping the bare "your files" needle keeps ransom-note phrasing covered by "have been encrypted", "all your files", or "decrypt your file".
- Finding-shape bucket threads also absorb sibling-dispute collateral (a comment refuting a different detector's finding on the same function's thread grades the T1486 leads there too); this is folded into the dual-use residual rather than promoted to a mechanism.
- The verbatim sentence "It exists to corroborate crypto_capability, not to stand alone" is from the user docs (detectors.md:323), not the module doc; the module doc (ransomware_behavior.rs:1-13) states the same intent in different words ("Individually these are weak; co-located with crypto_capability ... they are a strong ransomware signal").
