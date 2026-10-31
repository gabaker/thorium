# crypto_capability

Part of the [telemetry review](../detectors.md#what-the-telemetry-says) of the [preseed detectors](../detectors.md).

**Status (2026-10-31):** the fixes below shipped in the fleet-wide detector overhaul (see the main page's overhaul status note). Deviations: the always-noise routing shipped via a hit flag consumed by the seeder's fingerprint-free rollup (base64 weak signatures, P-less Q hits, CryptAcquireContext, CryptStringToBinary); uncalled crypto imports fold into the shared import census rather than a per-import opt-out. Post-replay addition: the repeating-XOR text gate consults a leet-aware second chance (same two-thirds bar over digit-substituted prose) so leetspeak ransom notes pass while printable wrong-key mash stays dead.

crypto_capability is the fleet's loudest detector and one of its least trusted: most engaged threads end disputed and most threads draw no engagement at all. Two sub-passes produce most of the disputed leads: the brute-force decrypt replay (crypto_decrypt_hits) fabricates "Recovered Strings" by XORing library data with unrelated defined strings — the ~50%-letter wordiness bar and 60%-span coverage gate accept the junk — and the base64 key carver (embedded_key_hits) flags every entry of an encrypted string table as a separate Likely/85 "Candidate Symmetric Key" (uncapped). The remainder is template over-claiming — dual-use CryptoAPI calls, base64 alphabets, the 0x9E3779B9 constant, and hash tables all narrated as ransomware T1486 — plus a severe small class where mbedTLS/OpenSSL PEM-writer constant tables emit as Fact/95 "Recovered Private Key" blocks. Every recovery hit carries a key/cert artifact that both the runtime-noise classifier (detections.rs:931-944) and the seeder's rollup veto (seed.rs:466-482) treat as pivot-class — and their Likely/Fact grades sit above the rollup's noise bar regardless — so the junk is structurally guaranteed a loud per-function thread (a CA bundle can seed a certificate lead per certificate). The salvageable core is real: AES S-box and RC5/RC6-P constant hits land on genuine cipher code, and encrypted-string-table key-cluster leads, even when mis-framed as keys, point agents straight at the encrypted string table.

## Where the false positives come from
---

### 1. Wrong key XOR fabrications

crypto_decrypt_hits brute-forces up to 64 defined strings (any fully-printable string 4-64 bytes, including Rust fmt fragments and library text) as RC4/repeating-XOR/RC6 keys against up to 64 referenced non-printable buffers. For repeating XOR, a wrong printable key over semi-structured library data (core::unicode lookup tables sitting beside panic-message strings) clears the source-side guards (is_degenerate_source histogram, is_structured_source) and the text gates — is_wordy needs only alpha*2 >= len with >=5 distinct chars, and is_strong_recovery only 24 total chars at 60% span coverage — so fabricated 'Recovered Strings' with barely half their characters letters are seeded at Likely/25. Each attaches the fake key as a SymmetricKey artifact, a pivot class that exempts the hit from runtime weakening and its thread from rollup suppression.

*Where:* `thorpal/src/thorpal/detect/detections/impact/crypto_capability.rs:710-855 (crypto_decrypt_hits + collect_keys; artifact at 776-790), gates in thorpal/src/thorpal/detect/decode/recover.rs:43-52,732-743`

### 2. String table carved as keys

embedded_key_hits carves any padding-anchored base64 token in a data section that decodes to 16/24/32 bytes with entropy >= 3.5 as a 'Candidate Symmetric Key' at Likely/85, with no per-run cap. A base64+RC4 encrypted string table is dozens of such tokens — every 16/32-char encrypted string decodes to a 'key-length high-entropy blob' — and the length-alignment look-back (find_base64_keys takes the first window within 44 chars of the padding whose length mod 4 and decoded size fit a key length) carves mid-token suffixes of longer blobs as separate keys. One such binary yields many separate key findings, none of them keys.

*Where:* `thorpal/src/thorpal/detect/detections/impact/crypto_capability.rs:1889-1972 (embedded_key_hits), 2023-2076 (find_base64_keys), MIN_KEY_ENTROPY at 1005`

### 3. Ransomware template dual use APIs

api_call_hits emits one finding per CryptEncrypt/CryptDecrypt/CryptImportKey/CryptGenKey/CryptAcquireContext calling function with hard-coded ransomware narration ('the core of ransomware file encryption', 'a ransomware key-management step') under T1486. The APIs are dual-use: they implement C2 channel crypto, config decryption, LM-hash computation, and exfil transport — agents dispute the T1486 claim, not the call.

*Where:* `thorpal/src/thorpal/detect/detections/impact/crypto_capability.rs:222-270 (APIS reasoning strings), driver detections.rs:1079 (api_call_hits)`

### 4. Ubiquitous base64 alphabet findings

The WEAK_SIGS base64-alphabet signature seeds a standalone Untrusted/15 T1486 finding per (match, section, attributed function) group. Every binary with a base64 codec (MIME libraries, proxy Basic-auth, license decoders, Delphi mail components) fires it; the reasoning text itself admits 'Ubiquitous, hence low confidence', yet it still becomes a per-function forum thread whenever no toolchain fingerprint lands (partition_suppressed collapses nothing without one).

*Where:* `thorpal/src/thorpal/detect/detections/impact/crypto_capability.rs:171-184 (WEAK_SIGS base64 entries), seeding gate thorpal/src/thorpal/seed.rs:452-497`

### 5. Q constant substring collisions

The 4-byte 0x9E3779B9 (Q) signature matches inside the 64-bit Fibonacci-hash multiplier 0x9E3779B97F4A7C15 that parking_lot/hashbrown bake into every Rust binary's code (LE top 4 bytes = b9 79 37 9e), inside UPX-compressed streams (random bytes), and in PRNG seeds. The detector's own reasoning says Q only 'corroborates an RC5/RC6 cipher when seen alongside the P constant', but it fires on Q alone; signature_hits scans every initialized section including code, so immediate operands match.

*Where:* `thorpal/src/thorpal/detect/detections/impact/crypto_capability.rs:185-203 (Q entries in WEAK_SIGS), scan at detections.rs:1626-1672`

### 6. PEM boundary pair junk blocks

find_pem_blocks pairs any '-----BEGIN' with the next '-----END ...-----' regardless of label (find_pem_end_close matches any END), and has_pem_body counts NUL/garbage bytes between two adjacent writer string constants as 'body' (str::trim strips only whitespace, so a line starting with NULs is non-empty and not '-----'-prefixed). mbedTLS/OpenSSL PEM-writer constant tables therefore emit as 'Recovered Private Key' at Fact/95 and 'Recovered Public Key' at Fact/80 — with mismatched labels (BEGIN RSA / END EC) or matching-label pairs holding no base64 body at all — the detector's highest-severity output, spent on library string tables.

*Where:* `thorpal/src/thorpal/detect/decode/recover.rs:903-930 (find_pem_blocks, no label match; find_pem_end_close at 977-998), crypto_capability.rs:1240-1361 (emit_pem) + 1656-1662 (has_pem_body)`

### 7. Library test key material

TEST_LABELS only inspects PEM armor label text (test_label filters to '-----' lines), so library test material whose marker lives in the certificate body/subject (PolarSSL Test EC CA, DigiCert root bundles) is emitted at Fact/60-80 as recovered attacker material; the weaken-only policy keeps every one emitted, and pem_hits has no cap (unlike MAX_DER_HITS=24), so a CA/test bundle floods a session.

*Where:* `thorpal/src/thorpal/detect/detections/impact/crypto_capability.rs:473-483 (TEST_LABELS) + 1319-1345 (weaken-only), uncapped pem_hits at 1040-1133`

### 8. Nearest reference misattribution

signature_hits attributes a data-section constant to the function referencing the nearest preceding data address within DATA_REF_WINDOW=4096 bytes (referencing_function takes the greatest referenced address <= the match), not to a function that references the match address itself. Genuine SHA-256/MD5 tables get bucketed under unrelated neighbours (a float formatter, crond's log gate), the seeder roots the thread at that function, and the agent assigned to it disputes the lead — plus unreferenced dead library tables are presented as if owned by code.

*Where:* `thorpal/src/thorpal/detect/detections.rs:38 (DATA_REF_WINDOW), 1652-1654 (containing_function / referencing_function fallback), thorpal/src/thorpal/detect/decode/buffers.rs:182-192`

## Why leads get ignored
---

- **Recovery floods with artifact shield** — The decrypt replay, key carver, and PEM/cert recovery passes flood sessions with low-value artifact-bearing findings: crypto_decrypt_hits caps at 10 per sample but multi-sample sessions multiply it; embedded_key_hits and pem_hits have no cap at all (only DER is capped at 24). Their Likely/Fact grades sit above the rollup's noise bar (is_noise = Untrusted outright, or Unsure inside classified runtime code), and the attached SymmetricKey/PrivateKey/Certificate artifacts are pivot-class — vetoing both runtime weakening (detections.rs:931-944) and bucket suppression (seed.rs:466-482) — so junk recoveries are structurally guaranteed loud per-function threads nobody opens.
- **Weak sig and API per function duplication** — signature_hits emits one finding per (signature, section, attributed function) and api_call_hits one per calling-function group, so a single statically-linked OpenSSL/busybox yields a dozen separate threads restating 'this binary has base64/SHA-256/AES/CryptoAPI' at Untrusted-15/Unsure-40. Agents triage the first and skip the rest.
- **Rollup gated on toolchain fingerprint** — partition_suppressed only collapses noise when a toolchain was fingerprinted (seed.rs:457-459); on samples where no fingerprint lands (stripped C/MSVC without CRT markers), every Untrusted-grade base64/Q/CryptAcquireContext hit seeds its own per-function thread. Where rollup does engage, its lines are near-universally ignored anyway — the cheap outcome, but still many dead lines.
- **Multi sample scope duplication** — In multi-sample sessions the same file-level recovery is seeded once per sample scope, doubling identical leads (and doubling the dispute workload when engaged).

## What would reduce the noise
---

Static-side changes, ordered as proposed; each was checked against the true positives so the fix does not suppress what the detector gets right.

### Tighten decrypt-replay acceptance and unbrand junk recoveries

In crypto_capability.rs: (1) for repeating-XOR results, raise the text gate — require letter share >= 2/3 and at least one vowel-bearing word (add a stricter is_wordy variant in recover.rs applied in recovered_runs when key_guard is Some, i.e. only the XOR branch); (2) in collect_keys, record each key string's referencing function (via ctx.reference_index, strings carry addresses) and, in crypto_decrypt_hits, when key and ciphertext are not referenced by the same function (or within KEY_REF_WINDOW of each other), demote the hit to low() and skip the artifact; (3) attach the SymmetricKey artifact only when decode::classify returns a key/config kind, not for RecoveredKind::String — so String-kind recoveries stop being pivot-shielded from both runtime weakening (detections.rs:931-944) and the seeder's rollup veto (seed.rs:466-482).

*Effort:* medium \
*True-positive risk:* A genuine recovered string (mostly letters, with a vowel-bearing word) passes the 2/3 bar, while barely-half-letters junk fails. Residual risk: if a real recovery's key/ciphertext affinity check fails AND the sample's toolchain is fingerprinted, the demoted artifact-less hit would land as a rollup line rather than a finding — visible but quieter, and rollup lines do get engaged, so the pointer survives.

Addresses: wrong key xor fabrications, recovery floods with artifact shield.

### Collapse carved-key clusters into one string-table lead

In embedded_key_hits, after carving, group tokens by section with gaps `< 4KB; when a group has >`= 3 tokens (or > 4 per binary), emit ONE finding ('N same-shape base64 blobs at 0x..-0x.. — likely an encoded/encrypted string table, not N independent keys') at Likely/60 listing the first 8 addresses (mirror seed.rs MAX_SITES_LISTED), instead of N Candidate-Symmetric-Key findings at 85. Keep lone (1-2 token) carvings at full severity. This also acts as the missing cap.

*Effort:* medium \
*True-positive risk:* Useful Candidate Symmetric Key leads on encrypted string tables are exactly such a cluster — the collapsed lead keeps the pointer agents use (the table address range) and frames it correctly as a string table, at the cost of itemized per-token leads. A genuine single embedded AES key is untouched.

Addresses: string table carved as keys, recovery floods with artifact shield.

### Validate PEM blocks: matching labels + base64 body

In emit_pem (or find_pem_blocks), require (a) the END label to equal the BEGIN label, and (b) has_pem_body to accept only lines made of base64 alphabet/whitespace with at least one full-length line that decodes — rejecting NUL/garbage 'body'. Mismatched-label or garbage-body pairs found via a defined string fall through to the existing low-confidence pem_marker path (pem_hits mechanism B's None branch); raw-scan-only junk simply stops emitting.

*Effort:* small \
*True-positive risk:* None expected: real recoveries (PKCS#1 keys, PolarSSL certs, DigiCert certs) have matching labels and a decodable base64 body; the junk blocks never do — including matching-label BEGIN/END RSA writer pairs, which the body check alone rejects.

Addresses: pem boundary pair junk blocks, recovery floods with artifact shield.

### Cap PEM recoveries and recognize library test/bundle material

Give pem_hits a per-run cap mirroring MAX_DER_HITS (with the same tracing::warn on truncation); when > 3 certificates are recovered in one image, collapse them into a single 'embedded certificate bundle (N certs)' finding at Unsure/40 (private keys stay itemized); extend the test-material check beyond armor labels by base64-decoding the body and matching known library-test subject strings ('PolarSSL Test', 'polarssl', Go 'TESTING KEY' bodies) and well-known root-CA subjects, demoting those to the weakened grade with an explicit caveat instead of Fact/60-95.

*Effort:* medium \
*True-positive risk:* A ransomware sample embedding one attacker public key or 1-3 certs keeps full severity; the demotion only hits bundles and known test subjects.

Addresses: library test key material, recovery floods with artifact shield.

### Gate Q on P and make base64 alphabets rollup-only

In CryptoCapability::run, collect P-constant (0xB7E15163, both endians) matches first; a Q-constant WEAK_SIGS hit with no P match anywhere in the image is NOT dropped but marked always-noise (TEA/XTEA use only the Q delta and never P — Q leads can point at a crypter's real TEA payload decryptor, so the signal must stay visible). Implement the flag as a per-sig 'always-noise' bool on ByteSig threaded through build_signature_hit onto DetectionHit and honoured by partition_suppressed, routing flagged hits (base64 alphabets unconditionally, P-less Q hits) to the runtime-noise rollup even without a toolchain fingerprint (the rollup post title/body needs a no-toolchain variant).

*Effort:* medium \
*True-positive risk:* P-constant RC5/RC6 leads are untouched (P still emits alone); routing Q-only hits to rollup (not dropping) preserves the TEA-decryptor pointer. Base64-alphabet leads are low-value and remain visible as rollup lines, which agents do still engage.

Addresses: q constant substring collisions, ubiquitous base64 alphabet findings, rollup gated on toolchain fingerprint, weak sig and api per function duplication.

### Neutral dual-use wording for CryptoAPI hits; CryptAcquireContext to rollup

Rewrite the APIS reasoning strings (crypto_capability.rs:222-270) to capability-neutral text ('encrypts data — used for file encryption, C2/exfil protection, or config decryption; T1486 only if file-walk evidence corroborates') and have the hit name say 'crypto API' rather than asserting ransomware key management; mark CryptAcquireContext (and CryptStringToBinary) with the always-noise flag from the previous fix (added to ApiSpec and honoured identically) so they never seed standalone threads.

*Effort:* small \
*True-positive risk:* None expected; the call sites stay visible (medium-grade APIs still seed findings, just without the falsifiable ransomware claim that drew the disputes).

Addresses: ransomware template dual use apis, weak sig and api per function duplication.

### Attribute constants only on a direct xref, else file level with caveat

In signature_hits (detections.rs:1652-1654), bucket a data-section match under a function only when the reference index contains a referenced address inside the match's byte range (partition_point lookup over [addr, addr+sig.bytes.len())); a nearest-preceding-reference-within-4096 fallback hit instead seeds file-level with an explicit 'no code reference found; located near FUN_x's data (unverified)' line. Keep containing_function attribution for code-section matches.

*Effort:* medium \
*True-positive risk:* Genuine constant leads (AES S-box, SHA-256) have genuinely-referencing consumers, so they either keep their function (when the operand parse captures the table base) or move to file level still visible; nothing is dropped, only re-anchored.

Addresses: nearest reference misattribution, weak sig and api per function duplication.

## Verification notes
---

- All eight mechanism code anchors verified exact at HEAD: crypto_decrypt_hits/collect_keys at crypto_capability.rs:710-855 with the SymmetricKey artifact at 776-790; embedded_key_hits 1889-1972, find_base64_keys 2023-2076, MIN_KEY_ENTROPY=3.5 at 1005; APIS 222-270; WEAK_SIGS base64 171-184 and Q 185-203; TEST_LABELS 473-483, weaken-only 1319-1345, uncapped pem_hits 1040-1133; emit_pem 1240-1361, has_pem_body 1656-1662; recover.rs is_wordy 43-52, is_strong_recovery 732-743, find_pem_blocks 903-930 (find_pem_end_close at 977-998 matches any END — no label comparison); detections.rs DATA_REF_WINDOW=4096 at 38, signature_hits 1626-1672 (fallback at 1652-1654), api_call_hits 1079, buffers.rs referencing_function 182-192; seed.rs partition_suppressed 452-497, toolchain gate 457-459, pivot-artifact veto 466-482. Severity mappings confirmed in decode.rs assess(): String=25, Certificate=60, PublicKey=80, SymmetricKey=85, PrivateKey=95; high()=(Likely,75), medium()=(Unsure,40), low()=(Untrusted,15).
- 'Active' status confirmed for all mechanisms: the whole detect/ tree landed in one squashed commit 9e4a6f1f (2026-10-26); the only later commit touching these paths, 5bfd2ab5 (2026-10-30), adds telemetry plumbing to seed.rs (SeedReport/roster) without changing seeding, rollup, or detector logic, and the working tree is clean. Nothing is already_fixed.
- Mechanism 1 covers only the schemes crypto_decrypt_hits replays (RC4, repeating XOR, and RC6); single-byte-ADD decodes belong to another detector. The pass already has source-side guards (is_degenerate_source, is_structured_source, reads_as_text) that semi-structured library data clears, so the fix targets the real residual gap.
- Mechanism 2's look-back: find_base64_keys iterates start from lo upward and takes the FIRST window satisfying the mod-4/key-length/entropy constraints — i.e. the longest alignment-valid window within 44 chars, not the tightest. Mid-token suffix carving follows arithmetically: a 22-byte RC4 string encodes to a 30+2-char token that decodes to 22 bytes (rejected), while its 22+2-char suffix window decodes to 16 bytes and is carved at token+8.
- The artifact shield works at two levels: runtime_classification (detections.rs:931-944) refuses to weaken artifact-bearing hits at all, and partition_suppressed (seed.rs:466-482) vetoes bucket suppression — and independently the recoveries' Likely/Fact grades already sit above the rollup noise bar (is_noise = Untrusted, or Unsure+runtime).
- Fix 5 (Q gating) amended: dropping Q hits outright would erase TEA/XTEA detection entirely — TEA uses only the Q delta and never the P constant. Changed to route P-less Q hits to the always-noise rollup instead of dropping, and raised effort S->M (the flag needs DetectionHit plumbing plus a no-toolchain rollup-post variant).
- Fix 1's residual risk is stated explicitly: a genuine recovered string survives the text gate but could demote to a rollup line if its key/ciphertext affinity check fails in a fingerprinted-toolchain session. Fix 3's fallback: the pem_marker fallback exists only for defined-string boundaries (mechanism B); rejected raw-scan junk simply stops emitting, which is the desired outcome.
- Grading artifact noted: the same disputing comment is graded against every lead in a thread, so leads can be marked disputed by comments that never address the constant itself — mildly inflating per-pattern dispute counts for mechanisms 7 and 8.
- Fix 2 reshapes encrypted-string-table key clusters into one lead, and fix 1 can demote a genuine recovered string to a rollup line; no fix removes a true-positive pointer outright.
