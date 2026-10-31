//! Debugger Evasion detection (MITRE ATT&CK **T1622**).
//!
//! Detects the classic ways malware notices — and hides from — a debugger so
//! that an analyst single-stepping the sample never observes its real behavior:
//!
//! 1. **Anti-debug API calls** — imported Windows/Nt APIs that detect or blind a
//!    debugger (e.g. `IsDebuggerPresent`, `CheckRemoteDebuggerPresent`,
//!    `NtQueryInformationProcess`). Found by matching the program's imports and
//!    reporting each calling function. The dual-use entries (the Nt*
//!    information queries, `Get`/`SetThreadContext`) are worded as capability +
//!    condition — the one discriminating argument (`InformationClass`,
//!    `ContextFlags`) decides the anti-debug claim. The post-detection validator
//!    (`crate::thorpal::detect::validate`) recovers register/push-staged
//!    `InformationClass` values; the struct-field `ContextFlags` is recovered by
//!    this module's own refinement pass.
//! 2. **Manual PEB reads** — inlined equivalents of `IsDebuggerPresent` that read
//!    the Process Environment Block directly via `FS:[0x30]` (x86) / `GS:[0x60]`
//!    (x64). The field consumed next is classified against per-architecture
//!    offset tables while tracking the register that received the PEB base, so
//!    stack spills and unrelated dereferences never classify. `Ldr` reads are
//!    reported as the manual API-resolution walk they are (T1106) rather than as
//!    anti-debug, `ProcessParameters` reads as startup-data access, and
//!    unclassified loads group per function and cap per program.
//! 3. **`INT 0x2D`** — a kernel debugging interrupt whose behavior diverges under
//!    a debugger. Found by scanning decoded instructions.
//! 4. **`ptrace` self-attach** — the Linux `ptrace(PTRACE_TRACEME)` trick: a
//!    process may be traced only once, so if a debugger is attached the call
//!    fails. Found via the `ptrace` import (dynamically linked) or by resolving a
//!    raw `syscall` with `eax == 101` (statically-linked, stripped ELF); the
//!    request argument, where recoverable, separates the self-attach trick from
//!    offensive tracing of another process.
//! 5. **`/proc/self/status` + `TracerPid`** — the Linux userland equivalent: a
//!    non-zero `TracerPid` reveals an attached debugger. Found via strings.
//! 6. **`OutputDebugString` last-error probe** — the XP-era trick reads
//!    `GetLastError` after the call, so a hit is emitted only when the calling
//!    function also touches the last-error APIs; the bare call is plain logging
//!    in essentially every MSVC binary and seeds nothing.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write;

use anyhow::{Context, Result};
use ghidra_cli::models::Instruction;

use crate::thorpal::detect::confidence::Confidence;
use crate::thorpal::detect::context::{AnalysisContext, CallSite, fmt_addr, parse_addr};
use crate::thorpal::detect::detections::execution::T1106;
use crate::thorpal::detect::detections::{
    ApiSpec, AttackTechnique, Detection, DetectionHit, EvidenceKind, api_call_hits, content_class,
    group_scan_hits, high, low, medium, normalize, parse_imm, reg_value_before, render_sites,
    scan_instructions, string_artifact_hits,
};
use crate::thorpal::detect::op_str;

use super::T1622;

/// The Debugger Evasion detection (ATT&CK T1622).
pub struct DebuggerEvasion;

/// Imported anti-debug APIs and why each matters (meaning + how it thwarts analysis).
///
/// Dual-use entries state the condition that makes the call anti-debug (the
/// `InformationClass`, the `ContextFlags`, the ptrace request) rather than
/// asserting the hostile reading outright; the validation and refinement
/// passes resolve the condition where it is statically visible.
const APIS: &[ApiSpec] = &[
    ApiSpec {
        key: "IsDebuggerPresent",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls kernel32!IsDebuggerPresent, which reads the PEB BeingDebugged byte. \
                    Code that branches on the result can divert to benign/exit logic so its real \
                    behavior is never observed under a debugger; the call also lives in every \
                    MSVC runtime's error-reporting path, so the branch on the result is what \
                    makes it evasion. Defeat by zeroing BeingDebugged or forcing the return \
                    value.",
    },
    ApiSpec {
        key: "CheckRemoteDebuggerPresent",
        always_noise: false,
        severity: high(),
        reasoning: "Calls CheckRemoteDebuggerPresent, which asks the kernel whether a debugger is \
                    attached (works even for debuggers hidden from BeingDebugged). Defeat by forcing \
                    the output BOOL to false.",
    },
    ApiSpec {
        key: "NtQueryInformationProcess",
        always_noise: false,
        severity: high(),
        reasoning: "Calls NtQueryInformationProcess. With InformationClass ProcessDebugPort (7), \
                    ProcessDebugObjectHandle (0x1E), or ProcessDebugFlags (0x1F) this is the \
                    low-level debugger probe that evades the obvious debug APIs; with other \
                    classes (ProcessBasicInformation 0 is the common one) it is ordinary process \
                    plumbing. The class argument, where it can be recovered, decides which claim \
                    holds. Defeat a debug-class query by hooking the call to return clean values.",
    },
    ApiSpec {
        key: "ZwQueryInformationProcess",
        always_noise: false,
        severity: high(),
        reasoning: "Calls ZwQueryInformationProcess (the Nt* twin); the same InformationClass \
                    condition applies — the debug classes 7/0x1E/0x1F are the debugger probe, \
                    other classes are ordinary process plumbing.",
    },
    ApiSpec {
        key: "NtSetInformationThread",
        always_noise: false,
        severity: high(),
        reasoning: "Calls NtSetInformationThread. With ThreadInformationClass \
                    ThreadHideFromDebugger (0x11) the thread detaches from debug events — \
                    breakpoints and single-stepping stop working and the analyst is blinded; \
                    with other classes (ThreadAffinityMask 4, priorities) it is routine thread \
                    management. The class argument, where it can be recovered, decides which \
                    claim holds. Defeat the hide call by neutralizing it.",
    },
    ApiSpec {
        key: "ZwSetInformationThread",
        always_noise: false,
        severity: high(),
        reasoning: "Calls ZwSetInformationThread (the Nt* twin); the same condition applies — \
                    only ThreadHideFromDebugger (0x11) blinds the debugger.",
    },
    ApiSpec {
        key: "NtQuerySystemInformation",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls NtQuerySystemInformation. With InformationClass \
                    SystemKernelDebuggerInformation (0x23) it detects a kernel debugger and can \
                    refuse to run while WinDbg/KD is active; with the common classes \
                    (SystemProcessInformation 5, SystemModuleInformation 0xB) it is \
                    process/module enumeration. The class argument, where it can be recovered, \
                    decides which claim holds.",
    },
    ApiSpec {
        key: "DebugActiveProcess",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls DebugActiveProcess; malware self-debugs (a child debugs the parent) so no \
                    second debugger can attach — locking out the analyst's debugger.",
    },
    ApiSpec {
        key: "DbgBreakPoint",
        always_noise: false,
        severity: medium(),
        reasoning: "References DbgBreakPoint, a routine debuggers use to break in; malware patches or \
                    watches it to detect/prevent debugger attachment.",
    },
    ApiSpec {
        key: "DbgUiRemoteBreakin",
        always_noise: false,
        severity: medium(),
        reasoning: "References DbgUiRemoteBreakin, the routine DebugActiveProcess injects to break \
                    into a target; patching it defeats attach-based debugging.",
    },
    ApiSpec {
        key: "GetThreadContext",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls GetThreadContext, which copies whatever thread state the caller's \
                    CONTEXT.ContextFlags request. Only when the CONTEXT_DEBUG_REGISTERS bit \
                    (0x10) is set does it read the DR0-DR7 hardware-breakpoint registers an \
                    analyst sets; hook engines, thread-hijack injectors, and pthread-cancel \
                    implementations request CONTEXT_CONTROL only. The flags, where statically \
                    visible, decide which claim holds.",
    },
    ApiSpec {
        key: "SetThreadContext",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls SetThreadContext, which writes whatever thread state \
                    CONTEXT.ContextFlags covers. Only with CONTEXT_DEBUG_REGISTERS (0x10) set \
                    can it clear hardware breakpoints (DR0-DR7); Detours-style hook commits and \
                    thread-hijack/EIP-migration code write control registers only. The flags, \
                    where statically visible, decide which claim holds.",
    },
    ApiSpec {
        key: "ptrace",
        always_noise: false,
        severity: high(),
        reasoning: "Calls ptrace. With request PTRACE_TRACEME (0) a process self-attaches — only \
                    one tracer may exist, so the call failing reveals a debugger; with other \
                    requests (ATTACH/PEEKDATA/POKEDATA) ptrace traces another process instead, \
                    which is offensive tooling rather than self-defense. The request, where it \
                    can be recovered, decides which claim holds. Defeat the self-attach by \
                    stubbing ptrace to succeed.",
    },
];

