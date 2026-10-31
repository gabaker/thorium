//! Encoded-string obfuscation / deobfuscation (MITRE ATT&CK **T1027** / **T1140**).
//!
//! Detects the near-universal malware habit of hiding its strings (C2 domains,
//! commands, ransom notes) from `strings`/YARA by storing them encoded and decoding
//! them at runtime, and — the point of this module — **statically recovers the
//! plaintext and the key** so the flag carries the deobfuscated IOCs directly.
//!
//! Three passes:
//!
//! 1. **Decoder-loop recovery** (T1027) — a function with a byte-wise `XOR`/`SUB`/
//!    `ADD` transform inside a loop is the stack-string / string-decrypt idiom. We
//!    recover the key from the loop's immediate, find the data buffer(s) the
//!    function references, decode them with that exact key (plus the pairwise
//!    SUB), and embed the recovered strings + key in the flag as markdown.
//! 2. **Bounded blob decode** (T1140) — brute-forcing trivial schemes (single-byte
//!    XOR/SUB/ADD, and the pairwise SUB) over data sections and surfacing a
//!    hard-capped number of recovered strings with their key.
//! 3. **Chained decode** (T1140) — passes 1 and 2 each peel exactly one layer, so a
//!    value stored as base64-over-a-cipher is invisible to both: the outer layer
//!    decodes to binary, which neither pass will look at again. This pass carves the
//!    stored runs that are well-formed under an encoding alphabet — including any
//!    custom 64-symbol table the program carries — and hands each to
//!    [`crate::thorpal::detect::decode::chain`], which peels layers in order under a bounded search.
//!    A recovered buffer is then read structurally by [`crate::thorpal::detect::decode::records`],
//!    turning a decoded blob into a table of classified configuration fields.
//!
//! The decode primitives, readable-run extraction, and buffer recovery live in
//! [`crate::thorpal::detect::decode`]; this file is the detection policy on top of them.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use ghidra_cli::models::Instruction;

use crate::thorpal::detect::context::{AnalysisContext, SectionBytes, fmt_addr, parse_addr};
use crate::thorpal::detect::decode::{self, base64, chain, records, recover};
use crate::thorpal::detect::detections::{
    ArtifactClass, AttackTechnique, Detection, DetectionHit, EvidenceKind, HitArtifact,
    confidence_rank, high, is_library_function, low, medium, parse_imm,
};

use super::{T1027, T1140};

/// The minimum byte-wise transforms in one function to call it a decoder loop.
const MIN_BYTE_TRANSFORMS: usize = 3;

/// The hard cap on strings surfaced by the blob-decode pass (keeps output sane).
const MAX_DECODED_STRINGS: usize = 25;

/// The max bytes scanned per section/buffer by the brute force (bounds cost).
const MAX_SCAN_BYTES: usize = 256 * 1024;

/// The max distinct data buffers decoded per decoder-loop function (bounds cost).
const MAX_BUFFERS_PER_FN: usize = 12;

/// The Encoded-string detection (ATT&CK T1027 / T1140).
pub struct EncodedStrings;

/// Return whether an instruction is a byte-wise `XOR`/`SUB`/`ADD` transform.
///
/// Matches either a byte-sized memory transform (`... byte ptr [..]`) or a
/// transform whose destination is an 8-bit register (`al`, `r8b`, …) — the shapes a
/// per-character string decoder takes.
///
/// # Arguments
///
/// * `insn` - The instruction to classify
fn is_byte_transform(insn: &Instruction) -> bool {
    // only add/sub/xor can be a per-byte decode step
    let m = insn.mnemonic.to_ascii_uppercase();
    if !matches!(m.as_str(), "XOR" | "SUB" | "ADD") {
        return false;
    }
    // require a byte-sized operand: a memory byte, or an 8-bit destination register
    let ops = crate::thorpal::detect::op_str(insn).to_ascii_lowercase();
    let first = ops.split(',').next().unwrap_or("").trim();
    ops.contains("byte ptr [")
        || matches!(
            first,
            "al" | "bl"
                | "cl"
                | "dl"
                | "sil"
                | "dil"
                | "bpl"
                | "spl"
                | "ah"
                | "bh"
                | "ch"
                | "dh"
                | "r8b"
                | "r9b"
                | "r10b"
                | "r11b"
                | "r12b"
                | "r13b"
                | "r14b"
                | "r15b"
        )
}

/// Return whether an instruction is a backward branch (a loop back-edge).
///
/// # Arguments
///
/// * `insn` - The instruction to classify
fn is_backward_branch(insn: &Instruction) -> bool {
    // any jump mnemonic (JMP or a Jcc) can close a loop
    let m = insn.mnemonic.to_ascii_uppercase();
    if !m.starts_with('J') {
        return false;
    }
    // a jump to a lower address is a back-edge (a loop)
    let ops = crate::thorpal::detect::op_str(insn);
    match (
        crate::thorpal::detect::context::parse_addr(ops.trim()),
        crate::thorpal::detect::context::parse_addr(&insn.address),
    ) {
        (Some(target), Some(here)) => target < here,
        _ => false,
    }
}

/// The inclusive `[target, branch]` address span one back-edge closes.
type LoopSpan = (u64, u64);

/// Find the back-edge spans that hold enough byte transforms to be decoder loops.
///
/// For each backward branch, counts the byte transforms whose address falls inside
/// `[branch target, branch address]` — the loop body — and keeps the spans holding
/// at least [`MIN_BYTE_TRANSFORMS`]. The per-span tally is the locality that
/// separates the per-byte decode idiom, whose transforms sit inside the loop by
/// construction, from a large function whose scattered boolean flips and digit
/// conversions merely coexist with some unrelated loop.
///
/// # Arguments
///
/// * `insns` - The function's instructions
fn decoder_loop_spans(insns: &[Instruction]) -> Vec<LoopSpan> {
    // the parsed addresses of every byte transform, sorted for range counting
    let mut transforms: Vec<u64> = insns
        .iter()
        .filter(|insn| is_byte_transform(insn))
        .filter_map(|insn| parse_addr(&insn.address))
        .collect();
    transforms.sort_unstable();
    // judge each back-edge by the transforms inside its span
    let mut spans = Vec::new();
    for insn in insns.iter().filter(|insn| is_backward_branch(insn)) {
        // both ends parse — is_backward_branch already required that
        let ops = crate::thorpal::detect::op_str(insn);
        let (Some(target), Some(branch)) = (parse_addr(ops.trim()), parse_addr(&insn.address))
        else {
            continue;
        };
        // count the transforms inside [target, branch] via the sorted index
        let first = transforms.partition_point(|addr| *addr < target);
        let last = transforms.partition_point(|addr| *addr <= branch);
        if last - first >= MIN_BYTE_TRANSFORMS {
            spans.push((target, branch));
        }
    }
    spans
}

/// Whether an instruction sits inside any qualifying loop span.
///
/// # Arguments
///
/// * `insn` - The instruction to locate
/// * `spans` - The qualifying `[target, branch]` loop spans
fn in_spans(insn: &Instruction, spans: &[LoopSpan]) -> bool {
    // an unparseable address cannot be placed in any span
    parse_addr(&insn.address).is_some_and(|addr| {
        spans
            .iter()
            .any(|(start, end)| addr >= *start && addr <= *end)
    })
}

/// The single-byte key recovered from a decoder loop: its op and immediate.
struct LoopKey {
    /// The transform op, as a display label (`"XOR"`, `"SUB"`, `"ADD"`).
    scheme: &'static str,
    /// The single-byte key value.
    key: u8,
}

