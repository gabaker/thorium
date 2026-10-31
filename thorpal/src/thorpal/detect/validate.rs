//! The post-detection validation pass over API and instruction evidence.
//!
//! The [`EvidenceKind`] contract reserves this pass: each detection tags its
//! hits with how they should be confirmed or refuted, and this module is the
//! runtime consumer that actually checks the code around the evidence. It runs
//! once over the merged hit list (see `crate::thorpal::seed`), before the
//! runtime-noise classification, and reads only the cached whole-program
//! disassembly — no new Ghidra queries.
//!
//! Two independent checks run per hit:
//!
//! - **Result use** (for [`EvidenceKind::ApiResultCheck`],
//!   [`EvidenceKind::Timing`], and [`EvidenceKind::ControlFlowGated`] hits):
//!   does the evidence's result feed a compare and a conditional branch within
//!   a bounded window after the site? A proven gate earns
//!   [`CallContext::GatesBranch`] and re-earns a clamped catalog severity; a
//!   provably discarded result weakens the hit one step. Absence of a local
//!   branch proves nothing — interprocedural gates survive untouched. APIs
//!   that deliver their result through an out-parameter ([`OUT_PARAM_APIS`])
//!   are judged by the compare-and-branch shape alone and are never weakened
//!   by the accumulator discard, which only ever drops their success flag.
//! - **Argument recovery** (for the dual-use catalogs in [`ARG_SPECS`]):
//!   recover the one discriminating argument from the call's staging. An
//!   in-set value confirms the catalog's claim (the decoded meaning is
//!   appended and a clamped catalog severity restored); an out-of-set value
//!   refutes it (the reasoning is rewritten around the recovered value and the
//!   hit weakened a step); an unrecoverable argument leaves the presence-only
//!   clamp standing.

use std::collections::HashMap;

use ghidra_cli::models::Instruction;
use tracing::instrument;

use crate::thorpal::detect::context::{AnalysisContext, parse_addr};
use crate::thorpal::detect::detections::{
    DetectionHit, EvidenceKind, confidence_rank, parse_imm, reg_value_before,
};
use crate::thorpal::detect::op_str;
use crate::thorpal::detect::provenance_types::CallContext;

/// How many instructions past an evidence site the result-use scan reads.
const RESULT_WINDOW: usize = 12;

/// How many instructions before a call site the argument-staging walk reads.
const ARG_WINDOW: usize = 16;

/// The Microsoft x64 integer argument registers by 1-based position, as the
/// `(32-bit, 64-bit)` name pair the register-recovery walk matches.
const X64_ARG_REGS: &[(&str, &str)] = &[("ecx", "rcx"), ("edx", "rdx"), ("r8d", "r8")];

/// Alias groups for the x86/x64 general registers the result scan tracks.
///
/// Each row lists one register's names widest-first; writing or reading any
/// alias is treated as touching the whole family (a 32-bit write zero-extends
/// into the 64-bit name on x64, and partial writes are handled conservatively
/// by [`is_full_width`]).
const REG_FAMILIES: &[&[&str]] = &[
    &["rax", "eax", "ax", "al", "ah"],
    &["rbx", "ebx", "bx", "bl", "bh"],
    &["rcx", "ecx", "cx", "cl", "ch"],
    &["rdx", "edx", "dx", "dl", "dh"],
    &["rsi", "esi", "si", "sil"],
    &["rdi", "edi", "di", "dil"],
    &["r8", "r8d", "r8w", "r8b"],
    &["r9", "r9d", "r9w", "r9b"],
    &["r10", "r10d", "r10w", "r10b"],
    &["r11", "r11d", "r11w", "r11b"],
];

/// The accumulator family's index in [`REG_FAMILIES`] (call results, RDTSC's
/// low half, CPUID's EAX).
const ACCUMULATOR: usize = 0;

/// The EBX family's index in [`REG_FAMILIES`] (CPUID's vendor/feature output).
const BASE_REG: usize = 1;

/// The ECX family's index in [`REG_FAMILIES`] (CPUID's feature/hypervisor bits).
const COUNT_REG: usize = 2;

/// The EDX family's index in [`REG_FAMILIES`] (RDTSC's high half, CPUID's EDX).
const DATA_REG: usize = 3;

/// The catalog APIs whose checked result arrives through an out-parameter.
///
/// For these the accumulator holds only a success flag — the result the
/// sample actually tests lives in caller memory the register tracker cannot
/// follow. The common consuming shape (`call GetCursorPos; mov eax, [pt];
/// cmp eax, imm; jnz …`) overwrites the accumulator before reading it, which
/// the accumulator scan would misjudge as a provable discard, so these hits
/// are routed to [`out_param_result_use`] instead: any compare arming a
/// conditional branch in the window is the gate, and the discard verdict is
/// never emitted.
const OUT_PARAM_APIS: &[&str] = &[
    "GetCursorPos",
    "GetLastInputInfo",
    "EnumDisplaySettings",
    "GlobalMemoryStatusEx",
];

/// A catalog entry describing the one discriminating argument of a dual-use API.
///
/// Keyed by the normalized API names `build_api_hit` stamps onto hits; the
/// recovery mechanics (register staging on x64, pushes on x86) live in
/// [`recover_argument`].
struct ArgSpec {
    /// The normalized API names this entry covers (Nt/Zw twins share one row).
    apis: &'static [&'static str],
    /// The argument's 1-based position in the call.
    position: usize,
    /// The argument's name in the API's own vocabulary (e.g. `InformationClass`).
    label: &'static str,
    /// The recovered values that confirm the catalog's claim for this API.
    relevant: &'static [u64],
    /// Whether `relevant` holds bit flags (any set bit confirms) rather than an
    /// exact enumeration of values.
    flags: bool,
    /// Decoded names for the values worth spelling out, relevant or not.
    names: &'static [(u64, &'static str)],
    /// The claim an out-of-set value refutes (e.g. `"a debug-class query"`).
    claim: &'static str,
}

