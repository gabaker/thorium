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
//! Every API and opcode in these catalogs is deeply dual-use (beacon pacing, C2
//! retry backoff, PRNG seeding, timeouts, CRT cookie entropy), so each sub-pass
//! separates *capability* from *use*:
//!
//! * `RDTSC`/`RDTSCP` reads are gated on their outputs actually being used:
//!   reads whose outputs are provably overwritten unused (VMProtect-style
//!   instruction-mutation junk) are skipped, a bracketing pair whose delta is
//!   subtracted/compared keeps medium confidence, and a lone read seeds as a
//!   weak observation.
//! * Timing/sleep API call sites arrive grouped per calling function (one hit
//!   naming every call), worded as the capability plus the condition under
//!   which it becomes evasion; the post-detection validator
//!   (`crate::thorpal::detect::validate`) independently marks results that
//!   gate a branch.
//! * `Sleep`/`SleepEx` millisecond arguments are recovered where statically
//!   visible: at/above [`STALL_SLEEP_MS`] the stall claim keeps medium
//!   confidence, [`LONG_SLEEP_MS`]–[`STALL_SLEEP_MS`] recoveries seed low and
//!   name both readings (sandbox stall vs retry/beacon cadence), and
//!   short/`INFINITE` recoveries annotate the bare call hit instead of
//!   duplicating it. A site covered by a recovered-constant hit is removed
//!   from the bare hit so each site seeds once.
//! * Unclassified tells (bare sleeps, lone timer reads, raw time syscalls) are
//!   capped per program at [`GENERIC_TELL_CAP`] via the shared grouping helper,
//!   with the overflow folded into one counted summary; recovered long-sleep
//!   constants and delta-compared `RDTSC` pairs are exempt from the cap.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, Result};
use ghidra_cli::models::Instruction;

use crate::thorpal::detect::context::{AnalysisContext, fmt_addr, parse_addr};
use crate::thorpal::detect::detections::{
    ApiSpec, AttackTechnique, Detection, DetectionHit, EvidenceKind, InsnCtx, api_call_hits,
    content_class, group_scan_hits, low, medium, normalize, parse_imm, reg_value_before,
    scan_instructions,
};
use crate::thorpal::detect::op_str;

use super::T1497_003;

/// The Time Based Evasion detection (ATT&CK T1497.003).
pub struct SandboxTimeEvasion;

/// Timing APIs used for the measure-elapsed-time evasion pattern.
///
/// Every entry is worded as capability + condition: what a lone read is for,
/// and what pairing would make it a timing check. The result-use validator
/// (`crate::thorpal::detect::validate`) marks sites whose result provably
/// gates a branch.
const TIMING_APIS: &[ApiSpec] = &[
    ApiSpec {
        key: "GetTickCount",
        always_noise: false,
        severity: low(),
        reasoning: "Reads the millisecond uptime (GetTickCount). A single read is a timestamp — \
                    PRNG seed, beacon nonce, timeout base, or rate-limit anchor are the routine \
                    readings. It evidences timing evasion only if a second reading's delta is \
                    compared against an expected elapsed time (for example around a sleep a \
                    sandbox may fast-forward); this flag asserts no such pairing.",
    },
    ApiSpec {
        key: "GetTickCount64",
        always_noise: false,
        severity: low(),
        reasoning: "Reads the 64-bit uptime (GetTickCount64). Same dual-use profile as \
                    GetTickCount: a lone read is a timestamp/timeout source, and only a paired \
                    read whose delta is compared against an expected elapsed time indicates \
                    timing evasion.",
    },
    ApiSpec {
        key: "QueryPerformanceCounter",
        always_noise: false,
        severity: low(),
        reasoning: "Reads the high-resolution performance counter (QueryPerformanceCounter). \
                    Routine uses are profiling, frame/media pacing, and CRT /GS stack-cookie \
                    entropy; it indicates timing evasion only when a region is bracketed by two \
                    reads and the delta is tested for debugger/emulator slowdown or a skipped \
                    sleep.",
    },
    ApiSpec {
        key: "QueryPerformanceFrequency",
        always_noise: false,
        severity: low(),
        reasoning: "Reads the performance-counter frequency (QueryPerformanceFrequency), used to \
                    convert QueryPerformanceCounter ticks to time. It carries no timing-check \
                    claim by itself — it matters only beside a bracketed pair of counter reads \
                    whose delta is compared.",
    },
    ApiSpec {
        key: "timeGetTime",
        always_noise: false,
        severity: low(),
        reasoning: "Reads the millisecond multimedia timer (timeGetTime). Routine uses are media \
                    and queue timeouts and frame pacing; it indicates timing evasion only when \
                    sampled before and after an operation and the delta is compared against an \
                    expected bound.",
    },
    ApiSpec {
        key: "GetSystemTimeAsFileTime",
        always_noise: false,
        severity: low(),
        reasoning: "Reads absolute system time (GetSystemTimeAsFileTime). Ubiquitous as a \
                    timestamp source and as /GS stack-cookie entropy in CRT startup; it \
                    indicates timing evasion only if two readings bracket an operation and \
                    their difference is compared against an expected elapsed time.",
    },
    ApiSpec {
        key: "NtQuerySystemTime",
        always_noise: false,
        severity: low(),
        reasoning: "Reads absolute system time via the native NtQuerySystemTime. The native call \
                    is a mild hook-avoidance tell, but a lone read is still just a timestamp; \
                    only a paired read with a compared delta indicates a timing check.",
    },
];

/// Sleep/delay APIs that can stall past a sandbox's analysis window.
///
/// Worded for their dual use: pacing, retry backoff, and timeouts are the
/// routine readings, and the stall claim is reserved for sites with a long
/// recovered delay (reported separately with the constant named).
const SLEEP_APIS: &[ApiSpec] = &[
    ApiSpec {
        key: "SetWaitableTimer",
        always_noise: false,
        severity: low(),
        reasoning: "Arms a waitable timer (SetWaitableTimer) — a Sleep-free way to delay \
                    execution. Routine schedulers and watchdogs use the same call; it stalls \
                    analysis only when armed with a long due time ahead of the payload path.",
    },
    ApiSpec {
        key: "CreateWaitableTimer",
        always_noise: false,
        severity: low(),
        reasoning: "Creates a waitable timer (CreateWaitableTimer). Dual-use scheduling \
                    machinery; it contributes to a sandbox stall only when the timer is armed \
                    with a long due time and waited on before the payload runs.",
    },
    ApiSpec {
        key: "Sleep",
        always_noise: false,
        severity: low(),
        reasoning: "Calls Sleep — the most dual-use delay API on Windows: beacon intervals, C2 \
                    retry/backoff, polling loops, and ordinary timeouts are the common readings, \
                    and a sleep stalls analysis only when a long or repeated delay precedes the \
                    payload. A bare call site is census input for cadence mapping, not evidence \
                    of evasion; sites with a recovered long constant are reported separately.",
    },
    ApiSpec {
        key: "SleepEx",
        always_noise: false,
        severity: low(),
        reasoning: "Calls SleepEx. Same dual-use profile as Sleep, plus the alertable form: \
                    SleepEx(INFINITE, TRUE) parks a worker thread for APC delivery and involves \
                    no finite delay at all. Only a long recovered delay ahead of the payload \
                    path reads as an analysis-window stall.",
    },
    ApiSpec {
        key: "NtDelayExecution",
        always_noise: false,
        severity: medium(),
        reasoning: "Calls NtDelayExecution, the native primitive beneath Sleep. Calling it \
                    directly is how loaders bypass user-mode Sleep hooks and sleep-skipping, \
                    which makes it a stronger tell than Sleep itself — but it is still a delay \
                    primitive, so confirm a long or looped duration before reading it as an \
                    analysis-window stall.",
    },
    ApiSpec {
        key: "nanosleep",
        always_noise: false,
        severity: low(),
        reasoning: "Calls nanosleep. In static Linux binaries the libc sleep wrapper itself \
                    matches, and short delays are retry/pacing; a long delay ahead of the \
                    payload is the analysis-window-stall reading.",
    },
    ApiSpec {
        key: "clock_nanosleep",
        always_noise: false,
        severity: low(),
        reasoning: "Calls clock_nanosleep. Same dual-use profile as nanosleep: pacing and \
                    timeouts are routine, and only a long delay ahead of the payload reads as a \
                    sandbox stall.",
    },
    ApiSpec {
        key: "usleep",
        always_noise: false,
        severity: low(),
        reasoning: "Calls usleep. Microsecond-scale delays are poll/retry pacing; repeated or \
                    long delays ahead of the payload are the sandbox-stall reading.",
    },
];