/// Strings that reveal a Linux debugger via `/proc/self/status` + `TracerPid`.
const PROC_STATUS_STRINGS: &[&str] = &["/proc/self/status", "tracerpid"];

/// The last-error APIs whose co-location with `OutputDebugString` matches the
/// probe shape (set an error, call ODS, read the error back).
const ODS_PROBE_APIS: &[&str] = &["GetLastError", "SetLastError"];

/// The reasoning attached to an `OutputDebugString` last-error probe hit.
const ODS_PROBE_REASONING: &str = "An old (XP-era) anti-debug probe sets a known last-error value, calls \
     OutputDebugString, and reads GetLastError back: with no debugger attached the call \
     fails and rewrites the error code, so a preserved value betrays the debugger. This \
     function calls OutputDebugString and the last-error APIs together, matching that \
     shape — but plain error logging co-locates the same calls, so this stays a weak \
     signal until the set/call/read sequence is confirmed.";

/// The most generic (unclassified) PEB-load findings kept individually per
/// program; the overflow folds into one file-level summary hit.
const GENERIC_PEB_CAP: usize = 8;

/// How many instructions after a PEB base load the field classifier reads.
const PEB_WINDOW: usize = 8;

/// How many instructions before a `Get`/`SetThreadContext` call site the
/// `ContextFlags` recovery reads.
const CONTEXT_FLAGS_WINDOW: usize = 24;

/// The `CONTEXT.ContextFlags` option bit selecting the DR0-DR7 debug registers.
const CONTEXT_DEBUG_REGISTERS: u64 = 0x10;

/// The PEB fields the classifier can attribute a post-load dereference to.
#[derive(Debug, Clone, Copy)]
enum PebField {
    /// `BeingDebugged` — the byte `IsDebuggerPresent` reads.
    BeingDebugged,
    /// `NtGlobalFlag` — the loader's flag word, whose heap-debug bits a
    /// debugger-created process carries.
    NtGlobalFlag,
    /// `Ldr` — the loaded-module list (manual API resolution, not anti-debug).
    Ldr,
    /// `ProcessHeap` — the default heap whose Flags/ForceFlags betray a debugger.
    ProcessHeap,
    /// `ProcessParameters` — command line/environment data (not anti-debug).
    ProcessParameters,
}

/// The x86 PEB field offsets reachable from an `FS:[0x30]` base load.
const PEB_FIELDS_X86: &[(u64, PebField)] = &[
    (0x2, PebField::BeingDebugged),
    (0x68, PebField::NtGlobalFlag),
    (0xC, PebField::Ldr),
    (0x18, PebField::ProcessHeap),
    (0x10, PebField::ProcessParameters),
];

/// The x64 PEB field offsets reachable from a `GS:[0x60]` base load.
const PEB_FIELDS_X64: &[(u64, PebField)] = &[
    (0x2, PebField::BeingDebugged),
    (0xBC, PebField::NtGlobalFlag),
    (0x18, PebField::Ldr),
    (0x30, PebField::ProcessHeap),
    (0x20, PebField::ProcessParameters),
];

/// The x86 `PEB_LDR_DATA` module-list head offsets that corroborate an Ldr walk.
const LDR_LIST_HEADS_X86: &[u64] = &[0xC, 0x14, 0x1C];

/// The x64 `PEB_LDR_DATA` module-list head offsets that corroborate an Ldr walk.
const LDR_LIST_HEADS_X64: &[u64] = &[0x10, 0x20, 0x30];

/// The x86/x64 general-register families, widest name first.
///
/// Writing any alias replaces the family's value for this module's purposes (a
/// 32-bit write zero-extends into the 64-bit name), so register tracking and
/// overwrite checks treat each family as one register.
const REG_FAMILIES: &[&[&str]] = &[
    &["rax", "eax", "ax", "al", "ah"],
    &["rbx", "ebx", "bx", "bl", "bh"],
    &["rcx", "ecx", "cx", "cl", "ch"],
    &["rdx", "edx", "dx", "dl", "dh"],
    &["rsi", "esi", "si", "sil"],
    &["rdi", "edi", "di", "dil"],
    &["rbp", "ebp", "bp", "bpl"],
    &["rsp", "esp", "sp", "spl"],
    &["r8", "r8d", "r8w", "r8b"],
    &["r9", "r9d", "r9w", "r9b"],
    &["r10", "r10d", "r10w", "r10b"],
    &["r11", "r11d", "r11w", "r11b"],
    &["r12", "r12d", "r12w", "r12b"],
    &["r13", "r13d", "r13w", "r13b"],
    &["r14", "r14d", "r14w", "r14b"],
    &["r15", "r15d", "r15w", "r15b"],
];

#[async_trait::async_trait]
impl Detection for DebuggerEvasion {
    /// Short, stable name for logs.
    fn name(&self) -> &'static str {
        "debugger_evasion"
    }

    /// The primary technique this detection reports.
    fn technique(&self) -> AttackTechnique {
        T1622
    }

    /// Run all Debugger Evasion sub-passes and merge their hits.
    ///
    /// # Arguments
    ///
    /// * `ctx` - The shared analysis context
    #[tracing::instrument(name = "DebuggerEvasion::run", skip_all, err(Debug))]
    async fn run(&self, ctx: &AnalysisContext) -> Result<Vec<DetectionHit>> {
        // collect hits from every sub-pass into one vector
        let mut hits = Vec::new();
        // 1) imported anti-debug API call sites (their return value gates behavior)
        hits.extend(api_call_hits(ctx, T1622, APIS, EvidenceKind::ApiResultCheck).await?);
        // name what Get/SetThreadContext actually access when the ContextFlags
        // initialization is statically visible near the call
        refine_thread_context_hits(ctx, &mut hits).await;
        // 2) OutputDebugString, gated on the last-error co-location that makes
        //    the probe shape real (the bare call is logging in every MSVC binary)
        hits.extend(output_debug_string_hits(ctx).await?);
        // the architecture decides which PEB field offsets apply below
        let is64 = ctx
            .program
            .language
            .as_deref()
            .unwrap_or_default()
            .contains(":64");
        // 3) decoded-instruction opcode tells: INT 0x2D, manual PEB reads, and the
        //    raw ptrace syscall (single scan over all functions' instructions)
        let scan = scan_instructions(ctx, |c| {
            let m = c.mnemonic();
            let ops = c.operands();
            let ops_nospace = ops.replace(' ', "");
            // INT 0x2D — a kernel debug interrupt whose flow diverges under a debugger
            if m == "INT" && (ops_nospace == "0x2d" || ops_nospace.ends_with(",0x2d")) {
                return Some(DetectionHit::new(
                    T1622,
                    c.addr(),
                    Some(c.func_entry),
                    medium(),
                    format!("INT 0x2D anti-debug interrupt @ {}", fmt_addr(c.addr())),
                    "Issues INT 0x2D, a kernel debugging interrupt whose flow diverges (and can \
                     skip a byte) under a debugger; malware uses that divergence to detect one. \
                     Defeat by adjusting the instruction pointer past the trap.",
                ).kind(EvidenceKind::ControlFlowGated));
            }
            // manual PEB base load (FS:[0x30] on x86, GS:[0x60] on x64)
            if ops_nospace.contains("fs:[0x30]") || ops_nospace.contains("gs:[0x60]") {
                // classify the field consumed next, tracking only the register
                // that received the PEB base (spills and unrelated dereferences
                // never classify)
                let dest = peb_load_dest(c.insn());
                let class = classify_peb(dest.as_deref(), c.following(PEB_WINDOW), is64);
                return Some(
                    DetectionHit::new(
                        class.technique,
                        c.addr(),
                        Some(c.func_entry),
                        class.severity,
                        format!("{} @ {}", class.label, fmt_addr(c.addr())),
                        class.reasoning,
                    )
                    .kind(class.kind),
                );
            }
            // raw ptrace syscall (statically-linked, stripped ELF): eax/rax == 101
            if m == "SYSCALL" && reg_value_before(c.preceding(8), "eax", "rax") == Some(101) {
                // the request (1st argument, edi/rdi) separates the TRACEME
                // self-attach from tracing another process
                let request = reg_value_before(c.preceding(8), "edi", "rdi");
                return Some(ptrace_syscall_hit(c.addr(), c.func_entry, request));
            }
            None
        })
        .await?;
        // group same-class tells per function and cap the generic PEB flood,
        // folding the overflow into one file-level summary
        hits.extend(group_scan_hits(scan, GENERIC_PEB_CAP, is_generic_peb));
        // 4) /proc/self/status + TracerPid strings (Linux userland debugger check)
        hits.extend(
            string_artifact_hits(
                ctx,
                T1622,
                PROC_STATUS_STRINGS,
                medium(),
                "References /proc/self/status / the TracerPid field; malware reads TracerPid and a \
                 non-zero value reveals an attached debugger without calling ptrace. Defeat by \
                 faking a zero TracerPid.",
                &[],
            )
            .await?,
        );
        Ok(hits)
    }
}

