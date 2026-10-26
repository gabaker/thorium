//! Virtualization/Sandbox Evasion — Time Based Evasion (MITRE ATT&CK **T1497.003**).
//!
//! Detects timing- and sleep-based evasion, which works two ways:
//!
//! * **Stalling** past a sandbox's analysis window with long/repeated sleeps so
//!   the payload only runs after monitoring stops.
//! * **Timing checks** that measure how long an operation takes: single-stepping
//!   in a debugger or running under an emulator inflates the elapsed time, and a
//!   sandbox that fast-forwards sleeps shows too little real elapsed time — either
//!   way the code can tell it is being analyzed.
//!
//! Sub-passes: `RDTSC`/`RDTSCP` opcodes and Linux time/sleep syscalls (by decoded
//! instruction); timing/sleep APIs (by import call site); and a long-sleep
//! constant heuristic on `Sleep`/`SleepEx` call sites.

use anyhow::{Context, Result};

use crate::thorpal::detect::context::{AnalysisContext, fmt_addr, parse_addr};
use crate::thorpal::detect::detections::{
    ApiSpec, AttackTechnique, Detection, DetectionHit, EvidenceKind, api_call_hits, low, medium,
    parse_imm, reg_value_before, scan_instructions,
};

use super::T1497_003;

/// The Time Based Evasion detection (ATT&CK T1497.003).
pub struct SandboxTimeEvasion;

/// Timing APIs used for the measure-elapsed-time evasion pattern.
const TIMING_APIS: &[ApiSpec] = &[
    ApiSpec {
        key: "GetTickCount",
        severity: low(),
        reasoning: "Reads the millisecond uptime (GetTickCount); sampled before/after an operation, \
                    the inflated delta under a debugger or the missing delay when a sandbox skips a \
                    sleep reveals analysis. Ubiquitous, hence low confidence.",
    },
    ApiSpec {
        key: "GetTickCount64",
        severity: low(),
        reasoning: "Reads the 64-bit uptime (GetTickCount64) for the measure-elapsed-time \
                    anti-analysis pattern.",
    },
    ApiSpec {
        key: "QueryPerformanceCounter",
        severity: low(),
        reasoning: "Reads a high-resolution timer (QueryPerformanceCounter) to precisely time a code \
                    region and detect the slowdown of single-stepping/emulation or a skipped sleep.",
    },
    ApiSpec {
        key: "QueryPerformanceFrequency",
        severity: low(),
        reasoning: "Reads the high-resolution timer frequency (QueryPerformanceFrequency), the \
                    companion to QueryPerformanceCounter timing checks.",
    },
    ApiSpec {
        key: "timeGetTime",
        severity: low(),
        reasoning: "Reads a millisecond timer (timeGetTime), another wall-clock source for the \
                    measure-elapsed-time anti-analysis pattern.",
    },
    ApiSpec {
        key: "GetSystemTimeAsFileTime",
        severity: low(),
        reasoning: "Reads absolute time (GetSystemTimeAsFileTime); timing a region with it detects \
                    debuggers/emulators or a sandbox that skipped a sleep.",
    },
    ApiSpec {
        key: "NtQuerySystemTime",
        severity: low(),
        reasoning: "Reads absolute time (NtQuerySystemTime) for timing-based analysis detection.",
    },
];

