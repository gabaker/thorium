//! Anti-instrumentation / anti-hooking detection (MITRE ATT&CK **T1622**).
//!
//! Detects malware that looks for *dynamic instrumentation* and function hooking
//! frameworks used by analysts, and refuses to run when it finds them:
//!
//! * **Frida** — a dynamic instrumentation toolkit; malware checks for the
//!   `frida` string, the `gum-js-loop` thread created by Frida's Gum engine, and
//!   the default control port.
//! * **`LD_PRELOAD` / `ld.so.preload`** — the Linux library-injection mechanisms
//!   analysts use to hook libc functions.
//! * **`/proc/self/maps`** — read to scan the process's own memory map for
//!   injected analysis libraries.
//! * **Analysis-tool names** — debuggers/monitors (`OllyDbg`, x64dbg, `WinDbg`, IDA,
//!   Wireshark, Procmon, Process Hacker) that malware embeds to scan for, to
//!   masquerade as, or to attack. Needles match letter-bounded with a
//!   base64-run veto (see [`TOOL_MATCH_OPTIONS`]), so names sliced out of
//!   longer identifiers — the `IDAQAB` tail ending every RSA public key's
//!   base64, Go runtime symbols like `procExpandEnvironmentStringsW` — never
//!   fire while delimited uses (`WIRESHARK.EXE`, `procexp64.exe`) still do.
//!   An unreferenced match sitting in a resource section is reported as a
//!   T1036 masquerade note (VERSIONINFO metadata naming the tool the sample
//!   impersonates) instead of as tool scanning.
//!
//! It also flags instruction-level anti-instrumentation tells over **decoded**
//! instructions (not raw bytes, which would be far too noisy for single-byte
//! opcodes): the undocumented `ICEBP` (0xF1) debug interrupt, the EFLAGS
//! trap-flag single-step check (a `0x100` test near a `PUSHF`/`POPF`), and code
//! that scans itself for planted `0xCC` (INT3) breakpoint bytes — recognized
//! only as a byte-wide compare against the exact `0xCC` immediate, so hex
//! tokens that merely contain the digits (`[RSP + 0xcc8]` displacements, a
//! packed `0xcccccccd`) stay silent. The tells collapse to one hit per
//! function (every site attached) and cap per program with an overflow
//! summary, so a tell repeated across a large binary is counted once instead
//! of flooding the forum.

use std::borrow::Cow;

use anyhow::Result;

use crate::thorpal::detect::context::{AnalysisContext, Section, fmt_addr, section_at};
use crate::thorpal::detect::detections::{
    AttackTactic, AttackTechnique, Detection, DetectionHit, EvidenceKind, InsnCtx, MatchMode,
    MatchOptions, group_scan_hits, high, low, medium, needle_matches, parse_imm, scan_instructions,
    string_artifact_hits, string_artifact_hits_with,
};

use super::T1622;

/// ATT&CK T1036 — Masquerading.
///
/// Reported instead of [`T1622`] when a tool-name string turns out to be the
/// sample's own resource metadata (a VERSIONINFO block naming the tool it
/// impersonates) rather than a value its code scans for.
const T1036: AttackTechnique = AttackTechnique {
    id: Cow::Borrowed("T1036"),
    name: Cow::Borrowed("Masquerading"),
    tactic: AttackTactic::DefenseEvasion,
};

/// The Anti-instrumentation detection (ATT&CK T1622).
pub struct AntiInstrumentation;

/// Frida instrumentation artifact strings (strong signal).
const FRIDA_STRINGS: &[&str] = &["frida", "gum-js-loop"];

/// Library-injection / self-inspection artifact strings.
const INJECTION_STRINGS: &[&str] = &["ld_preload", "ld.so.preload", "/proc/self/maps"];

/// Debugger/monitor tool-name strings that malware searches for.
const TOOL_STRINGS: &[&str] = &[
    "ollydbg",
    "x64dbg",
    "x32dbg",
    "windbg",
    "ida64",
    "idaq",
    "immunitydebugger",
    "wireshark",
    "procmon",
    "procexp",
    "processhacker",
    "sysinternals",
];