/// The dual-use APIs whose one discriminating argument the pass recovers.
const ARG_SPECS: &[ArgSpec] = &[
    ArgSpec {
        apis: &["NtQueryInformationProcess", "ZwQueryInformationProcess"],
        position: 2,
        label: "InformationClass",
        relevant: &[0x7, 0x1e, 0x1f],
        flags: false,
        names: &[
            (0x0, "ProcessBasicInformation"),
            (0x7, "ProcessDebugPort"),
            (0x1e, "ProcessDebugObjectHandle"),
            (0x1f, "ProcessDebugFlags"),
        ],
        claim: "a debug-class query",
    },
    ArgSpec {
        apis: &["NtQuerySystemInformation", "ZwQuerySystemInformation"],
        position: 1,
        label: "InformationClass",
        relevant: &[0x23],
        flags: false,
        names: &[
            (0x0, "SystemBasicInformation"),
            (0x5, "SystemProcessInformation"),
            (0x23, "SystemKernelDebuggerInformation"),
        ],
        claim: "the kernel-debugger query (SystemKernelDebuggerInformation)",
    },
    ArgSpec {
        apis: &["NtSetInformationThread", "ZwSetInformationThread"],
        position: 2,
        label: "InformationClass",
        relevant: &[0x11],
        flags: false,
        names: &[(0x11, "ThreadHideFromDebugger")],
        claim: "the debugger-blinding call (ThreadHideFromDebugger)",
    },
    // the class is the call's 5th argument: recoverable from x86 push staging;
    // on x64 it is a stack argument beyond the register table, so recovery
    // degrades to the presence-only clamp there
    ArgSpec {
        apis: &["NtSetInformationFile", "ZwSetInformationFile"],
        position: 5,
        label: "FileInformationClass",
        relevant: &[0x4, 0xa, 0xd, 0x41],
        flags: false,
        names: &[
            (0x4, "FileBasicInformation (timestomp surface)"),
            (0xa, "FileRenameInformation"),
            (0xd, "FileDispositionInformation (delete-on-close)"),
            (0x41, "FileRenameInformationEx"),
        ],
        claim: "a rename/delete/timestamp information class",
    },
    ArgSpec {
        apis: &["GetSystemMetrics"],
        position: 1,
        label: "index",
        relevant: &[0x0, 0x1, 0x13, 0x50, 0x1000],
        flags: false,
        names: &[
            (0x0, "SM_CXSCREEN"),
            (0x1, "SM_CYSCREEN"),
            (0xb, "SM_CXICON"),
            (0xc, "SM_CYICON"),
            (0x13, "SM_MOUSEPRESENT"),
            (0x14, "SM_CYVSCROLL"),
            (0x15, "SM_CXHSCROLL"),
            (0x2a, "SM_DBCSENABLED"),
            (0x4a, "SM_MIDEASTENABLED"),
            (0x50, "SM_CMONITORS"),
            (0x59, "SM_SERVERR2"),
            (0x1000, "SM_REMOTESESSION"),
        ],
        claim: "a sandbox-relevant metric probe",
    },
    ArgSpec {
        apis: &["CreateToolhelp32Snapshot"],
        position: 1,
        label: "dwFlags",
        relevant: &[0x2],
        flags: true,
        names: &[
            (0x2, "TH32CS_SNAPPROCESS"),
            (0x4, "TH32CS_SNAPTHREAD"),
            (0x8, "TH32CS_SNAPMODULE"),
        ],
        claim: "the process-snapshot flag (TH32CS_SNAPPROCESS)",
    },
    ArgSpec {
        apis: &["SetWindowsHookEx"],
        position: 1,
        label: "idHook",
        relevant: &[0x2, 0xd, 0xe],
        flags: false,
        names: &[
            (0x2, "WH_KEYBOARD"),
            (0x4, "WH_CALLWNDPROC"),
            (0x5, "WH_CBT"),
            (0x7, "WH_MOUSE"),
            (0xd, "WH_KEYBOARD_LL"),
            (0xe, "WH_MOUSE_LL"),
        ],
        claim: "an input-capture hook",
    },
    ArgSpec {
        apis: &["MoveFileEx"],
        position: 3,
        label: "dwFlags",
        relevant: &[0x4],
        flags: true,
        names: &[
            (0x1, "MOVEFILE_REPLACE_EXISTING"),
            (0x2, "MOVEFILE_COPY_ALLOWED"),
            (0x4, "MOVEFILE_DELAY_UNTIL_REBOOT"),
            (0x8, "MOVEFILE_WRITE_THROUGH"),
        ],
        claim: "the boot-time self-delete flag (MOVEFILE_DELAY_UNTIL_REBOOT)",
    },
];

impl ArgSpec {
    /// Whether a recovered value confirms the catalog's claim for this API.
    ///
    /// # Arguments
    ///
    /// * `value` - The recovered argument value
    fn is_relevant(&self, value: u64) -> bool {
        // flag masks confirm on any set bit; enumerations on an exact match
        if self.flags {
            self.relevant.iter().any(|flag| value & *flag != 0)
        } else {
            self.relevant.contains(&value)
        }
    }

    /// Render a recovered value with its decoded meaning (e.g.
    /// `InformationClass=0x1e (ProcessDebugObjectHandle)`).
    ///
    /// # Arguments
    ///
    /// * `value` - The recovered argument value
    fn decode(&self, value: u64) -> String {
        // name the value: the set bits for flag masks, an exact match otherwise
        let name = if self.flags {
            let set: Vec<&'static str> = self
                .names
                .iter()
                .filter(|(bit, _)| value & *bit != 0)
                .map(|(_, name)| *name)
                .collect();
            if set.is_empty() {
                None
            } else {
                Some(set.join("|"))
            }
        } else {
            self.names
                .iter()
                .find(|(known, _)| *known == value)
                .map(|(_, name)| (*name).to_string())
        };
        // render `label=value (meaning)`, leaving the meaning off when unknown
        match name {
            Some(name) => format!("{}={value:#x} ({name})", self.label),
            None => format!("{}={value:#x}", self.label),
        }
    }
}

/// Look up the argument-recovery entry for a catalog API name.
///
/// # Arguments
///
/// * `api` - The hit's normalized catalog API name
fn arg_spec_for(api: &str) -> Option<&'static ArgSpec> {
    // the table is tiny, so a linear scan over the name lists is fine
    ARG_SPECS.iter().find(|spec| spec.apis.contains(&api))
}

/// Whether a catalog API delivers its checked result via an out-parameter.
///
/// # Arguments
///
/// * `api` - The hit's normalized catalog API name
fn is_out_param_api(api: &str) -> bool {
    // a straight membership test against the out-parameter set
    OUT_PARAM_APIS.contains(&api)
}

/// What the result-use scan concluded about a hit's evidence sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResultUse {
    /// The result feeds a compare and then a conditional branch.
    Gates,
    /// The result register is provably overwritten before any read.
    Discarded,
    /// Neither was proven; the result may flow elsewhere (interprocedurally).
    Unknown,
}

/// What the argument recovery concluded for one hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recovered {
    /// A value in the API's relevant set was recovered at some site.
    InSet(u64),
    /// Only values outside the relevant set were recovered.
    OutOfSet(u64),
    /// No literal argument could be recovered at any site.
    Unknown,
}

/// Validate the merged detection hits against the code around them, in place.
///
/// For hits whose [`EvidenceKind`] promises a result-use check, a proven
/// result-gates-branch earns [`CallContext::GatesBranch`] (rendered by the
/// seeder's comment body) and restores a clamped catalog severity, while a
/// provably discarded result weakens the hit one step; nothing is weakened for
/// the mere absence of a local branch. For hits naming an API in the
/// argument-recovery table, the recovered discriminating argument confirms
/// (annotate + restore), refutes (reword + weaken), or leaves the hit as the
/// presence-only clamp seeded it. Reads only the cached whole-program
/// disassembly; any failure fetching it degrades to a no-op pass, because
/// seeding is best-effort.
///
/// # Arguments
///
/// * `ctx` - The analysis context (cached disassembly, program info)
/// * `hits` - The merged detection hits to validate in place
#[instrument(name = "validate::apply", skip_all)]
pub async fn apply(ctx: &AnalysisContext, hits: &mut [DetectionHit]) {
    // skip the disassembly lookup entirely when no hit carries a checkable
    // contract (a validated kind, or a catalog API the table recovers)
    let wanted = hits.iter().any(|hit| {
        hit.function.is_some()
            && (matches!(
                hit.kind,
                EvidenceKind::ApiResultCheck
                    | EvidenceKind::Timing
                    | EvidenceKind::ControlFlowGated
            ) || hit.api.and_then(arg_spec_for).is_some())
    });
    if !wanted {
        return;
    }
    // fetch the (cached) whole-program disassembly once for every hit below
    let all = match ctx.all_function_disassembly().await {
        Ok(all) => all,
        Err(error) => {
            tracing::warn!(?error, "validation pass skipped: disassembly unavailable");
            return;
        }
    };
    // index the listings by function entry for the per-hit lookups
    let listings: HashMap<u64, &[Instruction]> = all
        .iter()
        .map(|(entry, insns)| (*entry, insns.as_slice()))
        .collect();
    // the x64/x86 split decides how call arguments are staged
    let is64 = is_64_bit(ctx);
    // judge every hit by the same two checks
    for hit in hits.iter_mut() {
        validate_hit(hit, &listings, is64);
    }
}