/// Recover the dominant single-byte `(op, key)` from a function's byte transforms.
///
/// Tallies each `xor/sub/add <byte>, <imm>` immediate **inside the qualifying loop
/// spans** — so the reported key comes from the actual loop, not from digit
/// conversions elsewhere in the function — and returns the most common one (a
/// rolling/multi-byte key or a register-key buffer yields `None`, handled by the
/// pairwise / brute passes instead).
///
/// # Arguments
///
/// * `insns` - The function's instructions
/// * `spans` - The function's qualifying loop spans (see [`decoder_loop_spans`])
fn recover_loop_key(insns: &[Instruction], spans: &[LoopSpan]) -> Option<LoopKey> {
    // count occurrences of each (op, immediate) pair among in-span byte transforms
    let mut tally: Vec<(&'static str, u8, usize)> = Vec::new();
    for insn in insns
        .iter()
        .filter(|i| is_byte_transform(i) && in_spans(i, spans))
    {
        // the op label
        let scheme = match insn.mnemonic.to_ascii_uppercase().as_str() {
            "XOR" => "XOR",
            "SUB" => "SUB",
            "ADD" => "ADD",
            _ => continue,
        };
        // the immediate is the second operand (`al, 0x2a`)
        let ops = crate::thorpal::detect::op_str(insn);
        let Some(imm_tok) = ops.split(',').nth(1) else {
            continue;
        };
        let Some(imm) = parse_imm(imm_tok.trim()) else {
            continue;
        };
        // a byte key only (ignore wider immediates)
        if imm > 0xff {
            continue;
        }
        let key = imm as u8;
        // bump the tally for this (op, key)
        match tally.iter_mut().find(|(s, k, _)| *s == scheme && *k == key) {
            Some((_, _, n)) => *n += 1,
            None => tally.push((scheme, key, 1)),
        }
    }
    // return the most frequent (op, key), if any
    tally
        .into_iter()
        .max_by_key(|(_, _, n)| *n)
        .map(|(scheme, key, _)| LoopKey { scheme, key })
}

/// One item recovered from a decoder-loop function.
struct RecoveredItem {
    /// The address the item's encoded bytes start at.
    addr: u64,
    /// The section the bytes came from (e.g. `.rodata`).
    section: String,
    /// The recovered plaintext (may be multi-line, e.g. a PEM armor).
    text: String,
}

/// The analysis of a decoder-loop function: its scheme/key and recovered items.
struct DecoderResult {
    /// The loop's transform op label (`"XOR"`/`"SUB"`/`"ADD"`).
    scheme: &'static str,
    /// The recovered single-byte key, rendered for display (e.g. `"0x2a"`).
    key: String,
    /// Whether the `(op, key)` is a common compiler idiom (see
    /// [`is_codegen_idiom_key`]) — meaningful only when `items` is empty, since a
    /// decoded buffer proves the loop a decoder whatever its key looks like.
    codegen_idiom: bool,
    /// The recovered items (empty when only a key was recovered).
    items: Vec<RecoveredItem>,
}

/// Whether a single-byte `(op, key)` is one ordinary codegen emits constantly.
///
/// These are the keys the key-only findings dispute over: `xor reg8, 1` boolean
/// flips, `add`/`sub 0x30` digit↔ASCII conversion, `sub 0x20` space/case
/// arithmetic, and the `0x37`/`0x57`/`0x58`/`0x61` hex- and digit-value
/// adjustments (`'a' - 10` and friends, `strtoul`'s `0x58`). The boolean flip only
/// counts when the loop never transforms a memory byte — a real `XOR 0x01`
/// decryptor (`FlipLocker`'s) walks a buffer, which a `!flag` idiom never does.
/// Confirmed decoder keys (`0x6d`, `0x80`, `0x41`, `0x2b`, …) are not in the set.
///
/// # Arguments
///
/// * `scheme` - The transform op label (`"XOR"`/`"SUB"`/`"ADD"`)
/// * `key` - The single-byte key value
/// * `memory_transform` - Whether any in-span transform touches a memory byte
fn is_codegen_idiom_key(scheme: &str, key: u8, memory_transform: bool) -> bool {
    match (scheme, key) {
        // the boolean flip: register-only `xor reg8, 1`
        ("XOR", 0x01) => !memory_transform,
        // digit <-> ASCII conversion (0x30), space/case arithmetic (0x20), and
        // the hex/digit-value adjustments ('A'-10, 'a'-10, the strtoul forms)
        ("ADD" | "SUB", 0x20 | 0x30 | 0x37 | 0x57 | 0x58 | 0x61) => true,
        _ => false,
    }
}

/// A buffer read as interleaved `(value, key)` pairs, decoded with the loop's key.
struct KeyedPairs {
    /// The decoded value-column bytes.
    decoded: Vec<u8>,
    /// The stored value-column bytes the decode was applied to.
    values: Vec<u8>,
    /// The byte offset of the value column within each pair (0 or 1).
    phase: usize,
}

/// The least share of a buffer's key-column bytes equal to the loop's own key before
/// the buffer reads as interleaved `(value, key)` pair storage, in percent.
///
/// The pair layout stores the key beside every character, so a genuine buffer runs at
/// (nearly) 100%; the tolerance only absorbs the odd raw separator between encoded
/// lines. Prose, UTF-16 text and compiler tables have no reason to repeat a loop
/// immediate on alternate bytes, so the bar's job is just to shrug off coincidence.
const KEYED_PAIR_SHARE: usize = 75;

/// The fewest `(value, key)` pairs a buffer needs before its key column means
/// anything.
///
/// A short stretch of matching alternate bytes is a coincidence; a stack string worth
/// recovering carries at least [`recover::MIN_RECOVERY_CHARS`] characters anyway, so
/// the floor costs nothing real.
const MIN_KEYED_PAIRS: usize = 16;

/// Decode a buffer stored as interleaved `(value, key)` pairs with the loop's key.
///
/// The `GandCrab`-style stack-string layout stores each plaintext character as two
/// bytes — the shifted character, then the per-character key byte — and its decoder
/// loop transforms only the value bytes, so the contiguous decode renders every other
/// byte non-printable and recovers nothing even though the exact key was observed
/// ("SUB decoder — key 0x3a, no static buffer resolved"). When one column of the
/// buffer is (nearly) all the loop's own key, the other column is the encoded text:
/// decode it with the observed op and key. The key column equaling the observed
/// immediate is the corroboration — ordinary text, UTF-16LE strings (zero high
/// bytes; a zero key never qualifies) and compiler tables do not interleave a
/// constant equal to a loop immediate.
///
/// # Arguments
///
/// * `loop_key` - The loop's recovered `(op, key)`
/// * `bytes` - The referenced buffer's bytes
fn keyed_pair_decode(loop_key: &LoopKey, bytes: &[u8]) -> Option<KeyedPairs> {
    // a zero key would match UTF-16LE wide text's zero highs, never pair storage
    if loop_key.key == 0 {
        return None;
    }
    // try both phases: the key byte may trail or lead each pair
    for phase in 0..2usize {
        // the key sits on the opposite side of each two-byte pair
        let key_at = 1 - phase;
        // walk pairs from the head: an isolated miss is tolerated (a raw separator
        // between encoded lines), two in a row end the pair-storage region — the
        // referenced slice runs to the section end, far past the stored string
        let mut taken = 0usize;
        let mut matching = 0usize;
        let mut misses = 0usize;
        for pair in bytes.as_chunks::<2>().0 {
            if pair[key_at] == loop_key.key {
                misses = 0;
                matching += 1;
            } else {
                misses += 1;
                if misses > 1 {
                    break;
                }
            }
            taken += 1;
        }
        // enough pairs, and the walked region's key column is (nearly) all the key
        if matching < MIN_KEYED_PAIRS || matching * 100 < taken * KEYED_PAIR_SHARE {
            continue;
        }
        // the value column of the pair region, decoded with the loop's own op + key
        let values: Vec<u8> = bytes[..taken * 2]
            .iter()
            .skip(phase)
            .step_by(2)
            .copied()
            .collect();
        let decoded = match loop_key.scheme {
            "XOR" => decode::xor::single_byte(loop_key.key, &values),
            "SUB" => decode::arithmetic::sub_byte(loop_key.key, &values),
            "ADD" => decode::arithmetic::add_byte(loop_key.key, &values),
            _ => return None,
        };
        return Some(KeyedPairs {
            decoded,
            values,
            phase,
        });
    }
    None
}

/// Attempt to statically recover the plaintext a decoder loop produces.
///
/// Recovers the single-byte immediate key from the loop, reads the *encoded* data
/// buffer(s) the function references, decodes them with that exact key — contiguously
/// first, then as interleaved `(value, key)` pair storage ([`keyed_pair_decode`]) —
/// and splits the result into logical items. Returns the items + key, a key-only
/// result when the key is known but no encoded buffer decodes, or `None` when the
/// loop uses no single-byte immediate (e.g. a register/key-buffer or a pairwise
/// SUB — the blob pass recovers those).
///
/// A buffer is only decoded when its **source bytes look encoded** (mostly
/// non-printable, via [`recover::observed_key_runs`]) and the decode is a real
/// recovery ([`recover::is_strong_recovery`]): the function references every data
/// address it touches — including ordinary plaintext — so a near-identity key like
/// `0x01` would otherwise "recover" the section's real strings as false positives.
/// The observed key corroborates the decode, so the output-shape table vetoes the
/// brute-force sweeps need do not apply here; the source-side layout reads still do.
///
/// # Arguments
///
/// * `insns` - The decoder function's instructions
/// * `spans` - The function's qualifying loop spans (see [`decoder_loop_spans`])
/// * `sections` - The program's initialized section bytes
fn recover_decoder(
    insns: &[Instruction],
    spans: &[LoopSpan],
    sections: &[SectionBytes],
) -> Option<DecoderResult> {
    // require a single-byte immediate key; register/key-buffer/pairwise loops have
    // none and are left to the blob-decode pass
    let loop_key = recover_loop_key(insns, spans)?;
    // gather the distinct data buffers the function references
    let addrs = decode::data_addrs(insns);
    let mut items: Vec<RecoveredItem> = Vec::new();
    let mut buffers_tried = 0usize;

    for addr in addrs {
        if buffers_tried >= MAX_BUFFERS_PER_FN {
            break;
        }
        // slice the buffer from its (data-like) section
        let Some((section, bytes)) = decode::slice_at(sections, addr, MAX_SCAN_BYTES) else {
            continue;
        };
        if !decode::buffers::is_data_section(section) {
            continue;
        }
        buffers_tried += 1;

        // decode with the exact key the loop uses
        let decoded = match loop_key.scheme {
            "XOR" => decode::xor::single_byte(loop_key.key, bytes),
            "SUB" => decode::arithmetic::sub_byte(loop_key.key, bytes),
            "ADD" => decode::arithmetic::add_byte(loop_key.key, bytes),
            _ => Vec::new(),
        };
        // require a genuine recovery (encoded source + substantial contiguous text) —
        // not the section's plaintext or wordy-looking garbage from a near-identity key
        let runs = recover::observed_key_runs(&decoded, bytes);
        if recover::is_strong_recovery(&runs, recover::MIN_RECOVERY_CHARS) {
            // split into logical items (a PEM armor coalesced; trailing text split off)
            for (off, text) in recover::extract_items(runs) {
                items.push(RecoveredItem {
                    addr: addr.saturating_add(off as u64),
                    section: section.to_string(),
                    text,
                });
            }
            continue;
        }
        // the contiguous decode found nothing: the buffer may store (value, key)
        // pairs the loop walks at stride two — the GandCrab note's layout — which
        // the same observed key decodes column-wise
        let Some(pairs) = keyed_pair_decode(&loop_key, bytes) else {
            continue;
        };
        // the key column corroborates the decode structurally, so the value column
        // is judged by the output-quality and source-layout reads alone
        let runs = recover::keyed_pair_runs(&pairs.decoded, &pairs.values);
        if !recover::is_strong_recovery(&runs, recover::MIN_RECOVERY_CHARS) {
            continue;
        }
        // map value-column offsets back onto the interleaved buffer (two bytes per
        // character, plus the phase the value column sits on)
        for (off, text) in recover::extract_items(runs) {
            items.push(RecoveredItem {
                addr: addr.saturating_add((off * 2 + pairs.phase) as u64),
                section: section.to_string(),
                text,
            });
        }
    }

    // whether any in-span transform writes a memory byte: a register-only XOR 0x01
    // is the compiler's boolean flip, while a buffer decode walks memory
    let memory_transform = insns.iter().any(|insn| {
        is_byte_transform(insn)
            && in_spans(insn, spans)
            && crate::thorpal::detect::op_str(insn)
                .to_ascii_lowercase()
                .contains("byte ptr [")
    });
    // report the recovered items (or a key-only result when none decoded)
    Some(DecoderResult {
        scheme: loop_key.scheme,
        key: format!("0x{:02x}", loop_key.key),
        codegen_idiom: is_codegen_idiom_key(loop_key.scheme, loop_key.key, memory_transform),
        items,
    })
}

#[async_trait::async_trait]
impl Detection for EncodedStrings {
    /// Short, stable name for logs.
    fn name(&self) -> &'static str {
        "encoded_strings"
    }

    /// The primary technique this detection reports.
    fn technique(&self) -> AttackTechnique {
        T1027
    }

    /// Flag decoder loops (with recovered plaintext) and blob-decoded strings.
    #[tracing::instrument(name = "EncodedStrings::run", skip_all, err(Debug))]
    async fn run(&self, ctx: &AnalysisContext) -> Result<Vec<DetectionHit>> {
        // collect hits from each sub-pass
        let mut hits = Vec::new();

        // 1) decoder-loop recovery — over every function's (cached) disassembly
        hits.extend(decoder_loop_hits(ctx).await?);

        // 2) bounded blob decode — recover strings hidden by a trivial scheme
        hits.extend(blob_decode_hits(ctx).await?);

        // 3) chained decode — recover values stored behind more than one layer
        hits.extend(nested_decode_hits(ctx).await?);

        Ok(hits)
    }
}