/// How tool-name needles match: letter-bounded, with the base64-run veto.
///
/// Short tool names are substrings of longer identifiers — `idaq` is inside
/// the `IDAQAB` base64 tail every RSA-65537 public key ends with, `procexp`
/// is inside Go's `procExpandEnvironmentStringsW` — so a match flanked by a
/// letter or sitting inside an encoded run is a coincidence, while delimited
/// uses (`WIRESHARK.EXE` with its dot, `procexp64.exe` with its digit) still
/// fire.
const TOOL_MATCH_OPTIONS: MatchOptions = MatchOptions {
    mode: MatchMode::LetterBounded,
    base64_run_veto: true,
    shape: None,
};

/// The tool-name reasoning: the uses the string could serve, not one story.
///
/// The string alone does not say how it is used — scanning for analyst
/// tooling, masquerading as the tool, and attacking the tool all plant the
/// same name — so the reasoning offers the hypotheses and directs
/// verification instead of asserting process scanning for every match.
const TOOL_REASONING: &str = "Contains the name of a debugging/analysis/monitoring tool. Malware embeds these names \
     to scan running processes or windows for analyst tooling, to masquerade as the tool, \
     or to attack the tool itself — verify which use applies at the referencing site.";

/// The most instruction-scan tells kept as individual findings per program.
///
/// Grouped tells past this fold into one overflow summary (see
/// [`group_scan_hits`]); the strongest stay individual, so a genuine INT3
/// self-scan or trap-flag check is never buried by its own repetition.
const MAX_TELL_FINDINGS: usize = 8;

#[async_trait::async_trait]
impl Detection for AntiInstrumentation {
    /// Short, stable name for logs.
    fn name(&self) -> &'static str {
        "anti_instrumentation"
    }

    /// The primary technique this detection reports.
    fn technique(&self) -> AttackTechnique {
        T1622
    }

    /// Report instrumentation/hook-detection string artifacts and
    /// instruction-level tells.
    ///
    /// # Arguments
    ///
    /// * `ctx` - The analysis context
    #[tracing::instrument(name = "AntiInstrumentation::run", skip_all, err(Debug))]
    async fn run(&self, ctx: &AnalysisContext) -> Result<Vec<DetectionHit>> {
        // collect hits from each string group
        let mut hits = Vec::new();
        // 1) Frida artifacts (exclude "frida" inside the day name "friday")
        hits.extend(
            string_artifact_hits(
                ctx,
                T1622,
                FRIDA_STRINGS,
                high(),
                "References Frida, a dynamic instrumentation toolkit used by analysts (the 'frida' \
                 string / the 'gum-js-loop' Gum thread). Malware detects Frida hooking and shuts \
                 down. Defeat by stealthing the Frida agent.",
                &["friday"],
            )
            .await?,
        );
        // 2) LD_PRELOAD / ld.so.preload / self-maps inspection
        hits.extend(
            string_artifact_hits(
                ctx,
                T1622,
                INJECTION_STRINGS,
                medium(),
                "References a library-injection or self-inspection artifact (LD_PRELOAD, \
                 /etc/ld.so.preload, or /proc/self/maps). Malware checks these to detect preloaded \
                 analysis hooks or injected instrumentation libraries and refuse to run.",
                &[],
            )
            .await?,
        );
        // 3) analysis/debugger tool names, matched letter-bounded with the
        //    base64-run veto so encoded blobs and longer identifiers stay silent
        let tool_hits = string_artifact_hits_with(
            ctx,
            T1622,
            TOOL_STRINGS,
            medium(),
            TOOL_REASONING,
            &[],
            TOOL_MATCH_OPTIONS,
        )
        .await?;
        // fetch the section table only when an unreferenced tool name needs the
        // resource-metadata gate (referenced matches never consult it)
        let sections = if tool_hits.iter().any(|hit| hit.sites.is_empty()) {
            ctx.sections().await?
        } else {
            Vec::new()
        };
        // demote unreferenced tool names sitting in resource metadata to a
        // masquerade note instead of asserting tool scanning
        hits.extend(gate_resource_tool_hits(tool_hits, &sections));
        // 4) decoded-instruction anti-instrumentation tells (ICEBP, the EFLAGS
        //    trap-flag single-step check, 0xCC breakpoint-byte scanning),
        //    collapsed per function and capped per program
        hits.extend(collapse_scan_tells(
            scan_instructions(ctx, scan_tell).await?,
        ));
        Ok(hits)
    }
}

