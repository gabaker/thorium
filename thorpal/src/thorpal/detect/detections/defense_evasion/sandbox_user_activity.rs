//! Virtualization/Sandbox Evasion — User Activity Based Checks (MITRE ATT&CK **T1497.002**).
//!
//! Flags call sites of the Win32 user-interaction APIs (cursor and key state,
//! idle time, input hooks, foreground window, screen metrics) that malware can
//! poll to decide whether a real human is present before acting. These APIs are
//! also bread-and-butter GUI plumbing, keylogger/hVNC tracking primitives, and
//! telemetry sources, so every catalog entry states the observed *capability*
//! plus the condition under which the sandbox-gate reading would hold (a result
//! that feeds a comparison and branch) and names the common alternative uses —
//! presence alone proves none of them. The post-detection validator
//! (`crate::thorpal::detect::validate`) checks result use at each site and
//! recovers the `GetSystemMetrics` index and `SetWindowsHookEx` idHook
//! arguments, annotating or rewording these findings accordingly.
//!
//! Local shaping keeps the catalog's noise floor down:
//!
//! 1. Three or more distinct input-capture APIs resolving into one function
//!    re-tag as a single Input Capture: Keylogging (T1056.001) finding — the
//!    cluster is the classic capture poll-loop shape.
//! 2. `GetForegroundWindow`/`GetCursorPos` hits split by shape: repeated
//!    sampling (or a re-sampling loop around a lone site) keeps the catalog
//!    grade, while a lone call demotes to a weak observation with the catalog
//!    grade kept aside for the validator to restore on a proven gate.
//! 3. An API fanning out across more functions than a targeted check plausibly
//!    occupies demotes to a rollup-grade ubiquitous-GUI-usage observation.
//! 4. Floor-grade observations are capped per program via
//!    [`group_scan_hits`]; the overflow folds into one summary hit.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;

use anyhow::Result;
use ghidra_cli::models::Instruction;

use crate::thorpal::detect::confidence::Confidence;
use crate::thorpal::detect::context::{AnalysisContext, fmt_addr, parse_addr};
use crate::thorpal::detect::detections::{
    ApiSpec, AttackTactic, AttackTechnique, Detection, DetectionHit, EvidenceKind, api_call_hits,
    group_scan_hits, high, low, medium,
};
use crate::thorpal::detect::op_str;

use super::T1497_002;

/// The User Activity Based Checks detection (ATT&CK T1497.002).
pub struct SandboxUserActivity;

/// ATT&CK T1056.001 — Input Capture: Keylogging.
///
/// The re-tag target for per-function input-API clusters: when one function
/// resolves several distinct input-capture APIs, the capture claim fits what
/// agents actually find there better than per-API sandbox-evasion claims do.
const T1056_001: AttackTechnique = AttackTechnique {
    id: Cow::Borrowed("T1056.001"),
    name: Cow::Borrowed("Input Capture: Keylogging"),
    tactic: AttackTactic::Collection,
};