/// Detect decoder-loop functions and, for each, statically recover the plaintext.
///
/// Functions whose names mark them as statically-linked runtime code only produce a
/// hit here when plaintext was actually recovered: zlib, LZMA and CRT conversion
/// helpers are made of byte-arithmetic loops, and their artifact-less loop/key
/// findings are codegen noise rather than the sample hiding strings.
///
/// # Arguments
///
/// * `ctx` - The analysis context
///
/// # Errors
///
/// Returns an error if disassembling the program or reading its sections fails.
#[tracing::instrument(name = "encoded_strings::decoder_loop_hits", skip_all, err(Debug))]
async fn decoder_loop_hits(ctx: &AnalysisContext) -> Result<Vec<DetectionHit>> {
    // the whole program's disassembly + section bytes (both cached)
    let all = ctx.all_function_disassembly().await?;
    let sections = ctx.initialized_bytes().await?;

    let mut hits = Vec::new();
    for (entry, insns) in &all {
        // a decoder loop: a back-edge span holding enough byte transforms
        let spans = decoder_loop_spans(insns);
        if spans.is_empty() {
            continue;
        }
        // recognized runtime code (zlib, LZMA, CRT digit conversion) is full of
        // byte-arithmetic loops; artifact-less findings there are codegen noise
        let library = ctx.function_name(*entry).is_some_and(is_library_function);
        // the transforms that sit inside the qualifying loops
        let transforms = insns
            .iter()
            .filter(|i| is_byte_transform(i) && in_spans(i, &spans))
            .count();
        // try to statically recover the key + plaintext this loop produces
        match recover_decoder(insns, &spans, &sections) {
            // recovered plaintext: one typed flag per item, strong evidence
            Some(r) if !r.items.is_empty() => {
                for item in &r.items {
                    let kind = decode::classify(&item.text);
                    let name = decode::recovered_name(kind, &item.text, &item.section, item.addr);
                    let content = decode::recovered_content(
                        kind,
                        &item.text,
                        &item.section,
                        item.addr,
                        r.scheme,
                        Some(&r.key),
                    );
                    // an exact-key decoder-loop decode is reliable (Likely base); the
                    // content then sets suspicion and can only raise confidence further
                    let assessment = decode::assess(high().0, kind, &item.text);
                    let reasoning = decode::append_assessment(
                        decode::recovery_reasoning(&decoder_reasoning(), r.scheme, Some(&r.key)),
                        &assessment.notes,
                    );
                    hits.push(
                        DetectionHit::new(
                            T1027,
                            *entry,
                            Some(*entry),
                            (assessment.confidence, assessment.suspicion),
                            content,
                            reasoning,
                        )
                        .name(name)
                        .kind(EvidenceKind::ContextOnly)
                        // carry the full plaintext as a typed artifact so the
                        // report inventory lists it and later stages (the
                        // emulation stage's candidate ranking and dedup) can
                        // see what static recovery already produced
                        .artifact(
                            HitArtifact::new(decode::artifact_class(kind), item.text.clone())
                                .role(format!(
                                    "{} decoder loop that produces this value",
                                    r.scheme
                                ))
                                .detail(format!(
                                    "statically decoded from {} @ {} with key {}",
                                    item.section,
                                    fmt_addr(item.addr),
                                    r.key
                                )),
                        ),
                    );
                }
            }
            // only the key was recovered: report it, note plaintext needs inspection
            Some(r) => {
                // a recognized runtime function with nothing decoded is not the
                // sample hiding strings — skip rather than flood
                if library {
                    continue;
                }
                hits.push(key_only_hit(*entry, &r));
            }
            // nothing recoverable: fall back to flagging the loop itself
            None => {
                // the same runtime-noise rule as the key-only shape
                if library {
                    continue;
                }
                hits.push(
                    DetectionHit::new(
                        T1027,
                        *entry,
                        Some(*entry),
                        medium(),
                        format!(
                            "string-decoder loop ({transforms} byte transforms) @ {}",
                            fmt_addr(*entry)
                        ),
                        decoder_reasoning(),
                    )
                    .kind(EvidenceKind::ContextOnly),
                );
            }
        }
    }
    Ok(hits)
}

/// Build the hit for a decoder loop whose key was recovered but decoded nothing.
///
/// A codegen-idiom key with no decoded buffer is far more often digit conversion,
/// case arithmetic or a bool flip than a decoder, so those report at low grade —
/// they stay visible (and roll up) without crowding out real recoveries; every
/// other key-only loop keeps the medium grade.
///
/// # Arguments
///
/// * `entry` - The decoder function's entry address
/// * `r` - The function's decoder analysis (no items recovered)
fn key_only_hit(entry: u64, r: &DecoderResult) -> DetectionHit {
    // the finding names the op, the key, and the fact that nothing resolved
    let content = format!(
        "{} decoder — key {}, no static buffer resolved @ {}",
        r.scheme,
        r.key,
        fmt_addr(entry)
    );
    // demote the compiler-idiom keys; keep full grade for distinctive ones
    let (severity, reasoning) = if r.codegen_idiom {
        (
            low(),
            format!(
                "{}\n\nThe key is a byte value ordinary compiler codegen uses constantly \
                 (digit/hex conversion, case arithmetic, boolean flips), and no referenced \
                 buffer decoded with it — treat this as a weak lead unless the function \
                 also touches encoded data.",
                decoder_reasoning()
            ),
        )
    } else {
        (medium(), decoder_reasoning())
    };
    DetectionHit::new(T1027, entry, Some(entry), severity, content, reasoning)
        .kind(EvidenceKind::ContextOnly)
}

/// The shared analyst-facing reasoning for a decoder-loop finding.
fn decoder_reasoning() -> String {
    "Contains a loop that applies byte-wise XOR/SUB/ADD to a buffer — the stack-string / \
     string-decryption idiom malware uses to hide strings from `strings`/YARA and decode them at \
     runtime. Any recovered plaintext above was deobfuscated statically and should be treated as \
     candidate IOCs."
        .to_string()
}

/// Brute-force a few trivial obfuscation schemes over the program's data sections
/// and surface a hard-capped number of recovered, human-readable strings + keys.
///
/// Schemes: single-byte XOR (high-bit keys `0x80..=0xFF`), single-byte SUB/ADD, and the
/// pairwise SUB — all guarded by [`recover::hidden_runs`] /
/// [`decode::pairwise::hidden_runs`] so only genuinely-encoded data survives, and then by
/// [`swept_runs`]'s density test so an isolated coincidence in a quarter-megabyte section
/// does not survive with them.
///
/// # Arguments
///
/// * `ctx` - The analysis context
///
/// # Errors
///
/// Returns an error if reading the program's section bytes fails.
#[tracing::instrument(name = "encoded_strings::blob_decode_hits", skip_all, err(Debug))]
async fn blob_decode_hits(ctx: &AnalysisContext) -> Result<Vec<DetectionHit>> {
    let mut hits = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    // read (cached) the initialized section bytes once
    let sections = ctx.initialized_bytes().await?;
    // the reverse index used to attribute each recovered buffer to the function that
    // references it; a run with no referencing function falls back to the sample at
    // submit time
    let index = ctx.reference_index().await?;
    // each decoder-loop function's own recovered key, so an attribution can say
    // "decodes it" only after re-running that key over the buffer (both cached)
    let all = ctx.all_function_disassembly().await?;
    let decoders = decoder_keys(&all);
    // every recovery the sweep produces, held back so the sweep can be judged as a
    // whole before any of it is reported (see `unambiguous`)
    let mut found: Vec<BlobCandidate> = Vec::new();
    // and the ranges each key made readable at all, which is the wider ledger the
    // ambiguity test is judged on
    let mut claims: Vec<ClaimedRange> = Vec::new();
    'sweep: for (name, base, bytes) in &sections {
        // only scan data-like sections (skip code/other to limit FP and cost)
        if !decode::buffers::is_data_section(name) {
            continue;
        }
        // bound the brute force to the head of the section
        let slice = &bytes[..bytes.len().min(MAX_SCAN_BYTES)];

        // scheme A: pairwise SUB (wide strings excluded)
        let (readable, claimed) = decode::pairwise::hidden_runs_and_claims(slice);
        gather_claims(&mut claims, claimed, None, *base);
        if gather_groups(&mut found, readable, "pairwise SUB", None, name, *base) {
            break 'sweep;
        }

        // scheme B: single-byte XOR — high-bit keys only (0x80..=0xFF). A low key
        // XORs ASCII text (and 0x00 padding) back into the printable range, which is
        // indistinguishable from plaintext and floods the results with false
        // positives; a high-bit key maps plaintext/padding to non-printable bytes,
        // so only genuinely-encoded data survives the hidden_runs filter.
        for key in 0x80u8..=0xff {
            let decoded = decode::xor::single_byte(key, slice);
            let (readable, claimed) = swept_runs(&decoded, slice);
            gather_claims(&mut claims, claimed, Some(key), *base);
            if gather_groups(&mut found, readable, "XOR", Some(key), name, *base) {
                break 'sweep;
            }
        }

        // scheme C: single-byte SUB/ADD — the hidden_runs guard (source mostly
        // non-printable, and not a table) keeps these from firing on ordinary
        // plaintext or on a region of small integers.
        for key in 0x01u8..=0xff {
            for (scheme, decoded) in [
                ("SUB", decode::arithmetic::sub_byte(key, slice)),
                ("ADD", decode::arithmetic::add_byte(key, slice)),
            ] {
                let (readable, claimed) = swept_runs(&decoded, slice);
                gather_claims(&mut claims, claimed, Some(key), *base);
                if gather_groups(&mut found, readable, scheme, Some(key), name, *base) {
                    break 'sweep;
                }
            }
        }
    }

    // report only the recoveries no other key also claims, best first, up to the cap
    let mut ranked = unambiguous(found, &claims);
    ranked.sort_by_key(rank_key);
    for candidate in ranked {
        push_decoded(
            &mut hits, &mut seen, &index, &decoders, &sections, candidate,
        );
        if hits.len() >= MAX_DECODED_STRINGS {
            tracing::warn!(
                cap = MAX_DECODED_STRINGS,
                "blob decode hit its per-sample cap; later recoveries were dropped"
            );
            break;
        }
    }
    Ok(hits)
}

/// The runs one byte-key decode contributes to the sweep, as `(reportable, claimed)`.
///
/// [`recover::hidden_runs`] judges each run on its own merits, which is the right question
/// for a buffer a decoder loop pointed at. It is not enough for a section sweep: applied
/// to a quarter of a megabyte, a wrong key turns up a handful of individually-plausible
/// runs by chance whatever the section holds. [`recover::dense_clusters`] asks the second
/// question — is this run part of a recovery, or is it alone out there — which is the one
/// that separates them.
///
/// The second half is what [`unambiguous`] counts, and it keeps the decoded tables the
/// first half drops. A table is never reported, but it is still this key's claim on the
/// address it came out of: leave it out of the tally and filtering one key's table simply
/// hands the address to the next key's, which is a fabrication promoted rather than
/// suppressed.
///
/// # Arguments
///
/// * `decoded` - The decoded bytes
/// * `src` - The original bytes, aligned index-for-index with `decoded`
fn swept_runs(decoded: &[u8], src: &[u8]) -> recover::RunsAndClaims {
    // every run this key made readable, decoded tables included — its claim on the section
    let claimed = recover::hidden_claim_runs(decoded, src);
    // what may be reported is that, less the runs that are a decoded table
    let readable: Vec<(usize, String)> = claimed
        .iter()
        .filter(|(_, text)| !recover::is_table_run(text.as_bytes()))
        .cloned()
        .collect();
    (
        recover::dense_clusters(
            readable,
            recover::MAX_RECOVERY_GAP,
            recover::MIN_RECOVERY_CHARS,
        ),
        recover::dense_clusters(
            claimed,
            recover::MAX_RECOVERY_GAP,
            recover::MIN_RECOVERY_CHARS,
        ),
    )
}