/// A sleep argument at/above this many milliseconds is a "long sleep" worth its
/// own recovered-constant finding.
const LONG_SLEEP_MS: u64 = 60_000;

/// A sleep argument at/above this many milliseconds (10 minutes) has no routine
/// retry/pacing reading, so the stall claim keeps medium confidence; recoveries
/// between [`LONG_SLEEP_MS`] and this seed low and name both readings.
const STALL_SLEEP_MS: u64 = 600_000;

/// The 32-bit `INFINITE` sentinel (`Sleep`/`SleepEx` never return): an
/// alertable-wait/park idiom, not a finite stall.
const INFINITE_MS: u64 = 0xFFFF_FFFF;

/// How many instructions before a `Sleep`/`SleepEx` call the argument walk reads.
const SLEEP_ARG_WINDOW: usize = 6;

/// How many following instructions the TSC output-liveness walk inspects.
const RDTSC_USE_WINDOW: usize = 8;

/// The most unclassified timing tells kept as individual findings per program;
/// the overflow folds into one counted summary (see [`group_scan_hits`]).
const GENERIC_TELL_CAP: usize = 6;

/// The content class of a delta-compared `RDTSC` bracketing pair — the one
/// scan-hit class exempt from the generic-tell cap.
const PAIRED_DELTA_CLASS: &str = "Paired RDTSC timing check (delta compared)";

/// The max call addresses listed when re-rendering a reduced bare sleep hit.
const MAX_BARE_SITES: usize = 12;

/// The register-name family `RDTSC` writes its low half to (widest first).
const EAX_FAMILY: &[&str] = &["rax", "eax", "ax", "al", "ah"];

/// The register-name family `RDTSC` writes its high half to (widest first).
const EDX_FAMILY: &[&str] = &["rdx", "edx", "dx", "dl", "dh"];

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

    /// Run all Time Based Evasion sub-passes, dedupe per site, and cap the
    /// unclassified tells.
    #[tracing::instrument(name = "SandboxTimeEvasion::run", skip_all, err(Debug))]
    async fn run(&self, ctx: &AnalysisContext) -> Result<Vec<DetectionHit>> {
        // 1) use-gated RDTSC/RDTSCP reads + Linux time/sleep syscalls (one scan)
        let scan = scan_instructions(ctx, scan_tell).await?;
        // split the delta-compared bracketing pairs (cap-exempt) from the
        // generic tells the per-program cap applies to
        let (pairs, mut generic): (Vec<DetectionHit>, Vec<DetectionHit>) = scan
            .into_iter()
            .partition(|hit| content_class(&hit.content) == PAIRED_DELTA_CLASS);
        // 2) timing API call sites, grouped per calling function by the shared
        //    helper (the result-use validator confirms gates post-detection)
        generic.extend(api_call_hits(ctx, T1497_003, TIMING_APIS, EvidenceKind::Timing).await?);
        // sleep API call sites (context-only: reachability/clustering evidence)
        let mut sleep_api =
            api_call_hits(ctx, T1497_003, SLEEP_APIS, EvidenceKind::ContextOnly).await?;
        // 3) recover statically visible Sleep/SleepEx millisecond arguments
        let recovery = self.sleep_argument_hits(ctx).await?;
        // 4) dedupe: a site covered by a recovered-constant hit seeds once, and
        //    short/INFINITE recoveries annotate the bare hit they stay in
        rebuild_bare_sleep_hits(&mut sleep_api, &recovery.covered, &recovery.short);
        generic.extend(sleep_api);
        // 5) group the generic tells per function and cap them per program
        //    (file-level census/summary hits are exempt from the cap)
        let mut hits = group_scan_hits(generic, GENERIC_TELL_CAP, |hit| hit.function.is_some());
        // classified observations (long-sleep constants, delta-compared pairs)
        // group per function too but never count against the cap
        let mut classified = pairs;
        classified.extend(recovery.hits);
        hits.extend(group_scan_hits(classified, usize::MAX, |_| false));
        Ok(hits)
    }
}

/// The outcome of the `Sleep`/`SleepEx` argument-recovery pass.
#[derive(Default)]
struct SleepRecovery {
    /// The recovered-constant findings (sleeps at/above [`LONG_SLEEP_MS`]).
    hits: Vec<DetectionHit>,
    /// The call sites those findings cover (removed from the bare call hits).
    covered: HashSet<u64>,
    /// Recovered short/`INFINITE` delays by call site, kept in the bare hit as
    /// annotations rather than seeded as their own findings.
    short: BTreeMap<u64, u64>,
}

impl SandboxTimeEvasion {
    /// Recover the millisecond argument of every `Sleep`/`SleepEx` call site.
    ///
    /// The argument is read from the staging just before the call (`push <ms>`
    /// on x86, `mov ecx, <ms>` on x64), resolving through import thunks so the
    /// *real* caller is inspected (a thunk stub stages no argument). Recovered
    /// constants at/above [`LONG_SLEEP_MS`] become their own graded findings
    /// (see [`long_sleep_hit`]); shorter and `INFINITE` recoveries are returned
    /// for annotation of the bare call hit instead.
    ///
    /// # Arguments
    ///
    /// * `ctx` - The analysis context
    #[tracing::instrument(name = "SandboxTimeEvasion::sleep_argument_hits", skip_all, err(Debug))]
    async fn sleep_argument_hits(&self, ctx: &AnalysisContext) -> Result<SleepRecovery> {
        let mut recovery = SleepRecovery::default();
        // consider both Sleep and SleepEx imports (ANSI/Wide/decoration folded)
        for import in ctx.imports.iter().filter(|s| {
            let n = normalize(&s.name);
            n == "Sleep" || n == "SleepEx"
        }) {
            // the normalized name labels the recovered-constant finding
            let api = normalize(&import.name);
            // resolve call sites, hopping thunks to reach the staging caller
            let call_sites = ctx
                .resolve_call_sites(&import.address)
                .await
                .with_context(|| format!("resolving call sites of '{}'", import.name))?;
            for cs in call_sites {
                let site = cs.site;
                // disassemble the calling function to inspect the staged argument
                let Some((_, insns)) = ctx.disasm_containing(site).await? else {
                    continue;
                };
                let Some(idx) = insns
                    .iter()
                    .position(|i| parse_addr(&i.address) == Some(site))
                else {
                    continue;
                };
                // an invisible argument leaves the site in the bare call hit,
                // whose catalog wording already makes only the dual-use claim
                let Some(ms) = recover_sleep_ms(&insns, idx) else {
                    continue;
                };
                if is_long_sleep(ms) {
                    // a long constant earns its own named, graded finding and
                    // consumes the site so it is not double-seeded
                    recovery.hits.push(long_sleep_hit(
                        &api,
                        ms,
                        site,
                        cs.function,
                        cs.via_thunk.is_some(),
                    ));
                    recovery.covered.insert(site);
                } else {
                    // short and INFINITE delays annotate the bare hit instead
                    recovery.short.insert(site, ms.min(INFINITE_MS));
                }
            }
        }
        Ok(recovery)
    }
}

