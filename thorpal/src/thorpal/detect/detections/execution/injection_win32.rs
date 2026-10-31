//! Classic Win32-layer process injection (MITRE ATT&CK **T1055**), scored on
//! the chain, not the APIs.
//!
//! The Win32 process APIs catalogued here — `OpenProcess`, `VirtualAllocEx`,
//! `WriteProcessMemory`, `CreateRemoteThread`, … — are everywhere in benign
//! software: installers patch running products, debuggers and crash reporters
//! open and read processes, instrumentation writes trampolines. Presence alone
//! is therefore worth nothing, and this detection never emits per-API findings.
//! What is rare and specific is the *chain*: one function that opens/allocates
//! in a target (stage A), writes a payload into it (stage B), and starts that
//! payload running (stage C). Two co-located stages rate medium, all three rate
//! high, and stages that only co-occur program-wide (never in one function)
//! collapse into a single weak file-level note.
//!
//! The ntdll spellings of these primitives (`NtAllocateVirtualMemory`,
//! `NtCreateThreadEx`, direct syscalls, …) are `native_api`'s territory; the one
//! shared name, `NtWriteVirtualMemory`, is matched here only as an ordinary
//! import completing a mixed-layer chain. `SetThreadContext` is also catalogued
//! by `debugger_evasion` (debug-register clearing): here it evidences the
//! thread-hijack execute step only as a same-function pair with `ResumeThread`,
//! and a finding claims injection only when that pair co-occurs with stage A/B
//! evidence, so the two detections never claim the same thing from one lone call.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use ghidra_cli::models::Instruction;

use crate::thorpal::detect::context::{AnalysisContext, fmt_addr, parse_addr};
use crate::thorpal::detect::detections::{
    ApiSpec, AttackTechnique, Detection, DetectionHit, EvidenceKind, api_call_hits, high,
    is_library_function, low, medium,
};
use crate::thorpal::detect::op_str;

use super::T1055;

/// The Win32 process-injection detection (ATT&CK T1055).
pub struct InjectionWin32;

/// The analyst-facing label for stage A (open the target / allocate inside it).
const STAGE_ALLOCATE: &str = "allocate/open";

/// The analyst-facing label for stage B (write the payload into the target).
const STAGE_WRITE: &str = "write";

/// The analyst-facing label for stage C (start the written payload running).
const STAGE_EXECUTE: &str = "execute";

/// The composed name the execute pair is rendered under when both halves co-occur.
const PAIR_NAME: &str = "SetThreadContext+ResumeThread";

/// The most chain findings reported per program; the rest roll into one summary.
///
/// Injection chains are rare and specific — more than a few in one binary means
/// an injection helper library, and per-function repeats past this point stop
/// adding information.
const MAX_CHAIN_FINDINGS: usize = 4;

/// The max call sites listed per API in a finding's content.
///
/// Enough to spot-check where an API is called; past that the count says more
/// than the addresses do.
const MAX_SITES_SHOWN: usize = 6;

/// How many instructions before an `OpenProcess` call are searched for the
/// `GetCurrentProcessId`/`GetCurrentProcess` call that proves self-targeting.
const SELF_TARGET_WINDOW: usize = 10;

/// The chain role a catalogued API plays (which stage its call evidences).
#[derive(Debug, Clone, Copy)]
enum Role {
    /// Stage A — open the target process or allocate memory inside it.
    Allocate,
    /// Stage B — write the payload into the target's memory.
    Write,
    /// Stage C — start execution of the written payload, on its own.
    Execute,
    /// Half of the `SetThreadContext`+`ResumeThread` execute pair; evidence
    /// only when its partner is called in the same function.
    SetContext,
    /// The other half of the `SetThreadContext`+`ResumeThread` execute pair.
    Resume,
}

/// One catalogued Win32 injection API: its import spec plus its chain role.
struct ChainApi {
    /// The spec handed to [`api_call_hits`] (name, standalone severity, reasoning).
    spec: ApiSpec,
    /// The chain stage a call to this API evidences.
    role: Role,
}