/// One recovery the brute force produced, held back until the whole sweep can be
/// judged (see [`unambiguous`]).
struct BlobCandidate {
    /// The section address the encoded bytes start at.
    addr: u64,
    /// The name of the section the bytes came from.
    section: String,
    /// The decode scheme label (e.g. `"XOR"`).
    scheme: &'static str,
    /// The single-byte key, when the scheme uses one.
    key: Option<u8>,
    /// The recovered, human-readable item text.
    text: String,
}

/// One decode's claim on a stretch of section bytes: the range it made readable
/// and the key that did it.
struct ClaimedRange {
    /// The claimed range's first address.
    start: u64,
    /// The claimed range's end address (exclusive).
    end: u64,
    /// The single-byte key, when the scheme uses one.
    key: Option<u8>,
}

/// The max recoveries gathered before the sweep gives up.
///
/// Generous next to [`MAX_DECODED_STRINGS`], because most of what the sweep gathers is
/// meant to be thrown away by [`unambiguous`] — but still bounded, since a pathological
/// section should not be allowed to run the key space to its end.
const MAX_BLOB_CANDIDATES: usize = 20 * MAX_DECODED_STRINGS;

/// Drop every recovery whose claimed range another key's claim overlaps.
///
/// A buffer is encoded with one key. When one stretch of bytes reads as text under a
/// dozen of them, that is a property of the bytes rather than of anything hidden in
/// them — the region holds small numbers, and any additive or XOR constant walks them
/// into the printable range. The individual outputs are wordy, diverse and perfectly
/// plausible, so nothing about any one of them gives it away; only seeing the others
/// does.
///
/// Contention is judged on claimed **ranges**, not exact start addresses. A long
/// structured table read under several keys breaks into runs at different offsets —
/// each key's printable window snaps at different bytes — so no two of its
/// fabrications ever share a start address, and contention keyed on exact starts
/// would pass every one of them (the demo corpus holds a 256-byte table read as four
/// such runs: two keys, four offsets). Overlap must be strict, though: ranges that
/// merely abut are two adjacent buffers — a string table packs its entries end to
/// end — and merging them would hand one buffer's contention to its innocent
/// neighbour.
///
/// Both sides of a contested overlap are dropped rather than picking a winner. There
/// is no basis to prefer one key over another, and a fabrication asserted with a
/// specific key attached is worse than a gap.
///
/// The ledger is `claims` rather than `found`, and the two differ: a key whose output
/// over a range was a decoded table is never a candidate, but it is still a key that
/// made those bytes read as text, which is exactly the question being asked. Counting
/// only the reportable recoveries would make every run filter downstream of it a
/// promotion mechanism — quietly handing a contested range to whichever key's
/// fabrication happened to survive the filters.
///
/// # Arguments
///
/// * `found` - Every recovery the sweep gathered, in the order it found them
/// * `claims` - Every range a key made read as text, decoded tables included
fn unambiguous(found: Vec<BlobCandidate>, claims: &[ClaimedRange]) -> Vec<BlobCandidate> {
    // keep the recoveries no other key's claimed range strictly overlaps
    found
        .into_iter()
        .filter(|candidate| {
            // the candidate's own claimed range
            let start = candidate.addr;
            let end = candidate.addr.saturating_add(candidate.text.len() as u64);
            // contested when a different key's claim intersects it (abutting is not
            // intersecting: adjacent buffers legitimately decode under distinct keys)
            !claims
                .iter()
                .any(|claim| claim.key != candidate.key && claim.start < end && start < claim.end)
        })
        .collect()
}

/// How one recovery ranks against the rest of the sweep — smaller sorts first, and first
/// is best.
///
/// The sweep gathers up to [`MAX_BLOB_CANDIDATES`] recoveries and [`MAX_DECODED_STRINGS`]
/// has to throw most of them away. Left in the order they were gathered, that decision is
/// made by the order the key space is walked rather than by anything about the findings:
/// the first scheme swept fills the report and no later scheme is ever heard from,
/// however much better what it found was. A recovered private key that a `SUB 0xd3` pass
/// turns up at the end is exactly the finding the cap should be protecting.
///
/// So the cap is applied to a ranking instead, on the same two axes the finding is scored
/// on — [`decode::assess`]'s confidence and suspicion. Confidence leads because it is the
/// axis that says whether the recovery is real at all: a key, URL, email, or valid shell
/// command is a structure a wrong decode cannot produce by chance, so those outrank the
/// bare strings whatever either one is about. Suspicion then orders what is left by how
/// much an analyst wants it, and length breaks the rest of the ties — a longer recovery is
/// both harder to fabricate and more use once read.
///
/// # Arguments
///
/// * `candidate` - The recovery to rank
fn rank_key(candidate: &BlobCandidate) -> (i16, i64, i64, u64) {
    // score the content exactly as the reported finding will be scored
    let kind = decode::classify(&candidate.text);
    let assessment = decode::assess(low().0, kind, &candidate.text);
    (
        // negated, because the sort is ascending and the best is wanted first
        -i16::from(confidence_rank(assessment.confidence)),
        -assessment.suspicion,
        -(candidate.text.len() as i64),
        // and the address last, so two equally-good recoveries always come out in the
        // same order instead of in whichever order the sweep reached them
        candidate.addr,
    )
}

/// The base analyst explanation for a blob-recovered item (the "how" is appended by
/// [`decode::recovery_reasoning`]).
const BLOB_BASE_REASON: &str = "Statically deobfuscated data the sample stores encoded to hide IOCs from `strings`/YARA and \
     decodes at runtime. Treat the recovered text as a candidate IOC to confirm.";

/// Extract logical items from a decode's runs and gather each as its own candidate.
/// Returns `true` when the [`MAX_BLOB_CANDIDATES`] cap is reached.
///
/// # Arguments
///
/// * `found` - The accumulating candidate list to push onto
/// * `runs` - The `(offset, string)` runs from this decode
/// * `scheme` - The decode scheme label (e.g. `"XOR"`)
/// * `key` - The single-byte key, when the scheme uses one
/// * `section` - The name of the section the bytes came from
/// * `base` - The section base address (offsets are relative to it)
fn gather_groups(
    found: &mut Vec<BlobCandidate>,
    runs: Vec<(usize, String)>,
    scheme: &'static str,
    key: Option<u8>,
    section: &str,
    base: u64,
) -> bool {
    // one logical item per candidate (a PEM armor coalesced; trailing text split off)
    for (off, text) in recover::extract_items(runs) {
        found.push(BlobCandidate {
            addr: base.saturating_add(off as u64),
            section: section.to_string(),
            scheme,
            key,
            text,
        });
        if found.len() >= MAX_BLOB_CANDIDATES {
            tracing::warn!(
                cap = MAX_BLOB_CANDIDATES,
                "blob decode gathered its candidate cap; the rest of the key space was not swept"
            );
            return true;
        }
    }
    false
}

/// Record the range each of one decode's runs made read as text.
///
/// Unbounded on purpose, where [`gather_groups`] is capped: a claim is one range on
/// the ledger rather than a recovery to be reported, and dropping claims is precisely
/// what [`unambiguous`] must not do.
///
/// # Arguments
///
/// * `claims` - The accumulating claim ledger to record into
/// * `runs` - The `(offset, string)` runs from this decode, decoded tables included
/// * `key` - The single-byte key, when the scheme uses one
/// * `base` - The section base address (offsets are relative to it)
fn gather_claims(
    claims: &mut Vec<ClaimedRange>,
    runs: Vec<(usize, String)>,
    key: Option<u8>,
    base: u64,
) {
    // the same item extraction the candidates go through, so the ranges line up
    for (off, text) in recover::extract_items(runs) {
        let start = base.saturating_add(off as u64);
        claims.push(ClaimedRange {
            start,
            end: start.saturating_add(text.len() as u64),
            key,
        });
    }
}

/// The slack bytes read past a recovery's own length when re-running a decoder
/// function's key over the buffer (covers the separators item extraction crossed).
const KEY_CHECK_SLACK: usize = 64;

/// Map each decoder-loop function to the single-byte key its loop recovered.
///
/// Functions whose qualifying spans carry no single-byte immediate are absent: with
/// no key there is no decode to re-run, so no "decodes it" claim can ever be
/// demonstrated for them.
///
/// # Arguments
///
/// * `all` - Every function's `(entry, instructions)` disassembly
fn decoder_keys(all: &[(u64, Vec<Instruction>)]) -> HashMap<u64, LoopKey> {
    let mut keys = HashMap::new();
    for (entry, insns) in all {
        // only functions with a qualifying transform-bearing loop span count
        let spans = decoder_loop_spans(insns);
        if spans.is_empty() {
            continue;
        }
        // keep the dominant in-span (op, key) when the loop uses an immediate
        if let Some(loop_key) = recover_loop_key(insns, &spans) {
            keys.insert(*entry, loop_key);
        }
    }
    keys
}

/// Whether a decoder function's recovered `(op, key)`, re-run over a buffer's source
/// bytes, reproduces the reported text.
///
/// This is the demonstration behind every "contains the loop that decodes it" claim:
/// membership in the decoder roster only proves the function transforms bytes
/// *somewhere* — adjacency plus a loop is not a decode of *this* buffer. Comparison
/// is per line because item extraction joins a multi-run recovery (a PEM armor) with
/// newlines the source never held.
///
/// # Arguments
///
/// * `loop_key` - The function's recovered loop `(op, key)`
/// * `src` - The buffer's source bytes, sliced from the recovery's own address
/// * `text` - The reported recovered text
fn key_decodes_text(loop_key: &LoopKey, src: &[u8], text: &str) -> bool {
    // re-run the one decode the role would claim
    let decoded = match loop_key.scheme {
        "XOR" => decode::xor::single_byte(loop_key.key, src),
        "SUB" => decode::arithmetic::sub_byte(loop_key.key, src),
        "ADD" => decode::arithmetic::add_byte(loop_key.key, src),
        _ => return false,
    };
    // every line of the reported text must come out of the function's own key
    let haystack = String::from_utf8_lossy(&decoded);
    text.lines().all(|line| haystack.contains(line))
}

