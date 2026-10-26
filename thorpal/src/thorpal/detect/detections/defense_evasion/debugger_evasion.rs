//! Debugger Evasion detection (MITRE ATT&CK **T1622**).
//!
//! Detects the classic ways malware notices — and hides from — a debugger so
//! that an analyst single-stepping the sample never observes its real behavior:
//!
//! 1. **Anti-debug API calls** — imported Windows/Nt APIs whose whole purpose is
//!    to detect or blind a debugger (e.g. `IsDebuggerPresent`,
//!    `CheckRemoteDebuggerPresent`, `NtQueryInformationProcess`,
//!    `NtSetInformationThread(ThreadHideFromDebugger)`). Found by matching the
//!    program's imports and reporting each call site.
//! 2. **Manual PEB reads** — inlined equivalents of `IsDebuggerPresent` that read
//!    the Process Environment Block directly via `FS:[0x30]` (x86) / `GS:[0x60]`
//!    (x64) to inspect `BeingDebugged`/`NtGlobalFlag`/heap flags without a
//!    hookable API. Found by scanning decoded instructions for the segment-based
//!    PEB load and confirming the field offset read next.
//! 3. **`INT 0x2D`** — a kernel debugging interrupt whose behavior diverges under
//!    a debugger. Found by scanning decoded instructions.
//! 4. **`ptrace` self-attach** — the Linux `ptrace(PTRACE_TRACEME)` trick: a
//!    process may be traced only once, so if a debugger is attached the call
//!    fails. Found via the `ptrace` import (dynamically linked) or by resolving a
//!    raw `syscall` with `eax == 101` (statically-linked, stripped ELF).
//! 5. **`/proc/self/status` + `TracerPid`** — the Linux userland equivalent: a
//!    non-zero `TracerPid` reveals an attached debugger. Found via strings.

use anyhow::Result;

use crate::thorpal::detect::context::{AnalysisContext, fmt_addr};
use crate::thorpal::detect::detections::{
    ApiSpec, AttackTechnique, Detection, DetectionHit, EvidenceKind, api_call_hits, high, low,
    medium, reg_value_before, scan_instructions, string_artifact_hits,
};

use super::T1622;

/// The Debugger Evasion detection (ATT&CK T1622).
pub struct DebuggerEvasion;

/// Imported anti-debug APIs and why each matters (meaning + how it thwarts analysis).
const APIS: &[ApiSpec] = &[
    ApiSpec {
        key: "IsDebuggerPresent",
        severity: medium(),
        reasoning: "Calls kernel32!IsDebuggerPresent, which reads the PEB BeingDebugged byte. A \
                    non-zero result means a user-mode debugger is attached, letting the malware \
                    divert to benign/exit logic so its real behavior is never observed. Defeat by \
                    zeroing BeingDebugged or forcing the return value.",
    },
    ApiSpec {
        key: "CheckRemoteDebuggerPresent",
        severity: high(),
        reasoning: "Calls CheckRemoteDebuggerPresent, which asks the kernel whether a debugger is \
                    attached (works even for debuggers hidden from BeingDebugged). Defeat by forcing \
                    the output BOOL to false.",
    },
    ApiSpec {
        key: "NtQueryInformationProcess",
        severity: high(),
        reasoning: "Calls NtQueryInformationProcess, the low-level way to query ProcessDebugPort/\
                    Flags/ObjectHandle and reveal a debugger without the obvious debug APIs. Defeat \
                    by hooking the call to return the clean values.",
    },
    ApiSpec {
        key: "ZwQueryInformationProcess",
        severity: high(),
        reasoning: "Calls ZwQueryInformationProcess (the Nt* twin) to query ProcessDebugPort/Flags/\
                    ObjectHandle and detect a debugger.",
    },
    ApiSpec {
        key: "NtSetInformationThread",
        severity: high(),
        reasoning: "Calls NtSetInformationThread with ThreadHideFromDebugger (0x11) to detach the \
                    thread from debug events, so breakpoints and single-stepping stop working and \
                    the analyst is blinded. Defeat by neutralizing the call.",
    },
    ApiSpec {
        key: "ZwSetInformationThread",
        severity: high(),
        reasoning: "Calls ZwSetInformationThread with ThreadHideFromDebugger to blind the debugger.",
    },
    ApiSpec {
        key: "NtQuerySystemInformation",
        severity: medium(),
        reasoning: "Calls NtQuerySystemInformation, which can return SystemKernelDebuggerInformation \
                    to detect a kernel debugger and refuse to run while WinDbg/KD is active.",
    },
    ApiSpec {
        key: "OutputDebugString",
        severity: low(),
        reasoning: "Calls OutputDebugString; an old trick checks GetLastError/a magic return to \
                    infer whether a debugger is capturing the output. Common in benign code.",
    },
    ApiSpec {
        key: "DebugActiveProcess",
        severity: medium(),
        reasoning: "Calls DebugActiveProcess; malware self-debugs (a child debugs the parent) so no \
                    second debugger can attach — locking out the analyst's debugger.",
    },
    ApiSpec {
        key: "DbgBreakPoint",
        severity: medium(),
        reasoning: "References DbgBreakPoint, a routine debuggers use to break in; malware patches or \
                    watches it to detect/prevent debugger attachment.",
    },
    ApiSpec {
        key: "DbgUiRemoteBreakin",
        severity: medium(),
        reasoning: "References DbgUiRemoteBreakin, the routine DebugActiveProcess injects to break \
                    into a target; patching it defeats attach-based debugging.",
    },
    ApiSpec {
        key: "GetThreadContext",
        severity: medium(),
        reasoning: "Calls GetThreadContext to read CPU debug registers (DR0-DR7); a non-zero debug \
                    register reveals a hardware breakpoint set by an analyst.",
    },
    ApiSpec {
        key: "SetThreadContext",
        severity: medium(),
        reasoning: "Calls SetThreadContext to clear the CPU debug registers (DR0-DR7), disabling any \
                    hardware breakpoints an analyst has set.",
    },
    ApiSpec {
        key: "NtClose",
        severity: low(),
        reasoning: "Calls NtClose, which can be fed an invalid handle on purpose: under a debugger \
                    this raises an exception, so its presence is used to detect debugging. Common \
                    API, hence low confidence.",
    },
    ApiSpec {
        key: "ptrace",
        severity: high(),
        reasoning: "Calls ptrace; the ptrace(PTRACE_TRACEME) self-attach fails if a debugger is \
                    already attached (a process can be traced only once), revealing the debugger. \
                    Defeat by stubbing ptrace to succeed.",
    },
];

