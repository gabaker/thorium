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

use crate::thorpal::detect::context::{AnalysisContext, SectionBytes, fmt_addr};
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

/// Return whether a function's instructions look like a byte-wise decoder loop.
///
/// Enough per-character `XOR`/`SUB`/`ADD` transforms to be a transform loop rather
/// than incidental arithmetic, plus a back-edge to run them over a buffer.
///
/// # Arguments
///
/// * `insns` - The function's instructions
fn is_decoder_loop(insns: &[Instruction]) -> bool {
    // both halves of the idiom must be present: the transform and the loop around it
    insns.iter().filter(|i| is_byte_transform(i)).count() >= MIN_BYTE_TRANSFORMS
        && insns.iter().any(is_backward_branch)
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
/// Tallies each `xor/sub/add <byte>, <imm>` immediate and returns the most common
/// one (a rolling/multi-byte key or a register-key buffer yields `None`, handled by
/// the pairwise / brute passes instead).
///
/// # Arguments
///
/// * `insns` - The function's instructions
fn recover_loop_key(insns: &[Instruction]) -> Option<LoopKey> {
    // count occurrences of each (op, immediate) pair among byte transforms
    let mut tally: Vec<(&'static str, u8, usize)> = Vec::new();
    for insn in insns.iter().filter(|i| is_byte_transform(i)) {
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
    /// The recovered items (empty when only a key was recovered).
    items: Vec<RecoveredItem>,
}

/// Attempt to statically recover the plaintext a decoder loop produces.
///
/// Recovers the single-byte immediate key from the loop, reads the *encoded* data
/// buffer(s) the function references, decodes them with that exact key, and splits the
/// result into logical items. Returns the items + key, a key-only result when the key
/// is known but no encoded buffer decodes, or `None` when the loop uses no single-byte
/// immediate (e.g. a register/key-buffer or a pairwise SUB — the blob pass
/// recovers those).
///
/// A buffer is only decoded when its **source bytes look encoded** (mostly
/// non-printable, via [`recover::hidden_runs`]) and the decode is a real recovery
/// ([`recover::is_strong_recovery`]): the function references every data address it
/// touches — including ordinary plaintext — so a near-identity key like `0x01` would
/// otherwise "recover" the section's real strings as false positives.
///
/// # Arguments
///
/// * `insns` - The decoder function's instructions
/// * `sections` - The program's initialized section bytes
fn recover_decoder(insns: &[Instruction], sections: &[SectionBytes]) -> Option<DecoderResult> {
    // require a single-byte immediate key; register/key-buffer/pairwise loops have
    // none and are left to the blob-decode pass
    let loop_key = recover_loop_key(insns)?;
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
        let runs = recover::hidden_runs(&decoded, bytes);
        if !recover::is_strong_recovery(&runs, recover::MIN_RECOVERY_CHARS) {
            continue;
        }
        // split into logical items (a PEM armor coalesced; trailing text split off)
        for (off, text) in recover::extract_items(runs) {
            items.push(RecoveredItem {
                addr: addr.saturating_add(off as u64),
                section: section.to_string(),
                text,
            });
        }
    }

    // report the recovered items (or a key-only result when none decoded)
    Some(DecoderResult {
        scheme: loop_key.scheme,
        key: format!("0x{:02x}", loop_key.key),
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
        // a decoder loop: enough byte transforms and a loop back-edge
        if !is_decoder_loop(insns) {
            continue;
        }
        let transforms = insns.iter().filter(|i| is_byte_transform(i)).count();

        // try to statically recover the key + plaintext this loop produces
        match recover_decoder(insns, &sections) {
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
                let content = format!(
                    "{} decoder — key {}, no static buffer resolved @ {}",
                    r.scheme,
                    r.key,
                    fmt_addr(*entry)
                );
                hits.push(
                    DetectionHit::new(
                        T1027,
                        *entry,
                        Some(*entry),
                        medium(),
                        content,
                        decoder_reasoning(),
                    )
                    .kind(EvidenceKind::ContextOnly),
                );
            }
            // nothing recoverable: fall back to flagging the loop itself
            None => {
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
    // references (decodes) it; a run with no referencing function falls back to the
    // sample at submit time
    let index = ctx.reference_index().await?;
    // every recovery the sweep produces, held back so the sweep can be judged as a
    // whole before any of it is reported (see `unambiguous`)
    let mut found: Vec<BlobCandidate> = Vec::new();
    // and the keys that made each address readable at all, which is the wider set the
    // ambiguity test is judged on
    let mut claims: HashMap<u64, HashSet<Option<u8>>> = HashMap::new();
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
            &mut hits,
            &mut seen,
            &index,
            candidate.addr,
            candidate.scheme,
            candidate.key,
            &candidate.section,
            candidate.text,
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

/// The max recoveries gathered before the sweep gives up.
///
/// Generous next to [`MAX_DECODED_STRINGS`], because most of what the sweep gathers is
/// meant to be thrown away by [`unambiguous`] — but still bounded, since a pathological
/// section should not be allowed to run the key space to its end.
const MAX_BLOB_CANDIDATES: usize = 20 * MAX_DECODED_STRINGS;

/// Drop every recovery whose address another key also claims.
///
/// A buffer is encoded with one key. When a single address reads as text under a dozen
/// of them, that is a property of the bytes rather than of anything hidden in them —
/// the region holds small numbers, and any additive or XOR constant walks them into the
/// printable range. The individual outputs are wordy, diverse and perfectly plausible,
/// so nothing about any one of them gives it away; only seeing the others does.
///
/// Both sides are dropped rather than picking a winner. There is no basis to prefer one
/// key over another, and a fabrication asserted with a specific key attached is worse
/// than a gap.
///
/// The tally is taken over `claims` rather than over `found`, and the two differ: a key
/// whose output at an address was a decoded table is never a candidate, but it is still
/// a key that made those bytes read as text, which is exactly the question being asked.
/// Counting only the reportable recoveries would make every run filter downstream of it
/// a promotion mechanism — quietly handing a contested address to whichever key's
/// fabrication happened to survive the filters.
///
/// # Arguments
///
/// * `found` - Every recovery the sweep gathered, in the order it found them
/// * `claims` - The distinct keys that made each address read as text, tables included
fn unambiguous(
    found: Vec<BlobCandidate>,
    claims: &HashMap<u64, HashSet<Option<u8>>>,
) -> Vec<BlobCandidate> {
    // keep the recoveries whose address only one key ever produced
    found
        .into_iter()
        .filter(|candidate| {
            claims
                .get(&candidate.addr)
                .is_none_or(|keys| keys.len() == 1)
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

/// Record that one decode made each of its runs' addresses read as text.
///
/// Unbounded on purpose, where [`gather_groups`] is capped: a claim is one entry in a
/// set rather than a recovery to be reported, and dropping claims is precisely what
/// [`unambiguous`] must not do.
///
/// # Arguments
///
/// * `claims` - The accumulating per-address set of keys to record into
/// * `runs` - The `(offset, string)` runs from this decode, decoded tables included
/// * `key` - The single-byte key, when the scheme uses one
/// * `base` - The section base address (offsets are relative to it)
fn gather_claims(
    claims: &mut HashMap<u64, HashSet<Option<u8>>>,
    runs: Vec<(usize, String)>,
    key: Option<u8>,
    base: u64,
) {
    // the same item extraction the candidates go through, so the addresses line up
    for (off, _) in recover::extract_items(runs) {
        claims
            .entry(base.saturating_add(off as u64))
            .or_default()
            .insert(key);
    }
}

/// Record one recovered item as a typed, function-attributed blob hit.
///
/// # Arguments
///
/// * `hits` - The accumulating hit list to push onto
/// * `seen` - The set of already-surfaced recoveries (dedup)
/// * `index` - The reference index used to attribute a buffer to its function
/// * `addr` - The section address the encoded bytes start at
/// * `scheme` - The decode scheme label (e.g. `"XOR"`)
/// * `key` - The single-byte key, when the scheme uses one
/// * `section` - The name of the section the bytes came from
/// * `text` - The recovered, human-readable item text
#[allow(clippy::too_many_arguments)]
fn push_decoded(
    hits: &mut Vec<DetectionHit>,
    seen: &mut HashSet<String>,
    index: &[(u64, u64)],
    addr: u64,
    scheme: &str,
    key: Option<u8>,
    section: &str,
    text: String,
) {
    // skip recoveries already surfaced (same text from another key/section)
    if !seen.insert(text.clone()) {
        return;
    }
    // classify the item and build its typed name + markdown content
    let key_str = key.map(|k| format!("0x{k:02x}"));
    let kind = decode::classify(&text);
    let name = decode::recovered_name(kind, &text, section, addr);
    let content = decode::recovered_content(kind, &text, section, addr, scheme, key_str.as_deref());
    // score both axes from the content, starting at the blob method's (low) reliability
    let assessment = decode::assess(low().0, kind, &text);
    let reasoning = decode::append_assessment(
        decode::recovery_reasoning(BLOB_BASE_REASON, scheme, key_str.as_deref()),
        &assessment.notes,
    );
    // attribute to the function that references this buffer (nearest preceding data
    // reference within the scan window); None → submit links the flag to the sample
    let function = decode::buffers::referencing_function(index, addr, MAX_SCAN_BYTES as u64);
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
                .role(format!("{scheme} decode of a referenced data buffer"))
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

/// The window searched for the function that references a chained-decode buffer.
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
/// function contains the byte-transform loop that consumes the buffer, it merely
/// references it, or nothing references it and the artifact must not read as
/// attributed.
///
/// # Arguments
///
/// * `decoders` - The entries of every function carrying a byte-transform decode loop
/// * `function` - The entry of the function that references the buffer, if found
fn nested_attribution(
    decoders: &HashSet<u64>,
    function: Option<u64>,
) -> (Option<String>, Option<String>) {
    match function {
        // the referencing function also transforms bytes in a loop — the decoder itself
        Some(entry) if decoders.contains(&entry) => (
            Some(
                "references this buffer and contains the byte-wise XOR/SUB/ADD loop that \
                 decodes it"
                    .to_string(),
            ),
            None,
        ),
        // referenced, but nothing in it transforms bytes: say only what was seen
        Some(_) => (
            Some("references this buffer".to_string()),
            Some(
                "no byte-transform decode loop was observed in the referencing function; the \
                 data reference is the whole of the attribution"
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
    // the functions that carry a byte-transform loop, for artifact attribution
    let decoders: HashSet<u64> = all
        .iter()
        .filter(|(_, insns)| is_decoder_loop(insns))
        .map(|(entry, _)| *entry)
        .collect();

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
        if let Some(hit) = nested_hit(ctx, &decoders, &index, &mut seen, section, addr, &chain) {
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
/// * `decoders` - The entries of every function carrying a byte-transform decode loop
/// * `index` - The reference index used to attribute a buffer to its function
/// * `seen` - The set of already-surfaced recoveries (dedup)
/// * `section` - The name of the section the encoded run came from
/// * `addr` - The address the encoded run starts at
/// * `chain` - The recovered chain
#[allow(clippy::too_many_arguments)]
fn nested_hit(
    ctx: &AnalysisContext,
    decoders: &HashSet<u64>,
    index: &[(u64, u64)],
    seen: &mut HashSet<String>,
    section: &str,
    addr: u64,
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
    let (role, note) = nested_attribution(decoders, function);
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
        let lk = recover_loop_key(&insns).expect("a key");
        assert_eq!(lk.scheme, "XOR");
        assert_eq!(lk.key, 0x2a);
    }

    /// A register-key transform (no immediate) yields no single-byte key.
    #[test]
    fn recover_loop_key_none_without_immediate() {
        let insns = vec![insn("XOR", "byte ptr [rax], dl", "1000")];
        assert!(recover_loop_key(&insns).is_none());
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
        let r = recover_decoder(&insns, &sections).expect("recovered");
        assert_eq!(r.scheme, "XOR");
        assert_eq!(r.key, "0x9d");
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
        let r = recover_decoder(&insns, &sections).expect("key recovered");
        // key is reported, but no false items are recovered from plaintext
        assert_eq!(r.key, "0x01");
        assert!(r.items.is_empty());
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

    /// Attribution separates the decoding function, a bare reference, and no reference
    /// at all — the three cases mean different things to an analyst.
    #[test]
    fn nested_attribution_distinguishes_its_three_outcomes() {
        let decoders: HashSet<u64> = [0x1000u64].into_iter().collect();
        // the referencing function is itself the decode loop
        let (role, note) = nested_attribution(&decoders, Some(0x1000));
        assert!(role.expect("a role").contains("byte-wise XOR/SUB/ADD loop"));
        assert!(note.is_none());
        // referenced, but no transform loop in it: the role says only that
        let (role, note) = nested_attribution(&decoders, Some(0x2000));
        assert_eq!(role.as_deref(), Some("references this buffer"));
        assert!(
            note.expect("a note")
                .contains("no byte-transform decode loop")
        );
        // nothing references it: no role at all, and the note says why
        let (role, note) = nested_attribution(&decoders, None);
        assert!(role.is_none());
        assert!(note.expect("a note").contains("was not located"));
    }

    /// `unambiguous` keeps a recovery only one key claims and drops a contested address.
    ///
    /// A region of small integers reads as text under a whole band of consecutive keys,
    /// and each individual output is wordy, diverse and entirely plausible — nothing
    /// about one of them gives it away. Seeing that the same address also decoded under
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
    fn claims_of(found: &[BlobCandidate]) -> HashMap<u64, HashSet<Option<u8>>> {
        let mut claims: HashMap<u64, HashSet<Option<u8>>> = HashMap::new();
        for candidate in found {
            claims
                .entry(candidate.addr)
                .or_default()
                .insert(candidate.key);
        }
        claims
    }

    /// A key whose output was a decoded table still contests the address, even though
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
        // could be a candidate but is still on record as having claimed the address
        let mut claims = claims_of(&found);
        claims.entry(0x2000).or_default().insert(Some(0x43));
        assert!(unambiguous(found, &claims).is_empty());
    }

    /// Two buffers with two different keys are both kept — the guard is per-address.
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
}