/// Record one recovered item as a typed, honestly-attributed blob hit.
///
/// # Arguments
///
/// * `hits` - The accumulating hit list to push onto
/// * `seen` - The set of already-surfaced recoveries (dedup)
/// * `index` - The reference index used to attribute a buffer to its function
/// * `decoders` - Each decoder-loop function's own recovered `(op, key)`
/// * `sections` - The program's initialized section bytes
/// * `candidate` - The ranked recovery to report
fn push_decoded(
    hits: &mut Vec<DetectionHit>,
    seen: &mut HashSet<String>,
    index: &[(u64, u64)],
    decoders: &HashMap<u64, LoopKey>,
    sections: &[SectionBytes],
    candidate: BlobCandidate,
) {
    // unpack the candidate once; everything below reads from it
    let BlobCandidate {
        addr,
        section,
        scheme,
        key,
        text,
    } = candidate;
    // skip recoveries already surfaced (same text from another key/section)
    if !seen.insert(text.clone()) {
        return;
    }
    // classify the item and build its typed name + markdown content
    let key_str = key.map(|k| format!("0x{k:02x}"));
    let kind = decode::classify(&text);
    let name = decode::recovered_name(kind, &text, &section, addr);
    let content =
        decode::recovered_content(kind, &text, &section, addr, scheme, key_str.as_deref());
    // score both axes from the content, starting at the blob method's (low) reliability
    let assessment = decode::assess(low().0, kind, &text);
    let reasoning = decode::append_assessment(
        decode::recovery_reasoning(BLOB_BASE_REASON, scheme, key_str.as_deref()),
        &assessment.notes,
    );
    // attribute to the function with the nearest preceding data reference, within the
    // same honest window the chained pass uses — the full scan radius attributed
    // whatever function happened to precede the section; None → submit links the
    // flag to the sample
    let function = decode::buffers::referencing_function(index, addr, NESTED_REF_WINDOW);
    // claim the function decodes this buffer only when its own loop key, re-run over
    // the buffer's source bytes, reproduces the reported text
    let verified = function
        .and_then(|entry| decoders.get(&entry))
        .zip(decode::slice_at(
            sections,
            addr,
            text.len().saturating_add(KEY_CHECK_SLACK),
        ))
        .is_some_and(|(loop_key, (_, src))| key_decodes_text(loop_key, src, &text));
    // the role states exactly what was demonstrated, nothing more
    let role = match (function, verified) {
        // the attributed function's own loop was re-run and produced this value
        (Some(_), true) => format!(
            "references this buffer and contains the byte-wise {scheme} loop that decodes it"
        ),
        // referenced, but no decode by that function was demonstrated
        (Some(_), false) => "references this buffer".to_string(),
        // nothing references it: the role describes only the decode itself
        (None, _) => format!("{scheme} decode of a stored data buffer"),
    };
    hits.push(
        DetectionHit::new(
            T1140,
            addr,
            function,
            (assessment.confidence, assessment.suspicion),
            content,
            reasoning,
        )
        .name(name)
        .kind(EvidenceKind::ContextOnly)
        // carry the full plaintext as a typed artifact so the report inventory
        // lists it and the emulation stage's dedup never re-seeds this value
        .artifact(
            HitArtifact::new(decode::artifact_class(kind), text.clone())
                .role(role)
                .detail(match key_str.as_deref() {
                    Some(key) => {
                        format!("statically decoded @ {} with key {key}", fmt_addr(addr))
                    }
                    None => format!("statically decoded @ {}", fmt_addr(addr)),
                }),
        ),
    );
}

/// The max custom 64-symbol tables read out of the image and handed to the engine.
const MAX_CUSTOM_ALPHABETS: usize = 8;

/// The min characters in a run before it is worth handing to the chained decoder.
const MIN_NESTED_RUN_CHARS: usize = 32;

/// The max bytes of a candidate run fed to the chained decoder (bounds cost).
const MAX_NESTED_RUN_BYTES: usize = 8 * 1024;

/// The entropy floor, in bits per character, a candidate run must clear.
///
/// Base64 output regroups its input across bit boundaries, which flattens the symbol
/// histogram; ordinary text and the concatenated identifier blobs a statically linked
/// runtime leaves behind sit well below this. It is the cheapest filter that keeps the
/// engine from being run over every alphanumeric word in the image.
const MIN_NESTED_RUN_ENTROPY: f32 = 4.8;

/// The max candidate runs handed to the chained decoder across the whole program.
const MAX_NESTED_CANDIDATES: usize = 32;

/// The max hits the chained-decode pass surfaces.
const MAX_NESTED_HITS: usize = 15;

/// The window searched for the function a recovered buffer is attributed to, for
/// the blob and chained passes alike.
///
/// Four kilobytes keeps genuinely-adjacent decoders (`FlipLocker`'s sits a few
/// hundred bytes from its blob), where a quarter-megabyte reach attributes
/// recoveries to whatever unrelated function last referenced the section — and
/// losing a far-fetched attribution only moves a lead to file level.
const NESTED_REF_WINDOW: u64 = 4 * 1024;

/// The base analyst explanation for a chained-decode recovery.
const NESTED_BASE_REASON: &str = "Statically decoded a stored run of encoding-alphabet characters by peeling its layers in \
     order. Storing a value behind a transport encoding — on its own or over a cipher — keeps it \
     out of `strings` output while leaving it trivially recoverable at runtime. Treat the \
     recovered text as a candidate IOC to confirm.";

/// Whether a byte can be part of a run that some encoding alphabet would accept.
///
/// # Arguments
///
/// * `byte` - The byte to test
/// * `alphabets` - Any custom 64-symbol tables recovered from the image
fn is_run_byte(byte: u8, alphabets: &[base64::Alphabet]) -> bool {
    // the standard, URL-safe, hex, and padding symbols, plus anything a custom table uses
    byte.is_ascii_alphanumeric()
        || matches!(byte, b'+' | b'/' | b'-' | b'_' | b'=')
        || alphabets.iter().any(|alphabet| alphabet.contains(byte))
}

/// Carve the maximal runs of encoding-alphabet characters out of a buffer.
///
/// Maximality is the word-boundary guard here: a run is bounded on both sides by a
/// byte no alphabet accepts, so it is a whole stored token rather than a slice through
/// the middle of a statically linked runtime's concatenated string blob.
///
/// # Arguments
///
/// * `bytes` - The section bytes to carve
/// * `alphabets` - Any custom 64-symbol tables recovered from the image
fn encoded_runs<'a>(bytes: &'a [u8], alphabets: &[base64::Alphabet]) -> Vec<(usize, &'a [u8])> {
    let mut runs = Vec::new();
    let mut at = 0usize;
    while at < bytes.len() {
        // skip anything that cannot start a run
        if !is_run_byte(bytes[at], alphabets) {
            at += 1;
            continue;
        }
        // extend to the end of the run, then judge it as a whole
        let start = at;
        while at < bytes.len() && is_run_byte(bytes[at], alphabets) {
            at += 1;
        }
        let run = &bytes[start..at];
        if run.len() >= MIN_NESTED_RUN_CHARS && run.len() <= MAX_NESTED_RUN_BYTES {
            runs.push((start, run));
        }
    }
    runs
}

/// Whether a candidate run's symbol distribution is flat enough to be an encoding.
///
/// The absolute floor is unreachable for a short run — 32 characters cannot carry more
/// than 5 bits of per-symbol entropy — so the bar is capped by what the length allows.
///
/// # Arguments
///
/// * `run` - The candidate run
fn clears_entropy_bar(run: &[u8]) -> bool {
    // the highest entropy a run this short could reach, less a little slack
    let ceiling = (run.len() as f32).log2() - 0.5;
    decode::shannon_entropy(run) >= MIN_NESTED_RUN_ENTROPY.min(ceiling)
}

/// Read any custom 64-symbol alphabet tables the program carries in its data.
///
/// A permuted base64 alphabet is not recoverable from the ciphertext — the permutation
/// is one of 64! — but a program that uses one must carry the table to decode with it,
/// so the table is read out of the image and handed to the engine as a known alphabet.
///
/// # Arguments
///
/// * `sections` - The program's initialized section bytes
fn custom_alphabets(sections: &[SectionBytes]) -> Vec<base64::Alphabet> {
    let mut found = Vec::new();
    for (name, _, bytes) in sections {
        if !decode::buffers::is_data_section(name) {
            continue;
        }
        let slice = &bytes[..bytes.len().min(MAX_SCAN_BYTES)];
        found.extend(
            base64::find_alphabets(slice)
                .into_iter()
                .map(|(_, alphabet)| alphabet),
        );
    }
    // more tables than this is not a program's alphabet, it is a coincidence pile
    if found.len() > MAX_CUSTOM_ALPHABETS {
        tracing::warn!(
            found = found.len(),
            kept = MAX_CUSTOM_ALPHABETS,
            "more candidate encoding alphabets than the chained decoder will carry"
        );
        found.truncate(MAX_CUSTOM_ALPHABETS);
    }
    found
}

/// Build the artifact attribution for a chained-decode recovery.
///
/// Three outcomes, and an analyst needs the difference between them: the referencing
/// function's own key was re-run over the buffer and reproduced the text, it merely
/// references the buffer, or nothing references it and the artifact must not read as
/// attributed. Membership in the decoder roster is never enough for the first claim —
/// a DES core transforms bytes in loops all day without decoding this buffer — so the
/// strong role is only written over a demonstrated decode ([`key_decodes_text`]).
///
/// # Arguments
///
/// * `verified` - Whether the referencing function's key reproduces the text
/// * `function` - The entry of the function that references the buffer, if found
fn nested_attribution(verified: bool, function: Option<u64>) -> (Option<String>, Option<String>) {
    match function {
        // the referencing function's own key was re-run and produced the text
        Some(_) if verified => (
            Some(
                "references this buffer and contains the byte-wise XOR/SUB/ADD loop that \
                 decodes it"
                    .to_string(),
            ),
            None,
        ),
        // referenced, but no decode by that function was demonstrated
        Some(_) => (
            Some("references this buffer".to_string()),
            Some(
                "the referencing function was not shown to decode this buffer (no recovered \
                 key of its own reproduces the text); the data reference is the whole of the \
                 attribution"
                    .to_string(),
            ),
        ),
        // no code reference within the window — report it, but never imply an owner
        None => (
            None,
            Some(format!(
                "no function references this buffer within {NESTED_REF_WINDOW} bytes, so its \
                 decoder was not located; the encoded value is present in the image regardless"
            )),
        ),
    }
}