/// What one manual PEB base load should seed, as decided by [`classify_peb`].
struct PebClass {
    /// The `(confidence, suspicion)` severity for the hit.
    severity: (Confidence, i64),
    /// The ATT&CK technique the classified observation evidences.
    technique: AttackTechnique,
    /// How the post-detection validator should confirm/refute the hit.
    kind: EvidenceKind,
    /// The content label rendered ahead of the site address.
    label: &'static str,
    /// The analyst-facing reasoning.
    reasoning: &'static str,
}

/// Classify the PEB field consumed after a manual PEB base load.
///
/// Applies the per-architecture offset table to dereferences of the register
/// that received the PEB base, walking a bounded look-ahead window. Tracking
/// the register keeps stack spills (`[rsp+0x18]`) and later unrelated
/// dereferences from classifying; the look-ahead ends when that register is
/// replaced or control may leave the straight line (`CALL`/`JMP`/`RET`), and a
/// load with no attributable field read falls back to the generic claim.
///
/// # Arguments
///
/// * `dest` - The register that received the PEB base, when the load named one
/// * `following` - The instructions immediately after the PEB base load
/// * `is64` - Whether the program is 64-bit (selects the offset table)
fn classify_peb(dest: Option<&str>, following: &[Instruction], is64: bool) -> PebClass {
    // the architecture decides which PEB layout the offsets mean
    let fields = if is64 { PEB_FIELDS_X64 } else { PEB_FIELDS_X86 };
    // without a tracked destination register no dereference can be attributed
    let Some(dest) = dest else {
        return generic_peb();
    };
    // widen the destination to its register family (an eax write aliases rax)
    let aliases = register_aliases(dest);
    // walk the look-ahead window for a dereference of the tracked register
    for (idx, insn) in following.iter().enumerate() {
        let mnemonic = insn.mnemonic.to_ascii_uppercase();
        // a call may pass the pointer elsewhere (and clobbers volatiles) and an
        // unconditional transfer ends the straight line — attribute nothing past either
        if mnemonic == "CALL" || mnemonic == "JMP" || mnemonic.starts_with("RET") {
            return generic_peb();
        }
        let ops = op_str(insn).to_ascii_lowercase().replace(' ', "");
        // a dereference of the tracked register at a known offset classifies the load
        if let Some(offset) = deref_offset(&ops, &aliases)
            && let Some((_, field)) = fields.iter().find(|(known, _)| *known == offset)
        {
            return classify_field(*field, insn, &following[idx + 1..], is64);
        }
        // the tracked register being replaced ends what the look-ahead can attribute
        let first = ops.split(',').next().unwrap_or_default();
        if overwrites_reg(&mnemonic, first, &aliases) {
            return generic_peb();
        }
    }
    // nothing attributable within the window: the generic dual-use claim stands
    generic_peb()
}

/// The generic manual-PEB-access classification, for loads whose consumed
/// field could not be attributed.
fn generic_peb() -> PebClass {
    // a manual PEB load is notable but dual-use, so both readings are named
    PebClass {
        severity: medium(),
        technique: T1622,
        kind: EvidenceKind::ControlFlowGated,
        label: "Direct PEB access",
        reasoning: "Loads the Process Environment Block base directly (FS:[0x30] / GS:[0x60]) \
                    instead of calling an API. Inlined anti-debug checks read \
                    BeingDebugged/NtGlobalFlag/heap flags this way to dodge hookable APIs, but \
                    manual PEB loads serve module-list (Ldr) walks for API resolution and \
                    ProcessParameters (command line) reads just as often; the field consumed \
                    was not statically resolved at this site.",
    }
}

/// Build the classification for one recognized PEB field read.
///
/// # Arguments
///
/// * `field` - The PEB field the dereference reached
/// * `insn` - The dereferencing instruction itself
/// * `rest` - The look-ahead instructions after the dereference
/// * `is64` - Whether the program is 64-bit (selects Ldr list-head offsets)
fn classify_field(
    field: PebField,
    insn: &Instruction,
    rest: &[Instruction],
    is64: bool,
) -> PebClass {
    // each field carries its own claim, severity, and validation contract
    match field {
        PebField::BeingDebugged => PebClass {
            severity: high(),
            technique: T1622,
            kind: EvidenceKind::ControlFlowGated,
            label: "PEB BeingDebugged check",
            reasoning: "Reads PEB BeingDebugged (offset +0x02) off a directly loaded PEB base — \
                        an inlined IsDebuggerPresent with no API to hook. Defeat by zeroing \
                        BeingDebugged.",
        },
        PebField::NtGlobalFlag => classify_nt_global_flag(insn, rest),
        PebField::ProcessHeap => PebClass {
            severity: medium(),
            technique: T1622,
            kind: EvidenceKind::ControlFlowGated,
            label: "PEB ProcessHeap flags check",
            reasoning: "Reads PEB ProcessHeap (offset 0x18 x86 / 0x30 x64) off a directly \
                        loaded PEB base — the heap whose Flags/ForceFlags stay set in a \
                        debugger-created process. Defeat by normalizing the heap flags.",
        },
        PebField::Ldr => classify_ldr_walk(insn, rest, is64),
        PebField::ProcessParameters => PebClass {
            severity: low(),
            technique: T1622,
            kind: EvidenceKind::ContextOnly,
            label: "PEB ProcessParameters read",
            reasoning: "Reads PEB ProcessParameters (command line, image path, environment) off \
                        a directly loaded PEB base — process-startup data, not a debugger-state \
                        field. Recorded only to account for the manual PEB load; it carries no \
                        anti-debug claim.",
        },
    }
}

/// Classify an `NtGlobalFlag` read by the mask it is tested against.
///
/// Under a debugger the loader sets the 0x70 heap-debug bits, so a 0x70 test —
/// or an unresolved mask — keeps the strong anti-debug claim. The UCRT reads
/// the same field to test `FLG_APPLICATION_VERIFIER` (0x100), both literally
/// and via the `SHR reg,8` + `TEST reg,1` idiom, and that mask demotes the hit
/// to a verifier-presence observation.
///
/// # Arguments
///
/// * `insn` - The instruction that read the `NtGlobalFlag` field
/// * `rest` - The look-ahead instructions after that read
fn classify_nt_global_flag(insn: &Instruction, rest: &[Instruction]) -> PebClass {
    // the register the UCRT idiom shifted, once a `SHR reg,8` is seen
    let mut shifted: Option<Vec<String>> = None;
    // scan the dereferencing instruction itself (it may be the TEST), then the
    // rest of the window, for the mask the flag word is tested against
    for candidate in std::iter::once(insn).chain(rest.iter()) {
        let mnemonic = candidate.mnemonic.to_ascii_uppercase();
        let ops = op_str(candidate).to_ascii_lowercase().replace(' ', "");
        let first = ops.split(',').next().unwrap_or_default().to_string();
        let imm = last_imm(candidate);
        // a TEST/AND resolves the mask directly
        if matches!(mnemonic.as_str(), "TEST" | "AND") {
            // the classic anti-debug mask: the three heap-debug bits
            if imm == Some(0x70) {
                return PebClass {
                    severity: high(),
                    technique: T1622,
                    kind: EvidenceKind::ControlFlowGated,
                    label: "PEB NtGlobalFlag check",
                    reasoning: "Reads PEB NtGlobalFlag (offset 0x68 x86 / 0xBC x64) off a \
                                directly loaded PEB base and tests the 0x70 heap-debug mask \
                                (FLG_HEAP_ENABLE_TAIL_CHECK | FLG_HEAP_ENABLE_FREE_CHECK | \
                                FLG_HEAP_VALIDATE_PARAMETERS) — the bits the loader sets under \
                                a debugger: the classic NtGlobalFlag anti-debug check. Defeat \
                                by clearing those bits.",
                };
            }
            // the Application Verifier bit, tested literally
            if imm == Some(0x100) {
                return verifier_nt_global_flag();
            }
            // the second half of the UCRT idiom: the shifted 0x100 bit lands at bit 0
            if imm == Some(0x1)
                && let Some(aliases) = &shifted
                && aliases.contains(&first)
            {
                return verifier_nt_global_flag();
            }
        }
        // the first half of the UCRT idiom: SHR reg,8 moves bit 0x100 to bit 0
        if mnemonic == "SHR" && imm == Some(0x8) {
            shifted = Some(register_aliases(&first));
        }
    }
    // the mask was not resolved: the strong claim stands, said honestly
    PebClass {
        severity: high(),
        technique: T1622,
        kind: EvidenceKind::ControlFlowGated,
        label: "PEB NtGlobalFlag check",
        reasoning: "Reads PEB NtGlobalFlag (offset 0x68 x86 / 0xBC x64) off a directly loaded \
                    PEB base; under a debugger the loader sets the 0x70 heap-debug bits here, \
                    betraying it. The tested mask was not statically resolved at this site \
                    (the UCRT tests the unrelated FLG_APPLICATION_VERIFIER bit 0x100 the same \
                    way). Defeat by clearing the flags.",
    }
}

