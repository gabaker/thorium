# host_binding

Part of the [telemetry review](../detectors.md#what-the-telemetry-says) of the [preseed detectors](../detectors.md).

**Status (2026-10-31):** the fixes below shipped in the fleet-wide detector overhaul (see the main page's overhaul status note). Deviation: the binary MAC-carving sub-pass was retired outright instead of hardened (fix 3); the text-scan MAC and GUID machinery, comparison-gated keying, name gating, and GUID flood controls shipped as written.

host_binding seeds a heavy forum load and is not earning it: most engaged threads end disputed and most threads are ignored outright. Its signature medium tier — "stored value used by an enumerating function" co-location emitted as T1480.001 at Unsure/40 — is where the environmental-keying claim goes wrong. False positives are dominated by the raw-byte MAC scan carving fabricated addresses out of format-string bytes, C++ RTTI/EH metadata, MSVC security cookies, IAT dwords, and CRT tables, and by the name-candidate pass promoting crypter decoy strings, encoded registry value names, and a brute-force username to "stored host key" claims; even where data and enumerator are both real, agents dispute the T1480.001 headline because victim-ID generators, recon writers, and worm self-checks USE identity rather than compare it against a stored key. Ignores are dominated by unattributed well-known-GUID floods and per-function T1082 spam, all locked out of the runtime-noise rollup by the seeder's HostIdentifier veto plus the file-level-bucket exemption (the rollup shape itself works fine where it applies). The value that remains is cheap but real: T1082 enumeration anchors on genuine fingerprinting/victim-ID functions (disputes repeatedly end "T1082 stands") and unique campaign GUID literals.

## Where the false positives come from
---

### 1. Binary MAC carving

The raw-byte scan (binary_hits -> collect_stored_values -> scan_window/render_mac) fabricates MAC addresses from structured non-MAC data in buffers an "enumerating function" references. Four sub-shapes defeat the current neighbourhood tests, verified against the actual filter logic: (a) high-byte string regions — repeating a2/c2 filler around string literals and \r\n\r\n template bytes evade in_text_run/in_string_table because 0xa2/0xc2 are above 0x7e so is_text_byte never counts them; (b) C++ RTTI/throw-info/vftable metadata whose -1 sentinels and encrypted-string fragments pass is_degenerate_mac (00:00:ff:ff:ff:ff has only 2 zeros, no all-same run, even first octet, non-zero OUI) and evade straddles_pointer because 0xffffffff and zeroed words never decode to in-image addresses; (c) MSVC /GS security-cookie constants (0x00002B992DDFA232 -> 32:a2:df:2d:99:2b, its complement -> 00:00:cd:5d:20:d2 and 66:d4:ff:ff:01:00, and 0xBB40E64E) sliced 6 bytes at a time; (d) IAT hint/name-RVA and ordinal dwords plus CRT ctype case-map tables — straddles_pointer misses them because RVAs and ordinal-flagged values (high bit set) do not decode to in-image addresses, and the function "references" the IAT only because data_addrs harvests indirect-call operands (verified: reference_index is built from data_addrs over every function and is_data_section indexes .rdata/.idata, so `call [IAT slot]` lands the IAT in the index). The 512-byte BINARY_SCAN_WINDOW then sprays many hits per function.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/host_binding.rs:1109-1655 (binary_hits, collect_stored_values, scan_window, render_mac, in_text_run/is_text_byte at :1603, straddles_pointer at :1329) + indicators.rs:1285-1327 (is_degenerate_mac) + decode/buffers.rs:33-48 (data_addrs) and :147-168 (reference_index built on it)`

### 2. Name colocation overclaim

name_candidate_hits reports up to MAX_NAMES_PER_FUNCTION=8 literals per enumerating function as a medium "Stored computer/account name used by ..." T1480.001 finding with no comparison requirement, no dataflow, and only plausible_host_name as a filter — which accepts any identifier-charset string up to 63 chars (rejects only filenames, bare numbers, and non-identifier bytes). So crypter decoy pools (gibberish strings written once each to a never-read global), sub-4-encoded registry value names, plain registry value names that are enumeration INPUTS (InstallDate/RegisteredOwner/RegisteredOrganization), a brute-force username (`Administrator`), and glibc env-var names (`RES_OPTIONS` in __res_vinit) all become "stored host keys" at Unsure/suspicion 40.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/host_binding.rs:973-1087 (name_candidate_hits/referenced_names; MAX_NAMES_PER_FUNCTION=8 at :183) + indicators.rs:1423-1451 (plausible_host_name)`

### 3. Keying claim on colocation

build_identity_hit headlines T1480.001 "Execution Guardrails: Environmental Keying" at medium/susp-40 for mere co-location — the `(Some(_), _)` match arm covers both comparison=None and Some(false), and the body itself concedes "No comparison was visible ... the literal may equally be a default, a log field, or a value being sent". Agents dispute the technique claim even when the data and enumerator are real: victim-ID generators (MAC/hostname hashed into a C2 bot id), ipconfig-style recon writers, and worm self-infection checks all read identity to USE it, not to compare against a stored key.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/host_binding.rs:1831-1839 (severity match: (Some(_), _) => medium()/ContextOnly) and :1907 (every identity hit emitted as T1480_001)`

### 4. Library code attribution

The enumerator anchoring a whole hit family is a statically linked library's generic routine, not the author's check. is_library_function is applied only in name_candidate_hits (host_binding.rs:999-1004) and knows only MSVC CRT markers (no POCO, no glibc __res_*/_dl_*). apply_runtime_context almost never helps, for two verified reasons: runtime_classification (detections.rs:931-944) categorically excludes any hit carrying a pivot-class artifact — including every HostIdentifier stored-value hit — from runtime classification, and the remaining T1082 hits sit in unnamed FUN_* functions no marker can match. So Poco::Net::NetworkInterface::map (unnamed in stripped binaries), glibc __res_vinit/_dl_discover_osversion, and Go runtime wrappers (RUNTIME_ENUMERATORS marks the runtime's own routine as the "enumerating function") anchor T1082 hits and scope the binary MAC scan onto library data structures.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/host_binding.rs:999-1004 (filter only in name path) + detections.rs:1006-1039 (is_library_function MSVC-only markers) + detections.rs:931-944 and :979-991 (runtime_classification artifact exclusion + apply_runtime_context)`

### 5. Dead call enumeration

enumeration_hits emits a T1082 finding for every resolved call site of an identity API with no check that the result is ever used; crypter junk-API batteries call GetUserName into a write-only buffer purely as decoy, so the "read" is not even discovery.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/host_binding.rs:612-712 (find_enumerators + enumeration_hits; no dataflow/result-use check anywhere in the path)`

## Why leads get ignored
---

- **GUID literal flood** — literal_hits/blob_literal_hits (host_binding.rs:855-900, :1704-1767) report every GUID-shaped string at low severity with no enumeration gate, up to MAX_PER_CLASS=24 per class (host_binding.rs:176), and multiple GUIDs matched inside one defined string each get their own finding at the same address (verified: build_identity_hit is called with the string's base, not base+offset). Ubiquitous COM/DirectX interface IDs and runtime/packer GUIDs recur across many samples, so the same benign values flood session after session; agents never touch them.
- **Hostid rollup veto** — The seeder's noise suppression explicitly protects any bucket carrying a HostIdentifier artifact (ArtifactClass::HostIdentifier in the veto list at seed.rs:478) and never suppresses file-level buckets (function.is_some() gate at seed.rs:489), so every "Stored MAC/GUID/name" finding — including fabricated ones at untrusted/unsure — seeds a full thread instead of a rollup line. The lock is actually triple: suppression requires a fingerprinted toolchain at all (seed.rs:457), unsure-tier hits only count as noise when runtime-located (seed.rs:461-465) and runtime_classification (detections.rs:931-944) refuses to runtime-classify artifact-carrying hits, and the artifact veto keeps the bucket even when everything else lines up. Result: nearly all ignored leads are full findings rather than rollup lines.
- **Per function t1082 spam** — One low/ContextOnly T1082 "Reads machine identity in `<fn>`" finding per enumerating function, plus runtime-symbol hits (emitted at file level with no function, so never suppressible) and Go-wrapper variants, floods binaries with many identity readers (static glibc, CRT helper DLLs, Go). "This program reads the computer name" alone gives agents nothing to engage.
- **Fabricated value residue** — The same binary-scan and co-location families that drive disputes also dominate ignores in the sessions where no agent picked up the anti-analysis thread — the junk is seeded identically every session a sample recurs in, and most sessions never engage it. Re-analysis of overlapping samples across sessions multiplies every pattern.

## What would reduce the noise
---

Static-side changes, ordered as proposed; each was checked against the true positives above so the fix does not suppress what the detector gets right.

### Reserve T1480.001/medium for an observed comparison; co-location becomes low T1082

In build_identity_hit (host_binding.rs:1831-1839), change the (Some(_), _) arm — which today catches comparison None and Some(false) alike — to emit T1082 at low() with ContextOnly and a headline like "Stored `<class>` value co-located with identity read" instead of T1480_001 at medium() (technique set at :1907, headline at :1841-1848); keep T1480_001 + medium() + ControlFlowGated only for the (Some(_), Some(true)) arm where comparison_in found the literal in a branch condition. No seeder change needed for questions: gate_questions (seed.rs:1374-1421) already requires ControlFlowGated + HostIdentifier and evasion_question (seed.rs:1611-1640) only cites T1497*/T1622 at Likely+, so the non-compared tier derives no questions today.

*Effort:* small \
*True-positive risk:* None observed: the true-positive leads are already low-tier (T1082 enumeration leads and unattributed GUID literals at untrusted/15). A genuine guardrail with a visible strcmp-in-branch keeps full severity via the Some(true) arm.

Addresses: keying claim on colocation, name colocation overclaim, binary mac carving.

### Gate name candidates on a visible comparison and denylist enumeration-input words

In name_candidate_hits (host_binding.rs:973-1035): only emit a candidate when observed_comparison(ctx, record.entry, &value.text) returns Some(_) — the signature matches (host_binding.rs:1964) and build_identity_hit fetches the same decompile for scoring anyway, so this adds no Ghidra traffic; add a small denylist of registry value names and env vars that are enumeration inputs (InstallDate, RegisteredOwner, RegisteredOrganization, MachineGuid, DigitalProductId, ProductId, RES_OPTIONS, HOSTALIASES, LOCALDOMAIN); reduce MAX_NAMES_PER_FUNCTION (host_binding.rs:183) from 8 to 3 and roll overflow into one summary line.

*Effort:* medium \
*True-positive risk:* No true-positive lead is a name candidate, so nothing observed is lost. Theoretical cost: a hostname compared via a custom hash loop (no strcmp/== token in the decompile) drops to the low co-location tier — acceptable given fix 1 still reports it as T1082 with the artifact attached.

Addresses: name colocation overclaim.

### Harden the binary MAC scan against compiler metadata

(a) Extend is_degenerate_mac (indicators.rs:1285-1327): reject any adjacent repeating byte-pair (o[i..i+2]==o[i+2..i+4] for any i in 0..=2 — covers full period-2 a2:c2:a2:c2:a2:c2 AND the banner-boundary a2:c2:a2:c2:0d:0a), any byte occurring >=4 times (kills 10:10:10:10:10:00 and ff-sentinel slices), and (zero + 0xff octets) >= 3 (kills 00:00:ff:ff:ff:ff, 66:d4:ff:ff:01:00). (b) In render_mac/scan_window (host_binding.rs:1642, :1381), reject candidates whose six bytes are all text-or-NUL class using the existing is_text_byte (which already counts \r\n\t) — kills 00:00:0d:0a:0d:0a and 0a:00:23:25:73:00. (c) Add a tiny denylist of MSVC /GS cookie constants: reject a candidate when any aligned 4/8-byte word overlapping it equals the LE encoding of 0xBB40E64E or 0x00002B992DDFA232 or their complements. (d) In collect_stored_values (host_binding.rs:1183-1238), skip reference-index entries whose address lies in import metadata — a section-name test alone is insufficient since an IAT can live in .rdata (and is_data_section in decode/buffers.rs:131 indexes .idata/.rdata alike), so also filter data_addrs (decode/buffers.rs:33-48) to drop memory operands of indirect call/jmp instructions, which is safe for its decoder consumers (ciphertext is referenced by lea/mov). (e) Cap binary-scan findings per enumerating function (e.g. 4) so one library routine cannot spray many artifacts.

*Effort:* medium \
*True-positive risk:* No true-positive lead is a binary-scan MAC, so observed cost is zero. Real stored MACs (e.g. 08:00:27:aa:bb:cc, 00:0c:29:77:88:99) pass every added test: no repeating pair, no 4x byte, zeros+ff < 3, contain non-text bytes (0x08/0x0c/0xaa...), not a cookie constant, and sit in author-referenced data rather than the IAT. Residual theoretical cost: an all-printable-plus-zeros real MAC (~1% of random MACs) would be dropped by (b).

Addresses: binary mac carving, fabricated value residue.

### GUID flood control: ubiquity denylist, per-string dedupe, lower unattributed cap

In literal_hits/blob_literal_hits (host_binding.rs:855-900, :1704-1767): (a) ship a static denylist of ubiquitous COM/DirectX/shell GUIDs plus cheap family heuristics (data2-data3 == E260-11CF, node suffix 00AA004A34D5 or C000-000000000046, the DirectShow baseclasses pair 42E65C96-15E8-40c0-B04A-90D5CCF6E2EB / 3CFFFC43-D199-46fb-BFEE-8342B653A491); (b) when find_host_identifiers matches several GUIDs inside one defined string, emit one finding listing them instead of N findings — trivial because they already share one reported address (literal_hits passes the string base to build_identity_hit); (c) when no enumerator of class Guid exists, drop the unattributed-GUID allowance from MAX_PER_CLASS=24 (host_binding.rs:176) to ~6, overflow into a single summary line.

*Effort:* medium \
*True-positive risk:* Material and must be respected: unattributed GUID literals include genuine campaign GUIDs. These are unique non-COM values, few per binary, referenced by user code, so they survive the denylist, the per-string dedupe, and a cap of 6. Do NOT drop the unattributed-GUID tier wholesale.

Addresses: guid literal flood.

### Lift the HostIdentifier rollup veto for non-comparison MAC artifacts

In partition_suppressed (seed.rs:452-497): stop treating every ArtifactClass::HostIdentifier as suppression-proof (seed.rs:478). Protect a bucket only when the host-identifier hit is EvidenceKind::ControlFlowGated or above Untrusted confidence; at minimum, lift the veto for MacAddress-valued artifacts while keeping it for Guid. Implementation note: HitArtifact carries only ArtifactClass::HostIdentifier with no MAC/GUID distinction, so either test the artifact value's shape (reuse indicators' MAC rendering/parse) or add the identity class to the artifact when building it. Optionally also let file-level Untrusted host-identifier hits join the rollup (the function.is_some() gate at seed.rs:489 currently exempts the GUID floods). Note the dependency: unsure-tier hits are never noise without runtime context (seed.rs:461-465), so this fix bites on the fabricated-MAC findings only after fix 1 demotes them to Untrusted, and only on binaries with a fingerprinted toolchain (seed.rs:457).

*Effort:* small \
*True-positive risk:* True-positive GUID literals are Untrusted artifact hits in function buckets — a blanket veto removal could roll them up on toolchain-fingerprinted binaries. Scoping the lift to MacAddress artifacts and keeping Guid protected removes that risk entirely.

Addresses: hostid rollup veto, fabricated value residue, guid literal flood.

### Demote library-internal enumerators

Extend is_library_function (detections.rs:1006-1039) with glibc/loader/resolver markers (__res_, _dl_, __nss_, __libc, _IO_) and apply it (plus the runtime-marker case) beyond name_candidate_hits: in enumeration_hits (host_binding.rs:680-712) skip or floor library-named functions so they can roll up; and in collect_stored_values/build_identity_hit refuse to let a library-named or RUNTIME_ENUMERATORS-matched wrapper (a marker match means the function IS the runtime's own routine, e.g. os.Hostname — carry a flag on EnumeratingFunction from find_enumerators) anchor stored-value scans or medium attribution. Also consider letting runtime_symbol_hits findings (file-level, host_binding.rs:757-804) participate in suppression, since file-level buckets are currently exempt.

*Effort:* medium \
*True-positive risk:* True-positive T1082 leads anchor user-code functions with no library-marker names, so none is affected. Unnamed statically-linked library code (POCO) is not caught by name — fixes 1/3/5 cover its fallout instead.

Addresses: library code attribution, per function t1082 spam, dead call enumeration.

## Verification notes
---

An independent pass re-checked every mechanism, anchor, and fix against the code:

- All code anchors verified against HEAD. Last commit touching host_binding.rs and indicators.rs is 9e4a6f1f (2026-10-26), so all five mechanisms are correctly 'active'; no already_fixed candidates exist.
- Traced each cited junk value through the actual filters to confirm the mechanisms: 00:00:ff:ff:ff:ff passes is_degenerate_mac (2 zeros < 3, non-zero OUI, even first octet, no all-same/monotone run); 0xa2/0xc2 are > 0x7e so is_text_byte never counts them and in_text_run/in_string_table cannot see the banner; IAT RVAs and ordinal flags do not decode to in-image addresses so straddles_pointer misses them; -1 sentinels and zeroed words likewise.
- Verified the IAT pathway end to end: AnalysisContext::reference_index delegates to buffers::reference_index which is built from data_addrs over every function's operands (including indirect-call memory operands) and indexes any section whose name contains 'data'/'rdata' — so `call [IAT slot]` genuinely puts the import table in the stored-value scan's scope.
- Corrected mechanism keying-claim-on-colocation's arm description: the code arm is (Some(_), _) at host_binding.rs:1836, covering comparison None AND Some(false) (the original said (Some(_), None)); also note ControlFlowGated hits share the same medium() severity.
- Added to library-code-attribution the second verified reason apply_runtime_context never helps: runtime_classification (detections.rs:931-944) categorically excludes hits carrying pivot-class artifacts (incl. HostIdentifier) from runtime classification, so no stored-value hit can ever be runtime-weakened — this also triple-locks the rollup (with the seed.rs:478 artifact veto and the seed.rs:461-465 is_noise rule requiring Runtime context for Unsure hits).
- Dropped fix 1's 'stop deriving anti-analysis questions from the non-compared tier' sub-change as a no-op: gate_questions (seed.rs:1374-1421) already requires EvidenceKind::ControlFlowGated + HostIdentifier, and evasion_question (seed.rs:1611-1640) only cites T1497*/T1622 at Likely+, so ContextOnly medium hits derive no questions today.
- Refined fix 3: generalized the period-2 repeat test to any adjacent repeating byte-pair so the banner-boundary window a2:c2:a2:c2:0d:0a is caught too, and flagged that a '.idata section name' test is insufficient when the IAT lives in .rdata — the data_addrs indirect-call-operand filter is the robust half of (d).
- Corrected fix 5's implementability detail: HitArtifact only records ArtifactClass::HostIdentifier (no MAC/GUID distinction), so the scoped lift needs a value-shape test or a new artifact field; also noted its effect on the unsure-tier MAC junk is contingent on fix 1 demoting them to Untrusted (is_noise never counts Unsure hits without Runtime context) and on a fingerprinted toolchain (partition_suppressed returns early otherwise).
- Bumped fix 6 effort S -> M: it touches find_enumerators (flag plumbing), enumeration_hits, collect_stored_values, build_identity_hit, and the shared marker list.
- Verified the per-string same-address behavior supporting fix 4(b): literal_hits passes the string's base address to build_identity_hit for every GUID matched inside it.