/// Whether a recovered sleep argument is a long (finite) stall candidate.
///
/// `INFINITE` is excluded: a sleep that never returns is an alertable-wait/park
/// idiom, not a delay that outlasts an analysis window.
///
/// # Arguments
///
/// * `ms` - The recovered millisecond argument
fn is_long_sleep(ms: u64) -> bool {
    // long means a finite delay at/above the long-sleep floor
    (LONG_SLEEP_MS..INFINITE_MS).contains(&ms)
}

/// Recover the `dwMilliseconds` constant staged before a sleep call site.
///
/// Walks back up to [`SLEEP_ARG_WINDOW`] instructions for the nearest
/// `push <imm>` (x86 stdcall) or `mov ecx/rcx, <imm>` (x64 fastcall); the
/// nearest staging instruction wins, which also picks the first argument over
/// the alertable flag in `SleepEx(ms, alertable)` staging.
///
/// # Arguments
///
/// * `insns` - The containing function's disassembly
/// * `idx` - The call site's index within `insns`
fn recover_sleep_ms(insns: &[Instruction], idx: usize) -> Option<u64> {
    // clamp the look-back window to the function start
    let start = idx.saturating_sub(SLEEP_ARG_WINDOW);
    // the nearest staging instruction before the call carries the argument
    insns[start..idx].iter().rev().find_map(|insn| {
        let m = insn.mnemonic.to_ascii_uppercase();
        let ops = op_str(insn).to_ascii_lowercase();
        if m == "PUSH" {
            // x86 stdcall: `push <ms>`
            parse_sleep_imm(&ops)
        } else if m == "MOV" && (ops.starts_with("ecx,") || ops.starts_with("rcx,")) {
            // x64 fastcall: `mov ecx, <ms>`
            ops.split(',').nth(1).and_then(parse_sleep_imm)
        } else {
            None
        }
    })
}

/// Parse an immediate operand to a millisecond value, honoring the sign.
///
/// A negative immediate is the 32-bit two's-complement idiom (`push -0x1` for
/// `INFINITE`), so it is wrapped into its unsigned 32-bit value.
///
/// # Arguments
///
/// * `text` - The operand token (hex `0x..`, decimal, or `-`-prefixed)
fn parse_sleep_imm(text: &str) -> Option<u64> {
    let t = text.trim();
    // `push -0x1` style negatives wrap to their 32-bit unsigned value
    if let Some(neg) = t.strip_prefix('-') {
        let magnitude = u32::try_from(parse_imm(neg)?).ok()?;
        return Some(u64::from(0u32.wrapping_sub(magnitude)));
    }
    // everything else is an ordinary unsigned immediate
    parse_imm(t)
}

/// Build the graded finding for a recovered long sleep constant.
///
/// At/above [`STALL_SLEEP_MS`] the delay has no routine retry/pacing reading,
/// so the analysis-window-stall claim keeps medium confidence; between
/// [`LONG_SLEEP_MS`] and that bar the finding seeds low and names both
/// readings (sandbox stall vs retry/beacon cadence), since disputed delays in
/// that band are routinely C2 reconnect backoff or beacon-round pauses.
///
/// # Arguments
///
/// * `api` - The normalized API name (`Sleep` or `SleepEx`)
/// * `ms` - The recovered millisecond argument
/// * `site` - The call-site address
/// * `function` - The containing function entry, when known
/// * `via_thunk` - Whether the call reached the import through a thunk stub
fn long_sleep_hit(
    api: &str,
    ms: u64,
    site: u64,
    function: Option<u64>,
    via_thunk: bool,
) -> DetectionHit {
    // render the duration in human units beside the raw constant
    let human = human_ms(ms);
    // note the thunk indirection when the sleep is reached via a stub
    let via = if via_thunk { " (via import thunk)" } else { "" };
    let content = format!("{api}({ms}) — {human}{via} @ {}", fmt_addr(site));
    // split the grade at the no-routine-reading bar
    if ms >= STALL_SLEEP_MS {
        DetectionHit::new(
            T1497_003,
            site,
            function,
            medium(),
            content,
            format!(
                "Sleeps for {ms} ms ({human}) in a single call — far past any routine retry or \
                 pacing delay. A stall this long outlasts a sandbox's analysis window so the \
                 payload only runs after monitoring stops. Defeat by patching the delay or \
                 fast-forwarding time."
            ),
        )
        .kind(EvidenceKind::ContextOnly)
    } else {
        DetectionHit::new(
            T1497_003,
            site,
            function,
            low(),
            content,
            format!(
                "Sleeps for {ms} ms ({human}). Two readings fit a delay of this size: a sandbox \
                 stall, or retry/beacon cadence — one-to-ten-minute delays in malware are \
                 routinely C2 reconnect backoff or pauses between beacon rounds. Read it as \
                 evasion only if it gates the initial payload path rather than pacing a network \
                 loop."
            ),
        )
        .kind(EvidenceKind::ContextOnly)
    }
}

/// Render a millisecond count in human units (ms, seconds, minutes, hours).
///
/// Whole counts render bare (`5 minutes`, `24 hours`); inexact ones carry one
/// decimal (`1.5 minutes`), and a sub-tenth remainder folds to the whole unit.
///
/// # Arguments
///
/// * `ms` - The millisecond count to render
fn human_ms(ms: u64) -> String {
    // pick the largest unit that fits; bare milliseconds stay as-is
    let (divisor, unit) = if ms >= 3_600_000 {
        (3_600_000, "hour")
    } else if ms >= 60_000 {
        (60_000, "minute")
    } else if ms >= 1_000 {
        (1_000, "second")
    } else {
        return format!("{ms} ms");
    };
    // split into whole units and tenths of a unit
    let whole = ms / divisor;
    let tenths = ms % divisor * 10 / divisor;
    // tenths render only when they carry information
    if tenths == 0 {
        let plural = if whole == 1 { "" } else { "s" };
        format!("{whole} {unit}{plural}")
    } else {
        format!("{whole}.{tenths} {unit}s")
    }
}