/// Recover values stored behind a chain of encoding layers.
///
/// Carves the runs a transport alphabet would accept, hands each to
/// [`chain::Engine`], and reports what comes back — as a parsed configuration record
/// when [`records::parse`] can read the output structurally, and as a decoded buffer
/// otherwise.
///
/// Only printable candidate runs are carved. A chain that begins with a cipher over
/// raw binary is what the blob pass already brute-forces, and two stacked byte ciphers
/// collapse into one key, so transport-first is the layering that is actually missed.
///
/// # Arguments
///
/// * `ctx` - The analysis context
///
/// # Errors
///
/// Returns an error if disassembling the program or reading its sections fails.
#[tracing::instrument(name = "encoded_strings::nested_decode_hits", skip_all, err(Debug))]
async fn nested_decode_hits(ctx: &AnalysisContext) -> Result<Vec<DetectionHit>> {
    // the (cached) section bytes, data-reference index, and whole-program disassembly
    let sections = ctx.initialized_bytes().await?;
    let index = ctx.reference_index().await?;
    let all = ctx.all_function_disassembly().await?;
    // each decoder-loop function's own recovered key, for artifact attribution: a
    // "decodes it" claim is demonstrated by re-running that key, never inferred
    let decoders = decoder_keys(&all);

    // read any custom alphabet the program carries, then build the engine around it
    let alphabets = custom_alphabets(&sections);
    if !alphabets.is_empty() {
        tracing::info!(
            count = alphabets.len(),
            "recovered custom encoding alphabets"
        );
    }
    let engine = chain::Engine::new(alphabets.clone());

    // carve the candidate runs, keeping the cheapest filters first
    let mut candidates: Vec<(&str, u64, &[u8])> = Vec::new();
    let mut skipped = 0usize;
    for (name, base, bytes) in &sections {
        if !decode::buffers::is_data_section(name) {
            continue;
        }
        let slice = &bytes[..bytes.len().min(MAX_SCAN_BYTES)];
        for (off, run) in encoded_runs(slice, &alphabets) {
            if !clears_entropy_bar(run) {
                continue;
            }
            // past the cap, count what is being left out rather than dropping it quietly
            if candidates.len() >= MAX_NESTED_CANDIDATES {
                skipped += 1;
                continue;
            }
            candidates.push((name.as_str(), base.saturating_add(off as u64), run));
        }
    }
    if skipped > 0 {
        tracing::warn!(
            scanned = candidates.len(),
            skipped,
            "more encoded-run candidates than the chained decoder will attempt"
        );
    }

    // run the engine over each candidate and report what it recovers
    let mut hits = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut unattempted = 0usize;
    for (section, addr, run) in candidates {
        if hits.len() >= MAX_NESTED_HITS {
            unattempted += 1;
            continue;
        }
        let Some(chain) = engine.recover(run) else {
            continue;
        };
        if let Some(hit) = nested_hit(
            ctx, &decoders, &index, &mut seen, section, addr, run, &chain,
        ) {
            hits.push(hit);
        }
    }
    if unattempted > 0 {
        tracing::warn!(
            reported = hits.len(),
            unattempted,
            "chained-decode candidates left unattempted at the hit cap"
        );
    }
    Ok(hits)
}