/// User-interaction APIs a sample can poll to detect an unattended (sandbox)
/// environment — reported as capabilities, with the gate reading conditional.
const APIS: &[ApiSpec] = &[
    ApiSpec {
        key: "GetCursorPos",
        always_noise: false,
        severity: medium(),
        reasoning: "Samples the mouse cursor position (GetCursorPos). Read repeatedly and \
                    compared against a previous sample, this is the classic 'has the mouse \
                    moved' probe of an unattended sandbox — but only a result that feeds such \
                    a comparison supports that reading. The same call is everyday GUI plumbing \
                    (hit-testing, cursor-follow drawing), the mouse-move trigger in \
                    keylogger/hVNC tracking loops, and occasionally just a bounded startup \
                    delay.",
    },
    ApiSpec {
        key: "mouse_event",
        always_noise: false,
        severity: medium(),
        reasoning: "Synthesizes mouse input (mouse_event). Paired with cursor sampling it can \
                    fabricate or probe for the user activity an unattended sandbox lacks; on \
                    its own it is also how installers, UI automation, and accessibility tools \
                    click and move the pointer. Treat it as corroborating an activity check \
                    only when the surrounding code reads input state back.",
    },
    ApiSpec {
        key: "GetAsyncKeyState",
        always_noise: false,
        severity: medium(),
        reasoning: "Polls asynchronous key state (GetAsyncKeyState). Gating on 'any key seen' \
                    is a user-presence check; sweeping many virtual keys in a loop is \
                    keystroke capture (T1056.001, the more common hostile use); hotkey \
                    handling in GUI code is the common benign one. Which claim holds depends \
                    on the keys polled and what the result gates — this flag alone asserts \
                    only the capability.",
    },
    ApiSpec {
        key: "GetKeyState",
        always_noise: false,
        severity: medium(),
        reasoning: "Reads keyboard state (GetKeyState). A gate on real input would branch on \
                    the result, but the API's bread-and-butter uses are modifier-state \
                    handling in window procedures (Shift/Ctrl decisions in framework message \
                    handlers) and key capture in keyloggers — the surrounding comparison, not \
                    the call, decides which reading is right.",
    },
    ApiSpec {
        key: "GetLastInputInfo",
        always_noise: false,
        severity: high(),
        reasoning: "Measures idle time since the last user input (GetLastInputInfo). Compared \
                    against a threshold to gate behavior it is a strong unattended-environment \
                    check; formatted into output instead it is idle telemetry (a beacon's \
                    idle-seconds field), and screensaver-style benign logic reads it too. Only \
                    a result that steers a branch supports the sandbox-gate reading.",
    },
    ApiSpec {
        key: "SetWindowsHookEx",
        always_noise: false,
        severity: medium(),
        reasoning: "Installs a Windows hook (SetWindowsHookEx). Only the input-capture hook \
                    types (WH_KEYBOARD/WH_KEYBOARD_LL, WH_MOUSE/WH_MOUSE_LL) can watch for \
                    real user activity — and those same hooks are the standard keylogging \
                    primitive (T1056.001) and even an entropy source; a remote-thread \
                    WH_GETMESSAGE/WH_CBT hook is DLL injection (T1055.011), and WH_MSGFILTER \
                    is routine framework message filtering. The recovered idHook, where \
                    available, decides which of these this call is.",
    },
    ApiSpec {
        key: "GetForegroundWindow",
        always_noise: false,
        severity: medium(),
        reasoning: "Reads the current foreground window (GetForegroundWindow). Polled and \
                    compared for change it can serve as a user-activity probe, but the common \
                    real uses are different: foreground-window tracking with window-title \
                    logging in keyloggers and hVNC implants, focus checks in ordinary GUI \
                    code, and helpers that force the foreground rather than test it. The claim \
                    rests on what the result gates, not on the call being present.",
    },
    ApiSpec {
        key: "GetSystemMetrics",
        always_noise: false,
        severity: low(),
        reasoning: "Reads a system metric (GetSystemMetrics); which metric the index selects \
                    decides what, if anything, this says. SM_CXSCREEN/SM_CYSCREEN (0/1), \
                    SM_MOUSEPRESENT (19), SM_CMONITORS (80), and SM_REMOTESESSION (0x1000) \
                    are the sandbox-relevant probes (small, headless, or remote displays); \
                    the hundreds of other indices are routine UI-layout, locale, and \
                    OS-version lookups (icon and scrollbar sizes, DBCS/MidEast flags, \
                    SM_SERVERR2). Where the call site's index was recovered it is named on \
                    this finding; without it, this is an unresolved metric read, not evidence \
                    of a resolution check.",
    },
    ApiSpec {
        key: "EnumDisplaySettings",
        always_noise: false,
        severity: low(),
        reasoning: "Enumerates display modes (EnumDisplaySettings). Comparing the current mode \
                    against the small/default resolutions of headless sandboxes is one use; \
                    resolution pickers, installers, and screen-capture code enumerate the same \
                    way. A sandbox reading needs the returned mode to feed a comparison that \
                    gates behavior.",
    },
];

/// The catalog APIs whose per-function clustering marks input capture.
///
/// Three or more of these resolving into one function is the classic capture
/// poll-loop shape (foreground tracking plus cursor/key sampling or an input
/// hook), so that function's per-API hits re-tag as one keylogging finding.
const INPUT_CAPTURE_APIS: &[&str] = &[
    "GetForegroundWindow",
    "GetCursorPos",
    "GetAsyncKeyState",
    "GetKeyState",
    "SetWindowsHookEx",
];

/// How many distinct input-capture APIs one function must resolve to re-tag.
const CLUSTER_MIN: usize = 3;

/// The APIs whose idle-probe reading needs a polling shape, not a lone call.
const POLLED_APIS: &[&str] = &["GetForegroundWindow", "GetCursorPos"];

/// How many instructions past a call site the re-sampling-loop scan reads.
const POLL_WINDOW: usize = 16;

/// The most sites named in a cluster hit's content before eliding the rest.
const MAX_CLUSTER_SITES: usize = 6;

/// Distinct calling functions above which an API counts as ubiquitous GUI use.
const UBIQUITY_FANOUT: usize = 5;

/// The most floor-grade observations kept as individual findings per program.
const GENERIC_HIT_CAP: usize = 8;