/// Whether the analyzed program is 64-bit, read from its language spec.
///
/// # Arguments
///
/// * `ctx` - The analysis context carrying the program info
fn is_64_bit(ctx: &AnalysisContext) -> bool {
    // Ghidra language ids carry the pointer size as `<proc>:<endian>:<bits>:…`
    ctx.program
        .language
        .as_deref()
        .unwrap_or_default()
        .contains(":64")
}

/// Run both validation checks over one hit, adjusting it in place.
///
/// # Arguments
///
/// * `hit` - The hit to validate
/// * `listings` - The per-function disassembly, keyed by entry address
/// * `is64` - Whether the program is 64-bit (decides argument staging)
fn validate_hit(hit: &mut DetectionHit, listings: &HashMap<u64, &[Instruction]>, is64: bool) {
    // only hits inside a known, disassembled function can be checked
    let Some(entry) = hit.function else {
        return;
    };
    let Some(insns) = listings.get(&entry).copied() else {
        return;
    };
    // every evidence site backing the hit (the primary stands alone when the
    // builder attached no site list)
    let sites: &[u64] = if hit.sites.is_empty() {
        std::slice::from_ref(&hit.address)
    } else {
        &hit.sites
    };
    // the result-use check, for the kinds whose contract promises one; APIs
    // whose result arrives via an out-parameter get the compare-only scan,
    // because the accumulator holds just their success flag
    let checked = matches!(
        hit.kind,
        EvidenceKind::ApiResultCheck | EvidenceKind::Timing | EvidenceKind::ControlFlowGated
    );
    let result = if !checked {
        ResultUse::Unknown
    } else if hit.api.is_some_and(is_out_param_api) {
        out_param_result_use(insns, sites)
    } else {
        result_use(insns, sites)
    };
    // the argument recovery, for the known dual-use catalogs
    let spec = hit.api.and_then(arg_spec_for);
    let recovered = spec.map(|spec| recover_argument(insns, sites, spec, is64));
    // a proven gate is recorded as context; the seeder renders its note
    if result == ResultUse::Gates && hit.context.is_none() {
        hit.context = Some(CallContext::GatesBranch);
    }
    // only a provable discard — never the absence of a local branch — weakens,
    // and only hits still above the floor move
    if result == ResultUse::Discarded && confidence_rank(hit.confidence) > 0 {
        hit.weaken();
    }
    // apply what the argument recovery concluded
    match (spec, recovered) {
        (Some(spec), Some(Recovered::InSet(value))) => {
            // the discriminating argument confirms the catalog claim: append
            // the decoded meaning and restore the clamped catalog severity
            hit.content.push_str(&format!(" — {}", spec.decode(value)));
            restore_catalog_severity(hit);
        }
        (Some(spec), Some(Recovered::OutOfSet(value))) => {
            // the argument refutes the catalog claim: rewrite the reasoning
            // around what was actually recovered and weaken the hit a step
            hit.reasoning = format!(
                "{} is called with {} — not {}. The recovered argument does not support \
                 the catalog's claim for this call.",
                hit.api.unwrap_or("the API"),
                spec.decode(value),
                spec.claim
            );
            hit.weaken();
        }
        _ => {
            // no recovered argument: a proven gate alone still re-earns the
            // catalog severity; otherwise the presence-only clamp stands
            if result == ResultUse::Gates {
                restore_catalog_severity(hit);
            }
        }
    }
}

/// Restore the catalog severity the presence-only clamp held back, when any.
///
/// # Arguments
///
/// * `hit` - The hit whose verified use re-earned its catalog severity
fn restore_catalog_severity(hit: &mut DetectionHit) {
    // only clamped hits carry a withheld severity; restoring consumes it
    if let Some((confidence, suspicion)) = hit.catalog_severity.take() {
        hit.confidence = confidence;
        hit.suspicion = suspicion;
    }
}

/// Judge the result use across every site backing one hit.
///
/// One proven gate confirms the hit; a provable discard must hold at every
/// locatable site (an unlocatable site proves nothing, so it blocks the
/// discard verdict).
///
/// # Arguments
///
/// * `insns` - The containing function's disassembly
/// * `sites` - The evidence sites to judge
fn result_use(insns: &[Instruction], sites: &[u64]) -> ResultUse {
    // scan each site, short-circuiting on the first proven gate
    let mut any_discarded = false;
    let mut all_discarded = true;
    for site in sites {
        // judge the site when it can be located in the listing
        let outcome =
            site_index(insns, *site).map_or(ResultUse::Unknown, |idx| result_use_at(insns, idx));
        match outcome {
            ResultUse::Gates => return ResultUse::Gates,
            ResultUse::Discarded => any_discarded = true,
            ResultUse::Unknown => all_discarded = false,
        }
    }
    // the discard verdict needs every site discarded and at least one judged
    if any_discarded && all_discarded {
        ResultUse::Discarded
    } else {
        ResultUse::Unknown
    }
}