/// Strings that reveal a Linux debugger via `/proc/self/status` + `TracerPid`.
const PROC_STATUS_STRINGS: &[&str] = &["/proc/self/status", "tracerpid"];

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
    #[tracing::instrument(name = "DebuggerEvasion::run", skip_all, err(Debug))]
    async fn run(&self, ctx: &AnalysisContext) -> Result<Vec<DetectionHit>> {
        // collect hits from every sub-pass into one vector
        let mut hits = Vec::new();

        // 1) imported anti-debug API call sites (their return value gates behavior)
        hits.extend(api_call_hits(ctx, T1622, APIS, EvidenceKind::ApiResultCheck).await?);

        // 2) decoded-instruction opcode tells: INT 0x2D, manual PEB reads, and the
        //    raw ptrace syscall (single scan over all functions' instructions)
        hits.extend(
            scan_instructions(ctx, |c| {
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
                    // classify the field the code reads next (best effort look-ahead)
                    let (severity, label, reasoning) = classify_peb(c.following(8));
                    return Some(DetectionHit::new(
                        T1622,
                        c.addr(),
                        Some(c.func_entry),
                        severity,
                        format!("{label} @ {}", fmt_addr(c.addr())),
                        reasoning,
                    ).kind(EvidenceKind::ControlFlowGated));
                }

                // raw ptrace syscall (statically-linked, stripped ELF): eax/rax == 101
                if m == "SYSCALL" && reg_value_before(c.preceding(8), "eax", "rax") == Some(101) {
                    return Some(DetectionHit::new(
                        T1622,
                        c.addr(),
                        Some(c.func_entry),
                        high(),
                        format!("ptrace syscall (eax=101) @ {}", fmt_addr(c.addr())),
                        "Invokes the ptrace syscall (number 101) directly — the PTRACE_TRACEME \
                         self-attach anti-debug trick. With no ptrace import (static/stripped ELF) \
                         the raw syscall is the only evidence. Defeat by stubbing ptrace to succeed."
                            .to_string(),
                    ).kind(EvidenceKind::ApiResultCheck));
                }

                None
            })
            .await?,
        );

        // 3) /proc/self/status + TracerPid strings (Linux userland debugger check)
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

/// Classify a PEB field read that follows a manual PEB base load.
///
/// Looks a few instructions ahead of the `FS:[0x30]`/`GS:[0x60]` load for the
/// field offset accessed next and returns the appropriate severity + label +
/// reasoning. Defaults to a generic (medium) PEB access when nothing specific is
/// found.
///
/// # Arguments
///
/// * `following` - The instructions immediately after the PEB base load
fn classify_peb(
    following: &[ghidra_cli::models::Instruction],
) -> (
    (crate::thorpal::detect::confidence::Confidence, i64),
    String,
    String,
) {
    // inspect the operands of the next few instructions for a known field offset
    for insn in following {
        let ops = crate::thorpal::detect::op_str(insn)
            .to_ascii_lowercase()
            .replace(' ', "");
        if ops.contains("+0x2]") {
            // PEB+0x02 == BeingDebugged (the inlined IsDebuggerPresent)
            return (
                high(),
                "PEB BeingDebugged check".to_string(),
                "Reads PEB+0x02 (BeingDebugged) after a manual PEB load — an inlined \
                 IsDebuggerPresent. Defeat by zeroing BeingDebugged."
                    .to_string(),
            );
        }
        if ops.contains("+0x68]") || ops.contains("+0xbc]") {
            // PEB+0x68 (x86) / +0xBC (x64) == NtGlobalFlag
            return (
                high(),
                "PEB NtGlobalFlag check".to_string(),
                "Reads PEB NtGlobalFlag (offset 0x68 x86 / 0xBC x64); the loader sets heap-debug \
                 bits here under a debugger, betraying it. Defeat by clearing those flags."
                    .to_string(),
            );
        }
        if ops.contains("+0x18]") {
            // PEB+0x18 == ProcessHeap (its Flags/ForceFlags are the debugger tell)
            return (
                medium(),
                "PEB ProcessHeap flags check".to_string(),
                "Reads PEB ProcessHeap (offset 0x18) to inspect heap Flags/ForceFlags, which a \
                 debugger leaves set. Defeat by normalizing the heap flags."
                    .to_string(),
            );
        }
    }
    // nothing specific matched: a generic manual PEB access is still notable
    (
        medium(),
        "Direct PEB access".to_string(),
        "Loads the Process Environment Block base directly (FS:[0x30] / GS:[0x60]); malware does \
         this to inline anti-debug checks without a hookable API."
            .to_string(),
    )
}