/// Turn one recovered chain into a typed, attributed hit.
///
/// Returns `None` when the recovered text has already been surfaced by another
/// candidate.
///
/// # Arguments
///
/// * `ctx` - The analysis context, for naming the referencing function
/// * `decoders` - Each decoder-loop function's own recovered `(op, key)`
/// * `index` - The reference index used to attribute a buffer to its function
/// * `seen` - The set of already-surfaced recoveries (dedup)
/// * `section` - The name of the section the encoded run came from
/// * `addr` - The address the encoded run starts at
/// * `run` - The stored (encoded) run the chain decoded
/// * `chain` - The recovered chain
#[allow(clippy::too_many_arguments)]
fn nested_hit(
    ctx: &AnalysisContext,
    decoders: &HashMap<u64, LoopKey>,
    index: &[(u64, u64)],
    seen: &mut HashSet<String>,
    section: &str,
    addr: u64,
    run: &[u8],
    chain: &chain::Chain,
) -> Option<DetectionHit> {
    // the recovered text, one line per run
    let text = chain
        .runs
        .iter()
        .map(|(_, run)| run.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if !seen.insert(text.clone()) {
        return None;
    }
    let scheme = chain.scheme();
    let kind = decode::classify(&text);
    let name = decode::recovered_name(kind, &text, section, addr);
    let mut content = decode::recovered_content(kind, &text, section, addr, &scheme, chain.key());

    // a structural chain proves itself — every layer's whole run had to be well-formed
    // under its alphabet — while a brute-forced key is only as good as its output
    let method = if chain.is_structural() {
        medium().0
    } else {
        low().0
    };
    let assessment = decode::assess(method, kind, &text);
    let mut reasoning = decode::append_assessment(
        decode::recovery_reasoning(NESTED_BASE_REASON, &scheme, chain.key()),
        &assessment.notes,
    );
    // a layered value is the one case neither single-pass recovery can reach; say how
    // deep it was, since that is the reason it went unreported until now
    if chain.is_nested() {
        reasoning.push_str(&format!(
            "\n\nThe value was stored behind {} stacked layers, so neither the decoder-loop \
             recovery nor the single-pass blob decode reaches it — each peels one layer and \
             stops at binary.",
            chain.layers.len()
        ));
    }
    // a decode that lands on another encoding has not reached the plaintext; say so
    // rather than presenting a half-peeled value as the answer
    if chain.still_encoded {
        reasoning.push_str(
            "\n\nThe recovered output is itself still well-formed under a transport encoding, so \
             a further layer may remain beyond the search depth.",
        );
    }

    // read the decode structurally, and fold the table into the flag when it parses
    let record = records::parse(&chain.decoded);
    if let Some(record) = &record {
        content.push_str("\n\n");
        content.push_str(&records::render(record));
    }

    // attribute the buffer to the function that references (and usually decodes) it
    let function = decode::buffers::referencing_function(index, addr, NESTED_REF_WINDOW);
    let mut hit = DetectionHit::new(
        T1140,
        addr,
        function,
        (assessment.confidence, assessment.suspicion),
        content,
        reasoning.clone(),
    )
    .name(name)
    .kind(EvidenceKind::ContextOnly);

    // a statically linked runtime ships encoded tables of its own; weaken rather than
    // drop, because the recovery itself is real either way
    if let Some(fn_name) = function.and_then(|entry| ctx.function_name(entry)) {
        if is_library_function(fn_name) {
            hit.weaken();
            hit.reasoning = format!(
                "{reasoning}\n\nWeakened because the referencing function `{fn_name}` is \
                 statically linked runtime code rather than the program's own."
            );
        }
    }

    // the artifact: a parsed record when the output had structure, the buffer otherwise
    let (class, value) = match &record {
        Some(record) => (ArtifactClass::ConfigRecord, records::summary(record)),
        None => (ArtifactClass::DecodedData, decode::snippet(kind, &text)),
    };
    // the decoder role must be demonstrated: the referencing function's own key,
    // re-run over the stored run, has to reproduce the recovered text
    let verified = function
        .and_then(|entry| decoders.get(&entry))
        .is_some_and(|loop_key| key_decodes_text(loop_key, run, &text));
    let (role, note) = nested_attribution(verified, function);
    // the encoding chain belongs with the artifact alongside any attribution note
    let chain_note = match &record {
        Some(record) => format!("recovered through {scheme}; {}", record.shape.label()),
        None => format!("recovered through {scheme}"),
    };
    let mut artifact = HitArtifact::new(class, value).detail(match note {
        Some(note) => format!("{chain_note}; {note}"),
        None => chain_note,
    });
    if let Some(role) = role {
        artifact = artifact.role(role);
    }
    Some(hit.artifact(artifact))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal instruction for the classifier tests.
    fn insn(mnemonic: &str, operands: &str, address: &str) -> Instruction {
        Instruction {
            address: address.into(),
            mnemonic: mnemonic.into(),
            operands: vec![operands.into()],
            bytes: None,
            length: None,
            flow_type: None,
        }
    }

    /// A byte-sized XOR/SUB/ADD (memory or 8-bit reg) is a transform; others not.
    #[test]
    fn byte_transform_recognizes_per_char_ops() {
        assert!(is_byte_transform(&insn(
            "XOR",
            "byte ptr [rax + rcx*0x1], dl",
            "1000"
        )));
        assert!(is_byte_transform(&insn("SUB", "al, 0x2a", "1000")));
        assert!(!is_byte_transform(&insn("MOV", "rax, rbx", "1000")));
        assert!(!is_byte_transform(&insn("XOR", "rax, rbx", "1000")));
    }

    /// A jump to a lower address is a loop back-edge; forward jumps are not.
    #[test]
    fn backward_branch_detects_loop_edge() {
        assert!(is_backward_branch(&insn("JNZ", "0x1000", "0x1020")));
        assert!(!is_backward_branch(&insn("JMP", "0x1040", "0x1020")));
        assert!(!is_backward_branch(&insn("MOV", "rax, rbx", "0x1020")));
    }

    /// The dominant single-byte `(op, key)` is recovered from the loop's transforms.
    #[test]
    fn recover_loop_key_picks_dominant_immediate() {
        let insns = vec![
            insn("XOR", "al, 0x2a", "1000"),
            insn("XOR", "cl, 0x2a", "1004"),
            insn("ADD", "dl, 0x01", "1008"), // minority op — ignored
        ];
        let lk = recover_loop_key(&insns, &[(0x1000, 0x1008)]).expect("a key");
        assert_eq!(lk.scheme, "XOR");
        assert_eq!(lk.key, 0x2a);
    }

    /// A register-key transform (no immediate) yields no single-byte key.
    #[test]
    fn recover_loop_key_none_without_immediate() {
        let insns = vec![insn("XOR", "byte ptr [rax], dl", "1000")];
        assert!(recover_loop_key(&insns, &[(0x1000, 0x1000)]).is_none());
    }

    /// Transforms must sit inside one back-edge's span to make a decoder loop.
    #[test]
    fn decoder_loop_requires_transforms_inside_one_loop() {
        // three bool-flip XORs scattered across a large function plus a loop that
        // contains none of them — the shape the pack's 6,574-instruction Rust
        // function disputes came from — is not a decoder
        let scattered = vec![
            insn("XOR", "al, 0x1", "1000"),
            insn("XOR", "bl, 0x1", "3000"),
            insn("XOR", "cl, 0x1", "5000"),
            insn("ADD", "rax, 0x8", "7000"),
            insn("JNZ", "0x7000", "7008"),
        ];
        assert!(decoder_loop_spans(&scattered).is_empty());
        // the same transforms packed inside the back-edge span are the decode idiom
        let tight = vec![
            insn("XOR", "al, 0x1", "7000"),
            insn("XOR", "al, 0x1", "7004"),
            insn("XOR", "al, 0x1", "7008"),
            insn("JNZ", "0x7000", "700c"),
        ];
        assert_eq!(decoder_loop_spans(&tight), vec![(0x7000, 0x700c)]);
    }

    /// The reported key comes from the loop's own span, not the whole function.
    #[test]
    fn recover_loop_key_reads_only_the_loop_span() {
        // four digit-conversion ADDs outside any loop would win a whole-function
        // tally; the loop's own three XORs must win instead
        let insns = vec![
            insn("ADD", "al, 0x30", "1000"),
            insn("ADD", "bl, 0x30", "1004"),
            insn("ADD", "cl, 0x30", "1008"),
            insn("ADD", "dl, 0x30", "100c"),
            insn("XOR", "dl, 0x2a", "2000"),
            insn("XOR", "dl, 0x2a", "2004"),
            insn("XOR", "dl, 0x2a", "2008"),
            insn("JNZ", "0x2000", "200c"),
        ];
        let spans = decoder_loop_spans(&insns);
        assert_eq!(spans, vec![(0x2000, 0x200c)]);
        let lk = recover_loop_key(&insns, &spans).expect("a key");
        assert_eq!(lk.scheme, "XOR");
        assert_eq!(lk.key, 0x2a);
    }

    /// Codegen-idiom `(op, key)` pairs are recognized; real decoder keys are not.
    #[test]
    fn codegen_idiom_keys_are_recognized() {
        // the boolean flip only counts without a memory-byte transform in the loop
        assert!(is_codegen_idiom_key("XOR", 0x01, false));
        assert!(!is_codegen_idiom_key("XOR", 0x01, true));
        // digit, space/case, and hex/digit-value adjustment keys are idioms
        assert!(is_codegen_idiom_key("ADD", 0x30, false));
        assert!(is_codegen_idiom_key("SUB", 0x30, true));
        assert!(is_codegen_idiom_key("SUB", 0x20, false));
        assert!(is_codegen_idiom_key("ADD", 0x37, false));
        assert!(is_codegen_idiom_key("ADD", 0x57, false));
        assert!(is_codegen_idiom_key("SUB", 0x58, false));
        assert!(is_codegen_idiom_key("SUB", 0x61, false));
        // the pack's confirmed decoder keys stay full grade
        assert!(!is_codegen_idiom_key("ADD", 0x6d, false));
        assert!(!is_codegen_idiom_key("ADD", 0x80, false));
        assert!(!is_codegen_idiom_key("XOR", 0x41, false));
        assert!(!is_codegen_idiom_key("SUB", 0x2b, false));
        assert!(!is_codegen_idiom_key("XOR", 0x2a, false));
    }

    /// A strtoul-shaped SUB 0x58 loop with no decoded buffer is marked an idiom.
    #[test]
    fn recover_decoder_marks_idiom_key_only_results() {
        let insns = vec![
            insn("SUB", "al, 0x58", "1000"),
            insn("SUB", "al, 0x58", "1004"),
            insn("SUB", "al, 0x58", "1008"),
            insn("JB", "0x1000", "100c"),
        ];
        let spans = decoder_loop_spans(&insns);
        let r = recover_decoder(&insns, &spans, &[]).expect("key recovered");
        assert_eq!(r.key, "0x58");
        assert!(r.items.is_empty());
        assert!(r.codegen_idiom);
    }

    /// `recover_decoder` decodes a single-byte-XOR buffer the function references,
    /// when the encoded source looks non-printable.
    #[test]
    fn recover_decoder_decodes_referenced_buffer() {
        // a function that loads a .rodata buffer and XORs bytes with a high-bit key
        // (0x9d maps ASCII into the non-printable range, so the source looks encoded)
        let insns = vec![
            insn("LEA", "RSI, [0x00102000]", "1000"),
            insn("XOR", "al, 0x9d", "1008"),
            insn("XOR", "al, 0x9d", "100c"),
            insn("XOR", "al, 0x9d", "1010"),
            insn("JNZ", "0x1008", "1014"),
        ];
        // the buffer: a long C2 URL XORed with 0x9d (source non-printable); it must be
        // long enough to clear the strong-recovery bar
        let plain = b"http://evil.example.com/gate.php";
        let enc = decode::xor::single_byte(0x9d, plain);
        let sections = vec![(".rodata".to_string(), 0x00102000u64, enc)];
        let spans = decoder_loop_spans(&insns);
        let r = recover_decoder(&insns, &spans, &sections).expect("recovered");
        assert_eq!(r.scheme, "XOR");
        assert_eq!(r.key, "0x9d");
        // a real decoder key is no codegen idiom
        assert!(!r.codegen_idiom);
        assert!(r.items.iter().any(|it| it.text.contains("evil.example")));
        assert!(r.items.iter().any(|it| it.section == ".rodata"));
    }

    /// A near-identity key (0x01) over ordinary plaintext yields NO recovery — the
    /// source-looks-encoded guard rejects the section's real strings (no FP flood).
    #[test]
    fn recover_decoder_rejects_plaintext_buffer() {
        let insns = vec![
            insn("LEA", "RSI, [0x00102000]", "1000"),
            insn("XOR", "al, 0x1", "1008"),
            insn("XOR", "al, 0x1", "100c"),
            insn("XOR", "al, 0x1", "1010"),
            insn("JNZ", "0x1008", "1014"),
        ];
        // the buffer is ordinary printable text (not encoded)
        let sections = vec![(
            ".rodata".to_string(),
            0x00102000u64,
            b"the quick brown fox jumps".to_vec(),
        )];
        let spans = decoder_loop_spans(&insns);
        let r = recover_decoder(&insns, &spans, &sections).expect("key recovered");
        // key is reported, but no false items are recovered from plaintext
        assert_eq!(r.key, "0x01");
        assert!(r.items.is_empty());
        // a register-only XOR 0x01 with nothing decoded is the boolean-flip idiom
        assert!(r.codegen_idiom);
    }

    /// The confirmed `GandCrab` leetspeak ransom note, line for line as the baseline
    /// telemetry recovered it.
    const GANDCRAB_NOTE: &[&str] = &[
        " GANDC2AB 6",
        "ATTenTion! ",
        "All YoUR fileS, docUmenTS, PhoToS, daTabaSeS and oTheR imPoRTanT fileS aRe \
         encRYPTed and haVe The eXTenSion",
        "4he onlY meThod of RecoVeRing fileS iS To PURchaSe an UniQUe PRiVaTe keY. \
         OnlY We can giVe YoU ThiS keY and onlY We can RecoVeR YoUR fileS.",
        "4he SeRVeR WiTh YoUR keY iS in a cloSed neTWoRk 4O2. 9oU can geT TheRe bY \
         The folloWing WaYS",
        ". DoWnload 4oR bRoWSeR - hTTPS",
        "//WWW.ToRPRojecT.oRg/ ",
        "A44EN4ION!",
        "IN O2DE2 4O 02E6EN4 DA4A DAMAGE",
        "* DO NO4 MODIF9 ENC2904ED FILE3",
        "* DO NO4 CHANGE DA4A BELO7",
    ];

    /// Encode text the way the `GandCrab` note stores it: each character as the
    /// interleaved `(char + key, key)` pair, lines separated by encoded CRLFs.
    ///
    /// # Arguments
    ///
    /// * `lines` - The plaintext lines to encode
    /// * `key` - The single-byte key paired with every character
    fn keyed_pair_encode(lines: &[&str], key: u8) -> Vec<u8> {
        // join with CRLF, then store every character as (char + key, key)
        lines
            .join("\r\n")
            .bytes()
            .flat_map(|c| [c.wrapping_add(key), key])
            .collect()
    }

    /// A decoder loop whose exact key was recovered still decodes a buffer stored as
    /// interleaved `(value, key)` pairs.
    ///
    /// The `GandCrab` note is stored two bytes per character — the shifted character,
    /// then the key — so the contiguous SUB decode renders every other byte
    /// non-printable and recovers nothing ("SUB decoder — key 0x3a, no static buffer
    /// resolved" is exactly what the replay emitted), even though the loop's own key
    /// was observed. The keyed-pair shape must be recognized and decoded with that
    /// exact key, line for line.
    #[test]
    fn recover_decoder_decodes_keyed_pair_storage() {
        // the decoder loop the replay found: three SUB 0x3a byte transforms in a loop
        let insns = vec![
            insn("LEA", "RSI, [0x00102000]", "1000"),
            insn("SUB", "al, 0x3a", "1008"),
            insn("SUB", "al, 0x3a", "100c"),
            insn("SUB", "al, 0x3a", "1010"),
            insn("JNZ", "0x1008", "1014"),
        ];
        // the note stored as (char + 0x3a, 0x3a) pairs in a data section
        let sections = vec![(
            ".rdata".to_string(),
            0x0010_2000_u64,
            keyed_pair_encode(GANDCRAB_NOTE, 0x3a),
        )];
        let spans = decoder_loop_spans(&insns);
        let r = recover_decoder(&insns, &spans, &sections).expect("key recovered");
        assert_eq!(r.scheme, "SUB");
        assert_eq!(r.key, "0x3a");
        // the leetspeak lines come back verbatim from the observed-key decode
        for want in ["A44EN4ION!", "IN O2DE2 4O 02E6EN4 DA4A DAMAGE", "GANDC2AB"] {
            assert!(
                r.items.iter().any(|item| item.text.contains(want)),
                "missing {want}"
            );
        }
        // the items point back into the buffer the pairs were stored in
        assert!(r.items.iter().all(|item| item.addr >= 0x0010_2000));
        assert!(r.items.iter().any(|item| item.section == ".rdata"));
    }

    /// The keyed-pair path never resurrects the fabrication kills.
    ///
    /// Interleaving a compiler table with a constant equal to the loop key is the
    /// adversarial best case for the pair shape, and the source-side reads still
    /// condemn it: the popcount family trips the paired-delta/parity/alphabet reads
    /// and a PPMd-style suballocator ramp is monotone. Both decode to wordy,
    /// text-reading junk (`~~|~||z~…`, `$(,048<@…`), so the output reads alone would
    /// keep them.
    #[test]
    fn keyed_pair_storage_keeps_fabrications_dead() {
        // the MSVC STL popcount table, interleaved with the loop key 0x70
        let popcount: Vec<u8> = (0u32..256)
            .map(|i| 16 - 2 * u8::try_from(i.count_ones()).expect("popcount of a byte"))
            .collect();
        let interleaved: Vec<u8> = popcount.iter().flat_map(|b| [*b, 0x70]).collect();
        let insns = vec![
            insn("LEA", "RSI, [0x00102000]", "1000"),
            insn("ADD", "al, 0x70", "1008"),
            insn("ADD", "al, 0x70", "100c"),
            insn("ADD", "al, 0x70", "1010"),
            insn("JNZ", "0x1008", "1014"),
        ];
        let sections = vec![(".rdata".to_string(), 0x0010_2000_u64, interleaved)];
        let spans = decoder_loop_spans(&insns);
        let r = recover_decoder(&insns, &spans, &sections).expect("key recovered");
        assert!(
            r.items.is_empty(),
            "popcount junk recovered: {:?}",
            r.items.first().map(|item| &item.text)
        );
        // a PPMd-style ramp (0x00, 0x04, 0x08, …) under its matching ADD key
        let ramp: Vec<u8> = (0u8..64).map(|i| i * 4).collect();
        let interleaved: Vec<u8> = ramp.iter().flat_map(|b| [*b, 0x24]).collect();
        let insns = vec![
            insn("LEA", "RSI, [0x00102000]", "1000"),
            insn("ADD", "al, 0x24", "1008"),
            insn("ADD", "al, 0x24", "100c"),
            insn("ADD", "al, 0x24", "1010"),
            insn("JNZ", "0x1008", "1014"),
        ];
        let sections = vec![(".rdata".to_string(), 0x0010_2000_u64, interleaved)];
        let spans = decoder_loop_spans(&insns);
        let r = recover_decoder(&insns, &spans, &sections).expect("key recovered");
        assert!(
            r.items.is_empty(),
            "ramp junk recovered: {:?}",
            r.items.first().map(|item| &item.text)
        );
        // the 4d13-style consonant mashes were brute-force XOR guesses, never an
        // observed key: their contiguous storage has no key column, so the pair
        // rescue refuses them outright for any loop key
        for mash in ["ypzmh8WJC_hPQEORqAWYBGGr", "xz:j>gfy\"jbxf%|tkakalddZ"] {
            let stored = decode::xor::single_byte(0xb0, mash.as_bytes());
            let guessed = LoopKey {
                scheme: "XOR",
                key: 0x11,
            };
            assert!(keyed_pair_decode(&guessed, &stored).is_none(), "{mash}");
        }
    }

    /// Candidate runs are carved whole: bounded by a byte no alphabet accepts, and
    /// short fragments are not carried into the engine at all.
    #[test]
    fn encoded_runs_carves_whole_tokens_only() {
        let token = b"aGVsbG8gd29ybGQgdGhpcyBpcyBhIGxvbmcgcnVu";
        let mut blob = b"\x00\x00".to_vec();
        blob.extend_from_slice(token);
        blob.extend_from_slice(b"\x00short\x00");
        let runs = encoded_runs(&blob, &[]);
        // the long token, carved from its exact offset; the 5-char fragment dropped
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].0, 2);
        assert_eq!(runs[0].1, token);
    }

    /// The entropy bar admits an encoding's flat symbol histogram and rejects the
    /// lopsided one ordinary text has, scaling itself to what a short run can reach.
    #[test]
    fn entropy_bar_separates_encodings_from_words() {
        let flat = base64::encode(&(0u8..=95).collect::<Vec<_>>());
        assert!(clears_entropy_bar(flat.as_bytes()));
        // a repeated character carries no entropy at any length
        assert!(!clears_entropy_bar(&[b'a'; 64]));
        // an English identifier blob is alphanumeric but nowhere near flat
        assert!(!clears_entropy_bar(
            b"GetProcAddressLoadLibraryAVirtualAllocVirtualProtect"
        ));
    }

    /// Attribution separates a demonstrated decode, a bare reference, and no
    /// reference at all — the three cases mean different things to an analyst.
    #[test]
    fn nested_attribution_distinguishes_its_three_outcomes() {
        // the referencing function's own key was re-run and reproduced the text
        let (role, note) = nested_attribution(true, Some(0x1000));
        assert!(role.expect("a role").contains("byte-wise XOR/SUB/ADD loop"));
        assert!(note.is_none());
        // referenced, but no decode demonstrated: roster membership is not enough
        let (role, note) = nested_attribution(false, Some(0x2000));
        assert_eq!(role.as_deref(), Some("references this buffer"));
        assert!(note.expect("a note").contains("not shown to decode"));
        // nothing references it: no role at all, and the note says why
        let (role, note) = nested_attribution(false, None);
        assert!(role.is_none());
        assert!(note.expect("a note").contains("was not located"));
    }

    /// A "decodes it" claim needs the function's own key to reproduce the text.
    #[test]
    fn key_decodes_text_requires_a_real_decode() {
        let plain = "http://evil.example.com/gate.php";
        let src = decode::xor::single_byte(0x9d, plain.as_bytes());
        // the right (op, key) reproduces the text; a wrong key or op does not
        let right = LoopKey {
            scheme: "XOR",
            key: 0x9d,
        };
        assert!(key_decodes_text(&right, &src, plain));
        let wrong_key = LoopKey {
            scheme: "XOR",
            key: 0x11,
        };
        assert!(!key_decodes_text(&wrong_key, &src, plain));
        let wrong_op = LoopKey {
            scheme: "ADD",
            key: 0x9d,
        };
        assert!(!key_decodes_text(&wrong_op, &src, plain));
    }

    /// `unambiguous` keeps a recovery only one key claims and drops a contested range.
    ///
    /// A region of small integers reads as text under a whole band of consecutive keys,
    /// and each individual output is wordy, diverse and entirely plausible — nothing
    /// about one of them gives it away. Seeing that the same range also decoded under
    /// eleven other keys is the only signal there is, and it condemns all twelve.
    #[test]
    fn unambiguous_drops_a_contested_address() {
        let mut found = vec![BlobCandidate {
            addr: 0x1000,
            section: ".rdata".to_string(),
            scheme: "XOR",
            key: Some(0x9c),
            text: "http://c2.example.com/gate.php".to_string(),
        }];
        // the same address "recovering" under a band of consecutive ADD keys
        for key in 0x31u8..=0x3c {
            found.push(BlobCandidate {
                addr: 0x2000,
                section: ".rdata".to_string(),
                scheme: "ADD",
                key: Some(key),
                text: format!("n{key:02x}AvK1AvK1"),
            });
        }
        let claims = claims_of(&found);
        let kept = unambiguous(found, &claims);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].addr, 0x1000);
    }

    /// The claim ledger the sweep would have built from a set of recoveries.
    ///
    /// # Arguments
    ///
    /// * `found` - The recoveries to tally
    fn claims_of(found: &[BlobCandidate]) -> Vec<ClaimedRange> {
        found
            .iter()
            .map(|candidate| ClaimedRange {
                start: candidate.addr,
                end: candidate.addr + candidate.text.len() as u64,
                key: candidate.key,
            })
            .collect()
    }

    /// A key whose output was a decoded table still contests the range, even though
    /// it never became a candidate.
    ///
    /// This is the promotion the ledger exists to prevent. The run filters drop the
    /// table long before the sweep is judged, and if the claim goes with it the one
    /// surviving key looks like the only key that ever read those bytes — so tightening
    /// a filter turns into a way of promoting whatever fabrication it did not catch.
    #[test]
    fn unambiguous_counts_a_claim_with_no_candidate() {
        let found = vec![BlobCandidate {
            addr: 0x2000,
            section: ".rdata".to_string(),
            scheme: "ADD",
            key: Some(0x42),
            text: "MlhaGlh Glh-Alh".to_string(),
        }];
        // a second key read the same bytes as a table, which was filtered before it
        // could be a candidate but is still on record as having claimed the range
        let mut claims = claims_of(&found);
        claims.push(ClaimedRange {
            start: 0x2000,
            end: 0x2000 + 15,
            key: Some(0x43),
        });
        assert!(unambiguous(found, &claims).is_empty());
    }

    /// One table read under two keys at interleaved offsets is contested whole.
    ///
    /// This is the demo-corpus shape the start-address ledger passed: a 256-byte
    /// lookup table decoding under ADD 0x70 at +0x01/+0x41 and under ADD 0x72 at
    /// +0x21/+0x81 — four runs, four distinct start addresses, one fabricated table.
    /// The runs' ranges overlap, and the overlap condemns all four.
    #[test]
    fn unambiguous_drops_interleaved_runs_over_one_table() {
        // four long runs out of one table, offsets and keys as the corpus saw them
        let run = |addr: u64, key: u8| BlobCandidate {
            addr,
            section: ".rdata".to_string(),
            scheme: "ADD",
            key: Some(key),
            text: "x".repeat(0x60),
        };
        let found = vec![
            run(0x1001, 0x70),
            run(0x1021, 0x72),
            run(0x1041, 0x70),
            run(0x1081, 0x72),
        ];
        let claims = claims_of(&found);
        assert!(unambiguous(found, &claims).is_empty());
    }

    /// Abutting ranges are adjacent buffers, not a contested one: both are kept.
    #[test]
    fn unambiguous_keeps_abutting_ranges() {
        // two 16-byte recoveries packed end to end under different keys — a string
        // table's entries, exactly what strict (non-touching) overlap must spare
        let found = vec![
            BlobCandidate {
                addr: 0x1000,
                section: ".rdata".to_string(),
                scheme: "XOR",
                key: Some(0x9c),
                text: "http://a.example".to_string(),
            },
            BlobCandidate {
                addr: 0x1010,
                section: ".rdata".to_string(),
                scheme: "ADD",
                key: Some(0x42),
                text: "http://b.example".to_string(),
            },
        ];
        let claims = claims_of(&found);
        assert_eq!(unambiguous(found, &claims).len(), 2);
    }

    /// Two buffers with two different keys are both kept — the guard is per-range.
    #[test]
    fn unambiguous_keeps_distinct_buffers() {
        let found = vec![
            BlobCandidate {
                addr: 0x1000,
                section: ".rdata".to_string(),
                scheme: "XOR",
                key: Some(0x9c),
                text: "http://c2.example.com/gate.php".to_string(),
            },
            BlobCandidate {
                addr: 0x2000,
                section: ".rdata".to_string(),
                scheme: "ADD",
                key: Some(0x42),
                text: "SOFTWARE\\Microsoft\\Windows".to_string(),
            },
        ];
        let claims = claims_of(&found);
        assert_eq!(unambiguous(found, &claims).len(), 2);
    }

    /// `push_decoded` claims "decodes it" only for a demonstrated key, and only
    /// attributes functions within the honest window.
    #[test]
    fn push_decoded_attributes_honestly() {
        // a real XOR 0x9d buffer in .rodata, referenced by the function at 0x1000
        let plain = "http://evil.example.com/gate.php";
        let sections = vec![(
            ".rodata".to_string(),
            0x0010_2000_u64,
            decode::xor::single_byte(0x9d, plain.as_bytes()),
        )];
        let candidate = || BlobCandidate {
            addr: 0x0010_2000,
            section: ".rodata".to_string(),
            scheme: "XOR",
            key: Some(0x9d),
            text: plain.to_string(),
        };
        let index = vec![(0x0010_2000_u64, 0x1000u64)];
        // the referencing function's own XOR 0x9d re-decodes the buffer: full claim
        let decoders: HashMap<u64, LoopKey> = [(
            0x1000u64,
            LoopKey {
                scheme: "XOR",
                key: 0x9d,
            },
        )]
        .into_iter()
        .collect();
        let mut hits = Vec::new();
        push_decoded(
            &mut hits,
            &mut HashSet::new(),
            &index,
            &decoders,
            &sections,
            candidate(),
        );
        assert_eq!(hits[0].function, Some(0x1000));
        let role = hits[0].artifact.as_ref().expect("artifact").role.as_deref();
        assert!(role.expect("a role").contains("decodes it"));
        // a referencing function whose key does not reproduce the text gets only
        // the reference — a full DES core, in miniature
        let wrong: HashMap<u64, LoopKey> = [(
            0x1000u64,
            LoopKey {
                scheme: "XOR",
                key: 0x11,
            },
        )]
        .into_iter()
        .collect();
        let mut hits = Vec::new();
        push_decoded(
            &mut hits,
            &mut HashSet::new(),
            &index,
            &wrong,
            &sections,
            candidate(),
        );
        let role = hits[0].artifact.as_ref().expect("artifact").role.as_deref();
        assert_eq!(role, Some("references this buffer"));
        // a data reference 8KB away is outside the attribution window: sample-level
        let far_index = vec![(0x0010_0000_u64, 0x1000u64)];
        let mut hits = Vec::new();
        push_decoded(
            &mut hits,
            &mut HashSet::new(),
            &far_index,
            &decoders,
            &sections,
            candidate(),
        );
        assert_eq!(hits[0].function, None);
        let role = hits[0].artifact.as_ref().expect("artifact").role.as_deref();
        assert!(role.expect("a role").contains("stored data buffer"));
    }
}