/// Dedupe bare `Sleep`/`SleepEx` call hits against the recovered constants.
///
/// Sites covered by a recovered long-sleep finding are removed from the bare
/// per-function call hit (dropping the hit when nothing remains) so each site
/// seeds exactly once, and recovered short/`INFINITE` delays are appended to
/// the surviving hit's reasoning — naming the value is worth more than a
/// duplicate finding, and the short values themselves argue for the pacing
/// reading rather than a stall.
///
/// # Arguments
///
/// * `hits` - The sleep-catalog call hits to rewrite in place
/// * `covered` - The call sites consumed by recovered-constant findings
/// * `short` - Recovered short/`INFINITE` delays by call site
fn rebuild_bare_sleep_hits(
    hits: &mut Vec<DetectionHit>,
    covered: &HashSet<u64>,
    short: &BTreeMap<u64, u64>,
) {
    hits.retain_mut(|hit| {
        // only the bare Sleep/SleepEx call-site hits participate (the census
        // hit carries no API stamp and no call sites)
        if !matches!(hit.api, Some("Sleep" | "SleepEx")) {
            return true;
        }
        // the effective backing sites (a hit with no list backs its address)
        let mut sites: Vec<u64> = if hit.sites.is_empty() {
            vec![hit.address]
        } else {
            hit.sites.clone()
        };
        // drop the sites a recovered-constant finding already seeds
        let before = sites.len();
        sites.retain(|site| !covered.contains(site));
        // every site covered: the bare hit is fully superseded
        if sites.is_empty() {
            return false;
        }
        // a reduced site list re-renders the hit around what remains
        if sites.len() != before {
            let api = hit.api.unwrap_or("Sleep");
            let thunked = hit.content.contains("(via import thunk)");
            hit.address = sites[0];
            hit.content = render_bare_sleep_content(api, thunked, &sites);
        }
        // annotate the recovered short/INFINITE delays that stay in this hit
        annotate_short_sleeps(hit, &sites, short);
        // restore the only-when-grouped site-list convention
        hit.sites = if sites.len() > 1 { sites } else { Vec::new() };
        true
    });
}

/// Re-render a bare sleep call hit's content for a reduced site list.
///
/// Mirrors the shared catalog helper's content shapes so a reduced hit stays
/// indistinguishable from a freshly built one.
///
/// # Arguments
///
/// * `api` - The normalized API name (`Sleep` or `SleepEx`)
/// * `thunked` - Whether the original hit was attributed through a thunk stub
/// * `sites` - The remaining call sites, in address order
fn render_bare_sleep_content(api: &str, thunked: bool, sites: &[u64]) -> String {
    // list the remaining call addresses, capped like the shared renderer
    let addrs = list_sites(sites);
    // reproduce the four content shapes of the shared catalog helper
    match (thunked, sites.len()) {
        (true, n) if n > 1 => format!("{api} call (via import thunk) — {n} calls @ {addrs}"),
        (true, _) => format!("{api} call (via import thunk) @ {addrs}"),
        (false, n) if n > 1 => format!("{api} — {n} calls @ {addrs}"),
        (false, _) => format!("{api} call @ {addrs}"),
    }
}

/// Render a site list, naming the first few addresses and counting the rest.
///
/// # Arguments
///
/// * `sites` - The sites to render, in address order
fn list_sites(sites: &[u64]) -> String {
    // name the first few addresses
    let listed = sites
        .iter()
        .take(MAX_BARE_SITES)
        .map(|site| fmt_addr(*site))
        .collect::<Vec<_>>()
        .join(", ");
    // no silent truncation: count what the cap left out
    match sites
        .len()
        .checked_sub(MAX_BARE_SITES)
        .filter(|extra| *extra > 0)
    {
        Some(extra) => format!("{listed} (+{extra} more; {} sites in total)", sites.len()),
        None => listed,
    }
}