/// Judge what the code after one evidence site does with its result.
///
/// Tracks the result's register family (and any memory slot it is stored to)
/// through a bounded forward window: a `TEST`/`CMP`/`BT` touching the result
/// followed by a conditional jump is a gate; a full-width overwrite with no
/// prior read on a straight line (no intervening branch) is a provable
/// discard; everything else — including the window simply ending — is unknown.
///
/// # Arguments
///
/// * `insns` - The containing function's disassembly
/// * `site_idx` - The evidence site's index within `insns`
fn result_use_at(insns: &[Instruction], site_idx: usize) -> ResultUse {
    // which register families hold the site's result
    let mut tracked = result_families(&insns[site_idx]);
    if tracked.is_empty() {
        return ResultUse::Unknown;
    }
    // a site that is itself a compare (0xCC scans, trap-flag tests) has already
    // consumed its evidence into the flags: the next conditional jump gates
    let site_mnemonic = insns[site_idx].mnemonic.to_ascii_uppercase();
    let mut compared = matches!(site_mnemonic.as_str(), "TEST" | "CMP" | "BT");
    // the memory slots the result was stored to (a compare of one still gates)
    let mut slots: Vec<String> = Vec::new();
    // whether anything read the result (a read vetoes the discard verdict)
    let mut read = compared;
    // whether control may have diverged (a Jcc also vetoes the discard verdict:
    // an overwrite past one is no longer guaranteed on every path)
    let mut diverged = false;
    // walk the bounded window after the site
    for insn in insns.iter().skip(site_idx + 1).take(RESULT_WINDOW) {
        let mnemonic = insn.mnemonic.to_ascii_uppercase();
        let ops = op_str(insn).to_ascii_lowercase();
        // a conditional branch: the gate when a compare armed it, otherwise a
        // divergence the discard verdict must respect
        if mnemonic.starts_with('J') && mnemonic != "JMP" {
            if compared {
                return ResultUse::Gates;
            }
            diverged = true;
            continue;
        }
        // straight-line flow ends here; nothing further can be proven locally
        if mnemonic == "JMP" || mnemonic.starts_with("RET") {
            return ResultUse::Unknown;
        }
        // a test/cmp/bt touching the result (register or stored slot) arms the gate
        if matches!(mnemonic.as_str(), "TEST" | "CMP" | "BT")
            && touches(&ops, insn, &tracked, &slots)
        {
            compared = true;
            read = true;
            continue;
        }
        // any other flag-writing instruction disarms a pending compare: a later
        // Jcc would test its flags, not the result's
        if compared && writes_flags(&mnemonic) {
            compared = false;
        }
        // register moves: stores to memory, overwrites, and result propagation
        if mnemonic == "MOV" {
            let parts = operand_list(insn);
            if let (Some(dest), Some(src)) = (parts.first(), parts.get(1)) {
                // mov [slot], reg stores the result; the slot inherits tracking
                if let Some(slot) = mem_slot(dest) {
                    if tracked.iter().any(|family| mentions_family(src, *family)) {
                        slots.push(slot);
                        read = true;
                    } else {
                        // the slot now holds something else; stop tracking it
                        slots.retain(|known| known != &slot);
                    }
                    continue;
                }
                // a full-width mov INTO a tracked register from a source that
                // never reads the family overwrites the result
                if let Some(family) = family_of(dest) {
                    if tracked.contains(&family)
                        && is_full_width(dest)
                        && !mentions_family(src, family)
                    {
                        // overwritten with no prior read on a straight line:
                        // provably discarded
                        if !read && !diverged {
                            return ResultUse::Discarded;
                        }
                        // the family no longer holds the result; stop tracking it
                        tracked.retain(|kept| *kept != family);
                        if tracked.is_empty() && slots.is_empty() {
                            return ResultUse::Unknown;
                        }
                        continue;
                    }
                    // mov of a tracked register into another register copies
                    // the result there, so track the copy too
                    if let Some(src_family) = family_of(src) {
                        if tracked.contains(&src_family) && !tracked.contains(&family) {
                            tracked.push(family);
                            read = true;
                            continue;
                        }
                    }
                }
            }
        }
        // xor/sub reg,reg zeroes a tracked register without using its value
        if matches!(mnemonic.as_str(), "XOR" | "SUB") {
            let parts = operand_list(insn);
            if let (Some(dest), Some(src)) = (parts.first(), parts.get(1)) {
                if dest == src && is_full_width(dest) {
                    if let Some(family) = family_of(dest) {
                        if tracked.contains(&family) {
                            // zeroed with no prior read on a straight line:
                            // provably discarded
                            if !read && !diverged {
                                return ResultUse::Discarded;
                            }
                            // the family no longer holds the result
                            tracked.retain(|kept| *kept != family);
                            if tracked.is_empty() && slots.is_empty() {
                                return ResultUse::Unknown;
                            }
                            continue;
                        }
                    }
                }
            }
        }
        // a call clobbers the volatile registers (and the flags) with its own
        // effects; stop tracking them (not a provable discard: the callee may
        // consume them)
        if mnemonic == "CALL" {
            tracked.clear();
            compared = false;
            if slots.is_empty() {
                return ResultUse::Unknown;
            }
            continue;
        }
        // any other mention of the result is a read the discard verdict respects
        if tracked.iter().any(|family| mentions_family(&ops, *family)) {
            read = true;
        }
    }
    ResultUse::Unknown
}

/// Judge the result use across the sites of an out-parameter API hit.
///
/// One gated site confirms the hit, exactly like [`result_use`]; the discard
/// verdict is never emitted, because an accumulator overwrite after one of
/// these calls only drops the success flag, not the result.
///
/// # Arguments
///
/// * `insns` - The containing function's disassembly
/// * `sites` - The evidence sites to judge
fn out_param_result_use(insns: &[Instruction], sites: &[u64]) -> ResultUse {
    // scan each locatable site, short-circuiting on the first proven gate
    for site in sites {
        if site_index(insns, *site).is_some_and(|idx| out_param_gates_at(insns, idx)) {
            return ResultUse::Gates;
        }
    }
    ResultUse::Unknown
}

/// Whether a compare arms a conditional jump in the window after one site.
///
/// The result lives in caller memory the register tracker cannot follow, so
/// any `TEST`/`CMP`/`BT` counts as reading it (or the success flag): a
/// conditional jump while that compare's flags are still live is the gate.
/// Straight-line flow ending (`JMP`/`RET`) proves nothing, and a call or any
/// other flag writer disarms a pending compare — a later jump would test its
/// flags, not the result's.
///
/// # Arguments
///
/// * `insns` - The containing function's disassembly
/// * `site_idx` - The evidence site's index within `insns`
fn out_param_gates_at(insns: &[Instruction], site_idx: usize) -> bool {
    // whether a compare has armed the flags since the site
    let mut compared = false;
    // walk the same bounded window the accumulator scan reads
    for insn in insns.iter().skip(site_idx + 1).take(RESULT_WINDOW) {
        let mnemonic = insn.mnemonic.to_ascii_uppercase();
        // a conditional jump is the gate when a compare armed it
        if mnemonic.starts_with('J') && mnemonic != "JMP" {
            if compared {
                return true;
            }
            continue;
        }
        // straight-line flow ends here; nothing further is local evidence
        if mnemonic == "JMP" || mnemonic.starts_with("RET") {
            return false;
        }
        // any compare arms the gate, since the out-parameter slot is untracked
        if matches!(mnemonic.as_str(), "TEST" | "CMP" | "BT") {
            compared = true;
            continue;
        }
        // a callee's flags replace a pending compare's before any later jump
        if mnemonic == "CALL" {
            compared = false;
            continue;
        }
        // any other flag-writing instruction disarms a pending compare too
        if compared && writes_flags(&mnemonic) {
            compared = false;
        }
    }
    false
}

/// Whether a mnemonic rewrites the CPU flags, disarming a pending compare.
///
/// # Arguments
///
/// * `mnemonic` - The uppercased instruction mnemonic
fn writes_flags(mnemonic: &str) -> bool {
    // the common ALU and compare mnemonics; MOV/LEA/PUSH/POP leave flags alone
    matches!(
        mnemonic,
        "ADD"
            | "ADC"
            | "SUB"
            | "SBB"
            | "AND"
            | "OR"
            | "XOR"
            | "NEG"
            | "INC"
            | "DEC"
            | "MUL"
            | "IMUL"
            | "DIV"
            | "IDIV"
            | "SHL"
            | "SHR"
            | "SAR"
            | "ROL"
            | "ROR"
            | "TEST"
            | "CMP"
            | "BT"
            | "BTS"
            | "BTR"
            | "CMPXCHG"
    )
}

/// Whether a compare's operands touch a tracked register family or stored slot.
///
/// # Arguments
///
/// * `ops` - The instruction's lowercased, joined operand string
/// * `insn` - The instruction itself (for per-operand slot extraction)
/// * `tracked` - The tracked register families
/// * `slots` - The normalized memory slots holding the result
fn touches(ops: &str, insn: &Instruction, tracked: &[usize], slots: &[String]) -> bool {
    // a tracked register named anywhere in the operands counts
    if tracked.iter().any(|family| mentions_family(ops, *family)) {
        return true;
    }
    // otherwise look for a tracked memory slot among the operands
    operand_list(insn)
        .iter()
        .filter_map(|operand| mem_slot(operand))
        .any(|slot| slots.contains(&slot))
}