/// The Win32 injection-chain API catalog, by stage.
///
/// Unlike the per-API catalogs elsewhere, no entry here produces a finding by
/// itself: every one of these APIs is routine in benign installers, debuggers,
/// and instrumentation, so presence alone is noise (hence the uniform `low`
/// standalone severity, which is never emitted directly). The specs exist to
/// resolve call sites through the shared helper; all scoring happens over stage
/// co-occurrence in [`build_findings`].
const APIS: &[ChainApi] = &[
    ChainApi {
        spec: ApiSpec {
            key: "OpenProcess",
            always_noise: false,
            severity: low(),
            reasoning: "Opens a handle to another process — the acquisition step of the Win32 \
                        injection chain. Routine on its own (task managers, crash reporters), so \
                        it only scores as part of a chain.",
        },
        role: Role::Allocate,
    },
    ChainApi {
        spec: ApiSpec {
            key: "VirtualAllocEx",
            always_noise: false,
            severity: low(),
            reasoning: "Allocates memory inside another process — the staging step of the Win32 \
                        injection chain (the payload needs somewhere to land). Also used by \
                        debuggers and instrumentation, so it only scores as part of a chain.",
        },
        role: Role::Allocate,
    },
    ChainApi {
        spec: ApiSpec {
            key: "WriteProcessMemory",
            always_noise: false,
            severity: low(),
            reasoning: "Writes into another process's memory — the payload-delivery step of the \
                        Win32 injection chain. Debuggers and patchers use it too (including on \
                        the current process), so it only scores as part of a chain.",
        },
        role: Role::Write,
    },
    // the ntdll twin matched as a plain import only: an injector that mixes the
    // Win32 and native layers still completes its chain here, while flagging
    // the native call itself (and raw-syscall use) stays native_api's job
    ChainApi {
        spec: ApiSpec {
            key: "NtWriteVirtualMemory",
            always_noise: false,
            severity: low(),
            reasoning: "Imports NtWriteVirtualMemory, the native twin of WriteProcessMemory — \
                        the payload-delivery step of an injection chain for injectors that mix \
                        the Win32 and native layers.",
        },
        role: Role::Write,
    },
    ChainApi {
        spec: ApiSpec {
            key: "CreateRemoteThread",
            always_noise: false,
            severity: low(),
            reasoning: "Starts a thread in another process — the classic execution trigger of \
                        the Win32 injection chain. Debuggers and some legitimate tooling use it \
                        too, so it only scores as part of a chain.",
        },
        role: Role::Execute,
    },
    // the extended form of the same primitive (adds an attribute list; the
    // injection meaning is identical)
    ChainApi {
        spec: ApiSpec {
            key: "CreateRemoteThreadEx",
            always_noise: false,
            severity: low(),
            reasoning: "Starts a thread in another process (the extended CreateRemoteThread \
                        form) — the execution trigger of the Win32 injection chain.",
        },
        role: Role::Execute,
    },
    ChainApi {
        spec: ApiSpec {
            key: "QueueUserAPC",
            always_noise: false,
            severity: low(),
            reasoning: "Queues an APC into a thread — the APC-injection execution trigger when \
                        aimed at another process's thread; also ordinary async plumbing in \
                        benign code, so it only scores as part of a chain.",
        },
        role: Role::Execute,
    },
    ChainApi {
        spec: ApiSpec {
            key: "RtlCreateUserThread",
            always_noise: false,
            severity: low(),
            reasoning: "Imports RtlCreateUserThread, the loader-level remote-thread primitive \
                        injectors reach for when CreateRemoteThread is hooked.",
        },
        role: Role::Execute,
    },
    // SetThreadContext/ResumeThread evidence the thread-hijack execute step only
    // as a same-function pair: SetThreadContext alone is debugger_evasion's
    // catalog entry (debug-register clearing) and ResumeThread alone resumes any
    // suspended thread, so neither half means injection by itself
    ChainApi {
        spec: ApiSpec {
            key: "SetThreadContext",
            always_noise: false,
            severity: low(),
            reasoning: "Rewrites a thread's CPU context — half of the \
                        SetThreadContext+ResumeThread hijack pair that points an existing \
                        thread at injected code; counted only when the pair completes in one \
                        function.",
        },
        role: Role::SetContext,
    },
    ChainApi {
        spec: ApiSpec {
            key: "ResumeThread",
            always_noise: false,
            severity: low(),
            reasoning: "Resumes a suspended thread — half of the \
                        SetThreadContext+ResumeThread hijack pair (and the trigger step of \
                        process hollowing); counted only when the pair completes in one \
                        function.",
        },
        role: Role::Resume,
    },
];

/// The resolved call sites of one catalogued API inside one calling function.
#[derive(Debug)]
struct StageCall {
    /// The chain role of the called API.
    role: Role,
    /// The normalized API name.
    api: &'static str,
    /// The calling function's entry address.
    function: u64,
    /// The call sites inside that function.
    sites: Vec<u64>,
}

/// Everything the catalog resolved inside one function, keyed for chain scoring.
#[derive(Debug, Default)]
struct FunctionChain {
    /// Stage A allocate/open call sites, by API name.
    allocate: BTreeMap<&'static str, Vec<u64>>,
    /// Stage B write call sites, by API name.
    write: BTreeMap<&'static str, Vec<u64>>,
    /// Stage C execute call sites, by API name (standalone execute APIs only).
    execute: BTreeMap<&'static str, Vec<u64>>,
    /// `SetThreadContext` call sites, counted as execute only when paired.
    set_context: Vec<u64>,
    /// `ResumeThread` call sites, counted as execute only when paired.
    resume: Vec<u64>,
}

/// One present chain stage and the API calls that evidence it.
#[derive(Debug)]
struct StageEvidence {
    /// The stage's analyst-facing label (see [`STAGE_ALLOCATE`] and friends).
    label: &'static str,
    /// The evidencing calls: `(API name, call sites)` pairs in stable order.
    calls: Vec<(String, Vec<u64>)>,
}

impl FunctionChain {
    /// Record one API's call sites under its chain role.
    ///
    /// # Arguments
    ///
    /// * `call` - The resolved call group to record
    fn add(&mut self, call: &StageCall) {
        // route the sites by the API's role; thunked and direct resolutions of
        // the same API merge here, so sites are sorted + deduped at render time
        match call.role {
            Role::Allocate => self
                .allocate
                .entry(call.api)
                .or_default()
                .extend(&call.sites),
            Role::Write => self.write.entry(call.api).or_default().extend(&call.sites),
            Role::Execute => self
                .execute
                .entry(call.api)
                .or_default()
                .extend(&call.sites),
            Role::SetContext => self.set_context.extend(&call.sites),
            Role::Resume => self.resume.extend(&call.sites),
        }
    }