/// Sleep/delay APIs used to stall past a sandbox's analysis window.
const SLEEP_APIS: &[ApiSpec] = &[
    ApiSpec {
        key: "SetWaitableTimer",
        severity: low(),
        reasoning: "Uses a waitable timer (SetWaitableTimer) as an alternative way to delay \
                    execution and outlast automated analysis without calling Sleep.",
    },
    ApiSpec {
        key: "CreateWaitableTimer",
        severity: low(),
        reasoning: "Creates a waitable timer (CreateWaitableTimer) to delay execution and outlast \
                    automated analysis.",
    },
    ApiSpec {
        key: "Sleep",
        severity: low(),
        reasoning: "Calls Sleep; long or repeated sleeps stall past a sandbox's analysis window so \
                    the payload only runs after monitoring stops. Severity rises with a large delay.",
    },
    ApiSpec {
        key: "SleepEx",
        severity: low(),
        reasoning: "Calls SleepEx; long or repeated sleeps stall past a sandbox's analysis window.",
    },
    ApiSpec {
        key: "NtDelayExecution",
        severity: medium(),
        reasoning: "Calls NtDelayExecution, the native sleep primitive often used to bypass Sleep \
                    hooks, to stall past a sandbox timeout.",
    },
    ApiSpec {
        key: "nanosleep",
        severity: low(),
        reasoning: "Calls nanosleep; long delays stall past sandbox timeouts, and a skipped sleep is \
                    detectable via real elapsed time.",
    },
    ApiSpec {
        key: "clock_nanosleep",
        severity: low(),
        reasoning: "Calls clock_nanosleep; long delays stall past sandbox timeouts.",
    },
    ApiSpec {
        key: "usleep",
        severity: low(),
        reasoning: "Calls usleep; repeated/long delays stall past sandbox timeouts.",
    },
];

/// A sleep argument at/above this many milliseconds is treated as a "long sleep".
const LONG_SLEEP_MS: u64 = 60_000;

#[async_trait::async_trait]
impl Detection for SandboxTimeEvasion {
    /// Short, stable name for logs.
    fn name(&self) -> &'static str {
        "sandbox_time_evasion"
    }

    /// The primary technique this detection reports.
    fn technique(&self) -> AttackTechnique {
        T1497_003
    }

    /// Run all Time Based Evasion sub-passes and merge their hits.
    #[tracing::instrument(name = "SandboxTimeEvasion::run", skip_all, err(Debug))]
    async fn run(&self, ctx: &AnalysisContext) -> Result<Vec<DetectionHit>> {
        // collect hits from every sub-pass
        let mut hits = Vec::new();

        // 1) RDTSC/RDTSCP timestamp reads + Linux time/sleep syscalls (one scan)
        hits.extend(
            scan_instructions(ctx, |c| {
                let m = c.mnemonic();
                // RDTSC — read the CPU timestamp counter directly, bypassing timing APIs
                if m == "RDTSC" {
                    return Some(DetectionHit::new(
                        T1497_003,
                        c.addr(),
                        Some(c.func_entry),
                        medium(),
                        format!("RDTSC timestamp read @ {}", fmt_addr(c.addr())),
                        "Executes RDTSC to read the CPU timestamp counter directly. Bracketing code \
                         with two RDTSC reads measures elapsed cycles; single-stepping or an \
                         emulator inflates the delta, so the code detects analysis. Defeat by \
                         patching the comparison or normalizing the timer.",
                    ).kind(EvidenceKind::Timing));
                }
                // RDTSCP — the serializing variant, used for cycle-accurate timing
                if m == "RDTSCP" {
                    return Some(DetectionHit::new(
                        T1497_003,
                        c.addr(),
                        Some(c.func_entry),
                        medium(),
                        format!("RDTSCP timestamp read @ {}", fmt_addr(c.addr())),
                        "Executes RDTSCP (serializing timestamp read) for a cycle-accurate timing \
                         check that detects single-stepping/emulation slowdown.",
                    ).kind(EvidenceKind::Timing));
                }
                // Linux time/sleep syscalls resolved by number (stripped static ELF)
                if m == "SYSCALL" {
                    return match reg_value_before(c.preceding(8), "eax", "rax") {
                        // nanosleep (35) / clock_nanosleep (230)
                        Some(35) | Some(230) => Some(DetectionHit::new(
                            T1497_003,
                            c.addr(),
                            Some(c.func_entry),
                            low(),
                            format!("nanosleep syscall @ {}", fmt_addr(c.addr())),
                            "Invokes (clock_)nanosleep directly; long delays stall past sandbox \
                             timeouts, and a skipped sleep is detectable via real elapsed time.",
                        ).kind(EvidenceKind::ContextOnly)),
                        // gettimeofday (96) / clock_gettime (228) / time (201)
                        Some(96) | Some(228) | Some(201) => Some(DetectionHit::new(
                            T1497_003,
                            c.addr(),
                            Some(c.func_entry),
                            low(),
                            format!("time syscall @ {}", fmt_addr(c.addr())),
                            "Invokes a time syscall (gettimeofday/clock_gettime/time); reading \
                             wall-clock time around a region detects debugger/emulator slowdown or a \
                             skipped sleep.",
                        ).kind(EvidenceKind::Timing)),
                        _ => None,
                    };
                }
                None
            })
            .await?,
        );

        // 2) timing API call sites (Timing: validated by delta correlation) and
        //    sleep API call sites (ContextOnly: validated by reachability/clustering)
        hits.extend(api_call_hits(ctx, T1497_003, TIMING_APIS, EvidenceKind::Timing).await?);
        hits.extend(api_call_hits(ctx, T1497_003, SLEEP_APIS, EvidenceKind::ContextOnly).await?);

        // 3) long-sleep heuristic on Sleep/SleepEx call sites
        hits.extend(self.long_sleep_hits(ctx).await?);

        Ok(hits)
    }
}