/// The register families an evidence site's result lands in.
///
/// # Arguments
///
/// * `site` - The evidence site's instruction
fn result_families(site: &Instruction) -> Vec<usize> {
    // decide by the site's own mnemonic
    let mnemonic = site.mnemonic.to_ascii_uppercase();
    // a call's integer result lands in the accumulator
    if mnemonic == "CALL" {
        return vec![ACCUMULATOR];
    }
    // RDTSC splits the counter across EDX:EAX
    if mnemonic == "RDTSC" || mnemonic == "RDTSCP" {
        return vec![ACCUMULATOR, DATA_REG];
    }
    // CPUID fills all four general output registers
    if mnemonic == "CPUID" {
        return vec![ACCUMULATOR, BASE_REG, COUNT_REG, DATA_REG];
    }
    // a compare site keeps its own operand registers in play for the window
    // (the flags carry the evidence; the registers matter only for reads)
    // otherwise the destination register of the instruction holds the result
    operand_list(site)
        .first()
        .and_then(|dest| family_of(dest))
        .map_or_else(Vec::new, |family| vec![family])
}

/// The register family index of a bare register token, when it is one.
///
/// # Arguments
///
/// * `token` - The lowercased, trimmed operand token
fn family_of(token: &str) -> Option<usize> {
    // match the token against every family's alias list
    REG_FAMILIES
        .iter()
        .position(|family| family.contains(&token))
}

/// Whether a register token is a family's full-width (64/32-bit) alias.
///
/// Partial (8/16-bit) writes do not replace a register's whole value, so only
/// full-width writes count as overwrites for the discard verdict.
///
/// # Arguments
///
/// * `token` - The lowercased register token
fn is_full_width(token: &str) -> bool {
    // the first two aliases in each family row are the 64- and 32-bit names
    REG_FAMILIES
        .iter()
        .any(|family| family.iter().take(2).any(|name| *name == token))
}

/// Whether an operand string mentions any register of the given family.
///
/// # Arguments
///
/// * `ops` - The lowercased operand text to scan
/// * `family` - The register family index to look for
fn mentions_family(ops: &str, family: usize) -> bool {
    // scan whole alphanumeric tokens so `ax` never matches inside e.g. `eax`
    ops.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .any(|token| REG_FAMILIES[family].contains(&token))
}

/// The normalized `[...]` memory expression inside an operand, when present.
///
/// # Arguments
///
/// * `operand` - The lowercased operand token
fn mem_slot(operand: &str) -> Option<String> {
    // keep just the bracketed addressing expression, spaces removed, so the
    // same slot compares equal however the bridge spaces it
    let start = operand.find('[')?;
    let end = operand[start..].find(']')? + start;
    Some(
        operand[start..=end]
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect(),
    )
}

/// Split an instruction's rendered operands into trimmed, lowercased tokens.
///
/// The bridge renders operands either per-operand or as one joined string, so
/// splitting the joined form on commas gives a uniform per-operand view.
///
/// # Arguments
///
/// * `insn` - The instruction whose operands to split
fn operand_list(insn: &Instruction) -> Vec<String> {
    // join, lowercase, and split so both bridge renderings look the same
    op_str(insn)
        .to_ascii_lowercase()
        .split(',')
        .map(|operand| operand.trim().to_string())
        .filter(|operand| !operand.is_empty())
        .collect()
}

/// Locate an evidence site's index within a function listing.
///
/// # Arguments
///
/// * `insns` - The function's disassembly
/// * `site` - The site address to find
fn site_index(insns: &[Instruction], site: u64) -> Option<usize> {
    // addresses come off the bridge as hex text, so compare parsed values
    insns
        .iter()
        .position(|insn| parse_addr(&insn.address) == Some(site))
}

/// Whether a mnemonic transfers control, ending an argument-staging window.
///
/// # Arguments
///
/// * `mnemonic` - The instruction mnemonic
fn is_control_transfer(mnemonic: &str) -> bool {
    // any jump family, call, or return ends the staging straight line
    let m = mnemonic.to_ascii_uppercase();
    m.starts_with('J') || m == "CALL" || m.starts_with("RET")
}

/// The index where a call site's argument-staging window begins.
///
/// # Arguments
///
/// * `insns` - The containing function's disassembly
/// * `site_idx` - The call site's index within `insns`
fn staging_start(insns: &[Instruction], site_idx: usize) -> usize {
    // bound the walk, then start after the nearest control transfer: a value
    // staged before one belongs to some other site
    let floor = site_idx.saturating_sub(ARG_WINDOW);
    insns[floor..site_idx]
        .iter()
        .rposition(|insn| is_control_transfer(&insn.mnemonic))
        .map_or(floor, |idx| floor + idx + 1)
}

/// Recover the literal staged into an x64 argument register before a call.
///
/// A generalization of the engine's accumulator recovery: the same
/// `mov reg, imm` / `xor reg, reg` walk ([`reg_value_before`]) runs over the
/// call's own staging window instead of the whole preceding body.
///
/// # Arguments
///
/// * `insns` - The containing function's disassembly
/// * `site_idx` - The call site's index within `insns`
/// * `reg32` - The argument register's 32-bit name (e.g. `"edx"`)
/// * `reg64` - The argument register's 64-bit name (e.g. `"rdx"`)
fn staged_reg_value(
    insns: &[Instruction],
    site_idx: usize,
    reg32: &str,
    reg64: &str,
) -> Option<u64> {
    // bound the staging window to this site, then reuse the engine's recovery
    let start = staging_start(insns, site_idx);
    reg_value_before(&insns[start..site_idx], reg32, reg64)
}

/// Recover the literal pushed as the Nth x86 stack argument before a call.
///
/// x86 arguments push right-to-left, so the Nth `PUSH` walking backwards from
/// the call is the Nth argument; non-literal pushes still occupy their
/// position, so a register push between the call and the target slot never
/// shifts the count.
///
/// # Arguments
///
/// * `insns` - The containing function's disassembly
/// * `site_idx` - The call site's index within `insns`
/// * `position` - The argument's 1-based position
fn pushed_arg_value(insns: &[Instruction], site_idx: usize, position: usize) -> Option<u64> {
    // bound the staging window the same way the register walk is bounded
    let start = staging_start(insns, site_idx);
    // count pushes backwards from the call until the target position
    let mut seen = 0;
    for insn in insns[start..site_idx].iter().rev() {
        if insn.mnemonic.eq_ignore_ascii_case("PUSH") {
            seen += 1;
            if seen == position {
                // only a literal push yields a value (reuse the engine's parser)
                return parse_imm(&op_str(insn).to_ascii_lowercase());
            }
        }
    }
    None
}