    /// The stages present in this function, with the calls evidencing each.
    ///
    /// Stage C counts the standalone execute APIs plus the
    /// `SetThreadContext`+`ResumeThread` pair — and the pair only when *both*
    /// halves are called in this function: `SetThreadContext` alone is
    /// `debugger_evasion`'s catalog (debug-register clearing) and `ResumeThread`
    /// alone resumes any suspended thread, so neither half is injection
    /// evidence by itself.
    fn stages(&self) -> Vec<StageEvidence> {
        let mut stages = Vec::new();
        // stage A: any open/allocate call places the function at the chain's start
        if let Some(stage) = stage_from(STAGE_ALLOCATE, &self.allocate) {
            stages.push(stage);
        }
        // stage B: any cross-process write
        if let Some(stage) = stage_from(STAGE_WRITE, &self.write) {
            stages.push(stage);
        }
        // stage C: the standalone execute APIs...
        let mut execute: Vec<(String, Vec<u64>)> = self
            .execute
            .iter()
            .map(|(api, sites)| ((*api).to_string(), sorted_sites(sites)))
            .collect();
        // ...plus the context-hijack pair, when both halves are present here
        if !self.set_context.is_empty() && !self.resume.is_empty() {
            let mut pair = self.set_context.clone();
            pair.extend(self.resume.iter().copied());
            execute.push((PAIR_NAME.to_string(), sorted_sites(&pair)));
        }
        if !execute.is_empty() {
            stages.push(StageEvidence {
                label: STAGE_EXECUTE,
                calls: execute,
            });
        }
        stages
    }
}

/// Build a stage's evidence from its per-API site map, when any call exists.
///
/// # Arguments
///
/// * `label` - The stage's analyst-facing label
/// * `by_api` - The stage's call sites, keyed by API name
fn stage_from(
    label: &'static str,
    by_api: &BTreeMap<&'static str, Vec<u64>>,
) -> Option<StageEvidence> {
    // no calls, no stage
    if by_api.is_empty() {
        return None;
    }
    // normalize each API's sites for stable rendering
    let calls = by_api
        .iter()
        .map(|(api, sites)| ((*api).to_string(), sorted_sites(sites)))
        .collect();
    Some(StageEvidence { label, calls })
}

/// Sort and de-duplicate a site list for stable rendering.
///
/// # Arguments
///
/// * `sites` - The call sites to normalize
fn sorted_sites(sites: &[u64]) -> Vec<u64> {
    // ascending, unique call addresses
    let mut sites = sites.to_vec();
    sites.sort_unstable();
    sites.dedup();
    sites
}

/// Render one API's call sites, naming the first few and counting the rest.
///
/// Local (rather than reusing the hidden renderer in `detections`) so the cap
/// suits chain content, where several APIs share one line.
///
/// # Arguments
///
/// * `sites` - The call sites to render, ascending
fn render_api_sites(sites: &[u64]) -> String {
    // list a spot-checkable handful of addresses
    let listed = sites
        .iter()
        .take(MAX_SITES_SHOWN)
        .map(|site| fmt_addr(*site))
        .collect::<Vec<_>>()
        .join(", ");
    // no silent truncation: say how many sites were left out
    match sites
        .len()
        .checked_sub(MAX_SITES_SHOWN)
        .filter(|extra| *extra > 0)
    {
        Some(extra) => format!("{listed} (+{extra} more)"),
        None => listed,
    }
}