/// Fold per-function clusters of distinct input-capture APIs into keylogging hits.
///
/// When one function resolves [`CLUSTER_MIN`] or more distinct
/// [`INPUT_CAPTURE_APIS`], its per-API T1497.002 hits are replaced by a single
/// T1056.001 Input Capture hit listing every site: the cluster is the classic
/// capture poll-loop shape, and one combined claim matches what agents actually
/// conclude about such functions. Hits outside the capture set (idle time,
/// screen metrics) and functions below the bar keep their per-API findings.
///
/// # Arguments
///
/// * `hits` - The catalog call-site hits to re-shape
fn retag_input_clusters(hits: Vec<DetectionHit>) -> Vec<DetectionHit> {
    // count each function's distinct capture APIs across every hit first
    let mut members: BTreeMap<u64, BTreeSet<&'static str>> = BTreeMap::new();
    for hit in &hits {
        if let (Some(api), Some(function)) = (hit.api, hit.function)
            && INPUT_CAPTURE_APIS.contains(&api)
        {
            members.entry(function).or_default().insert(api);
        }
    }
    // only functions clearing the cluster bar re-tag
    members.retain(|_, apis| apis.len() >= CLUSTER_MIN);
    // divert cluster members to their function's accumulator, keep the rest
    let mut kept: Vec<DetectionHit> = Vec::new();
    let mut groups: BTreeMap<u64, Vec<DetectionHit>> = BTreeMap::new();
    for hit in hits {
        // a hit joins a cluster only when it is a capture API inside a
        // function that cleared the bar
        let clustered = match (hit.api, hit.function) {
            (Some(api), Some(function)) => {
                INPUT_CAPTURE_APIS.contains(&api) && members.contains_key(&function)
            }
            _ => false,
        };
        if clustered {
            // the function is present by the clustered test above
            groups
                .entry(hit.function.unwrap_or_default())
                .or_default()
                .push(hit);
        } else {
            kept.push(hit);
        }
    }
    // build one combined keylogging hit per clustered function
    for (function, group) in groups {
        kept.push(cluster_hit(function, &group));
    }
    kept
}

/// Build the one keylogging hit replacing a function's input-API cluster.
///
/// # Arguments
///
/// * `function` - The clustered function's entry address
/// * `group` - That function's per-API capture hits
fn cluster_hit(function: u64, group: &[DetectionHit]) -> DetectionHit {
    // name the distinct APIs in a stable order
    let apis: BTreeSet<&'static str> = group.iter().filter_map(|hit| hit.api).collect();
    let names = apis.iter().copied().collect::<Vec<_>>().join(", ");
    // gather every member's sites (a hit with no list backs just its address)
    let mut sites: Vec<u64> = group
        .iter()
        .flat_map(|hit| {
            if hit.sites.is_empty() {
                vec![hit.address]
            } else {
                hit.sites.clone()
            }
        })
        .collect();
    // sort + dedupe so the rendered site list is stable and free of repeats
    sites.sort_unstable();
    sites.dedup();
    // render the first few sites and count the rest
    let content = format!(
        "Input-capture API cluster ({names}) — {} sites @ {}",
        sites.len(),
        render_sites_capped(&sites)
    );
    let reasoning = format!(
        "This function resolves {} distinct user-input APIs ({names}). Clustered \
         foreground/cursor/key-state sampling in one function is the classic input-capture \
         poll-loop shape — how keyloggers and hVNC implants watch the user — so this one \
         combined T1056.001 claim replaces the function's per-API sandbox-evasion findings. \
         A user-activity gauntlet gating detonation on real input clusters the same APIs, \
         and so can a busy benign window procedure; whichever reading holds, this function \
         is where the evidence sits.",
        apis.len()
    );
    // the combined hit keeps the result-use contract so the validator still
    // judges the sites (a provably discarded junk-API salad weakens it)
    let primary = sites.first().copied().unwrap_or_default();
    DetectionHit::new(
        T1056_001,
        primary,
        Some(function),
        medium(),
        content,
        reasoning,
    )
    .kind(EvidenceKind::ApiResultCheck)
    .sites(sites)
}

/// Render up to [`MAX_CLUSTER_SITES`] site addresses, counting the overflow.
///
/// # Arguments
///
/// * `sites` - The sorted, deduped evidence sites
fn render_sites_capped(sites: &[u64]) -> String {
    // name the first few sites in report style
    let listed = sites
        .iter()
        .take(MAX_CLUSTER_SITES)
        .map(|site| fmt_addr(*site))
        .collect::<Vec<_>>()
        .join(", ");
    // no silent truncation: say how many sites were left out
    match sites
        .len()
        .checked_sub(MAX_CLUSTER_SITES)
        .filter(|extra| *extra > 0)
    {
        Some(extra) => format!("{listed} (+{extra} more)"),
        None => listed,
    }
}