/// The demoted classification for an `NtGlobalFlag` read testing the
/// Application Verifier bit instead of the heap-debug mask.
fn verifier_nt_global_flag() -> PebClass {
    // the verifier bit is a UCRT-style environment probe, not the debugger check
    PebClass {
        severity: medium(),
        technique: T1622,
        kind: EvidenceKind::ControlFlowGated,
        label: "PEB NtGlobalFlag read (FLG_APPLICATION_VERIFIER mask)",
        reasoning: "Reads PEB NtGlobalFlag and tests bit 0x100 (FLG_APPLICATION_VERIFIER) — an \
                    Application Verifier presence check, the same test the UCRT performs in its \
                    exit/termination paths — not the 0x70 heap-flags debugger check.",
    }
}

/// Classify a PEB `Ldr` read: the module-list walk behind manual API resolution.
///
/// Walking `Ldr` is how reflective loaders and hash-based import resolvers find
/// module bases without an import table (T1106 native-API use) — `Ldr` holds no
/// debugger-state field — so the hit is relabeled instead of claiming T1622. A
/// visible walk (the Ldr pointer dereferenced at a module-list head) earns
/// medium; a bare field read stays low until something corroborates it.
///
/// # Arguments
///
/// * `insn` - The instruction that read the `Ldr` field
/// * `rest` - The look-ahead instructions after that read
/// * `is64` - Whether the program is 64-bit (selects the list-head offsets)
fn classify_ldr_walk(insn: &Instruction, rest: &[Instruction], is64: bool) -> PebClass {
    // the per-arch PEB_LDR_DATA module-list head offsets
    let heads = if is64 {
        LDR_LIST_HEADS_X64
    } else {
        LDR_LIST_HEADS_X86
    };
    // the walk is corroborated when the register that received the Ldr pointer
    // is dereferenced at a list head within the window
    let walked = peb_load_dest(insn).is_some_and(|dest| {
        let aliases = register_aliases(&dest);
        rest.iter().any(|candidate| {
            let ops = op_str(candidate).to_ascii_lowercase().replace(' ', "");
            deref_offset(&ops, &aliases).is_some_and(|offset| heads.contains(&offset))
        })
    });
    // a corroborated walk is a meaningful capability; a bare read stays weak
    let (severity, reasoning) = if walked {
        (
            medium(),
            "Reads PEB Ldr and then a module-list head — walking the loaded-module list by \
             hand. That is the manual API-resolution idiom (reflective loaders and hash-based \
             import resolvers find kernel32/ntdll exports this way: native-API use with no \
             import table), not a debugger check — Ldr holds no debugger-state field.",
        )
    } else {
        (
            low(),
            "Reads PEB Ldr, the loaded-module list used for manual API resolution (reflective \
             loaders, hash-based import resolvers) — not a debugger-state field, so this \
             carries no anti-debug claim; whether the list is actually walked was not \
             statically resolved at this site.",
        )
    };
    PebClass {
        severity,
        technique: T1106,
        kind: EvidenceKind::ContextOnly,
        label: "PEB Ldr module-list walk (manual API resolution)",
        reasoning,
    }
}

/// The register an instruction loads its first operand into, when it is one.
///
/// Used to track the PEB base (and the `Ldr` pointer) through the look-ahead:
/// only a bare register destination can be tracked, so a push or a
/// memory-destination form yields `None` and the classifier falls back to the
/// generic claim.
///
/// # Arguments
///
/// * `insn` - The instruction whose destination to extract
fn peb_load_dest(insn: &Instruction) -> Option<String> {
    // the destination is the first rendered operand, lowercased
    let ops = op_str(insn).to_ascii_lowercase();
    let first = ops.split(',').next().unwrap_or_default().trim().to_string();
    // only a bare register token can be tracked through later instructions
    (!first.is_empty() && first.chars().all(|c| c.is_ascii_alphanumeric())).then_some(first)
}

/// Widen a register token to every name in its family.
///
/// # Arguments
///
/// * `token` - The lowercased register token to widen
fn register_aliases(token: &str) -> Vec<String> {
    // a family member widens to the whole family; anything else stands alone
    for family in REG_FAMILIES {
        if family.contains(&token) {
            return family.iter().map(|name| (*name).to_string()).collect();
        }
    }
    vec![token.to_string()]
}

/// The constant displacement of a `[<tracked reg>+0x..]` dereference, when the
/// squeezed operand text contains one.
///
/// # Arguments
///
/// * `ops` - The instruction's lowercased operand text with spaces removed
/// * `aliases` - The tracked register family's names
fn deref_offset(ops: &str, aliases: &[String]) -> Option<u64> {
    // try each alias's bracketed base+displacement form
    for alias in aliases {
        let pattern = format!("[{alias}+0x");
        let Some(at) = ops.find(&pattern) else {
            continue;
        };
        // the displacement runs from the pattern to the closing bracket; a
        // scaled/multi-term expression fails the hex parse and is skipped
        let rest = &ops[at + pattern.len()..];
        let Some(end) = rest.find(']') else {
            continue;
        };
        if let Ok(offset) = u64::from_str_radix(&rest[..end], 16) {
            return Some(offset);
        }
    }
    None
}

/// Whether an instruction replaces the tracked register's value.
///
/// Compares, bit-tests, and pushes only read their first operand; any other
/// instruction whose destination names a tracked alias is treated as a write,
/// conservatively ending the look-ahead.
///
/// # Arguments
///
/// * `mnemonic` - The uppercased instruction mnemonic
/// * `first` - The instruction's first operand (lowercased, spaces removed)
/// * `aliases` - The tracked register family's names
fn overwrites_reg(mnemonic: &str, first: &str, aliases: &[String]) -> bool {
    // only a bare tracked register in destination position can be replaced
    !matches!(mnemonic, "CMP" | "TEST" | "BT" | "PUSH")
        && aliases.iter().any(|alias| alias == first)
}

/// The literal immediate in an instruction's last operand position, when any.
///
/// # Arguments
///
/// * `insn` - The instruction whose trailing operand to parse
fn last_imm(insn: &Instruction) -> Option<u64> {
    // the staged value is the final comma-separated operand
    op_str(insn)
        .to_ascii_lowercase()
        .split(',')
        .next_back()
        .map(str::trim)
        .and_then(parse_imm)
}

/// Whether a scan hit is the generic, unclassified PEB-load tell.
///
/// These count against [`GENERIC_PEB_CAP`]; classified field reads
/// (BeingDebugged/NtGlobalFlag/ProcessHeap/Ldr/ProcessParameters) and the
/// other opcode tells are exempt and always seed individually.
///
/// # Arguments
///
/// * `hit` - The grouped scan hit to judge
fn is_generic_peb(hit: &DetectionHit) -> bool {
    // the generic fallback's content class, as the PEB arm renders it
    content_class(&hit.content) == "Direct PEB access"
}