/// Append the recovered short/`INFINITE` delays to a bare sleep hit's reasoning.
///
/// Sub-second recoveries get the poll/retry-pacing reading spelled out, and
/// `INFINITE` gets the alertable-wait/park reading — the hypothesis framings
/// disputing analysts otherwise write by hand.
///
/// # Arguments
///
/// * `hit` - The bare sleep hit to annotate
/// * `sites` - The hit's remaining call sites
/// * `short` - Recovered short/`INFINITE` delays by call site
fn annotate_short_sleeps(hit: &mut DetectionHit, sites: &[u64], short: &BTreeMap<u64, u64>) {
    // collect this hit's recovered (site, ms) pairs in address order
    let recovered: Vec<(u64, u64)> = sites
        .iter()
        .filter_map(|site| short.get(site).map(|ms| (*site, *ms)))
        .collect();
    // nothing recovered means nothing to add
    if recovered.is_empty() {
        return;
    }
    // render each recovered delay at its site
    let listed = recovered
        .iter()
        .map(|(site, ms)| {
            if *ms == INFINITE_MS {
                format!("INFINITE @ {}", fmt_addr(*site))
            } else {
                format!("{} @ {}", human_ms(*ms), fmt_addr(*site))
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    hit.reasoning.push_str(" Recovered delays: ");
    hit.reasoning.push_str(&listed);
    hit.reasoning.push('.');
    // an INFINITE delay is a park, not a stall — say so
    if recovered.iter().any(|(_, ms)| *ms == INFINITE_MS) {
        hit.reasoning.push_str(
            " An INFINITE delay parks the thread until an APC or alert wakes it (alertable-wait \
             idiom); it is not a finite stall that outlasts an analysis window.",
        );
    }
    // sub-second delays argue for the pacing reading — say that too
    if recovered
        .iter()
        .any(|(_, ms)| *ms != INFINITE_MS && *ms < 1_000)
    {
        hit.reasoning.push_str(
            " Sub-second delays are poll/retry pacing; they do not stall an analysis window.",
        );
    }
}

/// The per-instruction visitor behind the one shared instruction scan.
///
/// Emits use-gated `RDTSC`/`RDTSCP` observations and Linux time/sleep syscall
/// tells; everything else is left to the other sub-passes.
///
/// # Arguments
///
/// * `c` - The instruction cursor from [`scan_instructions`]
fn scan_tell(c: &InsnCtx) -> Option<DetectionHit> {
    let m = c.mnemonic();
    // RDTSC/RDTSCP — read the CPU timestamp counter directly, bypassing APIs
    if m == "RDTSC" || m == "RDTSCP" {
        return rdtsc_hit(c);
    }
    // Linux time/sleep syscalls resolved by number (stripped static ELF)
    if m == "SYSCALL" {
        return match reg_value_before(c.preceding(8), "eax", "rax") {
            // nanosleep (35) / clock_nanosleep (230)
            Some(35 | 230) => Some(
                DetectionHit::new(
                    T1497_003,
                    c.addr(),
                    Some(c.func_entry),
                    low(),
                    format!("nanosleep syscall @ {}", fmt_addr(c.addr())),
                    "Invokes (clock_)nanosleep by raw syscall number — the shape of a stripped \
                     static binary, where the libc sleep wrapper itself matches. Short delays \
                     are retry/pacing; a long delay ahead of the payload is the \
                     analysis-window-stall reading.",
                )
                .kind(EvidenceKind::ContextOnly),
            ),
            // gettimeofday (96) / clock_gettime (228) / time (201)
            Some(96 | 228 | 201) => Some(
                DetectionHit::new(
                    T1497_003,
                    c.addr(),
                    Some(c.func_entry),
                    low(),
                    format!("time syscall @ {}", fmt_addr(c.addr())),
                    "Invokes a time syscall (gettimeofday/clock_gettime/time) by raw number. A \
                     lone read is a timestamp, and a static libc time wrapper matches at both \
                     its primary and fallback syscalls; it indicates timing evasion only when \
                     two readings bracket an operation and the delta is compared.",
                )
                .kind(EvidenceKind::Timing),
            ),
            _ => None,
        };
    }
    None
}

/// Build the use-gated observation for one `RDTSC`/`RDTSCP` read.
///
/// Reads whose outputs are provably overwritten unused (VMProtect-style
/// mutation junk) emit nothing; a function whose reads bracket a region with a
/// consumed delta keeps medium confidence as the classic timing check; a lone
/// read seeds as a weak low-confidence observation. Same-class reads in one
/// function are merged later by the shared grouping pass.
///
/// # Arguments
///
/// * `c` - The instruction cursor positioned on the read
fn rdtsc_hit(c: &InsnCtx) -> Option<DetectionHit> {
    // judge what the straight-line code after the read does with EDX:EAX
    let used = tsc_use(c);
    // both outputs provably overwritten before any use: instruction-mutation
    // junk (VMProtect laces functions with dead reads), not a timing read
    if used.dead {
        return None;
    }
    // every read in the function, for the pair shape and the site roster
    let reads = rdtsc_sites(c.insns);
    let paired = reads.len() >= 2;
    // the pair's delta is judged function-wide: either read may consume it
    let delta = used.delta || (paired && function_delta(c, &reads));
    let read_list = reads
        .iter()
        .map(|(_, addr)| fmt_addr(*addr))
        .collect::<Vec<_>>()
        .join(", ");
    let addr = fmt_addr(c.addr());
    // grade by what was proven: a bracketing pair with a consumed delta is the
    // classic timing check; a lone unpaired read is only a weak observation
    let (severity, content, reasoning) = match (paired, delta) {
        (true, true) => (
            medium(),
            format!("{PAIRED_DELTA_CLASS} @ {addr}"),
            format!(
                "Executes RDTSC/RDTSCP more than once in this function (reads @ {read_list}) \
                 and a subtraction or compare consumes a reading — the timestamp-bracketing \
                 shape: the delta between two reads is checked against an expected bound, which \
                 single-stepping, emulation, or a skipped sleep shifts. Defeat by patching the \
                 comparison or normalizing the timer."
            ),
        ),
        (true, false) => (
            medium(),
            format!("Paired RDTSC reads @ {addr}"),
            format!(
                "Executes RDTSC/RDTSCP more than once in this function (reads @ {read_list}). \
                 Two reads are the bracketing shape even though no local subtraction or compare \
                 of the readings was resolved — the delta may be consumed past the inspected \
                 window or in a caller."
            ),
        ),
        (false, true) => (
            medium(),
            format!("RDTSC read with delta compare @ {addr}"),
            "Executes RDTSC and a subtraction/compare consumes the value in the following \
             instructions — consistent with a timing check against a stored earlier reading or \
             an expected bound."
                .to_string(),
        ),
        (false, false) => (
            low(),
            format!("{m} timestamp read @ {addr}", m = c.mnemonic()),
            "Reads the CPU timestamp counter once. A lone read is a weak observation — \
             timestamp, PRNG seed, or inlined library code are the routine readings; it would \
             indicate timing-based evasion only if a second read's delta were compared against \
             an expected bound, and this function contains no second read."
                .to_string(),
        ),
    };
    Some(
        DetectionHit::new(
            T1497_003,
            c.addr(),
            Some(c.func_entry),
            severity,
            content,
            reasoning,
        )
        .kind(EvidenceKind::Timing),
    )
}

/// Every `RDTSC`/`RDTSCP` instruction in a function, as `(index, address)`.
///
/// # Arguments
///
/// * `insns` - The function's instruction listing
fn rdtsc_sites(insns: &[Instruction]) -> Vec<(usize, u64)> {
    // collect each timestamp read's position and parsed address
    insns
        .iter()
        .enumerate()
        .filter(|(_, insn)| {
            matches!(
                insn.mnemonic.to_ascii_uppercase().as_str(),
                "RDTSC" | "RDTSCP"
            )
        })
        .filter_map(|(idx, insn)| parse_addr(&insn.address).map(|addr| (idx, addr)))
        .collect()
}

/// Whether any read in the function delta-consumes its TSC value.
///
/// Judges the pair shape function-wide: in a bracketing check the first read
/// is stored and only the second read's window holds the subtraction, so a
/// per-read judgement would split the pair into two differently graded hits.
///
/// # Arguments
///
/// * `c` - A cursor into the function being scanned
/// * `reads` - Every timestamp read in the function (see [`rdtsc_sites`])
fn function_delta(c: &InsnCtx, reads: &[(usize, u64)]) -> bool {
    // re-run the liveness walk at each read, looking for a consumed delta
    reads.iter().any(|(idx, _)| {
        let cursor = InsnCtx {
            func_entry: c.func_entry,
            insns: c.insns,
            idx: *idx,
        };
        tsc_use(&cursor).delta
    })
}

/// What the straight-line code after one TSC read provably does with it.
struct TscUse {
    /// A subtraction or compare consumed a read value — the delta-check shape.
    delta: bool,
    /// Both output registers were overwritten with no propagation first.
    dead: bool,
}

/// Walk the instructions after a TSC read, tracking its output registers.
///
/// The walk is bounded ([`RDTSC_USE_WINDOW`]) and stops at any control
/// transfer, so its verdicts are provable on the straight line: `dead` is set
/// only when both outputs were overwritten with no propagation first, and a
/// window that ends without evidence proves nothing (the read is then kept as
/// a weak observation).
///
/// # Arguments
///
/// * `c` - The instruction cursor positioned on the read
fn tsc_use(c: &InsnCtx) -> TscUse {
    // which output registers still hold the TSC value (EAX, EDX families)
    let families: [&[&str]; 2] = [EAX_FAMILY, EDX_FAMILY];
    let mut live = [true, true];
    let mut consumed = false;
    let mut delta = false;
    // walk the bounded window after the read
    for insn in c.following(RDTSC_USE_WINDOW) {
        let m = insn.mnemonic.to_ascii_uppercase();
        // a control transfer ends the provable straight-line window
        if m == "CALL" || m.starts_with("RET") || m.starts_with('J') {
            break;
        }
        // a second timestamp read (or CPUID) overwrites both outputs at once
        if matches!(m.as_str(), "RDTSC" | "RDTSCP" | "CPUID") {
            live = [false, false];
            continue;
        }
        // split the rendered operands once for both register families
        let ops = op_str(insn).to_ascii_lowercase();
        let parts: Vec<String> = ops.split(',').map(|p| p.trim().to_string()).collect();
        // judge each still-live output register against this instruction
        for (slot, family) in families.iter().enumerate() {
            if !live[slot] {
                continue;
            }
            match tsc_step(&m, &parts, family) {
                TscStep::Consumed { delta: step_delta } => {
                    consumed = true;
                    delta |= step_delta;
                }
                TscStep::Killed => live[slot] = false,
                TscStep::None => {}
            }
        }
    }
    // dead only when both outputs were overwritten and nothing was propagated
    TscUse {
        delta,
        dead: !consumed && !live[0] && !live[1],
    }
}

/// What one instruction provably does to a live TSC output register.
enum TscStep {
    /// No provable effect on the tracked value.
    None,
    /// The value was propagated or compared.
    Consumed {
        /// Whether a subtraction/compare consumed the value (the delta shape).
        delta: bool,
    },
    /// The register was overwritten without the value being used.
    Killed,
}

/// Judge one instruction's effect on one tracked register family.
///
/// Propagation means the value reached somewhere else: stored to memory,
/// pushed, moved or combined into another location, or consumed by a compare.
/// In-place mutation by an immediate (`SAR EAX, 0xED` and friends — the
/// `VMProtect` junk idiom) is neither a use nor a kill, while a full-width
/// overwrite from an unrelated source kills the value.
///
/// # Arguments
///
/// * `mnemonic` - The instruction mnemonic, upper-cased
/// * `parts` - The rendered operands, lower-cased and split on commas
/// * `family` - The tracked register's alias names, widest first
fn tsc_step(mnemonic: &str, parts: &[String], family: &[&str]) -> TscStep {
    // classify the destination operand and whether any source reads the value
    let dest = parts.first().map_or("", String::as_str);
    let dest_is_family_reg = family.contains(&dest);
    let dest_full_overwrite = dest == family[0] || dest == family[1];
    let src_mentions = parts.iter().skip(1).any(|p| mentions_family(p, family));
    match mnemonic {
        // read-only consumers: a compare consuming the value is the delta shape
        "CMP" | "TEST" | "BT" => {
            if parts.iter().any(|p| mentions_family(p, family)) {
                TscStep::Consumed {
                    delta: mnemonic == "CMP",
                }
            } else {
                TscStep::None
            }
        }
        // a push propagates the value to the stack
        "PUSH" => {
            if mentions_family(dest, family) {
                TscStep::Consumed { delta: false }
            } else {
                TscStep::None
            }
        }
        // moves: a source mention propagates (register move or memory store);
        // a full-width overwrite from elsewhere kills
        "MOV" | "MOVZX" | "MOVSX" | "MOVSXD" => {
            if src_mentions || (!dest_is_family_reg && mentions_family(dest, family)) {
                TscStep::Consumed { delta: false }
            } else if dest_full_overwrite {
                TscStep::Killed
            } else {
                TscStep::None
            }
        }
        // a pop overwrites its destination register
        "POP" => {
            if dest_full_overwrite {
                TscStep::Killed
            } else {
                TscStep::None
            }
        }
        // lea: address arithmetic reading the value propagates it
        "LEA" => {
            if src_mentions {
                TscStep::Consumed { delta: false }
            } else if dest_full_overwrite {
                TscStep::Killed
            } else {
                TscStep::None
            }
        }
        // two-operand arithmetic: combining with another value is a use, but
        // in-place mutation by an immediate (the VMP junk idiom) is neither a
        // use nor a kill
        "ADD" | "SUB" | "ADC" | "SBB" | "AND" | "OR" | "XOR" | "SHL" | "SHR" | "SAR" | "ROL"
        | "ROR" | "IMUL" => {
            // `xor reg, reg` zeroes the register: a kill, not a combine
            if mnemonic == "XOR" && dest_is_family_reg && parts.get(1).is_some_and(|s| s == dest) {
                return if dest_full_overwrite {
                    TscStep::Killed
                } else {
                    TscStep::None
                };
            }
            // the second operand decides: immediate = mutation, value = combine
            let src_is_imm = parts.get(1).is_none_or(|s| parse_sleep_imm(s).is_some());
            if (dest_is_family_reg && !src_is_imm) || (!dest_is_family_reg && src_mentions) {
                TscStep::Consumed {
                    delta: matches!(mnemonic, "SUB" | "SBB"),
                }
            } else {
                TscStep::None
            }
        }
        // single-operand in-place mutations leave the tracked value in place
        "NEG" | "NOT" | "INC" | "DEC" => TscStep::None,
        // any other instruction reading the value is conservatively a use
        _ => {
            if src_mentions || (!dest_is_family_reg && mentions_family(dest, family)) {
                TscStep::Consumed { delta: false }
            } else {
                TscStep::None
            }
        }
    }
}

/// Whether an operand expression mentions any register in a family.
///
/// Tokenizes on non-alphanumerics so `[rax+8]` matches `rax` while `0xed`
/// never matches `ax` as a substring.
///
/// # Arguments
///
/// * `part` - One rendered operand expression, lower-cased
/// * `family` - The register alias names to look for
fn mentions_family(part: &str, family: &[&str]) -> bool {
    // compare whole alphanumeric tokens so hex digits never alias registers
    family.iter().any(|reg| {
        part.split(|ch: char| !ch.is_ascii_alphanumeric())
            .any(|tok| tok == *reg)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::thorpal::detect::confidence::Confidence;

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

    /// Run the scan visitor over a fixture function, collecting its hits.
    ///
    /// # Arguments
    ///
    /// * `entry` - The fixture function's entry address
    /// * `insns` - The fixture function's instructions
    fn scan_fixture(entry: u64, insns: &[Instruction]) -> Vec<DetectionHit> {
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

    /// Build a bare sleep-catalog hit shaped like the shared helper's output.
    ///
    /// # Arguments
    ///
    /// * `api` - The normalized API name (`Sleep` or `SleepEx`)
    /// * `function` - The containing function entry
    /// * `sites` - The call sites backing the hit
    fn bare_sleep_hit(api: &'static str, function: u64, sites: &[u64]) -> DetectionHit {
        // mirror the shared helper: first site primary, list only when grouped
        let mut hit = DetectionHit::new(
            T1497_003,
            sites[0],
            Some(function),
            low(),
            render_bare_sleep_content(api, false, sites),
            "catalog reasoning".to_string(),
        )
        .kind(EvidenceKind::ContextOnly)
        .api(api);
        if sites.len() > 1 {
            hit = hit.sites(sites.to_vec());
        }
        hit
    }

    /// The byte-proven-dead `VMProtect` mutation-junk read (both outputs
    /// overwritten before any use) emits no hit at all.
    #[test]
    fn vmp_dead_rdtsc_junk_no_longer_seeds() {
        // the flagship dead site: RDTSC @ 0x235e87841, SAR EAX,0xED in-place,
        // then both result registers overwritten with no propagation
        let insns = vec![
            insn(0x2_35e8_7841, "RDTSC", &[]),
            insn(0x2_35e8_7843, "SAR", &["EAX", "0xed"]),
            insn(0x2_35e8_7846, "MOVZX", &["EAX", "CX"]),
            insn(0x2_35e8_7849, "SAR", &["EDX", "0x5"]),
            insn(0x2_35e8_784c, "MOV", &["EDX", "ESI"]),
            insn(0x2_35e8_784e, "RET", &[]),
        ];
        let hits = scan_fixture(0x2_35e8_7800, &insns);
        assert!(hits.is_empty(), "dead mutation junk must emit nothing");
    }

    /// A real two-read bracket with a consumed delta merges into one medium
    /// pair finding naming both reads.
    #[test]
    fn paired_rdtsc_delta_bracket_keeps_medium() {
        // the classic shape: save the first read, subtract it from the second,
        // compare the delta against a bound, branch
        let insns = vec![
            insn(0x1000, "RDTSC", &[]),
            insn(0x1002, "MOV", &["ESI", "EAX"]),
            insn(0x1004, "NOP", &[]),
            insn(0x1005, "RDTSC", &[]),
            insn(0x1007, "SUB", &["EAX", "ESI"]),
            insn(0x1009, "CMP", &["EAX", "0x1000"]),
            insn(0x100c, "JA", &["0x2000"]),
            insn(0x100e, "RET", &[]),
        ];
        let scanned = scan_fixture(0x1000, &insns);
        // both reads classify as the delta-compared pair (function-wide delta)
        assert_eq!(scanned.len(), 2);
        for hit in &scanned {
            assert_eq!(content_class(&hit.content), PAIRED_DELTA_CLASS);
            assert!(matches!(hit.confidence, Confidence::Unsure));
        }
        // the grouping pass merges the pair into one finding naming both reads
        let grouped = group_scan_hits(scanned, usize::MAX, |_| false);
        assert_eq!(grouped.len(), 1);
        let merged = &grouped[0];
        assert!(matches!(merged.confidence, Confidence::Unsure));
        assert!(merged.content.contains("0x1000") && merged.content.contains("0x1005"));
        assert_eq!(merged.sites, vec![0x1000, 0x1005]);
    }

    /// A lone propagated read seeds as a weak low-confidence observation with
    /// hypothesis wording.
    #[test]
    fn lone_rdtsc_demotes_to_weak_observation() {
        // one read, stored to memory, never read again
        let insns = vec![
            insn(0x3000, "RDTSC", &[]),
            insn(0x3002, "MOV", &["dword ptr [0x5000]", "EAX"]),
            insn(0x3008, "RET", &[]),
        ];
        let hits = scan_fixture(0x3000, &insns);
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert!(matches!(hit.confidence, Confidence::Untrusted));
        assert_eq!(content_class(&hit.content), "RDTSC timestamp read");
        assert!(
            hit.reasoning.contains("weak observation"),
            "{}",
            hit.reasoning
        );
        assert!(hit.reasoning.contains("only if"), "{}", hit.reasoning);
    }

    /// In-place immediate mutation is neither a use nor a kill, while register
    /// combines, stores, and compares count as consumption.
    #[test]
    fn tsc_step_counts_combines_not_imm_mutation() {
        // the VMP junk mutation: SAR by an immediate, in place
        let sar = vec!["eax".to_string(), "0xed".to_string()];
        assert!(matches!(tsc_step("SAR", &sar, EAX_FAMILY), TscStep::None));
        // subtracting another register is the delta combine
        let sub = vec!["eax".to_string(), "esi".to_string()];
        assert!(matches!(
            tsc_step("SUB", &sub, EAX_FAMILY),
            TscStep::Consumed { delta: true }
        ));
        // a register move propagates the value
        let mov = vec!["esi".to_string(), "eax".to_string()];
        assert!(matches!(
            tsc_step("MOV", &mov, EAX_FAMILY),
            TscStep::Consumed { delta: false }
        ));
        // a full-width overwrite from an unrelated source kills it
        let movzx = vec!["eax".to_string(), "cx".to_string()];
        assert!(matches!(
            tsc_step("MOVZX", &movzx, EAX_FAMILY),
            TscStep::Killed
        ));
        // zeroing a register is a kill, not a combine
        let xor = vec!["edx".to_string(), "edx".to_string()];
        assert!(matches!(tsc_step("XOR", &xor, EDX_FAMILY), TscStep::Killed));
        // comparing against an immediate consumes the value as a delta check
        let cmp = vec!["eax".to_string(), "0x1000".to_string()];
        assert!(matches!(
            tsc_step("CMP", &cmp, EAX_FAMILY),
            TscStep::Consumed { delta: true }
        ));
    }

    /// Argument recovery reads x86 pushes, x64 moves, and the INFINITE idiom.
    #[test]
    fn recover_sleep_ms_reads_push_mov_and_infinite() {
        // x86 stdcall: push 0x493e0 (300000 ms) before the call @ 0x40151f
        let x86 = vec![
            insn(0x0040_1519, "PUSH", &["0x493e0"]),
            insn(0x0040_151f, "CALL", &["dword ptr [0x403000]"]),
        ];
        assert_eq!(recover_sleep_ms(&x86, 1), Some(300_000));
        // x64 fastcall: mov ecx, 0x1d4c0 (120000 ms) before the call
        let x64 = vec![
            insn(0x1_8000_4652, "MOV", &["ECX", "0x1d4c0"]),
            insn(0x1_8000_4658, "CALL", &["qword ptr [0x180010000]"]),
        ];
        assert_eq!(recover_sleep_ms(&x64, 1), Some(120_000));
        // the alertable APC park: push 1 (alertable), push -1 (INFINITE) — the
        // nearest push is the millisecond argument
        let park = vec![
            insn(0x5000, "PUSH", &["0x1"]),
            insn(0x5002, "PUSH", &["-0x1"]),
            insn(0x5004, "CALL", &["dword ptr [0x403004]"]),
        ];
        assert_eq!(recover_sleep_ms(&park, 2), Some(INFINITE_MS));
        // INFINITE is never a long-sleep stall candidate
        assert!(!is_long_sleep(INFINITE_MS));
        assert!(!is_long_sleep(59_999));
        assert!(is_long_sleep(60_000));
        assert!(is_long_sleep(86_400_000));
    }

    /// The threshold split keeps the 24-hour stall at medium and demotes the
    /// minute-scale delays to low with both readings named.
    #[test]
    fn long_sleep_threshold_split_grades_by_duration() {
        // the flagship confirmed stall: 86,400,000 ms keeps the medium claim
        let day = long_sleep_hit("Sleep", 86_400_000, 0x0040_1000, Some(0x0040_0f00), false);
        assert!(matches!(day.confidence, Confidence::Unsure));
        assert!(
            day.content.starts_with("Sleep(86400000) — 24 hours"),
            "{}",
            day.content
        );
        assert!(
            day.reasoning.contains("analysis window"),
            "{}",
            day.reasoning
        );
        // the disputed 5-minute beacon pause demotes to low with both readings
        let pause = long_sleep_hit("Sleep", 300_000, 0x0040_151f, Some(0x0040_1490), false);
        assert!(matches!(pause.confidence, Confidence::Untrusted));
        assert!(
            pause.content.starts_with("Sleep(300000) — 5 minutes"),
            "{}",
            pause.content
        );
        assert!(pause.reasoning.contains("beacon"), "{}", pause.reasoning);
        assert!(
            pause.reasoning.contains("sandbox stall"),
            "{}",
            pause.reasoning
        );
        // the disputed 2-minute C2 backoff demotes the same way
        let backoff = long_sleep_hit("Sleep", 120_000, 0x1_8000_4658, Some(0x1_8000_4600), false);
        assert!(matches!(backoff.confidence, Confidence::Untrusted));
        assert!(
            backoff.content.starts_with("Sleep(120000) — 2 minutes"),
            "{}",
            backoff.content
        );
    }

    /// A recovered long-sleep site is removed from the bare call hit so each
    /// site seeds once, and a fully covered bare hit is dropped.
    #[test]
    fn covered_long_sleep_site_leaves_the_bare_hit() {
        // the disputed double-seeded pair: Sleep — 2 calls @ 0x40151f, 0x40153d
        let mut hits = vec![bare_sleep_hit(
            "Sleep",
            0x0040_1490,
            &[0x0040_151f, 0x0040_153d],
        )];
        let covered: HashSet<u64> = [0x0040_151f].into_iter().collect();
        rebuild_bare_sleep_hits(&mut hits, &covered, &BTreeMap::new());
        // the bare hit now backs only the unrecovered site
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].address, 0x0040_153d);
        assert_eq!(hits[0].content, "Sleep call @ 0x40153d");
        assert!(hits[0].sites.is_empty(), "a lone site carries no list");
        // covering every site supersedes the bare hit entirely
        let mut all = vec![bare_sleep_hit(
            "Sleep",
            0x0040_1490,
            &[0x0040_151f, 0x0040_153d],
        )];
        let both: HashSet<u64> = [0x0040_151f, 0x0040_153d].into_iter().collect();
        rebuild_bare_sleep_hits(&mut all, &both, &BTreeMap::new());
        assert!(all.is_empty(), "a fully covered bare hit is dropped");
    }

    /// Recovered short and INFINITE delays annotate the bare hit with the
    /// pacing/park readings instead of seeding duplicate findings.
    #[test]
    fn short_recoveries_annotate_the_bare_sleep_hit() {
        // the disputed malloc-retry/reconnect trio: 10 ms, 10 ms, 10 s
        let mut hits = vec![bare_sleep_hit(
            "Sleep",
            0x0040_3790,
            &[0x0040_37cb, 0x0040_37ee, 0x0040_3a11],
        )];
        let short: BTreeMap<u64, u64> =
            [(0x0040_37cb, 10), (0x0040_37ee, 10), (0x0040_3a11, 10_000)]
                .into_iter()
                .collect();
        rebuild_bare_sleep_hits(&mut hits, &HashSet::new(), &short);
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert!(
            hit.reasoning.contains("10 ms @ 0x4037cb"),
            "{}",
            hit.reasoning
        );
        assert!(
            hit.reasoning.contains("10 seconds @ 0x403a11"),
            "{}",
            hit.reasoning
        );
        assert!(
            hit.reasoning.contains("poll/retry pacing"),
            "{}",
            hit.reasoning
        );
        // the alertable APC park: SleepEx(INFINITE) is named, not graded a stall
        let mut park = vec![bare_sleep_hit("SleepEx", 0x5000, &[0x5004])];
        let infinite: BTreeMap<u64, u64> = [(0x5004, INFINITE_MS)].into_iter().collect();
        rebuild_bare_sleep_hits(&mut park, &HashSet::new(), &infinite);
        assert_eq!(park.len(), 1);
        assert!(
            park[0].reasoning.contains("INFINITE @ 0x5004"),
            "{}",
            park[0].reasoning
        );
        assert!(
            park[0].reasoning.contains("alertable-wait"),
            "{}",
            park[0].reasoning
        );
        assert!(matches!(park[0].confidence, Confidence::Untrusted));
    }

    /// A static libc wrapper's primary and fallback time syscalls group into
    /// one finding instead of two.
    #[test]
    fn musl_fallback_syscalls_group_to_one_hit() {
        // the musl clock_gettime shape: syscall 228, then the ENOSYS 96 fallback
        let insns = vec![
            insn(0x1b_0d4a, "MOV", &["EAX", "0xe4"]),
            insn(0x1b_0d4f, "SYSCALL", &[]),
            insn(0x1b_0d51, "CMP", &["EAX", "-0x26"]),
            insn(0x1b_0d54, "JNZ", &["0x1b0d70"]),
            insn(0x1b_0d56, "MOV", &["EAX", "0x60"]),
            insn(0x1b_0d5b, "SYSCALL", &[]),
            insn(0x1b_0d5d, "RET", &[]),
        ];
        let scanned = scan_fixture(0x1b_0d00, &insns);
        // both syscalls are the same observation class in the same function
        assert_eq!(scanned.len(), 2);
        let grouped = group_scan_hits(scanned, GENERIC_TELL_CAP, |hit| hit.function.is_some());
        assert_eq!(grouped.len(), 1);
        assert!(
            grouped[0].content.starts_with("time syscall — 2 sites"),
            "{}",
            grouped[0].content
        );
        assert_eq!(grouped[0].sites, vec![0x1b_0d4f, 0x1b_0d5b]);
    }

    /// The per-program cap keeps the strongest tells individual (the confirmed
    /// `GetTickCount` cluster shape among them) and folds the rest into one
    /// counted summary.
    #[test]
    fn generic_tell_cap_folds_overflow_into_summary() {
        // the confirmed-shape grouped cluster: GetTickCount — 3 calls, the
        // strongest low tell by site count
        let mut generic = vec![
            DetectionHit::new(
                T1497_003,
                0x0040_1398,
                Some(0x0040_1370),
                low(),
                "GetTickCount — 3 calls @ 0x401398, 0x4013a4, 0x4013b0".to_string(),
                "catalog reasoning".to_string(),
            )
            .sites(vec![0x0040_1398, 0x0040_13a4, 0x0040_13b0]),
        ];
        // a VMP-style flood of lone reads across many functions
        for i in 0..u64::try_from(GENERIC_TELL_CAP).unwrap() + 2 {
            let addr = 0x50_0000 + i * 0x100;
            generic.push(DetectionHit::new(
                T1497_003,
                addr,
                Some(addr - 0x10),
                low(),
                format!("RDTSC timestamp read @ {}", fmt_addr(addr)),
                "weak observation".to_string(),
            ));
        }
        // a file-level census line is never a cap target
        generic.push(DetectionHit::new(
            T1497_003,
            0,
            None,
            low(),
            "2 catalog APIs imported but never referenced from code: Sleep, GetTickCount"
                .to_string(),
            "census reasoning".to_string(),
        ));
        let kept = group_scan_hits(generic, GENERIC_TELL_CAP, |hit| hit.function.is_some());
        // the cap keeps GENERIC_TELL_CAP tells + the census line + one summary
        assert_eq!(kept.len(), GENERIC_TELL_CAP + 2);
        // the confirmed cluster shape survives at its stated strength
        let cluster = kept
            .iter()
            .find(|hit| hit.content.starts_with("GetTickCount"))
            .expect("the GetTickCount cluster still seeds");
        assert!(matches!(cluster.confidence, Confidence::Untrusted));
        // the overflow is counted out loud, not dropped
        assert!(
            kept.iter()
                .any(|hit| hit.function.is_none() && hit.content.contains("generic tells elided"))
        );
    }

    /// Catalog wordings state the capability plus the condition that would
    /// make it evasion, and keep the asserted severities.
    #[test]
    fn catalog_wordings_are_condition_form() {
        // every timing entry is low and names its evasion condition
        for spec in TIMING_APIS {
            assert!(
                matches!(spec.severity.0, Confidence::Untrusted),
                "{} severity",
                spec.key
            );
            assert_eq!(spec.severity.1, low().1, "{} suspicion", spec.key);
            assert!(spec.reasoning.contains("only"), "{} wording", spec.key);
        }
        // the sleep entries make the dual-use reading explicit
        let sleep = SLEEP_APIS.iter().find(|s| s.key == "Sleep").unwrap();
        assert!(sleep.reasoning.contains("dual-use"), "{}", sleep.reasoning);
        let sleep_ex = SLEEP_APIS.iter().find(|s| s.key == "SleepEx").unwrap();
        assert!(
            sleep_ex.reasoning.contains("INFINITE"),
            "{}",
            sleep_ex.reasoning
        );
        // the native delay primitive keeps its stronger catalog grade
        let delay = SLEEP_APIS
            .iter()
            .find(|s| s.key == "NtDelayExecution")
            .unwrap();
        assert!(matches!(delay.severity.0, Confidence::Unsure));
        assert_eq!(delay.severity.1, medium().1);
    }

    /// Millisecond durations render in human units beside the raw constant.
    #[test]
    fn human_ms_renders_units() {
        assert_eq!(human_ms(10), "10 ms");
        assert_eq!(human_ms(10_000), "10 seconds");
        assert_eq!(human_ms(60_000), "1 minute");
        assert_eq!(human_ms(90_000), "1.5 minutes");
        assert_eq!(human_ms(120_000), "2 minutes");
        assert_eq!(human_ms(300_000), "5 minutes");
        assert_eq!(human_ms(86_400_000), "24 hours");
    }
}