/// Split the polled APIs' hits by shape: repeated sampling vs a lone call.
///
/// A `GetForegroundWindow`/`GetCursorPos` idle probe re-samples: either the
/// function calls the API at several sites, or a lone site sits in a loop that
/// compares and jumps back to sample again. Those shapes keep the catalog
/// grade (and say so). A lone call with neither shape demotes to a weak
/// observation, with the catalog grade kept aside so the validation pass
/// (`crate::thorpal::detect::validate`) restores it when the result provably
/// gates a branch — the shape a confirmed activity gate shows even at a single
/// site.
///
/// # Arguments
///
/// * `listings` - The per-function disassembly, keyed by entry (may be empty)
/// * `hits` - The hits to classify in place
fn classify_poll_shapes(listings: &HashMap<u64, Vec<Instruction>>, hits: &mut [DetectionHit]) {
    // total the call sites per (API, function) so thunked and direct hits of
    // one API in one function count as the same sampling behavior
    let mut counts: HashMap<(&'static str, u64), usize> = HashMap::new();
    for hit in hits.iter() {
        if let (Some(api), Some(function)) = (hit.api, hit.function)
            && POLLED_APIS.contains(&api)
        {
            *counts.entry((api, function)).or_default() += hit.sites.len().max(1);
        }
    }
    // classify each polled-API hit by its function's sampling shape
    for hit in hits.iter_mut() {
        // only per-function hits of the polled APIs are shaped here
        let (Some(api), Some(function)) = (hit.api, hit.function) else {
            continue;
        };
        if !POLLED_APIS.contains(&api) {
            continue;
        }
        // repeated sampling needs no disassembly; a lone site needs the loop scan
        let repeated = counts.get(&(api, function)).copied().unwrap_or(1) >= 2;
        let looped = || {
            listings.get(&function).is_some_and(|insns| {
                hit_sites(hit)
                    .iter()
                    .any(|site| poll_site_in_loop(insns, *site))
            })
        };
        if repeated || looped() {
            // the polling shape supports the idle-probe reading; say so, and
            // name the tracking loops that share the shape
            hit.reasoning.push_str(
                " This function re-samples the API (multiple call sites, or a compare-and-loop \
                 around one), the polling shape an idle probe shares with keylogger/hVNC \
                 tracking loops — what the compared result gates decides between them.",
            );
        } else {
            // a lone read is a weak observation until a gate is proven; the
            // catalog grade rides aside for the validator to restore
            hit.reasoning.push_str(
                " Only a single call site resolves in this function, with no re-sampling loop \
                 around it: a lone read is everyday GUI/library behavior far more often than \
                 an activity gate, so this stays a weak observation. A result that provably \
                 gates a branch re-earns the catalog grade.",
            );
            demote_to_observation(hit);
        }
    }
}

/// The evidence sites backing a hit (the primary stands alone with no list).
///
/// # Arguments
///
/// * `hit` - The hit whose sites to read
fn hit_sites(hit: &DetectionHit) -> Vec<u64> {
    // the builder attaches a list only when there is more than one site
    if hit.sites.is_empty() {
        vec![hit.address]
    } else {
        hit.sites.clone()
    }
}

/// Whether a call site sits in a compare-and-jump-back re-sampling loop.
///
/// Scans the bounded window after the site for a compare followed by a
/// conditional jump whose target is at or above the site — the loop that calls
/// again after testing what the last call produced. Any compare counts
/// (`GetCursorPos` gates read the out-param from memory, not a fixed
/// register); this is a shape heuristic, while proving what the result gates
/// stays the validator's job.
///
/// # Arguments
///
/// * `insns` - The containing function's disassembly
/// * `site` - The call site address
fn poll_site_in_loop(insns: &[Instruction], site: u64) -> bool {
    // locate the call site in the listing
    let Some(idx) = insns
        .iter()
        .position(|insn| parse_addr(&insn.address) == Some(site))
    else {
        return false;
    };
    // a compare must arm the loop test before the backward jump closes it
    let mut compared = false;
    for insn in insns.iter().skip(idx + 1).take(POLL_WINDOW) {
        let mnemonic = insn.mnemonic.to_ascii_uppercase();
        // any compare over the window arms the test
        if matches!(mnemonic.as_str(), "TEST" | "CMP") {
            compared = true;
            continue;
        }
        // a conditional jump back to (or above) the call re-samples: a poll loop
        if mnemonic.starts_with('J') && mnemonic != "JMP" {
            if compared && branch_target(insn).is_some_and(|target| target <= site) {
                return true;
            }
            continue;
        }
        // a return ends the straight-line window early
        if mnemonic.starts_with("RET") {
            return false;
        }
    }
    false
}

/// Parse a branch instruction's target address, when it is statically rendered.
///
/// # Arguments
///
/// * `insn` - The branch instruction
fn branch_target(insn: &Instruction) -> Option<u64> {
    // the target is the sole operand: a bare address or a Ghidra label
    let ops = op_str(insn);
    let token = ops.split(',').next()?.trim();
    // strip the label prefixes Ghidra renders for in-function targets
    let token = token.trim_start_matches("LAB_").trim_start_matches("FUN_");
    parse_addr(token)
}

/// Demote a hit to a floor-grade observation, keeping its grade aside.
///
/// The withheld `(confidence, suspicion)` rides in `catalog_severity` so the
/// validation pass restores it once the result is seen gating a branch (or a
/// recovered argument confirms the claim). Hits already at the floor are left
/// untouched so no meaningless restore target is recorded.
///
/// # Arguments
///
/// * `hit` - The hit to demote in place
fn demote_to_observation(hit: &mut DetectionHit) {
    // nothing to demote (or keep aside) at the floor already
    if matches!(hit.confidence, Confidence::Untrusted) {
        return;
    }
    // keep the stronger grade aside for a verified-use restore, unless the
    // presence clamp already banked one
    if hit.catalog_severity.is_none() {
        hit.catalog_severity = Some((hit.confidence, hit.suspicion));
    }
    // seed at the floor until use is verified
    let (confidence, suspicion) = low();
    hit.confidence = confidence;
    hit.suspicion = suspicion;
}

/// Demote every hit of an API resolving across more functions than a check uses.
///
/// A targeted user-activity check concentrates in a handful of functions; a
/// catalog API with call sites in more than [`UBIQUITY_FANOUT`] distinct
/// functions is the statically-linked-GUI wallpaper shape (frameworks call
/// these everywhere), so each of its per-function hits demotes to a
/// floor-grade observation, names the fan-out, and is marked inherent noise so
/// the seeder may roll it up regardless of toolchain fingerprint. The grade
/// rides aside in `catalog_severity`, so a site the validator proves to gate a
/// branch is restored and stays loud.
///
/// # Arguments
///
/// * `hits` - The hits to judge in place
fn demote_ubiquitous(hits: &mut [DetectionHit]) {
    // count the distinct calling functions behind each catalog API
    let mut fanout: HashMap<&'static str, BTreeSet<u64>> = HashMap::new();
    for hit in hits.iter() {
        if let (Some(api), Some(function)) = (hit.api, hit.function) {
            fanout.entry(api).or_default().insert(function);
        }
    }
    // demote the per-function hits of every API past the fan-out bar
    for hit in hits.iter_mut() {
        // only per-function catalog hits carry an API to judge
        let (Some(api), Some(_)) = (hit.api, hit.function) else {
            continue;
        };
        let functions = fanout.get(api).map_or(0, BTreeSet::len);
        if functions <= UBIQUITY_FANOUT {
            continue;
        }
        // name the fan-out so the number rides on the finding
        let _ = write!(
            hit.reasoning,
            " {api} resolves call sites in {functions} distinct functions in this program — \
             the ubiquitous-GUI-usage shape, not a targeted check — so this stays a \
             rollup-grade observation; a site whose result provably gates a branch re-earns \
             the catalog grade."
        );
        // floor the grade (keeping it aside) and let the seeder roll it up
        demote_to_observation(hit);
        hit.always_noise = true;
    }
}

/// Whether a hit counts against the per-program observation cap.
///
/// Only per-function, floor-grade observations are capped: file-level hits
/// (the import census, overflow summaries) are already aggregates, and
/// anything above the floor (polling shapes, clusters, the stronger catalog
/// entries) stays an individual finding.
///
/// # Arguments
///
/// * `hit` - The hit to judge
fn is_generic_observation(hit: &DetectionHit) -> bool {
    // per-function + floor grade = cap-eligible wallpaper
    hit.function.is_some() && matches!(hit.confidence, Confidence::Untrusted)
}

#[async_trait::async_trait]
impl Detection for SandboxUserActivity {
    /// Short, stable name for logs.
    fn name(&self) -> &'static str {
        "sandbox_user_activity"
    }

    /// The primary technique this detection reports.
    fn technique(&self) -> AttackTechnique {
        T1497_002
    }

    /// Report user-interaction API call sites, shaped by cluster, polling, and
    /// fan-out so lone and ubiquitous GUI calls seed as weak observations.
    ///
    /// # Arguments
    ///
    /// * `ctx` - The analysis context
    #[tracing::instrument(name = "SandboxUserActivity::run", skip_all, err(Debug))]
    async fn run(&self, ctx: &AnalysisContext) -> Result<Vec<DetectionHit>> {
        // catalog pass: each API's result is meant to gate behavior, so the
        // validator checks result use (and recovers the GetSystemMetrics
        // index / SetWindowsHookEx idHook) at every resolved call site
        let hits = api_call_hits(ctx, T1497_002, APIS, EvidenceKind::ApiResultCheck).await?;
        // fold per-function input-API clusters into single keylogging claims
        // before the per-API shaping below
        let mut hits = retag_input_clusters(hits);
        // the lone-site loop scan reads decoded bodies; seeding is best-effort,
        // so a disassembly failure degrades to site-count-only classification
        let listings: HashMap<u64, Vec<Instruction>> = match ctx.all_function_disassembly().await {
            Ok(all) => all.into_iter().collect(),
            Err(error) => {
                tracing::warn!(
                    ?error,
                    "poll-shape classification degraded: disassembly unavailable"
                );
                HashMap::new()
            }
        };
        // split the polled APIs by shape: sampling loops stay, lone calls demote
        classify_poll_shapes(&listings, &mut hits);
        // ubiquitous fan-out across many functions is GUI wallpaper, not a check
        demote_ubiquitous(&mut hits);
        // group per function and cap the floor-grade observations per program
        Ok(group_scan_hits(
            hits,
            GENERIC_HIT_CAP,
            is_generic_observation,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic instruction for the fixture bodies.
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

    /// Build a catalog call-site hit the way `build_api_hit` seeds this catalog.
    ///
    /// # Arguments
    ///
    /// * `api` - The normalized catalog API name
    /// * `severity` - The seeded `(confidence, suspicion)` pair
    /// * `function` - The containing function entry
    /// * `sites` - Every call site within that function (first is primary)
    fn api_hit(
        api: &'static str,
        severity: (Confidence, i64),
        function: u64,
        sites: &[u64],
    ) -> DetectionHit {
        // mirror the shared builder: primary address, grouped site list
        let primary = sites.first().copied().unwrap_or_default();
        let content = match sites.len() {
            n if n > 1 => format!("{api} — {n} calls @ {primary:#x}, …"),
            _ => format!("{api} call @ {primary:#x}"),
        };
        let mut hit = DetectionHit::new(
            T1497_002,
            primary,
            Some(function),
            severity,
            content,
            "catalog reasoning.".to_string(),
        )
        .kind(EvidenceKind::ApiResultCheck)
        .api(api);
        // the builder attaches the full list only when there is more than one
        if sites.len() > 1 {
            hit = hit.sites(sites.to_vec());
        }
        hit
    }

    /// Every catalog reasoning states capability + condition and names the
    /// common alternative uses instead of asserting detonation-gating intent.
    #[test]
    fn catalog_reasonings_hypothesize_instead_of_asserting_intent() {
        // the asserted-intent phrases the telemetry disputed, verbatim from the
        // old templates ('waits for genuine input before acting', 'the malware
        // delays or aborts', 'so the payload stays dormant')
        let asserted = [
            "before detonating",
            "stays dormant",
            "before acting",
            "delays or aborts",
            "frustrating automated analysis",
        ];
        for spec in APIS {
            for phrase in asserted {
                assert!(
                    !spec.reasoning.contains(phrase),
                    "{} still asserts intent: {phrase}",
                    spec.key
                );
            }
        }
        // the real-use alternatives from the disputes are named: hVNC
        // window-title tracking, force-foreground helpers, and beacon
        // idle telemetry
        let foreground = APIS
            .iter()
            .find(|s| s.key == "GetForegroundWindow")
            .expect("a GetForegroundWindow entry");
        assert!(foreground.reasoning.contains("hVNC"));
        assert!(foreground.reasoning.contains("window-title"));
        assert!(foreground.reasoning.contains("force the foreground"));
        let idle = APIS
            .iter()
            .find(|s| s.key == "GetLastInputInfo")
            .expect("a GetLastInputInfo entry");
        assert!(idle.reasoning.contains("telemetry"));
        // the hook entry names the disputed non-capture hook types (MFC's
        // WH_MSGFILTER, remote-thread injection T1055.011)
        let hook = APIS
            .iter()
            .find(|s| s.key == "SetWindowsHookEx")
            .expect("a SetWindowsHookEx entry");
        assert!(hook.reasoning.contains("WH_MSGFILTER"));
        assert!(hook.reasoning.contains("T1055.011"));
    }

    /// The metrics entry conditions on the index (naming the sandbox-relevant
    /// and the benign ones) so the validator's appended `index=…` annotation
    /// reads as one claim, and never asserts a resolution check outright.
    #[test]
    fn metrics_reasoning_conditions_on_the_recovered_index() {
        let metrics = APIS
            .iter()
            .find(|s| s.key == "GetSystemMetrics")
            .expect("a GetSystemMetrics entry");
        // the sandbox-relevant indices the validator confirms are named
        for name in [
            "SM_CXSCREEN",
            "SM_CYSCREEN",
            "SM_MOUSEPRESENT",
            "SM_CMONITORS",
            "SM_REMOTESESSION",
        ] {
            assert!(metrics.reasoning.contains(name), "missing {name}");
        }
        // the disputed benign class is named too (SM_SERVERR2 was flagged as a
        // resolution check in a disputed lead)
        assert!(metrics.reasoning.contains("SM_SERVERR2"));
        // the old template asserted small/default resolutions as characteristic
        // of sandboxes; the claim is now conditional on the recovered index
        assert!(!metrics.reasoning.contains("characteristic of headless"));
        assert_eq!(metrics.severity.1, low().1);
    }

    /// A three-API input cluster re-tags to one T1056.001 keylogging finding
    /// listing every site, while non-capture hits in the function survive.
    #[test]
    fn input_api_cluster_retags_to_one_keylogging_hit() {
        // the decoy-GUI WinMain shape: GetCursorPos +
        // GetAsyncKeyState + GetForegroundWindow resolving into one function
        let hits = vec![
            api_hit(
                "GetCursorPos",
                medium(),
                0x0040_1000,
                &[0x0040_1100, 0x0040_1140],
            ),
            api_hit("GetAsyncKeyState", medium(), 0x0040_1000, &[0x0040_1180]),
            api_hit("GetForegroundWindow", medium(), 0x0040_1000, &[0x0040_11c0]),
            api_hit("GetSystemMetrics", low(), 0x0040_1000, &[0x0040_1200]),
        ];
        let out = retag_input_clusters(hits);
        // the three capture hits folded into one; the metrics hit survived
        assert_eq!(out.len(), 2);
        let cluster = out
            .iter()
            .find(|hit| hit.technique.id == "T1056.001")
            .expect("a keylogging cluster hit");
        // the combined hit carries every capture site and names every API
        assert_eq!(
            cluster.sites,
            vec![0x0040_1100, 0x0040_1140, 0x0040_1180, 0x0040_11c0]
        );
        assert!(cluster.content.contains("GetAsyncKeyState"));
        assert!(cluster.content.contains("GetCursorPos"));
        assert!(cluster.content.contains("GetForegroundWindow"));
        assert!(matches!(cluster.confidence, Confidence::Unsure));
        assert!(matches!(cluster.technique.tactic, AttackTactic::Collection));
        // the surviving hit is the non-capture metrics observation
        assert!(out.iter().any(|hit| hit.api == Some("GetSystemMetrics")));
    }

    /// Two distinct input APIs in one function stay as their own per-API
    /// T1497.002 findings (the confirmed gates are single-API functions).
    #[test]
    fn two_api_function_keeps_per_api_findings() {
        // the keylogger pair shape: foreground tracking + cursor
        // sampling in one function is below the three-API cluster bar
        let hits = vec![
            api_hit("GetForegroundWindow", medium(), 0x1000_43d0, &[0x1000_4400]),
            api_hit("GetCursorPos", medium(), 0x1000_43d0, &[0x1000_4430]),
        ];
        let out = retag_input_clusters(hits);
        // both hits survive untouched under their own technique
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|hit| hit.technique.id == "T1497.002"));
    }

    /// The confirmed lone-site poll loop keeps the catalog grade: a compare
    /// and a backward conditional jump after the call is the re-sampling shape.
    #[test]
    fn confirmed_poll_loop_single_site_keeps_catalog_grade() {
        // the confirmed GetCursorPos gate @0x401ad8, re-sampled by a loop:
        // call, compare the out-param, jump back above the call
        let insns = vec![
            insn(0x0040_1ad0, "MOV", &["esi", "eax"]),
            insn(0x0040_1ad8, "CALL", &["dword ptr [0x40a000]"]),
            insn(0x0040_1ade, "CMP", &["dword ptr [esp + 0x10]", "esi"]),
            insn(0x0040_1ae2, "JZ", &["0x401ad8"]),
        ];
        let listings = HashMap::from([(0x0040_1a00u64, insns)]);
        let mut hits = vec![api_hit(
            "GetCursorPos",
            medium(),
            0x0040_1a00,
            &[0x0040_1ad8],
        )];
        classify_poll_shapes(&listings, &mut hits);
        // the polling shape keeps the catalog grade and says why
        assert!(matches!(hits[0].confidence, Confidence::Unsure));
        assert_eq!(hits[0].suspicion, 40);
        assert!(hits[0].catalog_severity.is_none());
        assert!(hits[0].reasoning.contains("re-samples"));
    }

    /// Repeated sampling keeps the catalog grade with no disassembly at all.
    #[test]
    fn repeated_sampling_keeps_grade_without_listings() {
        // two call sites in one function need no loop scan to count as polling
        let mut hits = vec![api_hit(
            "GetCursorPos",
            medium(),
            0x0040_1a00,
            &[0x0040_1ad8, 0x0040_1b10],
        )];
        classify_poll_shapes(&HashMap::new(), &mut hits);
        // the catalog grade stands
        assert!(matches!(hits[0].confidence, Confidence::Unsure));
        assert!(hits[0].catalog_severity.is_none());
    }

    /// A lone foreground-window read demotes to a weak observation but keeps
    /// the catalog grade aside so a proven gate (the confirmed @0x401a35
    /// finding's shape) is restored by the validation pass.
    #[test]
    fn lone_foreground_read_demotes_but_keeps_grade_aside() {
        // a lone call with straight-line, forward-only code after it: a gate
        // the validator can verify, but no polling shape
        let insns = vec![
            insn(0x0040_1a35, "CALL", &["dword ptr [0x40a004]"]),
            insn(0x0040_1a3b, "TEST", &["eax", "eax"]),
            insn(0x0040_1a3d, "JZ", &["0x401a60"]),
        ];
        let listings = HashMap::from([(0x0040_1a00u64, insns)]);
        let mut hits = vec![api_hit(
            "GetForegroundWindow",
            medium(),
            0x0040_1a00,
            &[0x0040_1a35],
        )];
        classify_poll_shapes(&listings, &mut hits);
        // demoted to the floor, with the catalog grade banked for the validator
        assert!(matches!(hits[0].confidence, Confidence::Untrusted));
        assert_eq!(hits[0].suspicion, low().1);
        let (kept_confidence, kept_suspicion) = hits[0].catalog_severity.expect("grade kept aside");
        assert!(matches!(kept_confidence, Confidence::Unsure));
        assert_eq!(kept_suspicion, 40);
        assert!(hits[0].reasoning.contains("weak observation"));
    }

    /// The loop detector requires both a compare and a backward conditional
    /// jump — a forward gate or a bare backward jump is not a poll loop.
    #[test]
    fn poll_loop_requires_compare_and_backward_branch() {
        // forward-gating code (branch past the call) is a gate, not a loop
        let forward = vec![
            insn(0x0040_1000, "CALL", &["dword ptr [0x40a000]"]),
            insn(0x0040_1006, "CMP", &["eax", "0x1"]),
            insn(0x0040_1009, "JNZ", &["0x401050"]),
        ];
        assert!(!poll_site_in_loop(&forward, 0x0040_1000));
        // a backward conditional jump without any compare proves nothing
        let no_compare = vec![
            insn(0x0040_2000, "CALL", &["dword ptr [0x40a000]"]),
            insn(0x0040_2006, "DEC", &["esi"]),
            insn(0x0040_2007, "JNZ", &["LAB_00402000"]),
        ];
        assert!(!poll_site_in_loop(&no_compare, 0x0040_2000));
        // compare + backward jump (label-rendered target) is the polling shape
        let looped = vec![
            insn(0x0040_3000, "CALL", &["dword ptr [0x40a000]"]),
            insn(0x0040_3006, "CMP", &["dword ptr [ebp + -0x8]", "eax"]),
            insn(0x0040_3009, "JZ", &["LAB_00403000"]),
        ];
        assert!(poll_site_in_loop(&looped, 0x0040_3000));
    }

    /// Delphi-scale fan-out demotes every per-function hit to a rollup-grade
    /// observation and the per-program cap folds the overflow into one summary.
    #[test]
    fn ubiquitous_fanout_demotes_and_caps() {
        // a flood shape: 42 functions each calling thunked
        // GetCursorPos once (42 leads from one API in one session)
        let hits: Vec<DetectionHit> = (0..42u64)
            .map(|idx| {
                api_hit(
                    "GetCursorPos",
                    medium(),
                    0x50_0000 + idx * 0x100,
                    &[0x50_0010 + idx * 0x100],
                )
            })
            .collect();
        let mut hits = retag_input_clusters(hits);
        classify_poll_shapes(&HashMap::new(), &mut hits);
        demote_ubiquitous(&mut hits);
        // every per-function hit floored, marked noise, and naming the fan-out
        for hit in &hits {
            assert!(matches!(hit.confidence, Confidence::Untrusted));
            assert!(hit.always_noise);
            assert!(hit.reasoning.contains("42 distinct functions"));
            assert!(hit.catalog_severity.is_some());
        }
        let out = group_scan_hits(hits, GENERIC_HIT_CAP, is_generic_observation);
        // the cap keeps a handful of observations plus one file-level summary
        let individual = out.iter().filter(|hit| hit.function.is_some()).count();
        assert_eq!(individual, GENERIC_HIT_CAP);
        let summary = out
            .iter()
            .find(|hit| hit.function.is_none())
            .expect("an overflow summary");
        assert!(summary.content.contains("34 generic tells elided"));
    }

    /// Low fan-out stays loud: a check concentrated in two functions keeps the
    /// catalog grade with nothing banked, floored, or capped.
    #[test]
    fn targeted_low_fanout_keeps_catalog_grade() {
        // two functions polling key state is a plausible targeted check
        let mut hits = vec![
            api_hit("GetKeyState", medium(), 0x0040_1000, &[0x0040_1010]),
            api_hit("GetKeyState", medium(), 0x0040_2000, &[0x0040_2010]),
        ];
        demote_ubiquitous(&mut hits);
        let out = group_scan_hits(hits, GENERIC_HIT_CAP, is_generic_observation);
        // both findings stand at the catalog grade with no summary added
        assert_eq!(out.len(), 2);
        for hit in &out {
            assert!(matches!(hit.confidence, Confidence::Unsure));
            assert!(!hit.always_noise);
            assert!(hit.catalog_severity.is_none());
        }
    }

    /// Floor-grade catalog entries never bank a restore target when the
    /// demoting passes run over them.
    #[test]
    fn floor_grade_hits_bank_no_withheld_severity() {
        // a lone metrics read is already at the floor the catalog asserts
        let mut hits = vec![api_hit(
            "GetSystemMetrics",
            low(),
            0x0040_1000,
            &[0x0040_1010],
        )];
        classify_poll_shapes(&HashMap::new(), &mut hits);
        demote_ubiquitous(&mut hits);
        // still floor grade, with no withheld severity recorded
        assert!(matches!(hits[0].confidence, Confidence::Untrusted));
        assert!(hits[0].catalog_severity.is_none());
    }
}
