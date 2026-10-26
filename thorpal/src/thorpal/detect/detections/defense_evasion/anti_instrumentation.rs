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
//! * **Analysis-tool names** — debuggers/monitors (OllyDbg, x64dbg, WinDbg, IDA,
//!   Wireshark, Procmon, Process Hacker) that malware searches for.
//!
//! It also flags instruction-level anti-instrumentation tells over **decoded**
//! instructions (not raw bytes, which would be far too noisy for single-byte
//! opcodes): the undocumented `ICEBP` (0xF1) debug interrupt, the EFLAGS
//! trap-flag single-step check (a `0x100` test near a `PUSHF`/`POPF`), and code
//! that scans itself for planted `0xCC` (INT3) breakpoint bytes.

use anyhow::Result;

use crate::thorpal::detect::context::{AnalysisContext, fmt_addr};
use crate::thorpal::detect::detections::{
    AttackTechnique, Detection, DetectionHit, EvidenceKind, high, low, medium, scan_instructions,
    string_artifact_hits,
};

use super::T1622;

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

    /// Report instrumentation/hook-detection string artifacts.
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

        // 3) analysis/debugger tool names
        hits.extend(
            string_artifact_hits(
                ctx,
                T1622,
                TOOL_STRINGS,
                medium(),
                "Contains the name of a debugging/analysis/monitoring tool; malware searches running \
                 processes or windows for these and refuses to detonate when an analyst tool is open.",
                &[],
            )
            .await?,
        );

        // 4) decoded-instruction anti-instrumentation tells: ICEBP, the EFLAGS
        //    trap-flag single-step check, and 0xCC breakpoint-byte scanning
        hits.extend(
            scan_instructions(ctx, |c| {
                let m = c.mnemonic();
                let ops = c.operands().replace(' ', "");

                // ICEBP (0xF1) — an undocumented single-byte debug interrupt used
                // in exception-based anti-debug (its exception fires differently
                // when a debugger is present)
                if m == "ICEBP" {
                    return Some(DetectionHit::new(
                        T1622,
                        c.addr(),
                        Some(c.func_entry),
                        low(),
                        format!("ICEBP (INT1) instruction @ {}", fmt_addr(c.addr())),
                        "Embeds an ICEBP (0xF1) instruction; malware installs its own handler and \
                         uses whether the resulting debug exception fires to detect a debugger.",
                    ).kind(EvidenceKind::ContextOnly));
                }

                // EFLAGS Trap Flag (bit 8 = 0x100) single-step check: a test/and/or
                // of 0x100 that is close to a PUSHF/POPF (i.e. actually operating on
                // the flags), not just any use of the constant 256
                if matches!(m.as_str(), "TEST" | "AND" | "OR" | "CMP" | "XOR")
                    && ops.contains("0x100")
                    && c.preceding(6).iter().any(|i| {
                        let pm = i.mnemonic.to_ascii_uppercase();
                        matches!(pm.as_str(), "PUSHF" | "PUSHFD" | "PUSHFQ" | "POPF" | "POPFD" | "POPFQ")
                    })
                {
                    return Some(DetectionHit::new(
                        T1622,
                        c.addr(),
                        Some(c.func_entry),
                        medium(),
                        format!("Trap flag (single-step) check @ {}", fmt_addr(c.addr())),
                        "Reads EFLAGS via PUSHF/POPF and tests bit 8 (0x100, the Trap Flag). Setting \
                         the trap flag and watching whether the expected single-step exception fires \
                         detects a debugger that swallows single-step events. Defeat by letting the \
                         exception through.",
                    ).kind(EvidenceKind::ControlFlowGated));
                }

                // scanning code for planted 0xCC (INT3) breakpoint bytes
                if matches!(m.as_str(), "CMP" | "TEST") && ops.contains("0xcc") {
                    return Some(DetectionHit::new(
                        T1622,
                        c.addr(),
                        Some(c.func_entry),
                        low(),
                        format!("0xCC breakpoint-byte scan @ {}", fmt_addr(c.addr())),
                        "Compares a byte against 0xCC (the INT3 opcode); malware reads its own code \
                         looking for software breakpoints an analyst has planted. Defeat by using \
                         hardware breakpoints or restoring original bytes before the check.",
                    ).kind(EvidenceKind::ControlFlowGated));
                }

                None
            })
            .await?,
        );

        Ok(hits)
    }
}