/// Build the raw-ptrace-syscall hit for one site, shaped by the recovered request.
///
/// # Arguments
///
/// * `addr` - The `SYSCALL` instruction's address
/// * `function` - The containing function's entry address
/// * `request` - The recovered first argument (`edi`/`rdi`), when staging was visible
fn ptrace_syscall_hit(addr: u64, function: u64, request: Option<u64>) -> DetectionHit {
    // the request number decides which claim the evidence supports
    let (severity, content, reasoning) = match request {
        Some(0) => (
            high(),
            format!("ptrace(PTRACE_TRACEME) syscall @ {}", fmt_addr(addr)),
            "Invokes the ptrace syscall (number 101) directly with request 0 (PTRACE_TRACEME) \
             — the self-attach anti-debug trick: a process may be traced only once, so the \
             call fails when a debugger is already attached. With no ptrace import \
             (static/stripped ELF) the raw syscall is the only evidence. Defeat by stubbing \
             ptrace to succeed."
                .to_string(),
        ),
        Some(request) => (
            medium(),
            format!("ptrace syscall (request {request:#x}) @ {}", fmt_addr(addr)),
            format!(
                "Invokes the ptrace syscall (number 101) directly with request {request:#x} — \
                 not PTRACE_TRACEME (0), so this is not the self-attach anti-debug trick: \
                 requests like ATTACH/PEEKDATA/POKEDATA trace another process instead \
                 (offensive debugger-style tampering with other processes)."
            ),
        ),
        None => (
            medium(),
            format!("ptrace syscall @ {}", fmt_addr(addr)),
            "Invokes the ptrace syscall (number 101) directly; the request was not statically \
             resolved — this may be the PTRACE_TRACEME self-attach anti-debug trick or \
             offensive tracing of another process (a statically linked glibc ptrace() wrapper \
             stages its caller's request here)."
                .to_string(),
        ),
    };
    // raw syscalls are judged like API calls: the result gating a branch confirms
    DetectionHit::new(T1622, addr, Some(function), severity, content, reasoning)
        .kind(EvidenceKind::ApiResultCheck)
}

/// Whether a hit is a `Get`/`SetThreadContext` call-site hit from this catalog.
///
/// # Arguments
///
/// * `hit` - The hit to test
fn is_thread_context_hit(hit: &DetectionHit) -> bool {
    // the builder stamps the normalized catalog key onto every call-site hit
    matches!(hit.api, Some("GetThreadContext" | "SetThreadContext"))
}

/// Whether a literal has the shape of a `CONTEXT.ContextFlags` value.
///
/// `ContextFlags` values carry an architecture tag (`CONTEXT_i386` `0x0001_0000`
/// or `CONTEXT_AMD64` `0x0010_0000`) plus low option bits (control/integer/
/// segments/floating-point/debug-registers/extended/xstate), so a tag with at
/// least one option bit and nothing else set is recognized as a flags
/// initialization.
///
/// # Arguments
///
/// * `value` - The recovered literal to test
fn context_flags_shape(value: u64) -> bool {
    // split the arch tag from the option bits and demand exactly one known tag
    let tag = value & !0x7F;
    (tag == 0x0001_0000 || tag == 0x0010_0000) && (value & 0x7F) != 0
}

/// Recover the `ContextFlags` literal staged near a hit's call sites.
///
/// Scans a bounded window before each site for an instruction whose trailing
/// operand is a `ContextFlags`-shaped immediate (the `MOV [slot], imm` /
/// `MOV reg, imm` initialization forms). A value including
/// `CONTEXT_DEBUG_REGISTERS` wins over any other recovery, mirroring the
/// validator's in-set-first rule.
///
/// # Arguments
///
/// * `insns` - The containing function's disassembly
/// * `sites` - The hit's call sites
fn recover_context_flags(insns: &[Instruction], sites: &[u64]) -> Option<u64> {
    // keep the first recovery as the fallback; a debug-bit recovery returns at once
    let mut seen = None;
    for site in sites {
        // skip sites that cannot be located in the listing
        let Some(idx) = insns
            .iter()
            .position(|insn| parse_addr(&insn.address) == Some(*site))
        else {
            continue;
        };
        // walk the bounded window before the call for a flags-shaped literal
        let start = idx.saturating_sub(CONTEXT_FLAGS_WINDOW);
        for insn in &insns[start..idx] {
            // only a ContextFlags-shaped immediate marks the initialization
            let Some(value) = last_imm(insn).filter(|value| context_flags_shape(*value)) else {
                continue;
            };
            // the debug bit decides the claim, so it short-circuits the search
            if value & CONTEXT_DEBUG_REGISTERS != 0 {
                return Some(value);
            }
            seen = seen.or(Some(value));
        }
    }
    seen
}

/// Apply a recovered `ContextFlags` value to a thread-context hit, in place.
///
/// A value including `CONTEXT_DEBUG_REGISTERS` names the DR access and raises
/// the hit to the strong tier; a value without it rewrites the claim around
/// what is actually accessed and weakens the hit one step; no recovery leaves
/// the conditional catalog wording standing.
///
/// # Arguments
///
/// * `hit` - The `Get`/`SetThreadContext` hit to refine
/// * `flags` - The recovered `ContextFlags` literal, when one was found
fn apply_context_flags(hit: &mut DetectionHit, flags: Option<u64>) {
    // the API name for the rewritten claims
    let api = hit.api.unwrap_or("the thread-context API");
    match flags {
        Some(value) if value & CONTEXT_DEBUG_REGISTERS != 0 => {
            // the debug-registers bit is requested: name it and raise the hit
            // to the strong tier the verified anti-debug use earns (writing to
            // a String cannot fail, so the formatter result is discarded)
            let _ = write!(
                hit.content,
                " — ContextFlags={value:#x} (includes CONTEXT_DEBUG_REGISTERS)"
            );
            hit.reasoning = format!(
                "{api} is called with ContextFlags {value:#x}, which includes \
                 CONTEXT_DEBUG_REGISTERS (0x10): the DR0-DR7 hardware-breakpoint registers an \
                 analyst sets are accessed here — the anti-debug use, confirmed at the flags \
                 level. Defeat by hiding/restoring the debug registers around the call."
            );
            // a verified debug-registers request earns the strong tier outright
            let (confidence, suspicion) = high();
            hit.confidence = confidence;
            hit.suspicion = suspicion;
        }
        Some(value) => {
            // flags recovered without the debug bit refute the DR claim: say
            // what is actually accessed and weaken the hit one step (writing
            // to a String cannot fail, so the formatter result is discarded)
            let _ = write!(
                hit.content,
                " — ContextFlags={value:#x} (CONTEXT_DEBUG_REGISTERS absent)"
            );
            hit.reasoning = format!(
                "{api} is called with ContextFlags {value:#x} — the CONTEXT_DEBUG_REGISTERS \
                 bit (0x10) is not set, so the DR0-DR7 hardware-breakpoint registers are \
                 neither read nor written. This shape matches hook-engine commits, \
                 thread-hijack/EIP-migration code, and pthread-cancel implementations rather \
                 than anti-debug tampering."
            );
            hit.weaken();
        }
        None => {}
    }
}

/// Name what `Get`/`SetThreadContext` access when the `ContextFlags` value is
/// statically visible near a call site.
///
/// The thread-context catalog entries are dual-use: only a `CONTEXT` whose
/// `ContextFlags` include `CONTEXT_DEBUG_REGISTERS` (0x10) touches DR0-DR7.
/// The flags live in a struct field rather than an argument register, so the
/// central argument-recovery pass cannot see them; this pass scans the cached
/// disassembly before each call site for a `ContextFlags`-shaped immediate — the
/// statically recoverable initialization shape — and applies what it proves
/// via [`apply_context_flags`]. Any failure fetching the disassembly degrades
/// to a no-op, because seeding is best-effort.
///
/// # Arguments
///
/// * `ctx` - The analysis context (cached disassembly)
/// * `hits` - The merged API hits to refine in place
#[tracing::instrument(name = "DebuggerEvasion::refine_thread_context_hits", skip_all)]
async fn refine_thread_context_hits(ctx: &AnalysisContext, hits: &mut [DetectionHit]) {
    // skip the disassembly fetch when no thread-context call-site hit exists
    if !hits.iter().any(is_thread_context_hit) {
        return;
    }
    // fetch the (cached) whole-program disassembly once
    let all = match ctx.all_function_disassembly().await {
        Ok(all) => all,
        Err(error) => {
            // the conditional catalog wording stands when nothing can be read
            tracing::warn!(
                ?error,
                "thread-context refinement skipped: disassembly unavailable"
            );
            return;
        }
    };
    // index the listings by function entry for the per-hit lookups
    let listings: HashMap<u64, &[Instruction]> = all
        .iter()
        .map(|(entry, insns)| (*entry, insns.as_slice()))
        .collect();
    // refine every thread-context hit that sits in a disassembled function
    for hit in hits.iter_mut().filter(|hit| is_thread_context_hit(hit)) {
        // only hits inside a known, disassembled function can be checked
        let Some(entry) = hit.function else {
            continue;
        };
        let Some(insns) = listings.get(&entry).copied() else {
            continue;
        };
        // every call site backing the hit (the primary stands alone otherwise)
        let sites: Vec<u64> = if hit.sites.is_empty() {
            vec![hit.address]
        } else {
            hit.sites.clone()
        };
        // recover the staged ContextFlags and apply what it proves
        let flags = recover_context_flags(insns, &sites);
        apply_context_flags(hit, flags);
    }
}