/// Flag one decoded instruction when it is an anti-instrumentation tell.
///
/// Three tells are recognized: the undocumented `ICEBP` debug interrupt, an
/// EFLAGS trap-flag (bit 8) test adjacent to a `PUSHF`/`POPF`, and a byte-wide
/// compare against the exact `0xCC` (INT3) immediate.
///
/// # Arguments
///
/// * `c` - The instruction cursor being visited
fn scan_tell(c: &InsnCtx) -> Option<DetectionHit> {
    // the mnemonic and the flattened operand string the tells match on
    let m = c.mnemonic();
    let ops = c.operands().replace(' ', "");
    // ICEBP (0xF1) — an undocumented single-byte debug interrupt used in
    // exception-based anti-debug (its exception fires differently when a
    // debugger is present)
    if m == "ICEBP" {
        return Some(
            DetectionHit::new(
                T1622,
                c.addr(),
                Some(c.func_entry),
                low(),
                format!("ICEBP (INT1) instruction @ {}", fmt_addr(c.addr())),
                "Embeds an ICEBP (0xF1) instruction; malware installs its own handler and \
                 uses whether the resulting debug exception fires to detect a debugger.",
            )
            .kind(EvidenceKind::ContextOnly),
        );
    }
    // EFLAGS Trap Flag (bit 8 = 0x100) single-step check: a test/and/or of
    // 0x100 that is close to a PUSHF/POPF (i.e. actually operating on the
    // flags), not just any use of the constant 256
    if matches!(m.as_str(), "TEST" | "AND" | "OR" | "CMP" | "XOR")
        && ops.contains("0x100")
        && c.preceding(6).iter().any(|i| {
            let pm = i.mnemonic.to_ascii_uppercase();
            matches!(
                pm.as_str(),
                "PUSHF" | "PUSHFD" | "PUSHFQ" | "POPF" | "POPFD" | "POPFQ"
            )
        })
    {
        return Some(
            DetectionHit::new(
                T1622,
                c.addr(),
                Some(c.func_entry),
                medium(),
                format!("Trap flag (single-step) check @ {}", fmt_addr(c.addr())),
                "Reads EFLAGS via PUSHF/POPF and tests bit 8 (0x100, the Trap Flag). Setting \
                 the trap flag and watching whether the expected single-step exception fires \
                 detects a debugger that swallows single-step events. Defeat by letting the \
                 exception through.",
            )
            .kind(EvidenceKind::ControlFlowGated),
        );
    }
    // scanning code for planted 0xCC (INT3) breakpoint bytes: only a byte-wide
    // compare against the exact 0xCC immediate qualifies — a displacement or
    // wider immediate that merely contains the digits (0xcc8, 0xcccccccd) is
    // ordinary code, not a self-scan
    if matches!(m.as_str(), "CMP" | "TEST") && is_int3_byte_compare(&ops) {
        return Some(
            DetectionHit::new(
                T1622,
                c.addr(),
                Some(c.func_entry),
                medium(),
                format!("0xCC breakpoint-byte scan @ {}", fmt_addr(c.addr())),
                "Compares a byte against 0xCC (the INT3 opcode); malware reads its own code \
                 looking for software breakpoints an analyst has planted. Defeat by using \
                 hardware breakpoints or restoring original bytes before the check.",
            )
            .kind(EvidenceKind::ControlFlowGated),
        );
    }
    None
}