impl SandboxTimeEvasion {
    /// Flag `Sleep`/`SleepEx` call sites whose millisecond argument is large.
    ///
    /// A large constant delay is a much stronger stalling signal than a bare
    /// `Sleep` call, so it earns its own medium-confidence hit with the recovered
    /// duration. The argument is recovered from the instruction just before the
    /// call (`push <ms>` on x86, `mov ecx, <ms>` on x64).
    ///
    /// # Arguments
    ///
    /// * `ctx` - The analysis context
    async fn long_sleep_hits(&self, ctx: &AnalysisContext) -> Result<Vec<DetectionHit>> {
        let mut hits = Vec::new();
        // consider both Sleep and SleepEx imports
        for import in ctx.imports.iter().filter(|s| {
            let n = crate::thorpal::detect::detections::normalize(&s.name);
            n == "Sleep" || n == "SleepEx"
        }) {
            // for each call site, recover the millisecond argument and test it;
            // resolve through import thunks so we inspect the *real* caller (a
            // thunk stub has no pushed argument to recover)
            let call_sites = ctx
                .resolve_call_sites(&import.address)
                .await
                .with_context(|| format!("resolving call sites of '{}'", import.name))?;
            for cs in call_sites {
                let site = cs.site;
                // disassemble the calling function to inspect the pushed argument
                let Some((_, insns)) = ctx.disasm_containing(site).await? else {
                    continue;
                };
                let Some(idx) = insns
                    .iter()
                    .position(|i| parse_addr(&i.address) == Some(site))
                else {
                    continue;
                };
                // look back a few instructions for the dwMilliseconds constant
                let start = idx.saturating_sub(6);
                let ms = insns[start..idx].iter().rev().find_map(|insn| {
                    let m = insn.mnemonic.to_ascii_uppercase();
                    let ops = crate::thorpal::detect::op_str(insn).to_ascii_lowercase();
                    if m == "PUSH" {
                        // x86 stdcall: `push <ms>`
                        parse_imm(&ops)
                    } else if m == "MOV" && (ops.starts_with("ecx,") || ops.starts_with("rcx,")) {
                        // x64 fastcall: `mov ecx, <ms>`
                        ops.split(',').nth(1).and_then(parse_imm)
                    } else {
                        None
                    }
                });
                // only emit when the recovered delay is large
                if let Some(ms) = ms {
                    if ms >= LONG_SLEEP_MS {
                        // note the thunk indirection when the sleep is reached via a stub
                        let via = if cs.via_thunk.is_some() {
                            " (via import thunk)"
                        } else {
                            ""
                        };
                        hits.push(
                            DetectionHit::new(
                                T1497_003,
                                site,
                                cs.function,
                                medium(),
                                format!("Long sleep ({ms} ms){via} @ {}", fmt_addr(site)),
                                format!(
                                    "Sleeps for {ms} ms — a long stall used to outlast a sandbox's \
                                 analysis window so the payload only runs after monitoring stops. \
                                 Defeat by patching the delay or fast-forwarding time."
                                ),
                            )
                            .kind(EvidenceKind::ContextOnly),
                        );
                    }
                }
            }
        }
        Ok(hits)
    }
}