/// Emit `OutputDebugString` hits only where the calling function also touches
/// the last-error APIs.
///
/// The bare call is plain logging in essentially every MSVC binary (and an
/// import-only sighting says nothing), so the catalog entry was retired in
/// favor of this gated pass: resolve the `OutputDebugString` call sites, resolve
/// the `GetLastError`/`SetLastError` call sites, and seed one weak hit per
/// function calling both.
///
/// # Arguments
///
/// * `ctx` - The analysis context
///
/// # Errors
///
/// Returns an error if a bridge xref query fails.
#[tracing::instrument(
    name = "DebuggerEvasion::output_debug_string_hits",
    skip_all,
    err(Debug)
)]
async fn output_debug_string_hits(ctx: &AnalysisContext) -> Result<Vec<DetectionHit>> {
    // resolve every OutputDebugString call site (A/W variants normalize together)
    let mut ods_sites: Vec<CallSite> = Vec::new();
    for import in &ctx.imports {
        if normalize(&import.name) == "OutputDebugString" {
            let sites = ctx
                .resolve_call_sites(&import.address)
                .await
                .with_context(|| format!("resolving call sites of import '{}'", import.name))?;
            ods_sites.extend(sites);
        }
    }
    // an uncalled or absent OutputDebugString import carries no probe signal
    if ods_sites.is_empty() {
        return Ok(Vec::new());
    }
    // the functions that also call a last-error API complete the probe shape
    let mut probe_fns: HashSet<u64> = HashSet::new();
    for import in &ctx.imports {
        if ODS_PROBE_APIS.contains(&normalize(&import.name).as_str()) {
            let sites = ctx
                .resolve_call_sites(&import.address)
                .await
                .with_context(|| format!("resolving call sites of import '{}'", import.name))?;
            probe_fns.extend(sites.iter().filter_map(|site| site.function));
        }
    }
    // group the ODS sites per calling function (one behavior per function)
    let mut by_fn: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    for site in ods_sites {
        if let Some(function) = site.function {
            by_fn.entry(function).or_default().push(site.site);
        }
    }
    // keep only the functions whose call set completes the probe shape
    Ok(ods_probe_hits(by_fn, &probe_fns))
}