/// Whether a flattened `CMP`/`TEST` operand string compares a byte against
/// exactly `0xCC`.
///
/// Operands arrive lower-cased with spaces stripped (so `byte ptr` reads
/// `byteptr`). One side must parse to the exact `0xCC` immediate and the other
/// must be byte-wide — a `byteptr` memory operand or an 8-bit register — the
/// only shape an INT3 self-scan compare takes. Hex tokens that merely contain
/// the digits (a `[rsp+0xcc8]` displacement, a packed `0xcccccccd`) never
/// qualify.
///
/// # Arguments
///
/// * `ops` - The flattened, lower-cased operand string
fn is_int3_byte_compare(ops: &str) -> bool {
    // split into the two comma-separated operands a compare carries
    let Some((lhs, rhs)) = ops.split_once(',') else {
        return false;
    };
    // either side may hold the immediate; the other side must be byte-wide
    (parse_imm(rhs) == Some(0xCC) && is_byte_wide(lhs))
        || (parse_imm(lhs) == Some(0xCC) && is_byte_wide(rhs))
}

/// Whether a flattened operand denotes a byte-wide location.
///
/// # Arguments
///
/// * `op` - The flattened, lower-cased operand
fn is_byte_wide(op: &str) -> bool {
    // a byte-sized memory operand ("byte ptr" with its space stripped)
    if op.contains("byteptr") {
        return true;
    }
    // otherwise only the 8-bit general registers are byte-wide
    matches!(
        op,
        "al" | "bl"
            | "cl"
            | "dl"
            | "ah"
            | "bh"
            | "ch"
            | "dh"
            | "sil"
            | "dil"
            | "spl"
            | "bpl"
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

/// Collapse this detector's instruction-scan tells per function and cap them.
///
/// Delegates to [`group_scan_hits`]: same-tell sites within one function merge
/// into a single hit carrying every site, and past [`MAX_TELL_FINDINGS`] the
/// grouped tells fold into one overflow summary.
///
/// # Arguments
///
/// * `hits` - The per-instruction hits from the scan pass
fn collapse_scan_tells(hits: Vec<DetectionHit>) -> Vec<DetectionHit> {
    // all three tells are generic instruction patterns, so none is cap-exempt
    group_scan_hits(hits, MAX_TELL_FINDINGS, |_| true)
}

/// Apply the resource-metadata gate to the tool-name string hits.
///
/// A referenced match (its xref sites ride on the hit) passes through
/// untouched. An unreferenced match is looked up in the section table: when it
/// sits in a resource section its hit is demoted to the masquerade note (the
/// string is the sample's own VERSIONINFO-style metadata, not a value its code
/// scans for); anywhere else — a data section, or an address the table does
/// not cover — the hit survives unchanged, so a real blocklist string kept
/// outside resources still seeds.
///
/// # Arguments
///
/// * `hits` - The tool-name hits from the string pass
/// * `sections` - The program's section table
fn gate_resource_tool_hits(hits: Vec<DetectionHit>, sections: &[Section]) -> Vec<DetectionHit> {
    // examine each hit, rewriting only the unreferenced resource-section ones
    hits.into_iter()
        .map(|hit| {
            // a referenced string carries its xref sites: the gate is only for
            // matches with no code xrefs
            if !hit.sites.is_empty() {
                return hit;
            }
            // resolve the string's section; an unmapped address proves nothing
            let Some(section) = section_at(sections, hit.address) else {
                return hit;
            };
            // outside a resource block the unreferenced hit stands as-is
            if !is_resource_section(section) {
                return hit;
            }
            // inside resources the string is metadata: reword to the masquerade note
            demote_to_masquerade_note(hit, &section.name)
        })
        .collect()
}

/// Whether a section is a non-executable resource block.
///
/// # Arguments
///
/// * `section` - The section to classify
fn is_resource_section(section: &Section) -> bool {
    // an executable mapping is never resource data
    if section.is_executable() {
        return false;
    }
    // PE resource blocks render as ".rsrc" (possibly decorated by the bridge)
    section.name.to_ascii_lowercase().contains("rsrc")
}

/// Demote an unreferenced resource-section tool-name hit to a masquerade note.
///
/// The T1622 scanning claim is replaced by what the evidence supports: the
/// resource metadata names the tool, which is consistent with T1036
/// masquerading as it. Severity stays low — the note is an IOC worth
/// surfacing, not a detection of anti-analysis behavior.
///
/// # Arguments
///
/// * `hit` - The unreferenced tool-name hit to rewrite
/// * `section` - The name of the resource section the string sits in
fn demote_to_masquerade_note(mut hit: DetectionHit, section: &str) -> DetectionHit {
    // name the needle that matched, falling back to a generic phrase when the
    // content's truncation hid it
    let tool = matched_tool(&hit.content).map_or_else(
        || "an analysis tool".to_string(),
        |needle| format!("`{needle}`"),
    );
    // the claim is masquerading, not debugger evasion
    hit.technique = T1036;
    // mark the content as resource metadata so the lead reads as what it is
    hit.content = format!("resource-metadata {}", hit.content);
    // replace the scanning assertion with the masquerade observation
    hit.reasoning = format!(
        "Version-info metadata naming {tool}; consistent with T1036 masquerading as the \
         tool, not necessarily tool scanning. The string sits in resource section \
         `{section}` with no code references, so it is the sample's own metadata rather \
         than a value its code scans for."
    );
    // a masquerade note is a low-severity observation
    let (confidence, suspicion) = low();
    hit.confidence = confidence;
    hit.suspicion = suspicion;
    hit
}

/// The tool-name needle a hit's content matched, when one still shows.
///
/// Replays the catalog under [`TOOL_MATCH_OPTIONS`] against the content (which
/// quotes the matched string); the content's truncation of long strings can
/// hide the match, in which case there is no needle to name.
///
/// # Arguments
///
/// * `content` - The hit content quoting the matched string
fn matched_tool(content: &str) -> Option<&'static str> {
    // lower-case once, the way the string pass matches
    let hay = content.to_ascii_lowercase();
    // the first catalog needle still visible in the quoted value
    TOOL_STRINGS
        .iter()
        .copied()
        .find(|needle| needle_matches(&hay, needle, TOOL_MATCH_OPTIONS))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thorpal::detect::confidence::Confidence;
    use ghidra_cli::models::Instruction;

    /// Build a minimal instruction for the fixture bodies.
    ///
    /// # Arguments
    ///
    /// * `mnemonic` - The instruction mnemonic
    /// * `operands` - The joined operand string
    /// * `address` - The instruction address
    fn insn(mnemonic: &str, operands: &str, address: u64) -> Instruction {
        Instruction {
            address: fmt_addr(address),
            mnemonic: mnemonic.into(),
            operands: vec![operands.into()],
            bytes: None,
            length: None,
            flow_type: None,
        }
    }

    /// Run the instruction tells over one fixture function body.
    ///
    /// # Arguments
    ///
    /// * `entry` - The function's entry address
    /// * `insns` - The function's instruction list
    fn scan_all(entry: u64, insns: &[Instruction]) -> Vec<DetectionHit> {
        // visit every instruction the way scan_instructions does
        (0..insns.len())
            .filter_map(|idx| {
                scan_tell(&InsnCtx {
                    func_entry: entry,
                    insns,
                    idx,
                })
            })
            .collect()
    }

    /// Build a loaded section for the gate tests.
    ///
    /// # Arguments
    ///
    /// * `name` - The block name
    /// * `base` - The block's base address
    /// * `end` - The block's (exclusive) end address
    /// * `permissions` - The permission string
    fn section(name: &str, base: u64, end: u64, permissions: &str) -> Section {
        Section {
            name: name.to_string(),
            base,
            end,
            size: end - base,
            permissions: permissions.to_string(),
            is_initialized: true,
            is_loaded: true,
            bytes: Vec::new(),
            truncated: false,
        }
    }

    /// Build an unreferenced tool-string hit the way the string pass does.
    ///
    /// # Arguments
    ///
    /// * `addr` - The string's address
    /// * `value` - The matched string value
    fn unreferenced_hit(addr: u64, value: &str) -> DetectionHit {
        DetectionHit::new(
            T1622,
            addr,
            None,
            low(),
            format!("string {value:?}"),
            format!("{TOOL_REASONING} (string not referenced by code in this analysis)"),
        )
    }

    /// The disputed displacement compare (`CMP qword ptr [RSP+0xcc8],0x0` @
    /// 0x135bf5) no longer fires the 0xCC arm.
    #[test]
    fn displacement_compare_stays_silent() {
        // the drop-guard null check the fleet disputed: immediate 0x0,
        // displacement 0xcc8 — the "0xcc" is inside the displacement token
        let body = [insn("CMP", "qword ptr [RSP + 0xcc8], 0x0", 0x0013_5bf5)];
        assert!(scan_all(0x0013_0ebf, &body).is_empty());
        // the flattened operand shape, rejected by the exact-immediate parse
        assert!(!is_int3_byte_compare("qwordptr[rsp+0xcc8],0x0"));
    }

    /// Wider compares that merely contain the 0xcc digits stay silent.
    #[test]
    fn wide_compares_stay_silent() {
        // a packed dword constant containing the digits is not a byte scan
        let packed = [insn("CMP", "EAX, 0xcccccccd", 0x40_1000)];
        assert!(scan_all(0x40_1000, &packed).is_empty());
        // a dword test against 0xCC is a flag mask, not an INT3 self-scan
        let mask = [insn("TEST", "ECX, 0xcc", 0x40_2000)];
        assert!(scan_all(0x40_2000, &mask).is_empty());
    }

    /// A byte-wide compare against exactly 0xCC still seeds, at medium.
    #[test]
    fn byte_compare_against_int3_seeds() {
        // the canonical self-scan shapes: a byte memory operand (the confirmed
        // rollup site 0x1000b8b0) and an 8-bit register
        for ops in ["byte ptr [RAX], 0xcc", "AL, 0xcc"] {
            let body = [insn("CMP", ops, 0x1000_b8b0)];
            let hits = scan_all(0x1000_b800, &body);
            assert_eq!(hits.len(), 1, "{ops}: {hits:?}");
            let hit = &hits[0];
            // surviving scans are near-certain, so they seed above rollup noise
            assert!(matches!(hit.confidence, Confidence::Unsure), "{ops}");
            assert_eq!(hit.suspicion, 40, "{ops}");
            assert_eq!(hit.kind, EvidenceKind::ControlFlowGated, "{ops}");
            assert_eq!(hit.function, Some(0x1000_b800), "{ops}");
            assert!(hit.content.contains("0xCC breakpoint-byte scan"), "{ops}");
        }
    }

    /// Same-function 0xCC sites collapse into one hit carrying every site.
    #[test]
    fn cc_sites_collapse_per_function() {
        // three self-scan compares inside one function
        let body = [
            insn("CMP", "byte ptr [RAX], 0xcc", 0x5000),
            insn("CMP", "byte ptr [RAX + 0x1], 0xcc", 0x5008),
            insn("CMP", "byte ptr [RAX + 0x2], 0xcc", 0x5010),
        ];
        let hits = collapse_scan_tells(scan_all(0x5000, &body));
        // one hit for the function, every site attached
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].sites, vec![0x5000, 0x5008, 0x5010]);
        assert!(hits[0].content.contains("3 sites"), "{}", hits[0].content);
        assert_eq!(hits[0].function, Some(0x5000));
    }

    /// Past the per-program cap, grouped tells fold into one overflow summary.
    #[test]
    fn cc_flood_caps_with_summary() {
        // one self-scan compare in each of twelve functions
        let mut hits = Vec::new();
        for n in 0..12u64 {
            let entry = 0x40_0000 + n * 0x100;
            let body = [insn("CMP", "byte ptr [RAX], 0xcc", entry)];
            hits.extend(scan_all(entry, &body));
        }
        let capped = collapse_scan_tells(hits);
        // the cap keeps eight individual tells plus the one summary
        assert_eq!(capped.len(), MAX_TELL_FINDINGS + 1, "{capped:?}");
        let summary = capped.last().expect("a summary hit");
        // the summary is file-level and counts everything elided
        assert_eq!(summary.function, None);
        assert!(
            summary.content.contains("4 generic tells elided"),
            "{}",
            summary.content
        );
    }

    /// ICEBP and trap-flag tells still seed, and a bare 0x100 test does not.
    #[test]
    fn icebp_and_trap_flag_tells_seed() {
        // a PUSHFQ-adjacent trap-flag test followed by an ICEBP
        let body = [
            insn("PUSHFQ", "", 0x7000),
            insn("TEST", "EAX, 0x100", 0x7001),
            insn("ICEBP", "", 0x7002),
        ];
        let hits = scan_all(0x7000, &body);
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert!(hits[0].content.contains("Trap flag"), "{}", hits[0].content);
        assert_eq!(hits[0].kind, EvidenceKind::ControlFlowGated);
        assert!(hits[1].content.contains("ICEBP"), "{}", hits[1].content);
        // without a PUSHF/POPF nearby the 0x100 constant proves nothing
        let bare = [insn("TEST", "EAX, 0x100", 0x8000)];
        assert!(scan_all(0x8000, &bare).is_empty());
    }

    /// The idaq needle no longer matches the base64 tail of an RSA public key.
    #[test]
    fn idaq_in_rsa_key_no_longer_matches() {
        // an RSA-2048 SPKI base64 body: every such key ends in the IDAQAB
        // rendering of the 65537 exponent, which contains "idaq"
        let key = "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAq7BFUpkGp3lxkDGANCwIDAQAB";
        let hay = key.to_ascii_lowercase();
        // the bare-substring behavior this catalog opted out of
        assert!(hay.contains("idaq"));
        // no catalog needle matches the key under the bounded matcher
        for needle in TOOL_STRINGS {
            assert!(
                !needle_matches(&hay, needle, TOOL_MATCH_OPTIONS),
                "{needle} matched the RSA key"
            );
        }
        // the demotion helper finds no tool to name in such a content either
        assert_eq!(matched_tool(&format!("string {key:?}")), None);
    }

    /// The confirmed tool-name values keep matching under the bounded matcher.
    #[test]
    fn confirmed_tool_names_still_match() {
        // the fleet's confirmed leads: WIRESHARK.EXE (dot boundary), bare
        // procexp and its digit-bounded variant, OLLYDBG, and the
        // space-bounded Sysinternals metadata value
        let confirmed = [
            ("WIRESHARK.EXE", "wireshark"),
            ("procexp", "procexp"),
            ("procexp64.exe", "procexp"),
            ("OLLYDBG", "ollydbg"),
            ("Sysinternals PsFile", "sysinternals"),
        ];
        for (value, needle) in confirmed {
            assert!(
                needle_matches(&value.to_ascii_lowercase(), needle, TOOL_MATCH_OPTIONS),
                "{needle} no longer matches {value:?}"
            );
        }
    }

    /// Go runtime symbols containing a tool name no longer match.
    #[test]
    fn go_symbol_no_longer_matches() {
        // procexp sits letter-flanked inside this Go runtime symbol
        let hay = "procExpandEnvironmentStringsW".to_ascii_lowercase();
        for needle in TOOL_STRINGS {
            assert!(
                !needle_matches(&hay, needle, TOOL_MATCH_OPTIONS),
                "{needle} matched the Go symbol"
            );
        }
    }

    /// An unreferenced tool name in .rsrc demotes to the T1036 masquerade note.
    #[test]
    fn rsrc_decoy_demotes_to_masquerade_note() {
        // the Sysinternals-decoy VERSIONINFO string at its .rsrc address
        let sections = [
            section(".text", 0x100_0000, 0x102_0000, "rx"),
            section(".rsrc", 0x102_2000, 0x102_3000, "r"),
        ];
        let hits = vec![unreferenced_hit(0x102_2130, "Sysinternals PsFile")];
        let gated = gate_resource_tool_hits(hits, &sections);
        assert_eq!(gated.len(), 1);
        let hit = &gated[0];
        // the claim moves from debugger evasion to masquerading
        assert_eq!(hit.technique.id.as_ref(), "T1036");
        assert!(
            hit.content.starts_with("resource-metadata "),
            "{}",
            hit.content
        );
        // the reworded reasoning names the tool and the masquerade hypothesis
        assert!(
            hit.reasoning.contains("`sysinternals`"),
            "{}",
            hit.reasoning
        );
        assert!(
            hit.reasoning
                .contains("masquerading as the tool, not necessarily tool scanning"),
            "{}",
            hit.reasoning
        );
        // the scanning assertion is gone and the severity stays low
        assert!(
            !hit.reasoning.contains("scan running processes"),
            "{}",
            hit.reasoning
        );
        assert!(matches!(hit.confidence, Confidence::Untrusted));
        assert_eq!(hit.suspicion, 15);
    }

    /// The confirmed unreferenced WIRESHARK lead outside resources survives
    /// unchanged.
    #[test]
    fn data_section_tool_name_survives() {
        // an observed shape: an unreferenced string in a 0x400000-based
        // PE's data section, not in .rsrc
        let sections = [
            section(".text", 0x40_1000, 0x41_5000, "rx"),
            section(".data", 0x41_5000, 0x41_b000, "r"),
        ];
        let hits = vec![unreferenced_hit(0x41_9fb8, "WIRESHARK.EXE")];
        let gated = gate_resource_tool_hits(hits, &sections);
        assert_eq!(gated.len(), 1);
        let hit = &gated[0];
        // the lead keeps its technique, strength, and unreferenced reasoning
        assert_eq!(hit.technique.id.as_ref(), "T1622");
        assert!(matches!(hit.confidence, Confidence::Untrusted));
        assert_eq!(hit.suspicion, 15);
        assert!(
            hit.reasoning
                .contains("(string not referenced by code in this analysis)"),
            "{}",
            hit.reasoning
        );
    }

    /// Referenced tool-name hits pass the gate untouched, wherever the string
    /// sits.
    #[test]
    fn referenced_tool_hit_passes_gate_untouched() {
        // a referenced OLLYDBG string anchored in .rsrc territory: the xref
        // sites on the hit mean the gate never applies
        let sections = [section(".rsrc", 0x102_2000, 0x102_3000, "r")];
        let hit = DetectionHit::new(
            T1622,
            0x102_2130,
            Some(0x40_1000),
            medium(),
            "string \"OLLYDBG\" referenced @ 0x401234".to_string(),
            TOOL_REASONING,
        )
        .sites(vec![0x40_1234]);
        let gated = gate_resource_tool_hits(vec![hit], &sections);
        assert_eq!(gated.len(), 1);
        // the referenced lead keeps its technique and medium strength
        assert_eq!(gated[0].technique.id.as_ref(), "T1622");
        assert!(matches!(gated[0].confidence, Confidence::Unsure));
        assert_eq!(gated[0].suspicion, 40);
    }

    /// An unreferenced match outside every known section survives the gate.
    #[test]
    fn unmapped_string_survives_gate() {
        // no section covers the address, so the gate proves nothing and keeps it
        let gated = gate_resource_tool_hits(vec![unreferenced_hit(0xdead_0000, "procmon")], &[]);
        assert_eq!(gated.len(), 1);
        assert_eq!(gated[0].technique.id.as_ref(), "T1622");
    }

    /// The tool reasoning offers the competing uses instead of one story.
    #[test]
    fn tool_reasoning_offers_hypotheses() {
        // all three hypotheses are offered, with verification directed
        assert!(TOOL_REASONING.contains("to scan running processes or windows"));
        assert!(TOOL_REASONING.contains("to masquerade as the tool"));
        assert!(TOOL_REASONING.contains("to attack the tool itself"));
        assert!(TOOL_REASONING.contains("verify which use applies"));
        // the old single-story detonation claim is gone
        assert!(!TOOL_REASONING.contains("refuses to detonate"));
    }
}