/// Recover one hit's discriminating argument across its call sites.
///
/// Any in-set recovery confirms the claim outright; otherwise the first
/// out-of-set recovery stands; otherwise nothing was recoverable.
///
/// # Arguments
///
/// * `insns` - The containing function's disassembly
/// * `sites` - The hit's call sites
/// * `spec` - The API's argument-recovery entry
/// * `is64` - Whether the program is 64-bit (decides the staging mechanics)
fn recover_argument(insns: &[Instruction], sites: &[u64], spec: &ArgSpec, is64: bool) -> Recovered {
    // the x64 register pair for this argument position, when one exists
    let regs = X64_ARG_REGS.get(spec.position - 1);
    // try every site; remember the first out-of-set value as the fallback
    let mut out_of_set = None;
    for site in sites {
        // skip sites that cannot be located in the listing
        let Some(idx) = site_index(insns, *site) else {
            continue;
        };
        // recover the staged value per the program's calling convention
        let value = if is64 {
            regs.and_then(|(reg32, reg64)| staged_reg_value(insns, idx, reg32, reg64))
        } else {
            pushed_arg_value(insns, idx, spec.position)
        };
        match value {
            // an in-set value confirms the catalog claim outright
            Some(value) if spec.is_relevant(value) => return Recovered::InSet(value),
            // keep the first out-of-set value as the refuting evidence
            Some(value) => out_of_set = out_of_set.or(Some(value)),
            None => {}
        }
    }
    // no site confirmed: report the refutation over nothing at all
    match out_of_set {
        Some(value) => Recovered::OutOfSet(value),
        None => Recovered::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::borrow::Cow;

    use crate::thorpal::detect::confidence::Confidence;
    use crate::thorpal::detect::detections::{AttackTactic, AttackTechnique, high, medium};

    /// A minimal technique for building test hits.
    const TECH: AttackTechnique = AttackTechnique {
        id: Cow::Borrowed("T1622"),
        name: Cow::Borrowed("Debugger Evasion"),
        tactic: AttackTactic::DefenseEvasion,
    };

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

    /// Build a clamped API-catalog hit for one call site, as `build_api_hit`
    /// seeds a catalog `high()` entry.
    ///
    /// # Arguments
    ///
    /// * `api` - The normalized catalog API name
    /// * `kind` - The evidence kind routing validation
    /// * `function` - The containing function entry
    /// * `site` - The call-site address
    fn api_hit(api: &'static str, kind: EvidenceKind, function: u64, site: u64) -> DetectionHit {
        // seeded at medium with the catalog's high() kept aside, like the clamp
        DetectionHit::new(
            TECH,
            site,
            Some(function),
            medium(),
            format!("{api} call @ {site:#x}"),
            "catalog reasoning".to_string(),
        )
        .kind(kind)
        .api(api)
        .catalog_severity(high())
    }

    /// Build the listings map for one function's fixture instructions.
    ///
    /// # Arguments
    ///
    /// * `entry` - The function entry address
    /// * `insns` - The function's fixture instructions
    fn listings(entry: u64, insns: &[Instruction]) -> HashMap<u64, &[Instruction]> {
        HashMap::from([(entry, insns)])
    }

    /// An x64 class-0 recovery demotes the hit and rewrites the reasoning.
    #[test]
    fn x64_class_zero_demotes_and_rewords() {
        // EDX staged with ProcessBasicInformation (0), then the call
        let insns = vec![
            insn(0x1000, "MOV", &["RCX", "-0x1"]),
            insn(0x1004, "MOV", &["EDX", "0x0"]),
            insn(0x1008, "CALL", &["qword ptr [0x5000]"]),
            insn(0x100e, "RET", &[]),
        ];
        let mut hit = api_hit(
            "NtQueryInformationProcess",
            EvidenceKind::ApiResultCheck,
            0x1000,
            0x1008,
        );
        validate_hit(&mut hit, &listings(0x1000, &insns), true);
        // out-of-set: one step below the clamped medium, with honest wording
        assert!(matches!(hit.confidence, Confidence::Untrusted));
        assert_eq!(hit.suspicion, 25);
        assert!(
            hit.reasoning
                .contains("InformationClass=0x0 (ProcessBasicInformation)"),
            "{}",
            hit.reasoning
        );
        assert!(
            hit.reasoning.contains("not a debug-class query"),
            "{}",
            hit.reasoning
        );
        // no gate was proven, so no context note is attached
        assert!(hit.context.is_none());
    }

    /// An x64 debug-class recovery keeps the catalog severity and annotates.
    #[test]
    fn x64_debug_class_keeps_catalog_severity() {
        // EDX staged with ProcessDebugObjectHandle (0x1e), then the call
        let insns = vec![
            insn(0x1000, "MOV", &["EDX", "0x1e"]),
            insn(0x1004, "CALL", &["qword ptr [0x5000]"]),
            insn(0x100a, "RET", &[]),
        ];
        let mut hit = api_hit(
            "NtQueryInformationProcess",
            EvidenceKind::ApiResultCheck,
            0x1000,
            0x1004,
        );
        validate_hit(&mut hit, &listings(0x1000, &insns), true);
        // in-set: the clamped catalog severity is restored in full
        assert!(matches!(hit.confidence, Confidence::Likely));
        assert_eq!(hit.suspicion, 75);
        // the decoded meaning rides on the content for the forum comment
        assert!(
            hit.content
                .ends_with("— InformationClass=0x1e (ProcessDebugObjectHandle)"),
            "{}",
            hit.content
        );
        // the argument alone proves no branch, so no GatesBranch context
        assert!(hit.context.is_none());
    }

    /// An x86 push walk reaches the class argument past a register push.
    #[test]
    fn x86_push_recovery_reaches_the_class_argument() {
        // right-to-left staging: the handle push sits between the call and the
        // class push, and its non-literal value must not shift the count
        let insns = vec![
            insn(0x2000, "PUSH", &["0x0"]),
            insn(0x2002, "PUSH", &["0x4"]),
            insn(0x2004, "PUSH", &["0x403000"]),
            insn(0x2006, "PUSH", &["0x7"]),
            insn(0x2008, "PUSH", &["-0x1"]),
            insn(0x200a, "CALL", &["dword ptr [0x5000]"]),
            insn(0x2010, "RET", &[]),
        ];
        let mut hit = api_hit(
            "NtQueryInformationProcess",
            EvidenceKind::ApiResultCheck,
            0x2000,
            0x200a,
        );
        validate_hit(&mut hit, &listings(0x2000, &insns), false);
        // ProcessDebugPort (7) is in-set: catalog severity restored + annotated
        assert!(matches!(hit.confidence, Confidence::Likely));
        assert!(
            hit.content
                .ends_with("— InformationClass=0x7 (ProcessDebugPort)"),
            "{}",
            hit.content
        );
    }

    /// A GetSystemMetrics index recovers from ECX and annotates the content.
    #[test]
    fn metrics_index_recovers_from_ecx() {
        // ECX staged with SM_REMOTESESSION (0x1000), then the call
        let insns = vec![
            insn(0x3000, "MOV", &["ECX", "0x1000"]),
            insn(0x3004, "CALL", &["qword ptr [0x5000]"]),
            insn(0x300a, "RET", &[]),
        ];
        // the metrics catalog asserts medium, so no clamp pair rides along
        let mut hit = DetectionHit::new(
            TECH,
            0x3004,
            Some(0x3000),
            medium(),
            "GetSystemMetrics call @ 0x3004".to_string(),
            "catalog reasoning".to_string(),
        )
        .kind(EvidenceKind::ApiResultCheck)
        .api("GetSystemMetrics");
        validate_hit(&mut hit, &listings(0x3000, &insns), true);
        // in-set with nothing withheld: the severity simply stands
        assert!(matches!(hit.confidence, Confidence::Unsure));
        assert_eq!(hit.suspicion, 40);
        assert!(
            hit.content.ends_with("— index=0x1000 (SM_REMOTESESSION)"),
            "{}",
            hit.content
        );
    }

    /// A tested result sets GatesBranch and restores the catalog severity.
    #[test]
    fn tested_result_sets_gates_branch() {
        // the classic gate: test eax, eax then a conditional jump
        let insns = vec![
            insn(0x4000, "CALL", &["qword ptr [0x5000]"]),
            insn(0x4006, "TEST", &["EAX", "EAX"]),
            insn(0x4008, "JNZ", &["0x4100"]),
            insn(0x400a, "RET", &[]),
        ];
        let mut hit = api_hit(
            "CheckRemoteDebuggerPresent",
            EvidenceKind::ApiResultCheck,
            0x4000,
            0x4000,
        );
        validate_hit(&mut hit, &listings(0x4000, &insns), true);
        // the gate earns the context note and re-earns the catalog severity
        assert_eq!(hit.context, Some(CallContext::GatesBranch));
        assert!(matches!(hit.confidence, Confidence::Likely));
        assert_eq!(hit.suspicion, 75);
    }

    /// A result stored to memory and compared there still gates.
    #[test]
    fn stored_result_compared_through_memory_gates() {
        // the result round-trips through a stack slot before the compare
        let insns = vec![
            insn(0x4000, "CALL", &["qword ptr [0x5000]"]),
            insn(0x4006, "MOV", &["dword ptr [ebp + -0x8]", "EAX"]),
            insn(0x4009, "CMP", &["dword ptr [ebp + -0x8]", "0x0"]),
            insn(0x400d, "JZ", &["0x4100"]),
            insn(0x400f, "RET", &[]),
        ];
        let mut hit = api_hit(
            "CheckRemoteDebuggerPresent",
            EvidenceKind::ApiResultCheck,
            0x4000,
            0x4000,
        );
        validate_hit(&mut hit, &listings(0x4000, &insns), true);
        assert_eq!(hit.context, Some(CallContext::GatesBranch));
    }

    /// A provably discarded result weakens the hit one step.
    #[test]
    fn provable_discard_demotes_one_step() {
        // EAX is overwritten by a fresh literal before anything reads it
        let insns = vec![
            insn(0x4000, "CALL", &["qword ptr [0x5000]"]),
            insn(0x4006, "MOV", &["EAX", "0x1"]),
            insn(0x400b, "RET", &[]),
        ];
        let mut hit = api_hit(
            "CheckRemoteDebuggerPresent",
            EvidenceKind::ApiResultCheck,
            0x4000,
            0x4000,
        );
        validate_hit(&mut hit, &listings(0x4000, &insns), true);
        // one step below the clamped medium, with no context attached
        assert!(matches!(hit.confidence, Confidence::Untrusted));
        assert_eq!(hit.suspicion, 25);
        assert!(hit.context.is_none());
        // the catalog pair stays withheld: nothing verified a use
        assert!(hit.catalog_severity.is_some());
    }

    /// Unprovable evidence is left exactly as the clamp seeded it.
    #[test]
    fn unrecoverable_evidence_stays_clamped() {
        // the result is copied away (a read), so neither gate nor discard holds,
        // and the class argument comes from a register the walk cannot read
        let insns = vec![
            insn(0x1000, "MOV", &["EDX", "EBX"]),
            insn(0x1002, "CALL", &["qword ptr [0x5000]"]),
            insn(0x1008, "MOV", &["EBX", "EAX"]),
            insn(0x100a, "RET", &[]),
        ];
        let mut hit = api_hit(
            "NtQueryInformationProcess",
            EvidenceKind::ApiResultCheck,
            0x1000,
            0x1002,
        );
        validate_hit(&mut hit, &listings(0x1000, &insns), true);
        // untouched: clamped severity, catalog reasoning, no context
        assert!(matches!(hit.confidence, Confidence::Unsure));
        assert_eq!(hit.suspicion, 40);
        assert_eq!(hit.reasoning, "catalog reasoning");
        assert!(hit.context.is_none());
        assert!(hit.catalog_severity.is_some());
    }

    /// A compare site arms the gate for the very next conditional jump.
    #[test]
    fn compare_site_arms_the_gate_directly() {
        // a 0xCC scan: the site itself is the compare, the next Jcc gates
        let insns = vec![
            insn(0x6000, "CMP", &["AL", "0xcc"]),
            insn(0x6002, "JZ", &["0x6100"]),
            insn(0x6004, "RET", &[]),
        ];
        let mut hit = DetectionHit::new(
            TECH,
            0x6000,
            Some(0x6000),
            medium(),
            "0xCC compare @ 0x6000".to_string(),
            "scan reasoning".to_string(),
        )
        .kind(EvidenceKind::ControlFlowGated);
        validate_hit(&mut hit, &listings(0x6000, &insns), true);
        // the gate is recorded; with nothing withheld the severity stands
        assert_eq!(hit.context, Some(CallContext::GatesBranch));
        assert!(matches!(hit.confidence, Confidence::Unsure));
    }

    /// The MoveFileEx flag mask confirms on the delay-until-reboot bit.
    #[test]
    fn move_file_flags_confirm_on_the_reboot_bit() {
        // x86 staging: dwFlags (4) pushes first, then the two path pointers
        let insns = vec![
            insn(0x7000, "PUSH", &["0x4"]),
            insn(0x7002, "PUSH", &["0x0"]),
            insn(0x7004, "PUSH", &["0x403000"]),
            insn(0x7006, "CALL", &["dword ptr [0x5000]"]),
            insn(0x700c, "RET", &[]),
        ];
        let mut hit = api_hit("MoveFileEx", EvidenceKind::ContextOnly, 0x7000, 0x7006);
        validate_hit(&mut hit, &listings(0x7000, &insns), false);
        // the flag bit is in-set: catalog severity restored and flag decoded
        assert!(matches!(hit.confidence, Confidence::Likely));
        assert!(
            hit.content
                .ends_with("— dwFlags=0x4 (MOVEFILE_DELAY_UNTIL_REBOOT)"),
            "{}",
            hit.content
        );
    }

    /// The out-parameter consuming shape earns the gate and is never weakened.
    #[test]
    fn out_param_overwrite_then_compare_gates() {
        // the common shape: the out-parameter is read back over the success
        // flag (an accumulator overwrite), then compared and branched on
        let insns = vec![
            insn(0x8000, "CALL", &["qword ptr [0x5000]"]),
            insn(0x8006, "MOV", &["EAX", "dword ptr [ebp + -0x8]"]),
            insn(0x8009, "CMP", &["EAX", "0x100"]),
            insn(0x800c, "JNZ", &["0x8100"]),
            insn(0x800e, "RET", &[]),
        ];
        let mut hit = api_hit("GetCursorPos", EvidenceKind::ApiResultCheck, 0x8000, 0x8000);
        validate_hit(&mut hit, &listings(0x8000, &insns), true);
        // the compare+branch is the gate: context noted, catalog severity back
        assert_eq!(hit.context, Some(CallContext::GatesBranch));
        assert!(matches!(hit.confidence, Confidence::Likely));
        assert_eq!(hit.suspicion, 75);
    }

    /// An out-parameter API with no local compare stays clamped, not discarded.
    #[test]
    fn out_param_result_is_never_discarded() {
        // the accumulator overwrite that would prove a discard for a plain API
        let insns = vec![
            insn(0x8000, "CALL", &["qword ptr [0x5000]"]),
            insn(0x8006, "MOV", &["EAX", "0x1"]),
            insn(0x800b, "RET", &[]),
        ];
        let mut hit = api_hit("GetCursorPos", EvidenceKind::ApiResultCheck, 0x8000, 0x8000);
        validate_hit(&mut hit, &listings(0x8000, &insns), true);
        // only the success flag was dropped: the clamp stands, nothing weakens
        assert!(matches!(hit.confidence, Confidence::Unsure));
        assert_eq!(hit.suspicion, 40);
        assert!(hit.context.is_none());
        assert!(hit.catalog_severity.is_some());
    }

    /// The same overwrite shape still discards a plain accumulator API.
    #[test]
    fn accumulator_overwrite_still_discards_plain_apis() {
        // identical shape to the out-param gate test, but the result of this
        // API lives in the accumulator, so the overwrite provably discards it
        let insns = vec![
            insn(0x8000, "CALL", &["qword ptr [0x5000]"]),
            insn(0x8006, "MOV", &["EAX", "dword ptr [ebp + -0x8]"]),
            insn(0x8009, "CMP", &["EAX", "0x100"]),
            insn(0x800c, "JNZ", &["0x8100"]),
            insn(0x800e, "RET", &[]),
        ];
        let mut hit = api_hit(
            "CheckRemoteDebuggerPresent",
            EvidenceKind::ApiResultCheck,
            0x8000,
            0x8000,
        );
        validate_hit(&mut hit, &listings(0x8000, &insns), true);
        // one step below the clamped medium, with no gate context attached
        assert!(matches!(hit.confidence, Confidence::Untrusted));
        assert_eq!(hit.suspicion, 25);
        assert!(hit.context.is_none());
    }

    /// The x86 push walk reaches NtSetInformationFile's 5th-argument class.
    #[test]
    fn file_information_class_recovers_from_the_x86_push_walk() {
        // right-to-left staging: FileInformationClass (0xd) pushes first, then
        // Length, FileInformation, IoStatusBlock, and FileHandle
        let insns = vec![
            insn(0x9000, "PUSH", &["0xd"]),
            insn(0x9002, "PUSH", &["0x1"]),
            insn(0x9004, "PUSH", &["0x403000"]),
            insn(0x9006, "PUSH", &["0x403010"]),
            insn(0x9008, "PUSH", &["-0x1"]),
            insn(0x900a, "CALL", &["dword ptr [0x5000]"]),
            insn(0x9010, "RET", &[]),
        ];
        let mut hit = api_hit(
            "NtSetInformationFile",
            EvidenceKind::ContextOnly,
            0x9000,
            0x900a,
        );
        validate_hit(&mut hit, &listings(0x9000, &insns), false);
        // delete-on-close is in-set: catalog severity restored and decoded
        assert!(matches!(hit.confidence, Confidence::Likely));
        assert!(
            hit.content.contains("— FileInformationClass=0xd"),
            "{}",
            hit.content
        );
        assert!(
            hit.content
                .ends_with("(FileDispositionInformation (delete-on-close))"),
            "{}",
            hit.content
        );
    }

    /// An out-of-set NtSetInformationFile class rewords and weakens the hit.
    #[test]
    fn file_information_class_out_of_set_rewords() {
        // FileEndOfFileInformation (0x14) is a plain file-size write
        let insns = vec![
            insn(0x9000, "PUSH", &["0x14"]),
            insn(0x9002, "PUSH", &["0x1"]),
            insn(0x9004, "PUSH", &["0x403000"]),
            insn(0x9006, "PUSH", &["0x403010"]),
            insn(0x9008, "PUSH", &["-0x1"]),
            insn(0x900a, "CALL", &["dword ptr [0x5000]"]),
            insn(0x9010, "RET", &[]),
        ];
        let mut hit = api_hit(
            "NtSetInformationFile",
            EvidenceKind::ContextOnly,
            0x9000,
            0x900a,
        );
        validate_hit(&mut hit, &listings(0x9000, &insns), false);
        // out-of-set: one step below the clamped medium, with honest wording
        assert!(matches!(hit.confidence, Confidence::Untrusted));
        assert_eq!(hit.suspicion, 25);
        assert!(
            hit.reasoning.contains("FileInformationClass=0x14"),
            "{}",
            hit.reasoning
        );
        assert!(
            hit.reasoning
                .contains("not a rename/delete/timestamp information class"),
            "{}",
            hit.reasoning
        );
    }

    /// On x64 the 5th argument is a stack slot, so the clamp simply stands.
    #[test]
    fn file_information_class_is_not_recovered_on_x64() {
        // x64 staging reaches only the register arguments; the class is at
        // [rsp + 0x20], which the register walk cannot see
        let insns = vec![
            insn(0x9000, "MOV", &["EDX", "0x403000"]),
            insn(0x9004, "CALL", &["qword ptr [0x5000]"]),
            insn(0x900a, "RET", &[]),
        ];
        let mut hit = api_hit(
            "NtSetInformationFile",
            EvidenceKind::ContextOnly,
            0x9000,
            0x9004,
        );
        validate_hit(&mut hit, &listings(0x9000, &insns), true);
        // nothing recovered: the presence-only clamp stays exactly as seeded
        assert!(matches!(hit.confidence, Confidence::Unsure));
        assert_eq!(hit.suspicion, 40);
        assert_eq!(hit.reasoning, "catalog reasoning");
        assert!(hit.catalog_severity.is_some());
    }

    /// The Toolhelp flags confirm on the process-snapshot bit.
    #[test]
    fn toolhelp_flags_confirm_on_the_process_bit() {
        // x64 staging: dwFlags is the first argument, in ECX
        let insns = vec![
            insn(0xa000, "MOV", &["ECX", "0x2"]),
            insn(0xa004, "CALL", &["qword ptr [0x5000]"]),
            insn(0xa00a, "RET", &[]),
        ];
        let mut hit = api_hit(
            "CreateToolhelp32Snapshot",
            EvidenceKind::ContextOnly,
            0xa000,
            0xa004,
        );
        validate_hit(&mut hit, &listings(0xa000, &insns), true);
        // the process bit is in-set: catalog severity restored and decoded
        assert!(matches!(hit.confidence, Confidence::Likely));
        assert!(
            hit.content.ends_with("— dwFlags=0x2 (TH32CS_SNAPPROCESS)"),
            "{}",
            hit.content
        );
    }

    /// A module-only Toolhelp snapshot rewords and weakens the hit.
    #[test]
    fn toolhelp_module_snapshot_rewords() {
        // x86 staging: a module snapshot (0x8) never lists processes
        let insns = vec![
            insn(0xa000, "PUSH", &["0x0"]),
            insn(0xa002, "PUSH", &["0x8"]),
            insn(0xa004, "CALL", &["dword ptr [0x5000]"]),
            insn(0xa00a, "RET", &[]),
        ];
        let mut hit = api_hit(
            "CreateToolhelp32Snapshot",
            EvidenceKind::ContextOnly,
            0xa000,
            0xa004,
        );
        validate_hit(&mut hit, &listings(0xa000, &insns), false);
        // out-of-set: one step below the clamped medium, with honest wording
        assert!(matches!(hit.confidence, Confidence::Untrusted));
        assert_eq!(hit.suspicion, 25);
        assert!(
            hit.reasoning.contains("dwFlags=0x8 (TH32CS_SNAPMODULE)"),
            "{}",
            hit.reasoning
        );
        assert!(
            hit.reasoning
                .contains("not the process-snapshot flag (TH32CS_SNAPPROCESS)"),
            "{}",
            hit.reasoning
        );
    }

    /// An out-of-set GetSystemMetrics index renders its decoded SM_ name.
    #[test]
    fn metrics_out_of_set_renders_the_decoded_name() {
        // SM_MIDEASTENABLED (0x4a) is a locale flag, not a sandbox probe
        let insns = vec![
            insn(0xb000, "MOV", &["ECX", "0x4a"]),
            insn(0xb004, "CALL", &["qword ptr [0x5000]"]),
            insn(0xb00a, "RET", &[]),
        ];
        let mut hit = api_hit(
            "GetSystemMetrics",
            EvidenceKind::ApiResultCheck,
            0xb000,
            0xb004,
        );
        validate_hit(&mut hit, &listings(0xb000, &insns), true);
        // the reworded reasoning names the metric instead of bare hex
        assert!(matches!(hit.confidence, Confidence::Untrusted));
        assert!(
            hit.reasoning.contains("index=0x4a (SM_MIDEASTENABLED)"),
            "{}",
            hit.reasoning
        );
        assert!(
            hit.reasoning
                .contains("not a sandbox-relevant metric probe"),
            "{}",
            hit.reasoning
        );
    }
}
