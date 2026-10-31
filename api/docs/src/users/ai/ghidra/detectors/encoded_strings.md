# encoded_strings

Part of the [telemetry review](../detectors.md#what-the-telemetry-says) of the [preseed detectors](../detectors.md).

**Status (2026-10-31):** the fixes below shipped in the fleet-wide detector overhaul (see the main page's overhaul status note). Deviation: the small-alphabet source test lives beside is_structured_source (applied in the hidden-claim sweep) rather than inside it — crypto_capability shares that helper and its contract deliberately tolerates narrow byte ranges. Later additions: an interleaved (value, key) pair-storage decode on the observed-key path and a leetspeak-aware second chance in the shared wordiness gate — digit-substituted ransom notes recover at full strength while the popcount/PPMd/keyboard-mash fabrications stay dead; decoder-verified recoveries no longer face the brute-force output-shape vetoes.

encoded_strings is the fleet's biggest lead producer and delivers real, sometimes analysis-anchoring recoveries — pairwise-SUB ransom notes, contact email addresses, RSA public keys, dead-drop URLs, and emulated API vocabulary like ResumeThread — but with poor precision and most threads ignored. Its false positives have three roots, all verified in code: the blob brute-force fabricates "recovered strings" from compiler tables whose shapes (popcount/fractal, mixed small-int) evade all four is_structured_source tests and the stride<=8 / 85%-ordered output tests; the emulation stage harvests AES T-table and PPMd table bytes as strings because sanitize_recovered applies only length/3-distinct/one-letter gates, none of the static path's is_wordy / is_table_run checks; and is_decoder_loop calls any function with >=3 byte-sized XOR/SUB/ADD anywhere plus any backward branch anywhere a decoder, matching bool flips, digit conversion, zlib/LZMA, and Pascal/JSON parsers. Two overlapping amplifiers make junk look actionable: a lone `||`/`&&` substring raises confidence to Unsure/50 and prints a "shell command" bullet, and blob attribution uses a 256KB window plus a decoders-set-membership "decodes it" role claim. The ignored leads are dominated by artifact-less loop/key findings that flood every statically linked binary (the runtime-noise rollup requires both a fingerprinted toolchain and a recognizable function name, so stripped FUN_ functions always seed full threads). All mechanisms are fixable statically without touching the true-positive classes.

## Where the false positives come from
---

### 1. Blob sweep table fabrication

The section brute-force (blob_decode_hits scheme B/C) decodes compiler-emitted lookup tables into wordy-looking junk and reports it as a hidden string. is_structured_source only tests counting-by-1 (counts_up_or_down, 25% stepped bar), wide-int zero columns, single-byte dominance (50%), and monotone order (80% over >=16 pairs); a popcount-shaped table (b[i]=16-2*popcount(i): even-only deltas so 0% stepped, max value share 70/256=27%, no empty stride column, ~50/50 rise/fall) or a mixed small-int table (values 0x00-0x1f, ~9-12 distinct) passes all four, and the output-side table tests cap at stride 8 (is_periodic_run/is_columnar_run) and 85% ordered (is_ordered_run) so the fractal pattern passes those too. One 256-byte table then yields several findings under several ADD keys.

*Where:* `thorpal/src/thorpal/detect/decode/recover.rs:552-557`

### 2. Emulated table bytes as strings

emulation_hits selects up to MAX_EMULATED_FUNCTIONS=8 functions the loop pass flagged (ranking ones where static recovery found no artifact first — exactly the crypto/compression FPs), emulates each, and seeds every printable write that survives sanitize_recovered: length 5..=2048, >=3 distinct chars, >=1 ASCII letter. None of the static path's is_wordy / is_table_run / reads_as_text gates apply, so AES/Rijndael T-table generation and PPMd model-update writes are seeded as 'Recovered String' at Unsure (medium method base).

*Where:* `thorpal/src/thorpal/detect/emulation.rs:338-364`

### 3. Dual use byte arithmetic loop

is_decoder_loop calls any function with >=MIN_BYTE_TRANSFORMS(3) byte-sized XOR/SUB/ADD anywhere plus any backward branch anywhere a string decoder; recover_loop_key then presents the most common immediate as 'the key'. This matches ubiquitous codegen: `xor reg8,1` boolean flips (key 0x01), `add/sub 0x30` digit`<->`ASCII conversion in JSON int-to-string and strtoul ('a'-10 adjustments -> keys 0x57/0x58), `sub 0x20` space comparison in Pascal Val(), zlib deflate, LZMA range-coder probability updates, and VMProtect dead junk. There is no locality requirement (transforms need not sit inside any back-edge span) and no is_library_function check anywhere in decoder_loop_hits (the later apply_runtime_context pass only weakens hits in *named* runtime functions).

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/encoded_strings.rs:132-136`

### 4. Shell marker substring amplifier

Overlapping amplifier on the two fabrication mechanisms (not additive share): looks_shell_command treats a single COMMAND_MARKERS substring — `&&`, `||`, `; `, ` > `, `sleep `, `echo `, etc. — as a shell command, which in assess() sets the validity floor to Unsure (raising Untrusted blob/junk one step), pushes a 50-suspicion 'shell command' candidate, and prints the 'looks like a shell command (a real decode → confidence raised)' bullet. Table-decoded junk is full of `|` and `&` runs, so fabrications get promoted into dispute-worthy, actionable-looking leads instead of ignorable Untrusted/25 noise.

*Where:* `thorpal/src/thorpal/detect/decode.rs:701-704`

### 5. Wrong function attribution

Overlapping amplifier: blob hits are attributed to the function with the nearest preceding indexed data reference within MAX_SCAN_BYTES = 256KB (push_decoded passes MAX_SCAN_BYTES as referencing_function's window), and the nested pass's artifact role claims the referencing function 'references this buffer and contains the byte-wise XOR/SUB/ADD loop that decodes it' merely because that function is in the decoders set (nested_attribution tests set membership only, never that its op/key decodes this buffer). Agents then dispute the attribution even when the recovered value is real.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/encoded_strings.rs:794`

### 6. Per address ambiguity ledger

Overlapping with blob-sweep-table-fabrication: unambiguous() drops a recovery only when another key claims the exact same start address (claims is a HashMap keyed by u64 addr). A long structured table produces runs starting at different offsets under different keys because each key's printable window breaks the runs at different bytes (for example two keys each claiming two different offsets of one table), so each address has exactly one claimant and all the fabrications pass the contention test that exists to catch precisely this.

*Where:* `thorpal/src/thorpal/detect/detections/defense_evasion/encoded_strings.rs:632-645`

## Why leads get ignored
---

- **Artifactless loop key flood** — The per-function loop/key-only shapes ('string-decoder loop (n byte transforms)', '`<OP>` decoder — key X, no static buffer resolved') are unbounded: one Unsure/40 (medium()) finding per matching function, no recovered text to act on. Statically linked runtimes (Rust, Go, zlib, CRT, OpenSSL) contain dozens-to-hundreds of byte-arithmetic loops, and partition_suppressed only rolls a bucket up when a toolchain was fingerprinted AND the Unsure hit carries CallContext::Runtime — which runtime_classification assigns purely by function-name markers, so stripped FUN_ names never classify as runtime and seed as full per-function threads. The same family re-seeds identical addresses across sessions.
- **Low grade junk and stdlib recoveries** — Untrusted/25 blob fabrications and emulated copies of boring runtime strings ('Runtime Error!' — a CRT string the emulator re-wrote, which known_strings missed; its dedup is exact trimmed-string match against Ghidra's string list and prior artifacts) seed as individual finding comments nobody opens.
- **Rollup lines counted as leads** — runtime-noise rollup bullet lines ('- T1140 @ 0x…: ### Recovered String') are themselves graded as leads; they are suppressed noise by design, so near-total ignoring is the intended outcome, but they inflate this detector's ignored count.
- **Duplicate IOC copies** — The same real recovered IOC seeds multiple leads across per-sample scopes and sibling sessions (per-sample `seen` dedup never spans scopes: a dead-drop URL stored in several .data copies seeds once per sample scope, pairwise-SUB email/PEM re-seed per sibling sample), and agents engage one copy while the twins idle — real TPs land in the ignore column.

## What would reduce the noise
---

Static-side changes, ordered as proposed; each was checked against the true positives above so the fix does not suppress what the detector gets right.

### Require loop-local byte transforms (locality gate)

In encoded_strings.rs, replace is_decoder_loop's whole-function tally with a per-loop one: for each backward branch, count is_byte_transform hits whose parsed address falls inside [branch_target, branch_address] (both parseable via context::parse_addr, as is_backward_branch already does); flag only when one span holds >= MIN_BYTE_TRANSFORMS. Keep recover_loop_key's tally scoped to the qualifying span so the reported key comes from the actual loop.

*Effort:* medium \
*True-positive risk:* Low: true-positive decoder loops are tight loops whose transforms sit inside the back-edge span (the per-byte decode idiom by construction); the FPs (bool flips scattered across a large function, VMProtect dead ADDs outside any buffer loop) are not. Keep the per-span threshold at 3 or tolerate unrolled bodies (several transforms in one span still pass). Also shrinks the emulation candidate pool's garbage without touching real candidates.

Addresses: dual use byte arithmetic loop, emulated table bytes as strings, artifactless loop key flood.

### Demote codegen-idiom keys unless a buffer actually decodes

In decoder_loop_hits, when recover_decoder returns key-only or nothing, check the dominant (op,key) against a codegen-idiom set — XOR 0x01 with no memory-byte transform in the loop, ADD/SUB 0x30 (digit), SUB/ADD 0x20 (space/case), ADD/SUB 0x37/0x57/0x58/0x61 (hex/digit-value adjustments; 0x58 comes from strtoul's 'a'-10 adjustment) — and emit those at low() instead of medium() (or skip the key label and emit the generic loop finding). Keep full grade whenever recover_decoder produced items.

*Effort:* small \
*True-positive risk:* Moderate-low: a true-positive 'XOR decoder — key 0x01' keeps full grade because a real decryptor transforms memory bytes in-loop; true-positive keys outside the idiom set are unaffected. Demotion rather than drop preserves rollup visibility; true-positive key-only leads are a small class.

Addresses: dual use byte arithmetic loop, artifactless loop key flood.

### Roll up artifact-less loop/key findings and cap them per sample

In seed.rs partition_suppressed, add a detector-scoped rule that does not require a fingerprinted toolchain: native T1027 hits with no artifact and the loop-only / 'no static buffer resolved' content shapes are suppressed into the rollup, keeping at most the top ~8 per sample as real finding comments, ranked by whether the function also references a data buffer whose bytes look encoded (recover::src_nonprintable_ratio >= 0.5 on its first referenced slice via decode::data_addrs + slice_at). Alternatively implement in decoder_loop_hits as a per-sample cap before seeding (simpler: the disassembly and sections are in scope there).

*Effort:* medium \
*True-positive risk:* Moderate: true-positive loop/key finding leads exist, but they cluster in samples where the same function also produced recovered artifacts or genuinely references encoded data, which the top-K ranking retains; rollup lines themselves still get engaged, so demoted leads remain engageable.

Addresses: artifactless loop key flood, dual use byte arithmetic loop.

### Apply the static text gates to emulation-recovered strings

In emulation.rs sanitize_recovered (or hits_from_run, where decode::classify already runs), additionally require !recover::is_table_run(text.as_bytes()) for every recovered string, and recover::is_wordy(text.as_bytes()) when decode::classify(text) == RecoveredKind::String (typed kinds — URL/email/PEM — are exempt so numeric hosts and base64 bodies are never lost). Optionally also demote kind==String results that land at the generic 25-suspicion floor when the emulated function's static flag was key-only junk (note: emulated hits start at Unsure/Likely method confidence, never Untrusted, so any grade-based skip must key on suspicion/kind, not on Untrusted).

*Effort:* small \
*True-positive risk:* Low: true-positive emulated recoveries are wordy, non-table text — 'ResumeThread' (12/12 letters), email addresses, '-----BEGIN PUBLIC KEY-----' (14/26 letters, passes) — and typed kinds are exempted; T-table junk such as `=&&jL66Zl??A~` (5/13 letters) fails is_wordy, and the PPMd ramp `$(,048<@DHLPTX…` (+4 steps throughout) fails is_ordered_run inside is_table_run.

Addresses: emulated table bytes as strings, low grade junk and stdlib recoveries.

### Catch small-int tables at the source and contest claims per buffer

Two changes in the blob sweep: (1) extend recover::is_structured_source with a small-alphabet test — a region whose bytes are all <= 0x1f with <= 12 distinct values (or distinct <= len/8) is a table, which catches the popcount/mixed small-int shapes the four existing tests miss; (2) in encoded_strings.rs, make gather_claims record run extents (the text length is already in hand) and unambiguous() merge claims whose [addr, addr+len) ranges intersect, so one table decoded by several keys at several offsets is contested once and dropped whole.

*Effort:* medium \
*True-positive risk:* Low-moderate: true-positive blob recoveries decode from high-distinct, mostly >0x1f ciphertext (XORed URLs, pairwise-SUB ransom text whose source is encoded prose), so the small-alphabet test does not claim them — and the pairwise/base64 recoveries bypass is_structured_source entirely (no index-aligned source is ever consulted for those schemes). Range-merged contention must measure overlap on claimed runs only, or adjacent distinct buffers sharing one string table could be falsely merged — keep the existing per-address behavior when extents merely abut.

Addresses: blob sweep table fabrication, per address ambiguity ledger, low grade junk and stdlib recoveries.

### Stop promoting junk on a lone shell-operator substring

In decode.rs, tighten looks_shell_command: require suspicious_command() to match, or at least two distinct COMMAND_MARKERS of which one is an alphabetic command token (powershell, sh -c, cmd /c, /bin/, sleep , echo ); a single `||`/`&&`/`; `/` > ` substring no longer sets the Unsure validity floor in assess() or pushes the 50-suspicion 'shell command' candidate.

*Effort:* small \
*True-positive risk:* Low: a self-wipe command matches suspicious_command (`dd if=` is in SUSPICIOUS_COMMANDS, giving Likely/90 independently — pinned by the existing assess test) and a multi-marker command line (`${` + `sleep ` + `&&` + ` >> `) still passes the two-marker rule, so both keep their raise; a ransom-note email lead's confidence comes from the Email kind floor and its suspicion from the anon-provider rule (85), not the shell note.

Addresses: shell marker substring amplifier, blob sweep table fabrication.

### Honest attribution: shrink the blob window and the 'decodes it' claim

In push_decoded, pass a dedicated attribution window of ~4KB (NESTED_REF_WINDOW) instead of MAX_SCAN_BYTES (256KB) to referencing_function; and in nested_attribution plus the blob artifact role, claim 'contains the loop that decodes it' only when the attributed function's recovered (op,key) actually decodes the buffer's source bytes to the reported text (re-run the one decode and compare), else say 'references this buffer' or leave the hit sample-level.

*Effort:* small \
*True-positive risk:* Low: true-positive blob recoveries attribute to genuinely-adjacent referencing functions (a few hundred bytes from the blob's recovered runs, well inside 4KB); losing a far-fetched attribution moves a lead to file-level rather than dropping it. Mis-attributions can also come through the nested pass's already-4KB window (a real URL attributed to an unrelated DES routine), so the role-honesty half is what fixes those cases, not the window shrink.

Addresses: wrong function attribution.

## Verification notes
---

An independent pass re-checked every mechanism, anchor, and fix against the code:

- All six code anchors verified at HEAD: recover.rs:552-557 is is_structured_source (exactly the four tests claimed); encoded_strings.rs:132-136 is is_decoder_loop (whole-function transform count + any backward branch, no locality, no library-function check in decoder_loop_hits); decode.rs:701-704 is looks_shell_command (single COMMAND_MARKERS substring suffices); encoded_strings.rs:794 is push_decoded's referencing_function(index, addr, MAX_SCAN_BYTES=256KB); encoded_strings.rs:632-645 is unambiguous() keyed on exact start address; emulation.rs:338-364 is sanitize_recovered (gates are exactly: control-strip, 5..=2048 chars, >=3 distinct, >=1 letter — no is_wordy/is_table_run/reads_as_text).
- Verified the popcount-table evasion arithmetically: b[i]=16-2*popcount(i) has even-only neighbor deltas (0% vs the 25% stepped bar), max byte share 70/256=27% (vs 50% dominance bar), no all-zero stride column, and ~50/50 rise/fall (vs 80% monotone bar) — passes all four is_structured_source tests; output tests cap at MAX_PERIOD=8 and ORDERED_SHARE=85, and the fractal output passes them.
- Verified the assess() promotion path: RecoveredKind::String + looks_shell_command -> validity floor Unsure + (50, 'shell command') candidate + the exact bullet text quoted above; a `dd if=` self-wipe gets Likely/90 via suspicious_command, pinned by an existing unit test, supporting fix 6's tp_risk.
- Verified the artifactless-flood driver mechanics: partition_suppressed (seed.rs:452-497) suppresses only with a fingerprinted toolchain AND every hit Untrusted or Unsure+CallContext::Runtime; runtime_classification (detections.rs:921-959) assigns Runtime purely from function-name markers (is_library_function / toolchain markers), so stripped FUN_ names never suppress — claim exact.
- 'active' statuses confirmed via git: encoded_strings.rs, recover.rs, decode.rs last changed in 9e4a6f1f (2026-10-26); 5bfd2ab5 (2026-10-30) touched emulation.rs (test plumbing only) and seed.rs (telemetry provenance recording only) — no heuristic changes, so nothing is already_fixed.
- Corrected mechanism 2's description: emulation selects up to MAX_EMULATED_FUNCTIONS=8 candidates per sample (unrecovered-first), not 'every function the loop pass flagged'.
- Corrected fix 4: its original 'skip strings whose assessment stays Untrusted with suspicion <= 25' clause was a no-op — emulated hits start at high()/medium() method confidence (Likely/Unsure) and assess() never lowers, so Untrusted is unreachable; rewritten to key on kind/suspicion. Also scoped the is_wordy gate to kind==String so numeric IOCs (IP:port hosts) and typed recoveries are never dropped.
- Amended fix 2's idiom key set to include 0x58 (strtoul's digit-value key); original listed only 0x37/0x57/0x61.
- Amended fix 7's tp_risk to note that mis-attribution can occur under the nested pass's existing 4KB window, so the window shrink alone does not fix that case — the role-honesty half does.
- Disputed rollup bullet lines where agents re-raised suppressed entries as real signal are disputes of the suppression grading, not of a detector heuristic, so no mechanism was added for them.