/// Render one stage's evidence as `label: Api @ sites, Api @ sites`.
///
/// # Arguments
///
/// * `stage` - The stage evidence to render
fn render_stage(stage: &StageEvidence) -> String {
    // name each API with its call sites, in the stage's stable order
    let calls = stage
        .calls
        .iter()
        .map(|(api, sites)| format!("{api} @ {}", render_api_sites(sites)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{}: {calls}", stage.label)
}

/// Fold the collected call groups into one chain record per function.
///
/// # Arguments
///
/// * `calls` - The collected call groups
fn aggregate(calls: &[StageCall]) -> BTreeMap<u64, FunctionChain> {
    // route every call group into its function's record by role
    let mut chains: BTreeMap<u64, FunctionChain> = BTreeMap::new();
    for call in calls {
        chains.entry(call.function).or_default().add(call);
    }
    chains
}

/// Count the distinct chain stages present anywhere in the program.
///
/// The execute-pair rule is per function, so presence has to be judged on each
/// function's record rather than on the raw API list.
///
/// # Arguments
///
/// * `calls` - The collected call groups
fn program_stage_count(calls: &[StageCall]) -> usize {
    // union the stage labels over every function's chain record
    let mut labels = BTreeSet::new();
    for chain in aggregate(calls).values() {
        for stage in chain.stages() {
            labels.insert(stage.label);
        }
    }
    labels.len()
}

/// Drop every call group inside a recognized C-runtime/support function.
///
/// A statically linked runtime legitimately touches process APIs (crash
/// reporting, fast-fail paths), so chains are scored over the sample's own code
/// only.
///
/// # Arguments
///
/// * `calls` - The collected call groups, filtered in place
/// * `is_library` - Whether a function entry is recognized runtime/support code
fn drop_library_calls(calls: &mut Vec<StageCall>, is_library: impl Fn(u64) -> bool) {
    // judge each group by its containing function
    calls.retain(|call| !is_library(call.function));
}

/// Whether rendered operands name `GetCurrentProcessId`/`GetCurrentProcess` as
/// a call target.
///
/// Accepts only whole-symbol matches, so lookalikes such as
/// `GetCurrentProcessorNumber` or `GetCurrentProcessToken` never pass.
///
/// # Arguments
///
/// * `ops` - The instruction's rendered operand text
fn names_current_process(ops: &str) -> bool {
    // lowercase once so Ghidra's renderings and import decorations all compare
    let ops = ops.to_ascii_lowercase();
    let mut rest = ops.as_str();
    // examine every occurrence, accepting only whole-symbol matches
    while let Some(pos) = rest.find("getcurrentprocess") {
        // the text right after the match decides whether it is the whole symbol
        let after = &rest[pos + "getcurrentprocess".len()..];
        // allow the bare form and the …Id form; any other continuation is a
        // different symbol (GetCurrentProcessorNumber, GetCurrentProcessToken)
        let tail = after.strip_prefix("id").unwrap_or(after);
        let continues = tail
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        if !continues {
            return true;
        }
        rest = after;
    }
    false
}

/// Whether the call at `site` is provably handed the current process.
///
/// Walks backwards from the call site: the first `CALL` met within the window
/// must name `GetCurrentProcessId`/`GetCurrentProcess`, with no jump between it
/// and the site (a branch means the value reaching the call is no longer
/// provably the current-process id — the enumerate-and-skip-self loop looks
/// exactly like that, and it really does open *other* processes). Anything
/// short of the proof keeps the site: a kept benign site costs one review,
/// while an overreaching exclusion hides a real chain.
///
/// # Arguments
///
/// * `insns` - The containing function's instructions, in program order
/// * `site` - The `OpenProcess` call-site address to judge
fn self_targeted(insns: &[Instruction], site: u64) -> bool {
    // locate the call instruction inside the function body
    let Some(idx) = insns
        .iter()
        .position(|insn| parse_addr(&insn.address) == Some(site))
    else {
        return false;
    };
    // walk the window backwards from the call
    let start = idx.saturating_sub(SELF_TARGET_WINDOW);
    for insn in insns[start..idx].iter().rev() {
        let mnemonic = insn.mnemonic.to_ascii_uppercase();
        // any jump between the candidate source and the call breaks the proof
        if mnemonic.starts_with('J') {
            return false;
        }
        // the first call back decides: either it is the proving source, or it
        // clobbers the value and nothing older can prove anything
        if mnemonic == "CALL" {
            return names_current_process(&op_str(insn));
        }
    }
    false
}

/// Remove `OpenProcess` call sites provably opening the sample's own process.
///
/// `OpenProcess(…, GetCurrentProcessId())` is a self-inspection idiom (crash
/// reporters, privilege probes, mitigation queries), not target acquisition, so
/// such sites must not count as the chain's allocate/open stage. The proof bar
/// is [`self_targeted`]'s, deliberately narrow. The scan is skipped entirely
/// while fewer than two stages exist program-wide, since nothing could fire
/// anyway and the disassembly is not free.
///
/// # Arguments
///
/// * `ctx` - The analysis context (per-function disassembly, cached)
/// * `calls` - The collected call groups, filtered in place
///
/// # Errors
///
/// Returns an error if a bridge disassembly query fails.
#[tracing::instrument(name = "injection_win32::exclude_self_open", skip_all, err(Debug))]
async fn exclude_self_open(ctx: &AnalysisContext, calls: &mut Vec<StageCall>) -> Result<()> {
    // below two program-wide stages nothing can fire; skip the disassembly
    if program_stage_count(calls) < 2 {
        return Ok(());
    }
    // judge each OpenProcess group against its function's instructions
    for call in calls.iter_mut().filter(|call| call.api == "OpenProcess") {
        // read the containing function's body (cached across detections)
        let insns = ctx.disasm_fn(call.function).await?;
        // keep only the sites not provably aimed at the current process
        call.sites.retain(|site| !self_targeted(&insns, *site));
    }
    // a group whose every site was excluded contributes nothing anywhere
    calls.retain(|call| !call.sites.is_empty());
    Ok(())
}

/// Resolve every catalogued API's call sites, grouped per calling function.
///
/// This is the one seam onto the shared import machinery: each API goes through
/// [`api_call_hits`] with its own single-spec slice (so the API behind every
/// returned hit is known without parsing content), and only resolved call-site
/// hits are kept. Imported-but-uncalled results (the file-level census hits —
/// see `api_call_hits_with`) are dropped: presence alone is exactly the
/// ubiquitous-API noise this detection exists to avoid. Sites outside any
/// function are dropped too, since they cannot anchor a chain.
///
/// # Arguments
///
/// * `ctx` - The analysis context
///
/// # Errors
///
/// Returns an error if resolving an import's call sites fails.
#[tracing::instrument(name = "injection_win32::collect_stage_calls", skip_all, err(Debug))]
async fn collect_stage_calls(ctx: &AnalysisContext) -> Result<Vec<StageCall>> {
    let mut calls = Vec::new();
    // resolve each catalogued API separately so hits map back to their API
    for api in APIS {
        let hits = api_call_hits(
            ctx,
            T1055,
            std::slice::from_ref(&api.spec),
            EvidenceKind::ApiResultCheck,
        )
        .await?;
        for hit in hits {
            // a chain stage needs a containing function; the imported-but-uncalled
            // sentinel (address 0) and function-less sites place nothing
            let Some(function) = hit.function else {
                continue;
            };
            if hit.address == 0 {
                continue;
            }
            // single-site hits carry no site list; reconstruct one
            let sites = if hit.sites.is_empty() {
                vec![hit.address]
            } else {
                hit.sites
            };
            calls.push(StageCall {
                role: api.role,
                api: api.spec.key,
                function,
                sites,
            });
        }
    }
    Ok(calls)
}

/// Compose the analyst-facing reasoning for a same-function chain finding.
///
/// # Arguments
///
/// * `stages` - The present stages backing the finding
fn chain_reasoning(stages: &[StageEvidence]) -> String {
    // a complete chain is a strong claim; a partial one names its gap
    let mut reasoning = if stages.len() >= 3 {
        "This function calls all three Win32 process-injection stages — allocate/open, write, \
         and execute — the classic T1055 sequence for running code in another process. Each API \
         is routine in benign software alone; the complete chain co-located in one function is \
         the rare, specific injection shape."
            .to_string()
    } else {
        // name the stage the chain is missing
        let missing = [STAGE_ALLOCATE, STAGE_WRITE, STAGE_EXECUTE]
            .into_iter()
            .find(|label| !stages.iter().any(|stage| stage.label == *label))
            .unwrap_or("remaining");
        let present = stages
            .iter()
            .map(|stage| stage.label)
            .collect::<Vec<_>>()
            .join(" + ");
        format!(
            "Two Win32 process-injection stages ({present}) co-occur in this function; the \
             {missing} stage is not visible here (it may live elsewhere in the binary, be \
             resolved dynamically, or be absent). These APIs are ubiquitous individually in \
             benign installers and debuggers — the same-function co-occurrence is what makes \
             this worth a look, so it rates a medium indicator, not a confirmed chain."
        )
    };
    // the hijack pair is claimed as injection only because other stage evidence
    // co-occurs here; alone, SetThreadContext is debugger_evasion's territory
    if stages
        .iter()
        .any(|stage| stage.calls.iter().any(|(api, _)| api == PAIR_NAME))
    {
        reasoning.push_str(
            " SetThreadContext+ResumeThread is also a debugger/thread-manipulation primitive on \
             its own (debugger_evasion catalogs SetThreadContext for debug-register clearing); \
             it is read as the injection execute step only because it co-occurs with the \
             chain's other stage evidence in this function.",
        );
    }
    reasoning
}

/// Build one same-function chain finding.
///
/// # Arguments
///
/// * `function` - The containing function's entry address
/// * `stages` - The present stages (two or three), with their evidencing calls
fn chain_hit(function: u64, stages: &[StageEvidence]) -> DetectionHit {
    // all three stages together is the rare, specific injection shape; two is a
    // meaningful indicator with a visible gap (severity never exceeds high)
    let severity = if stages.len() >= 3 { high() } else { medium() };
    // name every stage with its APIs and call sites
    let rendered = stages
        .iter()
        .map(render_stage)
        .collect::<Vec<_>>()
        .join("; ");
    let content = format!(
        "Win32 injection chain ({} of 3 stages) — {rendered}",
        stages.len()
    );
    // every call site across every stage backs this finding
    let mut sites: Vec<u64> = stages
        .iter()
        .flat_map(|stage| {
            stage
                .calls
                .iter()
                .flat_map(|(_, sites)| sites.iter().copied())
        })
        .collect();
    sites.sort_unstable();
    sites.dedup();
    // anchor at the first (lowest) call site
    let address = sites.first().copied().unwrap_or(0);
    DetectionHit::new(
        T1055,
        address,
        Some(function),
        severity,
        content,
        chain_reasoning(stages),
    )
    .kind(EvidenceKind::ApiResultCheck)
    .sites(sites)
}

/// Build the single roll-up for chain functions past the reporting cap.
///
/// # Arguments
///
/// * `rest` - The chained functions that did not make the cap, strongest first
fn overflow_hit(rest: &[(u64, Vec<StageEvidence>)]) -> DetectionHit {
    // name each remaining function with the stages it chains
    let listed = rest
        .iter()
        .map(|(entry, stages)| {
            let labels = stages
                .iter()
                .map(|stage| stage.label)
                .collect::<Vec<_>>()
                .join("+");
            format!("{} ({labels})", fmt_addr(*entry))
        })
        .collect::<Vec<_>>()
        .join(", ");
    // count the roll-up out loud so capping never reads as data loss
    let content = match rest.len() {
        1 => format!("1 more function with co-occurring Win32 injection stages — {listed}"),
        n => format!("{n} more functions with co-occurring Win32 injection stages — {listed}"),
    };
    // keep every call site so the markup pass can still note each one
    let mut sites: Vec<u64> = rest
        .iter()
        .flat_map(|(_, stages)| stages.iter())
        .flat_map(|stage| {
            stage
                .calls
                .iter()
                .flat_map(|(_, sites)| sites.iter().copied())
        })
        .collect();
    sites.sort_unstable();
    sites.dedup();
    DetectionHit::new(
        T1055,
        0,
        None,
        low(),
        content,
        format!(
            "Chain findings are capped at {MAX_CHAIN_FINDINGS} per program so a binary full of \
             injection helpers cannot flood the report; the strongest chains are reported \
             individually and the rest are rolled up here. Each function listed pairs at least \
             two injection stages, so it deserves the same scrutiny if the reported chains pan \
             out."
        ),
    )
    .kind(EvidenceKind::ContextOnly)
    .sites(sites)
}

/// Build the single file-level note for stages that never share a function.
///
/// # Arguments
///
/// * `singles` - The functions holding exactly one stage each
fn split_hit(singles: &[(u64, Vec<StageEvidence>)]) -> DetectionHit {
    // render each stage's evidence with its containing function, in chain order
    let mut parts = Vec::new();
    let mut sites_all: Vec<u64> = Vec::new();
    for label in [STAGE_ALLOCATE, STAGE_WRITE, STAGE_EXECUTE] {
        // gather every (function, API, sites) triple evidencing this stage
        let mut rendered = Vec::new();
        for (entry, stages) in singles {
            for stage in stages.iter().filter(|stage| stage.label == label) {
                for (api, sites) in &stage.calls {
                    rendered.push(format!(
                        "{api} @ {} (fn {})",
                        render_api_sites(sites),
                        fmt_addr(*entry)
                    ));
                    sites_all.extend(sites.iter().copied());
                }
            }
        }
        if !rendered.is_empty() {
            parts.push(format!("{label}: {}", rendered.join(", ")));
        }
    }
    sites_all.sort_unstable();
    sites_all.dedup();
    let content = format!(
        "Win32 injection-stage APIs present only in separate functions — {}",
        parts.join("; ")
    );
    DetectionHit::new(
        T1055,
        0,
        None,
        low(),
        content,
        "Two or more Win32 injection stages appear in this binary, but never together in one \
         function. These process APIs are ubiquitous in benign software — installers, \
         debuggers, crash reporters, and instrumentation all open processes and write memory — \
         so scattered presence earns this single weak file-level note instead of per-API \
         findings. Worth revisiting only if other evidence ties the functions together (one \
         calls the others, or the missing stage is resolved dynamically).",
    )
    .kind(EvidenceKind::ContextOnly)
    .sites(sites_all)
}

/// Score the collected call groups into findings: same-function chains first
/// (capped, strongest first), else one file-level note for scattered stages.
///
/// This is the whole scoring policy in one place: a finding fires **only** on
/// co-occurrence of two or more distinct stages, so no single API — however
/// often it is called — ever seeds anything (which covers the
/// `WriteProcessMemory`-on-the-current-process patching idiom by construction).
///
/// # Arguments
///
/// * `calls` - The collected (and already filtered) call groups
fn build_findings(calls: &[StageCall]) -> Vec<DetectionHit> {
    // fold every call group into its function's chain record
    let chains = aggregate(calls);
    // materialize each function's present stages once (the pair rule lives there)
    let scored: Vec<(u64, Vec<StageEvidence>)> = chains
        .iter()
        .map(|(entry, chain)| (*entry, chain.stages()))
        .filter(|(_, stages)| !stages.is_empty())
        .collect();
    // split the functions that chain (>=2 distinct stages) from the lone stages
    let (mut chained, singles): (Vec<_>, Vec<_>) = scored
        .into_iter()
        .partition(|(_, stages)| stages.len() >= 2);
    // same-function chains are the findings; when any exist, lone stray stages
    // are not reported at all — the chains already make the injection claim,
    // and per-stage leftovers would reintroduce the presence-only noise
    if !chained.is_empty() {
        // strongest first: complete chains before partial, then by address
        chained.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
        let mut hits: Vec<DetectionHit> = chained
            .iter()
            .take(MAX_CHAIN_FINDINGS)
            .map(|(entry, stages)| chain_hit(*entry, stages))
            .collect();
        // everything past the cap rolls into one summary, never dropped silently
        if chained.len() > MAX_CHAIN_FINDINGS {
            hits.push(overflow_hit(&chained[MAX_CHAIN_FINDINGS..]));
        }
        return hits;
    }
    // no function chains: two or more stages scattered program-wide earn one
    // weak file-level note; anything less is silence (presence-only Win32
    // process APIs are not evidence)
    let labels: BTreeSet<&'static str> = singles
        .iter()
        .flat_map(|(_, stages)| stages.iter().map(|stage| stage.label))
        .collect();
    if labels.len() >= 2 {
        return vec![split_hit(&singles)];
    }
    Vec::new()
}

#[async_trait::async_trait]
impl Detection for InjectionWin32 {
    /// Short, stable name for logs.
    fn name(&self) -> &'static str {
        "injection_win32"
    }

    /// The primary technique this detection reports.
    fn technique(&self) -> AttackTechnique {
        T1055
    }

    /// Score the Win32 injection chain over per-function API co-occurrence.
    ///
    /// # Arguments
    ///
    /// * `ctx` - The shared analysis context
    #[tracing::instrument(name = "InjectionWin32::run", skip_all, err(Debug))]
    async fn run(&self, ctx: &AnalysisContext) -> Result<Vec<DetectionHit>> {
        // 1) resolve every catalogued API's imported call sites, per function
        let mut calls = collect_stage_calls(ctx).await?;
        // 2) known-FP pass: a statically linked runtime legitimately touches
        //    process APIs, so chains are scored over the sample's own code only
        drop_library_calls(&mut calls, |entry| {
            ctx.function_name(entry).is_some_and(is_library_function)
        });
        // 3) known-FP pass: OpenProcess provably aimed at the current process is
        //    self-inspection (crash reporters, privilege probes), not targeting
        exclude_self_open(ctx, &mut calls).await?;
        // 4) score the chains: co-occurrence only, never per-API findings
        Ok(build_findings(&calls))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thorpal::detect::confidence::Confidence;

    /// Build a call-group fixture for one API inside one function.
    ///
    /// # Arguments
    ///
    /// * `api` - The catalogued API name
    /// * `function` - The calling function's entry address
    /// * `sites` - The call sites inside that function
    fn stage_call(api: &'static str, function: u64, sites: &[u64]) -> StageCall {
        // look the role up from the catalog so fixtures cannot drift from it
        let role = APIS
            .iter()
            .find(|entry| entry.spec.key == api)
            .expect("a catalogued API")
            .role;
        StageCall {
            role,
            api,
            function,
            sites: sites.to_vec(),
        }
    }

    /// Build an instruction fixture.
    ///
    /// # Arguments
    ///
    /// * `address` - The instruction address, as Ghidra renders it
    /// * `mnemonic` - The instruction mnemonic
    /// * `operands` - The rendered operand text
    fn insn(address: &str, mnemonic: &str, operands: &str) -> Instruction {
        Instruction {
            address: address.into(),
            mnemonic: mnemonic.into(),
            operands: vec![operands.into()],
            bytes: None,
            length: None,
            flow_type: None,
        }
    }

    /// A complete allocate+write+execute chain in one function is one high finding.
    #[test]
    fn full_chain_in_one_function_scores_high() {
        let groups = vec![
            stage_call("OpenProcess", 0x401000, &[0x401010]),
            stage_call("VirtualAllocEx", 0x401000, &[0x401020]),
            stage_call("WriteProcessMemory", 0x401000, &[0x401030]),
            stage_call("CreateRemoteThread", 0x401000, &[0x401040]),
        ];
        let hits = build_findings(&groups);
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        // the complete chain is the rare shape and rates Likely/75, never more
        assert!(matches!(hit.confidence, Confidence::Likely));
        assert_eq!(hit.suspicion, high().1);
        assert_eq!(hit.kind, EvidenceKind::ApiResultCheck);
        assert_eq!(hit.function, Some(0x401000));
        // the content names every stage and every call site
        assert_eq!(
            hit.content,
            "Win32 injection chain (3 of 3 stages) — allocate/open: OpenProcess @ 0x401010, \
             VirtualAllocEx @ 0x401020; write: WriteProcessMemory @ 0x401030; execute: \
             CreateRemoteThread @ 0x401040"
        );
        // every call site rides along for the validator and the markup pass
        assert_eq!(hit.sites, vec![0x401010, 0x401020, 0x401030, 0x401040]);
        // chain findings never carry artifacts (nothing is recovered)
        assert!(hit.artifact.is_none());
    }

    /// Two co-located stages score a medium chain finding naming the gap.
    #[test]
    fn two_stages_in_one_function_score_medium() {
        let groups = vec![
            stage_call("VirtualAllocEx", 0x402000, &[0x402010]),
            stage_call("WriteProcessMemory", 0x402000, &[0x402020, 0x402018]),
        ];
        let hits = build_findings(&groups);
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert!(matches!(hit.confidence, Confidence::Unsure));
        assert_eq!(hit.suspicion, medium().1);
        assert_eq!(hit.kind, EvidenceKind::ApiResultCheck);
        assert!(hit.content.contains("2 of 3 stages"), "{}", hit.content);
        // the reasoning names the missing execute stage and the benign ubiquity
        assert!(hit.reasoning.contains("execute stage"), "{}", hit.reasoning);
        assert!(hit.reasoning.contains("benign"), "{}", hit.reasoning);
        // sites are sorted and complete across both stages
        assert_eq!(hit.sites, vec![0x402010, 0x402018, 0x402020]);
    }

    /// Stages that never share a function collapse to one low file-level note.
    #[test]
    fn scattered_stages_collapse_to_one_file_level_note() {
        let groups = vec![
            stage_call("OpenProcess", 0x401000, &[0x401010]),
            stage_call("WriteProcessMemory", 0x402000, &[0x402010]),
            stage_call("CreateRemoteThread", 0x403000, &[0x403010]),
        ];
        let hits = build_findings(&groups);
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert!(matches!(hit.confidence, Confidence::Untrusted));
        assert_eq!(hit.suspicion, low().1);
        assert_eq!(hit.kind, EvidenceKind::ContextOnly);
        // file-level: no single function owns the evidence
        assert_eq!(hit.function, None);
        assert_eq!(hit.address, 0);
        // the content names the split, function by function
        assert!(
            hit.content.contains("separate functions"),
            "{}",
            hit.content
        );
        assert!(
            hit.content.contains("OpenProcess @ 0x401010 (fn 0x401000)"),
            "{}",
            hit.content
        );
        // the ubiquity of presence-only process APIs is said out loud
        assert!(hit.reasoning.contains("benign"), "{}", hit.reasoning);
    }

    /// One stage alone — however many calls — never produces a finding.
    #[test]
    fn a_single_stage_never_fires() {
        // WriteProcessMemory used heavily by one function (the self-patching idiom)
        let hot = vec![stage_call(
            "WriteProcessMemory",
            0x401000,
            &[0x401010, 0x401020, 0x401030],
        )];
        assert!(build_findings(&hot).is_empty());
        // spread across functions it is still only one stage program-wide
        let spread = vec![
            stage_call("WriteProcessMemory", 0x401000, &[0x401010]),
            stage_call("WriteProcessMemory", 0x402000, &[0x402010]),
        ];
        assert!(build_findings(&spread).is_empty());
    }

    /// `SetThreadContext`/`ResumeThread` count as the execute stage only paired.
    #[test]
    fn thread_context_counts_only_as_a_pair() {
        // half the pair next to an allocate stage is one stage, not two
        let half = vec![
            stage_call("OpenProcess", 0x401000, &[0x401010]),
            stage_call("SetThreadContext", 0x401000, &[0x401020]),
        ];
        assert!(build_findings(&half).is_empty());
        // the full pair alone is only the execute stage (debugger_evasion's
        // overlap): without allocate/write co-occurrence there is no finding
        let pair_only = vec![
            stage_call("SetThreadContext", 0x401000, &[0x401020]),
            stage_call("ResumeThread", 0x401000, &[0x401030]),
        ];
        assert!(build_findings(&pair_only).is_empty());
        // paired next to a write stage it completes a two-stage chain
        let paired = vec![
            stage_call("WriteProcessMemory", 0x401000, &[0x401010]),
            stage_call("SetThreadContext", 0x401000, &[0x401020]),
            stage_call("ResumeThread", 0x401000, &[0x401030]),
        ];
        let hits = build_findings(&paired);
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].content.contains("SetThreadContext+ResumeThread"),
            "{}",
            hits[0].content
        );
        // the reasoning owns the debugger_evasion boundary explicitly
        assert!(
            hits[0].reasoning.contains("debugger_evasion"),
            "{}",
            hits[0].reasoning
        );
    }

    /// Call groups inside recognized runtime support code never score.
    #[test]
    fn library_functions_are_excluded() {
        // a complete chain, but inside CRT support code (`__scrt_common_main_seh`)
        let mut groups = vec![
            stage_call("OpenProcess", 0x401000, &[0x401010]),
            stage_call("WriteProcessMemory", 0x401000, &[0x401020]),
            stage_call("CreateRemoteThread", 0x401000, &[0x401030]),
        ];
        drop_library_calls(&mut groups, |entry| {
            assert_eq!(entry, 0x401000);
            is_library_function("__scrt_common_main_seh")
        });
        assert!(groups.is_empty());
        assert!(build_findings(&groups).is_empty());
    }

    /// Findings report strongest first: complete chains before partial ones.
    #[test]
    fn complete_chains_outrank_partial_ones() {
        let groups = vec![
            // a two-stage chain at a lower address...
            stage_call("OpenProcess", 0x401000, &[0x401010]),
            stage_call("WriteProcessMemory", 0x401000, &[0x401020]),
            // ...a complete chain after it...
            stage_call("OpenProcess", 0x402000, &[0x402010]),
            stage_call("WriteProcessMemory", 0x402000, &[0x402020]),
            stage_call("QueueUserAPC", 0x402000, &[0x402030]),
            // ...and a stray lone stage that must not add a file-level note
            stage_call("VirtualAllocEx", 0x403000, &[0x403010]),
        ];
        let hits = build_findings(&groups);
        assert_eq!(hits.len(), 2);
        // the complete chain leads despite its higher address
        assert_eq!(hits[0].function, Some(0x402000));
        assert!(matches!(hits[0].confidence, Confidence::Likely));
        assert_eq!(hits[1].function, Some(0x401000));
        assert!(matches!(hits[1].confidence, Confidence::Unsure));
    }

    /// At most four chain findings are reported; the rest roll into one summary.
    #[test]
    fn chain_findings_are_capped_with_a_summary() {
        // six functions, each with a complete three-stage chain
        let mut groups = Vec::new();
        for i in 0..6u64 {
            let entry = 0x400000 + i * 0x1000;
            groups.push(stage_call("OpenProcess", entry, &[entry + 0x10]));
            groups.push(stage_call("WriteProcessMemory", entry, &[entry + 0x20]));
            groups.push(stage_call("CreateRemoteThread", entry, &[entry + 0x30]));
        }
        let hits = build_findings(&groups);
        // four chain findings plus exactly one roll-up
        assert_eq!(hits.len(), MAX_CHAIN_FINDINGS + 1);
        for hit in &hits[..MAX_CHAIN_FINDINGS] {
            assert!(matches!(hit.confidence, Confidence::Likely));
        }
        let summary = hits.last().expect("a roll-up finding");
        assert!(matches!(summary.confidence, Confidence::Untrusted));
        assert_eq!(summary.kind, EvidenceKind::ContextOnly);
        assert_eq!(summary.function, None);
        // the roll-up counts and names what the cap left out
        assert!(
            summary.content.contains("2 more functions"),
            "{}",
            summary.content
        );
        assert!(
            summary
                .content
                .contains("0x404000 (allocate/open+write+execute)"),
            "{}",
            summary.content
        );
        assert!(summary.content.contains("0x405000"), "{}", summary.content);
    }

    /// The self-target proof needs the current-process call feeding the site.
    #[test]
    fn self_targeted_requires_a_provable_current_process_source() {
        // the classic self-inspection idiom: OpenProcess(…, GetCurrentProcessId())
        let own = vec![
            insn(
                "401000",
                "CALL",
                "dword ptr [->KERNEL32.DLL::GetCurrentProcessId]",
            ),
            insn("401006", "MOV", "R8D, EAX"),
            insn("401009", "XOR", "EDX, EDX"),
            insn("40100b", "MOV", "ECX, 0x1fffff"),
            insn("401010", "CALL", "dword ptr [->KERNEL32.DLL::OpenProcess]"),
        ];
        assert!(self_targeted(&own, 0x401010));
        // a branch between source and call breaks the proof: that shape is the
        // enumerate-and-skip-self loop, which really does open other processes
        let skip_self = vec![
            insn("401000", "CALL", "GetCurrentProcessId"),
            insn("401006", "CMP", "EAX, EBX"),
            insn("401008", "JZ", "0x401020"),
            insn("40100a", "MOV", "R8D, EBX"),
            insn("401010", "CALL", "OpenProcess"),
        ];
        assert!(!self_targeted(&skip_self, 0x401010));
        // an intervening call clobbers the value, so the proof does not hold
        let clobbered = vec![
            insn("401000", "CALL", "GetCurrentProcessId"),
            insn("401006", "CALL", "GetLastError"),
            insn("401010", "CALL", "OpenProcess"),
        ];
        assert!(!self_targeted(&clobbered, 0x401010));
        // a site the body does not contain proves nothing
        assert!(!self_targeted(&own, 0xdead));
    }

    /// Lookalike symbols never pass for the current-process sources.
    #[test]
    fn current_process_names_reject_lookalikes() {
        assert!(names_current_process(
            "dword ptr [->KERNEL32.DLL::GetCurrentProcessId]"
        ));
        assert!(names_current_process("GetCurrentProcess"));
        assert!(names_current_process("__imp__GetCurrentProcessId@0"));
        assert!(!names_current_process("GetCurrentProcessorNumber"));
        assert!(!names_current_process("GetCurrentProcessToken"));
        assert!(!names_current_process("OpenProcess"));
    }

    /// Site rendering lists a handful of addresses and counts the rest.
    #[test]
    fn site_rendering_caps_the_list() {
        // a short list renders in full
        assert_eq!(
            render_api_sites(&[0x401000, 0x401010]),
            "0x401000, 0x401010"
        );
        // past the cap the addresses stop and the count takes over
        let many: Vec<u64> = (0..9).map(|i| 0x401000 + i * 0x10).collect();
        let rendered = render_api_sites(&many);
        assert!(rendered.starts_with("0x401000, 0x401010"), "{rendered}");
        assert!(rendered.ends_with("(+3 more)"), "{rendered}");
    }
}