/// Build the gated `OutputDebugString` hits from the grouped call sites.
///
/// # Arguments
///
/// * `by_fn` - The `OutputDebugString` call sites grouped per calling function
/// * `probe_fns` - The functions that also call `GetLastError`/`SetLastError`
fn ods_probe_hits(by_fn: BTreeMap<u64, Vec<u64>>, probe_fns: &HashSet<u64>) -> Vec<DetectionHit> {
    // one weak hit per function matching the probe shape
    let mut hits = Vec::new();
    for (function, mut sites) in by_fn {
        // without the last-error co-location the call is plain logging: emit nothing
        if !probe_fns.contains(&function) {
            continue;
        }
        // sort + dedupe the call addresses for stable, clean output
        sites.sort_unstable();
        sites.dedup();
        // render the call list the way the shared API hits do
        let addrs = render_sites(&sites);
        let content = if sites.len() > 1 {
            format!(
                "OutputDebugString last-error probe shape — {} calls @ {addrs}",
                sites.len()
            )
        } else {
            format!("OutputDebugString last-error probe shape @ {addrs}")
        };
        // assemble the hit; attach the full site list only when there is more than one
        let mut hit = DetectionHit::new(
            T1622,
            sites.first().copied().unwrap_or(0),
            Some(function),
            low(),
            content,
            ODS_PROBE_REASONING,
        )
        .kind(EvidenceKind::ContextOnly)
        .api("OutputDebugString");
        if sites.len() > 1 {
            hit = hit.sites(sites);
        }
        hits.push(hit);
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic instruction for the fixtures.
    ///
    /// # Arguments
    ///
    /// * `address` - The instruction address
    /// * `mnemonic` - The instruction mnemonic
    /// * `operands` - The rendered operand list
    fn insn(address: u64, mnemonic: &str, operands: &[&str]) -> Instruction {
        Instruction {
            // the bridge emits instruction addresses as bare, unprefixed hex
            address: format!("{address:x}"),
            mnemonic: mnemonic.to_owned(),
            operands: operands.iter().map(|op| (*op).to_owned()).collect(),
            bytes: None,
            length: None,
            flow_type: None,
        }
    }

    /// Build a thread-context API hit the way `build_api_hit` seeds the catalog.
    ///
    /// # Arguments
    ///
    /// * `api` - The normalized catalog API name
    /// * `function` - The containing function entry
    /// * `sites` - The call sites inside that function
    fn thread_context_hit(api: &'static str, function: u64, sites: &[u64]) -> DetectionHit {
        // seeded at the catalog's medium with the key stamped, like the builder
        let primary = sites.first().copied().unwrap_or(0);
        let mut hit = DetectionHit::new(
            T1622,
            primary,
            Some(function),
            medium(),
            format!("{api} call @ {primary:#x}"),
            "catalog reasoning".to_string(),
        )
        .kind(EvidenceKind::ApiResultCheck)
        .api(api);
        // attach the full site list only when there is more than one, as the builder does
        if sites.len() > 1 {
            hit = hit.sites(sites.to_vec());
        }
        hit
    }

    /// Build one generic PEB-load scan hit at an explicit site.
    ///
    /// # Arguments
    ///
    /// * `function` - The containing function entry
    /// * `site` - The PEB-load site address
    fn generic_hit_at(function: u64, site: u64) -> DetectionHit {
        DetectionHit::new(
            T1622,
            site,
            Some(function),
            medium(),
            format!("Direct PEB access @ {}", fmt_addr(site)),
            "generic".to_string(),
        )
        .kind(EvidenceKind::ControlFlowGated)
    }

    /// A `ReflectiveLoader` shape (`MOV R9,[RAX+0x18]` after a
    /// `GS:[0x60]` load) classifies as the Ldr walk, never as `ProcessHeap`.
    #[test]
    fn x64_ldr_read_relabels_as_api_resolution() {
        // the canonical x64 module-list walk: Ldr, then InLoadOrderModuleList
        let following = vec![
            insn(0x1000, "MOV", &["R9", "qword ptr [RAX + 0x18]"]),
            insn(0x1008, "MOV", &["R9", "qword ptr [R9 + 0x10]"]),
        ];
        let class = classify_peb(Some("rax"), &following, true);
        // relabeled: the truer API-resolution claim under T1106
        assert_eq!(
            class.label,
            "PEB Ldr module-list walk (manual API resolution)"
        );
        assert_eq!(class.technique.id, "T1106");
        assert_eq!(class.kind, EvidenceKind::ContextOnly);
        // the visible walk corroborates the claim up to medium
        assert!(matches!(class.severity.0, Confidence::Unsure));
        assert_eq!(class.severity.1, medium().1);
    }

    /// A bare x64 Ldr read with no visible walk seeds low/`ContextOnly`.
    #[test]
    fn x64_ldr_read_without_walk_stays_low() {
        // the Ldr pointer is fetched but never dereferenced at a list head
        let following = vec![insn(0x1000, "MOV", &["R9", "qword ptr [RAX + 0x18]"])];
        let class = classify_peb(Some("rax"), &following, true);
        assert_eq!(
            class.label,
            "PEB Ldr module-list walk (manual API resolution)"
        );
        assert!(matches!(class.severity.0, Confidence::Untrusted));
        assert_eq!(class.severity.1, low().1);
        assert_eq!(class.kind, EvidenceKind::ContextOnly);
    }

    /// A stack spill after the PEB load (the page's `[rsp+0x18]` evidence) no
    /// longer classifies as a `ProcessHeap` read.
    #[test]
    fn x64_stack_spill_no_longer_misclassifies_processheap() {
        // the spill dereferences RSP, not the PEB register, so nothing classifies
        let following = vec![
            insn(0x1000, "MOV", &["qword ptr [RSP + 0x18]", "RCX"]),
            insn(0x1008, "TEST", &["RAX", "RAX"]),
        ];
        let class = classify_peb(Some("rax"), &following, true);
        assert_eq!(class.label, "Direct PEB access");
        assert!(matches!(class.severity.0, Confidence::Unsure));
        // the generic claim names both readings instead of asserting anti-debug
        assert!(
            class.reasoning.contains("API resolution"),
            "{}",
            class.reasoning
        );
    }

    /// The classic x86 `BeingDebugged` read keeps its high-severity label.
    #[test]
    fn x86_being_debugged_check_keeps_high() {
        // the inlined IsDebuggerPresent: FS:[0x30] into EAX, then PEB+0x02
        let following = vec![insn(0x2000, "MOVZX", &["ECX", "byte ptr [EAX + 0x2]"])];
        let class = classify_peb(Some("eax"), &following, false);
        assert_eq!(class.label, "PEB BeingDebugged check");
        assert!(matches!(class.severity.0, Confidence::Likely));
        assert_eq!(class.severity.1, high().1);
        assert_eq!(class.kind, EvidenceKind::ControlFlowGated);
        assert_eq!(class.technique.id, "T1622");
    }

    /// An x86 shape (`FS:[0x30]` → `[+0xC]` → a later `[+0x18]`)
    /// classifies the Ldr read instead of latching the `ProcessHeap` offset.
    #[test]
    fn x86_ldr_chain_never_latches_the_later_processheap_offset() {
        // [eax+0xc] is Ldr on x86; the subsequent [eax+0x18] is the LDR entry's
        // DllBase, not PEB ProcessHeap — the old substring latch's exact FP
        let following = vec![
            insn(0x40_34f3, "MOV", &["EAX", "dword ptr [EAX + 0xc]"]),
            insn(0x40_34f9, "MOV", &["ESI", "dword ptr [EAX + 0x18]"]),
        ];
        let class = classify_peb(Some("eax"), &following, false);
        assert_eq!(
            class.label,
            "PEB Ldr module-list walk (manual API resolution)"
        );
        assert_eq!(class.technique.id, "T1106");
    }

    /// A genuine x86 +0x18 read keeps the `ProcessHeap` label and severity.
    #[test]
    fn x86_process_heap_read_keeps_label_and_severity() {
        // on x86 (unlike x64) PEB+0x18 really is ProcessHeap
        let following = vec![insn(0x3000, "MOV", &["EAX", "dword ptr [EAX + 0x18]"])];
        let class = classify_peb(Some("eax"), &following, false);
        assert_eq!(class.label, "PEB ProcessHeap flags check");
        assert!(matches!(class.severity.0, Confidence::Unsure));
        assert_eq!(class.severity.1, medium().1);
    }

    /// The x64 table maps +0x30 to `ProcessHeap` and +0x20 to `ProcessParameters`.
    #[test]
    fn x64_offsets_classify_per_arch() {
        // +0x30 is ProcessHeap on x64 (not +0x18, which is Ldr)
        let heap = vec![insn(0x5000, "MOV", &["RCX", "qword ptr [RAX + 0x30]"])];
        assert_eq!(
            classify_peb(Some("rax"), &heap, true).label,
            "PEB ProcessHeap flags check"
        );
        // +0x20 is ProcessParameters: startup data, no anti-debug claim
        let params = vec![insn(0x5000, "MOV", &["RDX", "qword ptr [RAX + 0x20]"])];
        let class = classify_peb(Some("rax"), &params, true);
        assert_eq!(class.label, "PEB ProcessParameters read");
        assert!(matches!(class.severity.0, Confidence::Untrusted));
        assert!(
            class.reasoning.contains("no anti-debug claim"),
            "{}",
            class.reasoning
        );
    }

    /// `NtGlobalFlag` keeps `high()` for the 0x70 heap mask and an unresolved mask.
    #[test]
    fn ntglobalflag_heap_mask_and_unresolved_stay_high() {
        // the classic 0x70 test right after the x64 +0xBC read
        let masked = vec![
            insn(0x6000, "MOV", &["EAX", "dword ptr [RAX + 0xbc]"]),
            insn(0x6006, "TEST", &["EAX", "0x70"]),
        ];
        let class = classify_peb(Some("rax"), &masked, true);
        assert_eq!(class.label, "PEB NtGlobalFlag check");
        assert!(matches!(class.severity.0, Confidence::Likely));
        assert!(class.reasoning.contains("0x70"), "{}", class.reasoning);
        // no visible mask keeps the strong claim with the honest caveat
        let unresolved = vec![insn(0x6000, "MOV", &["EAX", "dword ptr [RAX + 0xbc]"])];
        let class = classify_peb(Some("rax"), &unresolved, true);
        assert!(matches!(class.severity.0, Confidence::Likely));
        assert!(
            class.reasoning.contains("not statically resolved"),
            "{}",
            class.reasoning
        );
    }

    /// The `FLG_APPLICATION_VERIFIER` masks — the literal 0x100 test and the UCRT
    /// `SHR reg,8` + `TEST reg,1` idiom — demote the `NtGlobalFlag` claim.
    #[test]
    fn ntglobalflag_verifier_masks_demote_to_medium() {
        // the literal form the exit_or_terminate_process dispute quoted
        let literal = vec![
            insn(0x7000, "MOV", &["EAX", "dword ptr [EAX + 0x68]"]),
            insn(0x7006, "TEST", &["EAX", "0x100"]),
        ];
        let class = classify_peb(Some("eax"), &literal, false);
        assert_eq!(
            class.label,
            "PEB NtGlobalFlag read (FLG_APPLICATION_VERIFIER mask)"
        );
        assert!(matches!(class.severity.0, Confidence::Unsure));
        // the UCRT idiom: shift bit 0x100 down, test bit 0
        let idiom = vec![
            insn(0x7100, "MOV", &["EAX", "dword ptr [EAX + 0x68]"]),
            insn(0x7106, "SHR", &["EAX", "0x8"]),
            insn(0x7109, "TEST", &["AL", "0x1"]),
        ];
        let class = classify_peb(Some("eax"), &idiom, false);
        assert_eq!(
            class.label,
            "PEB NtGlobalFlag read (FLG_APPLICATION_VERIFIER mask)"
        );
        assert!(matches!(class.severity.0, Confidence::Unsure));
    }

    /// Tracking requires the loaded register: a replaced destination, a call,
    /// or no destination at all falls back to the generic dual-use claim.
    #[test]
    fn peb_tracking_requires_the_loaded_register() {
        // no register destination (e.g. a push of FS:[0x30]) → generic
        let following = vec![insn(0x8000, "MOV", &["ECX", "dword ptr [EAX + 0x2]"])];
        assert_eq!(
            classify_peb(None, &following, false).label,
            "Direct PEB access"
        );
        // the tracked register is replaced before any field read → generic
        let replaced = vec![
            insn(0x8000, "MOV", &["RAX", "RCX"]),
            insn(0x8006, "MOV", &["RDX", "qword ptr [RAX + 0x18]"]),
        ];
        assert_eq!(
            classify_peb(Some("rax"), &replaced, true).label,
            "Direct PEB access"
        );
        // a call ends what the look-ahead can attribute (pointer passed elsewhere)
        let called = vec![
            insn(0x8000, "CALL", &["0x401000"]),
            insn(0x8006, "MOVZX", &["ECX", "byte ptr [RAX + 0x2]"]),
        ];
        assert_eq!(
            classify_peb(Some("rax"), &called, true).label,
            "Direct PEB access"
        );
    }

    /// The destination extractor tracks only bare register loads.
    #[test]
    fn peb_load_dest_extracts_only_bare_registers() {
        // the ordinary x64 PEB load names its destination register
        let load = insn(0x1000, "MOV", &["RAX", "qword ptr GS:[0x60]"]);
        assert_eq!(peb_load_dest(&load).as_deref(), Some("rax"));
        // a memory destination (or a push) cannot be tracked
        let push = insn(0x1000, "PUSH", &["dword ptr FS:[0x30]"]);
        assert_eq!(peb_load_dest(&push), None);
    }

    /// Generic PEB tells group per function and cap at eight per program, with
    /// classified field reads exempt — the per-instruction flood killer.
    #[test]
    fn generic_peb_hits_group_and_cap() {
        // ten functions' worth of generic tells, two of them in one function,
        // plus one classified BeingDebugged read that must stay individual
        let mut hits = Vec::new();
        for i in 0..10u64 {
            let base = 0x40_0000 + i * 0x100;
            hits.push(generic_hit_at(base, base));
            // the first function repeats the tell at a second site
            if i == 0 {
                hits.push(generic_hit_at(base, base + 0x10));
            }
        }
        hits.push(DetectionHit::new(
            T1622,
            0x50_0000,
            Some(0x50_0000),
            high(),
            format!("PEB BeingDebugged check @ {}", fmt_addr(0x50_0000)),
            "classified".to_string(),
        ));
        let grouped = group_scan_hits(hits, GENERIC_PEB_CAP, is_generic_peb);
        // the classified read survives individually (cap-exempt)
        assert!(
            grouped
                .iter()
                .any(|hit| hit.content.starts_with("PEB BeingDebugged check"))
        );
        // exactly the cap's worth of generic tells stay individual findings
        let generic: Vec<_> = grouped.iter().filter(|hit| is_generic_peb(hit)).collect();
        assert_eq!(generic.len(), GENERIC_PEB_CAP);
        // the two same-function tells merged into one hit carrying both sites
        let merged = generic
            .iter()
            .find(|hit| hit.function == Some(0x40_0000))
            .expect("the two-site function survives the cap");
        assert_eq!(merged.sites, vec![0x40_0000, 0x40_0010]);
        // the overflow collapsed into one file-level summary, not a silent drop
        assert!(
            grouped
                .iter()
                .any(|hit| hit.content.contains("generic tells elided")),
            "summary hit missing"
        );
    }

    /// Two INT 0x2D tells in one function merge into one hit with both sites.
    #[test]
    fn int_2d_hits_group_per_function() {
        // the same interrupt twice in one function is one behavior repeated
        let hits = vec![
            DetectionHit::new(
                T1622,
                0x9000,
                Some(0x9000),
                medium(),
                format!("INT 0x2D anti-debug interrupt @ {}", fmt_addr(0x9000)),
                "r".to_string(),
            )
            .kind(EvidenceKind::ControlFlowGated),
            DetectionHit::new(
                T1622,
                0x9008,
                Some(0x9000),
                medium(),
                format!("INT 0x2D anti-debug interrupt @ {}", fmt_addr(0x9008)),
                "r".to_string(),
            )
            .kind(EvidenceKind::ControlFlowGated),
        ];
        let grouped = group_scan_hits(hits, GENERIC_PEB_CAP, is_generic_peb);
        assert_eq!(grouped.len(), 1);
        assert_eq!(grouped[0].sites, vec![0x9000, 0x9008]);
        // interrupt tells are exempt from the generic PEB cap
        assert!(!is_generic_peb(&grouped[0]));
    }

    /// A zeroed request keeps the raw ptrace syscall at the TRACEME claim.
    #[test]
    fn ptrace_traceme_request_keeps_high() {
        // the conventional staging zeroes edi right before the syscall
        let hit = ptrace_syscall_hit(0xa000, 0xa000, Some(0));
        assert!(matches!(hit.confidence, Confidence::Likely));
        assert_eq!(hit.suspicion, high().1);
        assert!(hit.content.contains("PTRACE_TRACEME"), "{}", hit.content);
    }

    /// A foreign or unresolved request softens the raw-syscall claim honestly
    /// (the glibc-wrapper shape).
    #[test]
    fn ptrace_foreign_or_unresolved_request_softens() {
        // PTRACE_ATTACH (0x10): tracing another process, said as such
        let foreign = ptrace_syscall_hit(0xa000, 0xa000, Some(0x10));
        assert!(matches!(foreign.confidence, Confidence::Unsure));
        assert!(foreign.content.contains("0x10"), "{}", foreign.content);
        assert!(
            foreign.reasoning.contains("another process"),
            "{}",
            foreign.reasoning
        );
        assert!(
            !foreign.content.contains("PTRACE_TRACEME"),
            "{}",
            foreign.content
        );
        // an unresolved request keeps both hypotheses open at medium
        let unresolved = ptrace_syscall_hit(0xa000, 0xa000, None);
        assert!(matches!(unresolved.confidence, Confidence::Unsure));
        assert!(
            unresolved.reasoning.contains("not statically resolved"),
            "{}",
            unresolved.reasoning
        );
    }

    /// Recovered `ContextFlags` without the debug bit demote the thread-context
    /// claim (the disputed 0x100001 and 0x10001 values).
    #[test]
    fn context_flags_without_debug_bit_demote() {
        // the x64 CONTEXT_AMD64|CONTEXT_CONTROL commit shape
        let insns = vec![
            insn(0xb000, "MOV", &["dword ptr [RSP + 0x30]", "0x100001"]),
            insn(0xb008, "LEA", &["RDX", "[RSP + 0x30]"]),
            insn(0xb010, "CALL", &["qword ptr [0x5000]"]),
        ];
        let mut hit = thread_context_hit("SetThreadContext", 0xb000, &[0xb010]);
        apply_context_flags(&mut hit, recover_context_flags(&insns, &[0xb010]));
        assert!(matches!(hit.confidence, Confidence::Untrusted));
        assert_eq!(hit.suspicion, 25);
        assert!(
            hit.content.contains("ContextFlags=0x100001"),
            "{}",
            hit.content
        );
        assert!(
            hit.reasoning.contains("neither read nor written"),
            "{}",
            hit.reasoning
        );
        // the winpthreads CONTEXT_i386|CONTEXT_CONTROL value demotes the same way
        let x86 = vec![
            insn(0xc000, "MOV", &["dword ptr [EBP + -0x2cc]", "0x10001"]),
            insn(0xc008, "CALL", &["dword ptr [0x5000]"]),
        ];
        let mut hit = thread_context_hit("GetThreadContext", 0xc000, &[0xc008]);
        apply_context_flags(&mut hit, recover_context_flags(&x86, &[0xc008]));
        assert!(matches!(hit.confidence, Confidence::Untrusted));
        assert!(hit.content.contains("0x10001"), "{}", hit.content);
    }

    /// `ContextFlags` including `CONTEXT_DEBUG_REGISTERS` name the DR access and
    /// raise the hit to the strong tier.
    #[test]
    fn context_flags_with_debug_bit_confirm_dr_access() {
        // CONTEXT_i386 | CONTEXT_DEBUG_REGISTERS: the hardware-breakpoint read
        let insns = vec![
            insn(0xd000, "MOV", &["dword ptr [ESP + 0x8]", "0x10010"]),
            insn(0xd008, "CALL", &["dword ptr [0x5000]"]),
        ];
        let mut hit = thread_context_hit("GetThreadContext", 0xd000, &[0xd008]);
        apply_context_flags(&mut hit, recover_context_flags(&insns, &[0xd008]));
        assert!(matches!(hit.confidence, Confidence::Likely));
        assert_eq!(hit.suspicion, high().1);
        assert!(
            hit.content.contains("CONTEXT_DEBUG_REGISTERS"),
            "{}",
            hit.content
        );
    }

    /// Unrecovered `ContextFlags` leave the conditional catalog claim untouched.
    #[test]
    fn context_flags_unrecovered_leave_the_hit() {
        // nothing in the window carries a flags-shaped literal
        let insns = vec![
            insn(0xe000, "MOV", &["EDX", "EBX"]),
            insn(0xe002, "CALL", &["dword ptr [0x5000]"]),
        ];
        let mut hit = thread_context_hit("GetThreadContext", 0xe000, &[0xe002]);
        apply_context_flags(&mut hit, recover_context_flags(&insns, &[0xe002]));
        assert!(matches!(hit.confidence, Confidence::Unsure));
        assert_eq!(hit.suspicion, medium().1);
        assert_eq!(hit.reasoning, "catalog reasoning");
    }

    /// The ODS hit requires the last-error co-location and groups per function.
    #[test]
    fn ods_probe_requires_last_error_colocation() {
        // two ODS-calling functions; only one also calls GetLastError
        let mut by_fn = BTreeMap::new();
        by_fn.insert(0x1000u64, vec![0x1010u64]);
        by_fn.insert(0x2000u64, vec![0x2010u64, 0x2008u64]);
        let probe: HashSet<u64> = HashSet::from([0x2000]);
        let hits = ods_probe_hits(by_fn, &probe);
        // the logging-only function seeds nothing
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit.function, Some(0x2000));
        assert!(matches!(hit.confidence, Confidence::Untrusted));
        assert_eq!(hit.suspicion, low().1);
        // both sites ride along, sorted, in one grouped hit
        assert_eq!(hit.sites, vec![0x2008, 0x2010]);
        assert!(hit.content.contains("2 calls"), "{}", hit.content);
        assert_eq!(hit.kind, EvidenceKind::ContextOnly);
    }

    /// The retired catalog entries are gone and the dual-use wording is conditional.
    #[test]
    fn catalog_retirements_and_conditional_wording() {
        // NtClose is retired outright; OutputDebugString moved to the gated pass
        assert!(!APIS.iter().any(|spec| spec.key == "NtClose"));
        assert!(!APIS.iter().any(|spec| spec.key == "OutputDebugString"));
        // the thread-context entries state the ContextFlags condition instead
        // of asserting the DR access
        let get = APIS
            .iter()
            .find(|spec| spec.key == "GetThreadContext")
            .expect("catalogued");
        assert!(
            get.reasoning.contains("CONTEXT_DEBUG_REGISTERS"),
            "{}",
            get.reasoning
        );
        // the Nt* information entries state the class condition
        let query = APIS
            .iter()
            .find(|spec| spec.key == "NtQueryInformationProcess")
            .expect("catalogued");
        assert!(
            query.reasoning.contains("ProcessBasicInformation"),
            "{}",
            query.reasoning
        );
    }
}
